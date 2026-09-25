//! `Teach { item, member }`: teach a TM or HM from the TM CASE, the way a
//! player does, closed-loop on every screen:
//!
//! Start → BAG → KEY ITEMS → TM CASE (OPEN) → the TM/HM → USE → "Teach
//! which POKéMON?" (every panel reads ABLE!/NOT ABLE!) → the member →
//! then the game's text: "<mon> learned <move>!" into a free slot, or
//! "<mon> wants to learn the move … Should a move be deleted and replaced
//! with …?" → YES → the KNOWN MOVES list → the move to forget → "Poof! …
//! forgot <old>. And… <mon> learned <move>!". Party knowledge changes only
//! when the "learned" page is read: `MoveLearned` / `MoveReplaced` (with
//! the new move's full PP). A taught TM is used up (`ItemsChanged`); HMs
//! are not.
//!
//! Which move to forget (four known): the lowest-value one by game data
//! (power × accuracy with STAB, status utility, and the super-effective
//! coverage only it brings), never an HM move (the game refuses) and never
//! the last damaging move.

use std::sync::Arc;

use pokebot_core::Button;
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, GameState, Observation, Pocket};

use super::menu::{
    open_start_menu, pick_row, Closer, MenuRow, Retries, CURSOR_FRAMES, SCREEN_FRAMES,
};
use super::{
    progress, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome, ToolStep,
};
use crate::bag::{fits, pocket_from_title, pocket_index, POCKETS};
use crate::learn::{parse, TextFact};
use crate::party::display_name;
use crate::{Action, Decision, Expectation, Outcome};

/// The KNOWN MOVES list's row for the offered move (picking it gives up).
const NEW_MOVE_ROW: u8 = 4;
/// Inputs inside the TM CASE before the item counts as missing.
const MAX_CASE_PRESSES: u32 = 24;

/// FireRed's TMs and HMs in item order (`include/constants/items.h`,
/// `ITEM_TM01_FOCUS_PUNCH` …, `ITEM_HM08_DIVE`).
const TM_MOVES: [&str; 50] = [
    "FOCUS_PUNCH",
    "DRAGON_CLAW",
    "WATER_PULSE",
    "CALM_MIND",
    "ROAR",
    "TOXIC",
    "HAIL",
    "BULK_UP",
    "BULLET_SEED",
    "HIDDEN_POWER",
    "SUNNY_DAY",
    "TAUNT",
    "ICE_BEAM",
    "BLIZZARD",
    "HYPER_BEAM",
    "LIGHT_SCREEN",
    "PROTECT",
    "RAIN_DANCE",
    "GIGA_DRAIN",
    "SAFEGUARD",
    "FRUSTRATION",
    "SOLAR_BEAM",
    "IRON_TAIL",
    "THUNDERBOLT",
    "THUNDER",
    "EARTHQUAKE",
    "RETURN",
    "DIG",
    "PSYCHIC",
    "SHADOW_BALL",
    "BRICK_BREAK",
    "DOUBLE_TEAM",
    "REFLECT",
    "SHOCK_WAVE",
    "FLAMETHROWER",
    "SLUDGE_BOMB",
    "SANDSTORM",
    "FIRE_BLAST",
    "ROCK_TOMB",
    "AERIAL_ACE",
    "TORMENT",
    "FACADE",
    "SECRET_POWER",
    "REST",
    "ATTRACT",
    "THIEF",
    "STEEL_WING",
    "SKILL_SWAP",
    "SNATCH",
    "OVERHEAT",
];
const HM_MOVES: [&str; 8] = [
    "MOVE_CUT",
    "MOVE_FLY",
    "MOVE_SURF",
    "MOVE_STRENGTH",
    "MOVE_FLASH",
    "MOVE_ROCK_SMASH",
    "MOVE_WATERFALL",
    "MOVE_DIVE",
];

/// The move a TM or HM item teaches (`ITEM_TM39` → `MOVE_ROCK_TOMB`).
pub fn taught_move(item: &str) -> Option<String> {
    if let Some(n) = item.strip_prefix("ITEM_TM") {
        let n: usize = n.parse().ok()?;
        return TM_MOVES.get(n.checked_sub(1)?).map(|m| format!("MOVE_{m}"));
    }
    let n: usize = item.strip_prefix("ITEM_HM")?.parse().ok()?;
    HM_MOVES.get(n.checked_sub(1)?).map(|m| (*m).to_owned())
}

pub fn is_hm_move(mv: &str) -> bool {
    HM_MOVES.contains(&mv)
}

/// Value of keeping `mv` next to `others`: damage (power × accuracy, ×1.5
/// with STAB), status utility, and the super-effective coverage only it
/// brings.
fn move_value(data: &GameData, types: &[String], mv: &str, others: &[&str]) -> u64 {
    let Some(m) = data.move_(mv) else {
        return 0;
    };
    let stab = m.kind.as_ref().is_some_and(|k| types.contains(k));
    let damage = u64::from(m.power) * u64::from(m.accuracy);
    let damage = if stab { damage * 3 / 2 } else { damage };
    let utility: u64 = match m.effect.as_deref().unwrap_or("") {
        "EFFECT_SLEEP" => 4,
        "EFFECT_PARALYZE" => 3,
        "EFFECT_POISON" | "EFFECT_TOXIC" | "EFFECT_LEECH_SEED" => 2,
        e if e.ends_with("_DOWN") || e.ends_with("_DOWN_2") => 1,
        _ => 0,
    };
    let mut coverage = 0u64;
    if m.power > 0 {
        if let Some(kind) = &m.kind {
            let mut defenders: Vec<&String> = data.type_chart.iter().map(|(_, t, _)| t).collect();
            defenders.sort();
            defenders.dedup();
            for t in defenders {
                let one = std::slice::from_ref(t);
                if data.effectiveness(kind, one) <= 10 {
                    continue;
                }
                let covered = others.iter().any(|o| {
                    data.move_(o).is_some_and(|om| {
                        om.power > 0
                            && om
                                .kind
                                .as_ref()
                                .is_some_and(|k| data.effectiveness(k, one) > 10)
                    })
                });
                coverage += u64::from(!covered);
            }
        }
    }
    damage + utility * 2500 + coverage * 1500
}

/// The slot to forget so that `species` (knowing `moves`) learns
/// `new_move`: the lowest-value move, never an HM move, never the last
/// damaging move. `None` when nothing may go.
pub fn forget_slot(
    data: &GameData,
    species: &str,
    moves: &[String],
    new_move: &str,
) -> Option<usize> {
    let types = data
        .species(species)
        .map(|s| s.types.clone())
        .unwrap_or_default();
    let damaging = |mv: &str| data.move_(mv).is_some_and(|m| m.power > 0);
    let mut best: Option<(u64, usize)> = None;
    for (slot, mv) in moves.iter().enumerate() {
        if is_hm_move(mv) {
            continue;
        }
        let kept: Vec<&str> = moves
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != slot)
            .map(|(_, m)| m.as_str())
            .chain(std::iter::once(new_move))
            .collect();
        if !kept.iter().any(|m| damaging(m)) {
            continue;
        }
        let v = move_value(data, &types, mv, &kept);
        if best.is_none_or(|(b, _)| v < b) {
            best = Some((v, slot));
        }
    }
    best.map(|(_, slot)| slot)
}

/// The party slot `member` names: `lead`, `slot:N`, or a species constant
/// (the first member of that species).
pub fn member_slot(state: &GameState, member: &str) -> Option<u8> {
    if member == "lead" {
        return Some(0);
    }
    if let Some(n) = member.strip_prefix("slot:") {
        return n.parse().ok();
    }
    state
        .party
        .value
        .as_ref()?
        .iter()
        .position(|m| m.species.value.as_deref() == Some(member))
        .and_then(|i| u8::try_from(i).ok())
}

/// What the teaching came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taught {
    /// "<mon> learned <move>!" was read.
    Learned,
    /// "<mon> already knows <move>."
    AlreadyKnown,
}

/// The step: from the overworld to "learned" and back.
pub struct TeachStep {
    item: String,
    mv: String,
    slot: u8,
    species: String,
    /// The member's moves in menu order (party knowledge, then what the
    /// KNOWN MOVES list shows).
    moves: Vec<String>,
    data: Arc<GameData>,
    retries: Retries,
    closer: Closer,
    case_presses: u32,
    /// The slot chosen to forget (`None`: nothing may go).
    forget: Option<usize>,
    /// "… forgot <old>." read: (move slot, old move).
    forgot: Option<(usize, String)>,
    /// How it ended (closing the menus follows).
    outcome: Option<Result<Taught, String>>,
    last_page: String,
    /// Pages read, for the log.
    pub pages: Vec<String>,
}

impl TeachStep {
    pub fn new(
        data: Arc<GameData>,
        item: &str,
        slot: u8,
        state: &GameState,
    ) -> Result<Self, ToolError> {
        let mv = taught_move(item)
            .ok_or_else(|| ToolError::Failed(format!("{item} is not a TM or HM")))?;
        let mon = state
            .party
            .value
            .as_ref()
            .and_then(|p| p.get(usize::from(slot)));
        let species = mon
            .and_then(|m| m.species.value.clone())
            .unwrap_or_default();
        let moves: Vec<String> = mon
            .map(|m| {
                m.moves
                    .iter()
                    .flatten()
                    .filter_map(|s| s.mv.value.clone())
                    .collect()
            })
            .unwrap_or_default();
        let forget = forget_slot(&data, &species, &moves, &mv);
        Ok(Self {
            item: item.to_owned(),
            mv,
            slot,
            species,
            moves,
            data,
            retries: Retries::default(),
            closer: Closer::default(),
            case_presses: 0,
            forget,
            forgot: None,
            outcome: None,
            last_page: String::new(),
            pages: Vec::new(),
        })
    }

    pub fn outcome(&self) -> Option<&Result<Taught, String>> {
        self.outcome.as_ref()
    }

    fn move_name(&self) -> String {
        self.data
            .move_(&self.mv)
            .and_then(|m| m.name.clone())
            .unwrap_or_else(|| self.mv.trim_start_matches("MOVE_").replace('_', " "))
    }

    /// A move name as read, to its constant ("forgot how to use X" too).
    fn resolve(&self, read: &str) -> Option<String> {
        let read = read.trim().trim_start_matches("how to use ").trim();
        self.data.move_named(read).map(str::to_owned)
    }

    /// Interprets a fully printed page once.
    fn page(&mut self, lines: &[String], events: &mut Vec<GameEvent>) {
        let page = lines.join(" ");
        if page.is_empty() || page == self.last_page {
            return;
        }
        // A reading still printing grows into the next one.
        if !self.last_page.is_empty() && page.starts_with(&self.last_page) {
            self.pages.pop();
        }
        self.last_page = page.clone();
        self.pages.push(page.clone());
        if page.contains("not compatible") || page.contains("can't be learned") {
            self.outcome = Some(Err(format!("{} can't learn {}", self.species, self.mv)));
            return;
        }
        if page.contains("already knows") && page.contains(&self.move_name()) {
            self.outcome = Some(Ok(Taught::AlreadyKnown));
            return;
        }
        if page.contains("did not learn") {
            self.outcome = Some(Err(format!("{} did not learn {}", self.species, self.mv)));
            return;
        }
        for fact in parse(&page) {
            match fact {
                TextFact::Forgot { mv, .. } => {
                    if let Some(old) = self.resolve(&mv) {
                        if let Some(at) = self.moves.iter().position(|m| *m == old) {
                            self.forgot = Some((at, old));
                        }
                    }
                }
                TextFact::Learned { mv, .. } => {
                    if self.resolve(&mv).as_deref() != Some(self.mv.as_str()) {
                        continue;
                    }
                    let max_pp = self.data.move_(&self.mv).map_or(0, |m| m.pp);
                    match self.forgot.take() {
                        Some((at, old)) => events.push(GameEvent::MoveReplaced {
                            slot: self.slot,
                            move_slot: at as u8,
                            old,
                            new: self.mv.clone(),
                            max_pp,
                        }),
                        None if self.moves.len() < 4 => events.push(GameEvent::MoveLearned {
                            slot: self.slot,
                            move_slot: self.moves.len() as u8,
                            mv: self.mv.clone(),
                            max_pp,
                        }),
                        None => {
                            // Learned over a forgotten move whose page was
                            // missed: the moves must be read again.
                            events.push(super::progress(
                                "Teach",
                                format!("learned {} but the forgotten move was not read", self.mv),
                            ));
                        }
                    }
                    self.outcome = Some(Ok(Taught::Learned));
                }
                _ => {}
            }
        }
    }

    /// The answer to a YES/NO about the move: YES to replacing a move when
    /// one may go, YES to giving up when none may.
    fn answer(&self, question: &str) -> bool {
        if question.contains("deleted") || question.contains("Delete an older move") {
            return self.forget.is_some();
        }
        if question.contains("Stop trying to teach") || question.contains("Stop learning") {
            return self.forget.is_none();
        }
        false
    }

    fn close(&mut self, o: &Observation) -> Decision {
        let Some(outcome) = self.outcome.clone() else {
            return Decision::Fail("closing without an outcome".into());
        };
        if let Some(d) = &o.dialogue {
            if o.menu.is_none() {
                return crate::new_game::advance_or_wait(Some(d), "reading the last page");
            }
        }
        match self.closer.next(o, "back in the overworld") {
            Decision::Done(_) => match outcome {
                Ok(Taught::Learned) => Decision::Done(format!("taught {}", self.mv)),
                Ok(Taught::AlreadyKnown) => Decision::Done(format!("already knew {}", self.mv)),
                Err(why) => Decision::Fail(why),
            },
            d => d,
        }
    }

    fn party(&mut self, o: &Observation, party: &pokebot_state::PartyMenuObservation) -> Decision {
        self.retries.enter("party");
        if !party.options.is_empty() {
            return self.retries.act(
                "close an action window",
                Button::B,
                Expectation::PartyList,
                SCREEN_FRAMES,
            );
        }
        if party.able.get(usize::from(self.slot)).copied().flatten() == Some(false) {
            self.outcome = Some(Err(format!(
                "slot {} ({}) is NOT ABLE to learn {}",
                self.slot, self.species, self.mv
            )));
            return self.retries.act(
                "back out: not able",
                Button::B,
                Expectation::InputsDone,
                SCREEN_FRAMES,
            );
        }
        if self.slot >= party.count {
            self.outcome = Some(Err(format!("slot {} is empty", self.slot)));
            return self.retries.act(
                "back out: no such member",
                Button::B,
                Expectation::InputsDone,
                SCREEN_FRAMES,
            );
        }
        let Some(at) = party.selected else {
            return self.retries.wait(o, "reading the selected member");
        };
        if at == self.slot {
            return self.retries.act(
                format!("teach slot {}", self.slot),
                Button::A,
                Expectation::DialogueOpen,
                SCREEN_FRAMES,
            );
        }
        let (button, next) = if at < self.slot {
            (Button::Down, at + 1)
        } else {
            (Button::Up, at - 1)
        };
        self.retries.act(
            format!("party: {button:?} toward slot {}", self.slot),
            button,
            Expectation::PartySelected(next),
            CURSOR_FRAMES,
        )
    }

    fn move_list(
        &mut self,
        list: &pokebot_state::MoveListObservation,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        self.retries.enter("known moves");
        // The list shows the real moves: adopt them when they read cleanly.
        let shown: Option<Vec<String>> = list
            .moves
            .iter()
            .take(4)
            .map(|n| self.data.move_named(n).map(str::to_owned))
            .collect();
        if let Some(shown) = shown.filter(|s| s.len() == 4 && *s != self.moves) {
            events.push(GameEvent::MovesObserved {
                slot: self.slot,
                moves: shown.clone(),
            });
            self.moves = shown;
            self.forget = forget_slot(&self.data, &self.species, &self.moves, &self.mv);
        }
        let target = self.forget.map_or(NEW_MOVE_ROW, |s| s as u8);
        let Some(at) = list.selected else {
            return Decision::Wait("move list: no selection yet".into());
        };
        if at == target {
            return self.retries.act(
                format!(
                    "forget row {} ({})",
                    target + 1,
                    list.moves.get(usize::from(target)).map_or("?", |s| s)
                ),
                Button::A,
                Expectation::MoveListClosed,
                SCREEN_FRAMES,
            );
        }
        let (button, next) = if at < target {
            (Button::Down, at + 1)
        } else {
            (Button::Up, at - 1)
        };
        self.retries.act(
            format!("move list: {button:?} toward row {}", target + 1),
            button,
            Expectation::MoveListAt(next),
            CURSOR_FRAMES,
        )
    }

    fn bag(&mut self, o: &Observation, bag: &pokebot_state::BagObservation) -> Decision {
        match pocket_from_title(&bag.pocket) {
            Some(Pocket::TmCase) => {
                self.retries.enter("tm case");
                self.case_presses += 1;
                if self.case_presses > MAX_CASE_PRESSES {
                    self.outcome = Some(Err(format!("{} is not in the TM CASE", self.item)));
                    return Decision::Wait("closing".into());
                }
                if bag.prompt.is_none()
                    && bag.rows.iter().any(|(n, _)| n == "CANCEL")
                    && bag.rows.len() < 5
                    && !bag
                        .rows
                        .iter()
                        .any(|(n, _)| self.data.item_named(n) == Some(self.item.as_str()))
                {
                    // The whole case is on screen and the item isn't in it.
                    self.outcome = Some(Err(format!("{} is not in the TM CASE", self.item)));
                    return Decision::Wait("closing".into());
                }
                crate::bag::select_item(o, &self.data, &self.item, Expectation::PartyList)
            }
            Some(Pocket::KeyItems) => {
                self.retries.enter("key items");
                crate::bag::select_item(
                    o,
                    &self.data,
                    "ITEM_TM_CASE",
                    Expectation::BagPocket("TM CASE".into()),
                )
            }
            Some(other) => {
                self.retries.enter("pocket");
                let (Some(at), Some(target)) =
                    (pocket_index(other), pocket_index(Pocket::KeyItems))
                else {
                    return self.retries.act(
                        "leave the pocket",
                        Button::B,
                        Expectation::InputsDone,
                        SCREEN_FRAMES,
                    );
                };
                let (button, next) = if at < target {
                    (Button::Right, at + 1)
                } else {
                    (Button::Left, at - 1)
                };
                self.retries.act(
                    format!("bag: {button:?} to {}", POCKETS[next].1),
                    button,
                    Expectation::BagPocket(POCKETS[next].1.to_owned()),
                    SCREEN_FRAMES,
                )
            }
            None => self.retries.wait(o, "reading the pocket title"),
        }
    }
}

impl ToolStep for TeachStep {
    fn expects(&self) -> Expects {
        Expects::MENUS
    }

    fn on_outcome(&mut self, _: &Action, outcome: Outcome, _: &mut StepContext<'_>) {
        if outcome != Outcome::Confirmed {
            self.retries.failed();
        }
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if self.outcome.is_some() {
            if let Some(d) = &o.dialogue {
                let lines = d.lines.clone();
                if d.ready_for_a() {
                    self.page(&lines, ctx.events);
                }
            }
            return self.close(o);
        }
        if self.retries.exhausted() {
            return Decision::Fail(format!(
                "teaching {}: no progress in {}",
                self.item,
                self.retries.phase()
            ));
        }
        if let Some(list) = &o.move_list {
            return self.move_list(list, ctx.events);
        }
        if let (Some(d), Some(menu)) = (&o.dialogue, &o.menu) {
            let yes = o.menu_lines.iter().position(|l| fits("YES", l));
            let no = o.menu_lines.iter().position(|l| fits("NO", l));
            if let (Some(yes), Some(no)) = (yes, no) {
                self.retries.enter("question");
                if d.stable_frames < super::field::QUESTION_PRINTED_FRAMES {
                    return Decision::Wait("the question is printing".into());
                }
                let question = d.lines.join(" ");
                let answer = self.answer(&question);
                let row = if answer { yes } else { no } as u8;
                return crate::new_game::select(
                    menu,
                    row,
                    if answer { "answer YES" } else { "answer NO" },
                );
            }
        }
        if let Some(d) = &o.dialogue {
            self.retries.enter("text");
            if d.ready_for_a() {
                let lines = d.lines.clone();
                self.page(&lines, ctx.events);
                if self.outcome.is_some() {
                    return self.close(o);
                }
            }
            return crate::new_game::advance_or_wait(Some(d), "reading");
        }
        if let Some(party) = &o.party_menu {
            let party = party.clone();
            return self.party(o, &party);
        }
        if let Some(bag) = &o.bag {
            let bag = bag.clone();
            return self.bag(o, &bag);
        }
        if let Some(menu) = &o.menu {
            self.retries.enter("start menu");
            return pick_row(
                &mut self.retries,
                o,
                menu,
                &MenuRow::Text("BAG"),
                ctx.state,
                Expectation::BagPocket(String::new()),
                SCREEN_FRAMES,
            )
            .0;
        }
        if o.summary.is_some() {
            return self.retries.act(
                "leave the summary",
                Button::B,
                Expectation::InputsDone,
                SCREEN_FRAMES,
            );
        }
        self.retries.enter("open");
        open_start_menu(&mut self.retries, o, ctx.quiet_frames)
    }
}

pub struct TeachTool;

/// Teaches `item` to the party member `member` names.
pub fn teach(ctx: &mut ToolContext<'_>, item: &str, member: &str) -> Result<Taught, ToolError> {
    let slot = member_slot(ctx.state(), member)
        .ok_or_else(|| ToolError::Failed(format!("no party member matches {member}")))?;
    let mut step = TeachStep::new(Arc::clone(&ctx.data), item, slot, ctx.state())?;
    ctx.info(format!(
        "teach: {item} ({}) to slot {slot} ({}), moves {:?}; would forget {:?}",
        step.mv,
        display_name(&step.species),
        step.moves,
        step.forget.and_then(|s| step.moves.get(s))
    ));
    ctx.drive(&mut step)?;
    let taught = match step.outcome() {
        Some(Ok(t)) => t.clone(),
        Some(Err(why)) => return Err(ToolError::Failed(why.clone())),
        None => {
            return Err(ToolError::Failed(
                "teaching ended without an outcome".into(),
            ))
        }
    };
    if taught == Taught::Learned && item.starts_with("ITEM_TM") {
        // FireRed's TMs break after one use.
        ctx.emit(GameEvent::ItemsChanged {
            pocket: Pocket::TmCase,
            item: item.to_owned(),
            delta: -1,
            reason: format!("taught to slot {slot}"),
        })?;
    }
    ctx.emit(progress(
        "Teach",
        format!("{item} to slot {slot}: {taught:?}; pages {:?}", step.pages),
    ))?;
    Ok(taught)
}

impl Tool for TeachTool {
    fn name(&self) -> &str {
        "Teach"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Teach { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Teach { item, member } = intent else {
            return ToolOutcome::failed("not a Teach");
        };
        teach(ctx, item, member).map(|_| ()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_core::ControllerCommand;
    use pokebot_state::{
        BagObservation, DialogueKind, DialogueObservation, Knowledge, MenuObservation,
        MoveListObservation, MoveSlot, Observed, PartyMenuObservation, PartyMon, Region,
        ScreenState,
    };

    fn data() -> Option<Arc<GameData>> {
        GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok()
        .map(Arc::new)
    }

    fn moves(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn items_map_to_their_moves() {
        assert_eq!(taught_move("ITEM_HM01").as_deref(), Some("MOVE_CUT"));
        assert_eq!(taught_move("ITEM_HM05").as_deref(), Some("MOVE_FLASH"));
        assert_eq!(
            taught_move("ITEM_TM03").as_deref(),
            Some("MOVE_WATER_PULSE")
        );
        assert_eq!(taught_move("ITEM_TM39").as_deref(), Some("MOVE_ROCK_TOMB"));
        assert_eq!(taught_move("ITEM_TM50").as_deref(), Some("MOVE_OVERHEAT"));
        assert_eq!(taught_move("ITEM_TM51"), None);
        assert_eq!(taught_move("ITEM_POTION"), None);
        let Some(data) = data() else { return };
        for n in 1..=50 {
            let mv = taught_move(&format!("ITEM_TM{n:02}")).unwrap();
            assert!(data.move_(&mv).is_some(), "{mv}");
        }
    }

    /// Ivysaur (TACKLE, SLEEP POWDER, RAZOR LEAF, VINE WHIP) learning CUT,
    /// as on the emulator: TACKLE goes (weakest damage, no STAB, no
    /// coverage); SLEEP POWDER stays for its utility.
    #[test]
    fn the_weakest_move_is_forgotten_but_never_an_hm_or_the_last_attack() {
        let Some(data) = data() else { return };
        let ivysaur = moves(&[
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_RAZOR_LEAF",
            "MOVE_VINE_WHIP",
        ]);
        assert_eq!(
            forget_slot(&data, "SPECIES_IVYSAUR", &ivysaur, "MOVE_CUT"),
            Some(0)
        );
        // HM moves can't be forgotten; with CUT first, TACKLE is still
        // the one to go.
        let with_cut = moves(&[
            "MOVE_CUT",
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_RAZOR_LEAF",
        ]);
        assert_eq!(
            forget_slot(&data, "SPECIES_IVYSAUR", &with_cut, "MOVE_FLASH"),
            Some(1)
        );
        // One damaging move among status moves, learning a status move:
        // the attack stays, a status move goes.
        let one_attack = moves(&[
            "MOVE_GROWL",
            "MOVE_TACKLE",
            "MOVE_LEECH_SEED",
            "MOVE_SLEEP_POWDER",
        ]);
        let slot = forget_slot(&data, "SPECIES_IVYSAUR", &one_attack, "MOVE_FLASH").unwrap();
        assert_ne!(slot, 1, "the last damaging move stays");
        assert_eq!(slot, 0, "GROWL is worth least");
        // All HM moves: nothing may go.
        let hms = moves(&["MOVE_CUT", "MOVE_FLASH", "MOVE_STRENGTH", "MOVE_ROCK_SMASH"]);
        assert_eq!(
            forget_slot(&data, "SPECIES_MACHOP", &hms, "MOVE_SURF"),
            None
        );
    }

    fn mon(species: &str, mv: &[&str]) -> PartyMon {
        let mut m = PartyMon {
            species: Knowledge::observed(species.to_owned(), 1),
            ..PartyMon::default()
        };
        for (i, x) in mv.iter().enumerate() {
            m.moves[i] = Some(MoveSlot {
                mv: Knowledge::observed((*x).to_owned(), 1),
                pp: Knowledge::observed((10, 10), 1),
            });
        }
        m
    }

    fn state() -> GameState {
        GameState {
            party: Knowledge::observed(
                vec![
                    mon(
                        "SPECIES_IVYSAUR",
                        &[
                            "MOVE_TACKLE",
                            "MOVE_SLEEP_POWDER",
                            "MOVE_RAZOR_LEAF",
                            "MOVE_VINE_WHIP",
                        ],
                    ),
                    mon(
                        "SPECIES_GEODUDE",
                        &["MOVE_TACKLE", "MOVE_DEFENSE_CURL", "MOVE_MUD_SPORT"],
                    ),
                ],
                1,
            ),
            ..GameState::default()
        }
    }

    #[test]
    fn members_are_found_by_species_lead_or_slot() {
        let s = state();
        assert_eq!(member_slot(&s, "SPECIES_GEODUDE"), Some(1));
        assert_eq!(member_slot(&s, "lead"), Some(0));
        assert_eq!(member_slot(&s, "slot:3"), Some(3));
        assert_eq!(member_slot(&s, "SPECIES_ZUBAT"), None);
    }

    fn bare(frame: u64) -> Observation {
        Observation::bare(
            frame,
            Observed {
                value: ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        )
    }

    fn said(frame: u64, lines: &[&str]) -> Observation {
        let mut o = bare(frame);
        o.dialogue = Some(DialogueObservation {
            kind: DialogueKind::MessageBox,
            region: Region::new(8, 118, 224, 36),
            waiting_for_input: true,
            arrow: None,
            stable_frames: 60,
            text_cells: vec![1; 8],
            lines: lines.iter().map(|s| (*s).to_owned()).collect(),
            help: false,
        });
        o
    }

    fn pressed(d: &Decision) -> Option<(Button, String)> {
        match d {
            Decision::Act(a) => match a.commands.first() {
                Some(ControllerCommand::Press(b)) => Some((*b, a.label.clone())),
                _ => None,
            },
            _ => None,
        }
    }

    /// The emulator's recorded teach of HM01 to Ivysaur, screen by screen
    /// (fixtures `emu-tm-case-hms`, `emu-tm-use-prompt`, `emu-teach-which`,
    /// `emu-teach-four-moves`, `emu-teach-replace-yes-no`,
    /// `emu-teach-known-moves`, `emu-teach-poof`, `emu-teach-learned`), as
    /// observations: every input and the resulting `MoveReplaced`.
    #[test]
    fn teaching_hm01_over_four_moves_forgets_tackle_and_replaces_it() {
        let Some(data) = data() else { return };
        let state = state();
        let mut step = TeachStep::new(Arc::clone(&data), "ITEM_HM01", 0, &state).unwrap();
        let mut events = Vec::new();
        let run = |step: &mut TeachStep, o: &Observation, events: &mut Vec<GameEvent>| {
            step.next(&mut StepContext {
                observation: o,
                state: &state,
                events,
                quiet_frames: 0,
                frame: None,
                learned: &[],
            })
        };
        // TM CASE, ▶ on TM03: up to HM01, then A.
        let mut o = bare(1);
        let rows = vec![
            ("HM01".to_owned(), Some(1)),
            ("HM05".to_owned(), Some(1)),
            ("TM03".to_owned(), Some(1)),
            ("TM39".to_owned(), Some(1)),
            ("CANCEL".to_owned(), None),
        ];
        o.bag = Some(BagObservation {
            pocket: "TM CASE".into(),
            rows: rows.clone(),
            cursor: Some(2),
            prompt: None,
        });
        assert_eq!(
            pressed(&run(&mut step, &o, &mut events)).unwrap().0,
            Button::Up
        );
        o.bag.as_mut().unwrap().cursor = Some(0);
        assert_eq!(
            pressed(&run(&mut step, &o, &mut events)).unwrap().0,
            Button::A
        );
        // USE / GIVE / EXIT: USE.
        o.bag.as_mut().unwrap().prompt = Some((moves(&["USE", "GIVE", "EXIT"]), 0));
        o.bag.as_mut().unwrap().cursor = None;
        let Decision::Act(a) = run(&mut step, &o, &mut events) else {
            panic!()
        };
        assert_eq!(a.expect, Expectation::PartyList);
        // Teach which POKéMON? Ivysaur ABLE!, already selected: A.
        let mut o = bare(2);
        o.party_menu = Some(PartyMenuObservation {
            count: 5,
            selected: Some(0),
            actions: false,
            prompt: "Teach which POKéMON?".into(),
            options: vec![],
            option_cursor: None,
            able: vec![
                Some(true),
                Some(false),
                Some(false),
                Some(true),
                Some(false),
            ],
        });
        let (b, label) = pressed(&run(&mut step, &o, &mut events)).unwrap();
        assert_eq!((b, label.as_str()), (Button::A, "teach slot 0"));
        // The pages, then the question: YES (a move may go).
        for (f, page) in [
            (3, vec!["IVYSAUR wants to learn the", "move CUT."]),
            (4, vec!["However, IVYSAUR already", "knows four moves."]),
        ] {
            let o = said(f, &page);
            assert_eq!(
                pressed(&run(&mut step, &o, &mut events)).unwrap().0,
                Button::A
            );
        }
        let mut o = said(5, &["Should a move be deleted and", "replaced with CUT?"]);
        o.menu = Some(MenuObservation {
            window: Region::new(166, 70, 50, 32),
            rows: 2,
            cursor_row: 0,
            cursor_y: 76,
        });
        o.menu_lines = moves(&["YES", "NO"]);
        let (b, label) = pressed(&run(&mut step, &o, &mut events)).unwrap();
        assert_eq!((b, label.as_str()), (Button::A, "answer YES"));
        let o = said(6, &["Which move should be forgotten?"]);
        assert_eq!(
            pressed(&run(&mut step, &o, &mut events)).unwrap().0,
            Button::A
        );
        // KNOWN MOVES, frame on TACKLE (row 1): pick it.
        let mut o = bare(7);
        o.move_list = Some(MoveListObservation {
            moves: moves(&["TACKLE", "SLEEP POWDER", "RAZOR LEAF", "VINE WHIP", "CUT"]),
            selected: Some(0),
        });
        let Decision::Act(a) = run(&mut step, &o, &mut events) else {
            panic!()
        };
        assert_eq!(a.expect, Expectation::MoveListClosed);
        assert!(events.is_empty(), "the list matched the party's moves");
        for (f, page) in [
            (8, vec!["1, 2, and … … … Poof!"]),
            (9, vec!["IVYSAUR forgot", "TACKLE."]),
            (10, vec!["And…"]),
            (11, vec!["Machine set!"]),
            (12, vec!["IVYSAUR learned", "CUT!"]),
        ] {
            let o = said(f, &page);
            let _ = run(&mut step, &o, &mut events);
        }
        assert_eq!(
            events,
            vec![GameEvent::MoveReplaced {
                slot: 0,
                move_slot: 0,
                old: "MOVE_TACKLE".into(),
                new: "MOVE_CUT".into(),
                max_pp: 30,
            }]
        );
        assert_eq!(step.outcome(), Some(&Ok(Taught::Learned)));
        // Back in the TM CASE: closing with B.
        let mut o = bare(13);
        o.bag = Some(BagObservation {
            pocket: "TM CASE".into(),
            rows,
            cursor: Some(0),
            prompt: None,
        });
        assert_eq!(
            pressed(&run(&mut step, &o, &mut events)).unwrap().0,
            Button::B
        );
    }

    /// A free slot: "GEODUDE learned ROCK TOMB!" is a `MoveLearned` into
    /// slot 3; NOT ABLE! backs out and fails.
    #[test]
    fn a_free_slot_is_learned_into_and_not_able_backs_out() {
        let Some(data) = data() else { return };
        let state = state();
        let mut events = Vec::new();
        let mut step = TeachStep::new(Arc::clone(&data), "ITEM_TM39", 1, &state).unwrap();
        let o = said(1, &["GEODUDE learned", "ROCK TOMB!"]);
        let _ = step.next(&mut StepContext {
            observation: &o,
            state: &state,
            events: &mut events,
            quiet_frames: 0,
            frame: None,
            learned: &[],
        });
        assert_eq!(
            events,
            vec![GameEvent::MoveLearned {
                slot: 1,
                move_slot: 3,
                mv: "MOVE_ROCK_TOMB".into(),
                max_pp: 10,
            }]
        );
        let mut step = TeachStep::new(Arc::clone(&data), "ITEM_TM03", 0, &state).unwrap();
        let mut o = bare(2);
        o.party_menu = Some(PartyMenuObservation {
            count: 2,
            selected: Some(0),
            actions: false,
            prompt: "Teach which POKéMON?".into(),
            options: vec![],
            option_cursor: None,
            able: vec![Some(false), Some(false)],
        });
        let d = step.next(&mut StepContext {
            observation: &o,
            state: &state,
            events: &mut events,
            quiet_frames: 0,
            frame: None,
            learned: &[],
        });
        assert_eq!(pressed(&d).unwrap().0, Button::B);
        assert!(matches!(step.outcome(), Some(Err(_))));
        // "… are not compatible." (the game's own check) fails too.
        let mut step = TeachStep::new(Arc::clone(&data), "ITEM_TM03", 0, &state).unwrap();
        let o = said(3, &["IVYSAUR and WATER PULSE", "are not compatible."]);
        let _ = step.next(&mut StepContext {
            observation: &o,
            state: &state,
            events: &mut events,
            quiet_frames: 0,
            frame: None,
            learned: &[],
        });
        assert!(matches!(step.outcome(), Some(Err(_))));
    }
}
