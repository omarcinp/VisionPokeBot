//! `Battle`: play the battle on screen to its end. Wild: the catch policy
//! decides catch, fight or flee; trainer: fight. New moves, evolution, the
//! battle bag during a throw and the Pokédex page after a catch are all
//! handled here, as in `StoryTask` (whose battle branch this copies; to be
//! removed there once the story runs on tools).

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_state::{BattleMenu, GameEvent, Observation, Status};

use super::{BattlePlan, Expects, Intent, StepContext, Tool, ToolContext, ToolOutcome, ToolStep};
use crate::battle::{self, BattleMemory, BattlePolicy};
use crate::catch;
use crate::learn::MoveLearning;
use crate::new_game::{advance_or_wait, select};
use crate::party::{self, Party};
use crate::track::TextTracker;
use crate::{Action, Decision, Expectation, Outcome};

/// Frames a page's text must stay unchanged to count as fully printed.
const PAGE_PRINTED_FRAMES: u32 = 8;
/// Longest wait for a page waiting for A to read the same on a second
/// frame.
const PAGE_READ_WAIT_FRAMES: u64 = 10;

pub struct BattleTool;

/// One battle, from its first frame to the overworld.
/// Frames our HUD must read 0 HP before the Pokémon counts as fainted.
const ZERO_HP_FRAMES: u64 = 30;

pub struct BattleStep {
    data: Arc<GameData>,
    plan: BattlePlan,
    party: Party,
    policy: BattlePolicy,
    pub memory: BattleMemory,
    learning: MoveLearning,
    tracker: TextTracker,
    lead_status: Option<Status>,
    /// The frame our HUD first read 0 HP (in a row).
    zero_hp_since: Option<u64>,
    /// Switch-training: the party slot to SHIFT to at the first command
    /// menu of a wild battle (the lead, being trained, only starts it and
    /// shares the experience); done once that member is out.
    shift_to: Option<u8>,
    shifted: bool,
    /// The party slot of our Pokémon in battle, from its HUD name (the
    /// lead until another is sent out).
    active: Option<u8>,
    /// A trainer's battle (dialogue just before it).
    trainer: bool,
    started: bool,
    in_battle: bool,
    battle_text_frame: Option<u64>,
    unread_since: Option<u64>,
    /// Trainers the moves are valued against when learning.
    upcoming: Vec<String>,
    /// The script fighting this battle goes on when it is lost (the
    /// early rival battles: `trainerbattle_earlyrival`, whose trainer has a
    /// victory line): a fainted lead is part of the story, not a failure.
    loss_ok: bool,
    /// A species hunted for: the catch policy takes it whenever our lead
    /// is safe ([`crate::catch::plan_catch_wanted`]); when the lead isn't
    /// (the catch declined or given up for its risk), RUN rather than
    /// faint it.
    spare: Option<String>,
}

impl BattleStep {
    /// `trainer`: the battle follows dialogue (a trainer's).
    pub fn new(data: Arc<GameData>, plan: BattlePlan, trainer: bool) -> Self {
        Self {
            data,
            plan,
            party: Party::default(),
            policy: BattlePolicy::default(),
            memory: BattleMemory::default(),
            learning: MoveLearning::default(),
            tracker: TextTracker::default(),
            lead_status: None,
            zero_hp_since: None,
            shift_to: None,
            shifted: false,
            active: None,
            trainer,
            started: false,
            in_battle: false,
            battle_text_frame: None,
            unread_since: None,
            upcoming: Vec::new(),
            loss_ok: false,
            spare: None,
        }
    }

    /// RUN from `species` whenever it isn't being caught ([`Self::spare`]).
    pub fn sparing(mut self, species: &str) -> Self {
        self.spare = Some(species.to_owned());
        self
    }

    /// Switch-training: at the first command menu of a wild battle, SHIFT
    /// to party slot `slot` (the lead started the battle, and shares the
    /// experience without fighting).
    pub fn shifting_to(mut self, slot: u8) -> Self {
        self.shift_to = Some(slot).filter(|s| *s != 0);
        self
    }

    /// The in-battle party screen, choosing the member to SHIFT to.
    fn shift_in_party_menu(
        &mut self,
        party: &pokebot_state::PartyMenuObservation,
        slot: u8,
        name: Option<&str>,
    ) -> Decision {
        // In battle the menu lists the party in battle order (the one out
        // first, then as switches left it; `UpdatePartyToBattleOrder`):
        // the member's row is where its name is, its slot only when no
        // name was read.
        let slot = name
            .and_then(|n| {
                party
                    .members
                    .iter()
                    .position(|r| r.nickname.as_deref() == Some(n))
            })
            .map_or(slot, |row| row as u8);
        let press = |label: String, button: Button, expect: Expectation| {
            Decision::Act(Action::new(
                label,
                vec![ControllerCommand::Press(button)],
                expect,
                60,
            ))
        };
        // The member isn't there (fleet worker 3: a WEEDLE the belief
        // still held, the battle's party showing BULBASAUR alone, and the
        // wait for a ▶ on a missing row, B, POKéMON, for 110k frames):
        // fight on as is.
        if party.count > 0 && slot >= party.count && !party.actions {
            self.shifted = true;
            return press(
                format!("shift: no slot {slot} in a party of {}, back", party.count),
                Button::B,
                Expectation::ScreenIsNot(pokebot_state::ScreenState::PartyMenu),
            );
        }
        if party.actions {
            if party.selected != Some(slot) {
                return press(
                    "shift: close another member's actions".into(),
                    Button::B,
                    Expectation::PartyList,
                );
            }
            let Some(row) = party
                .options
                .iter()
                .position(|l| crate::bag::fits("SHIFT", l))
            else {
                // No SHIFT (the member can't come out): fight on as is.
                self.shifted = true;
                return press(
                    "shift: no SHIFT, back".into(),
                    Button::B,
                    Expectation::PartyList,
                );
            };
            let Some(at) = party.option_cursor else {
                return Decision::Wait("reading the action window's ▶".into());
            };
            let row = row as u8;
            if at == row {
                return press("choose SHIFT".into(), Button::A, Expectation::InputsDone);
            }
            let (button, next) = if at < row {
                (Button::Down, at + 1)
            } else {
                (Button::Up, at - 1)
            };
            return press(
                format!("shift: {button:?} toward SHIFT"),
                button,
                Expectation::PartyOptionAt(next),
            );
        }
        let Some(at) = party.selected else {
            return Decision::Wait("reading the selected member".into());
        };
        if at == slot {
            return press(
                "shift: open the member's actions".into(),
                Button::A,
                Expectation::PartyActions,
            );
        }
        let (button, next) = if at < slot {
            (Button::Down, at + 1)
        } else {
            (Button::Up, at - 1)
        };
        press(
            format!("shift: {button:?} toward slot {slot}"),
            button,
            Expectation::PartySelected(next),
        )
    }

    /// Losing doesn't end the story ([`loss_allowed`]).
    pub fn loss_ok(mut self, loss_ok: bool) -> Self {
        self.loss_ok = loss_ok;
        self
    }

    /// Whether RUN was chosen in this battle (an escape, or its attempt).
    pub fn ran(&self) -> bool {
        self.memory.run_attempts > 0
    }

    /// The species caught in this battle, if one was.
    pub fn caught(&self) -> Option<&str> {
        self.memory.catch.caught.as_deref()
    }

    fn begin(&mut self, events: &mut Vec<GameEvent>) {
        if self.in_battle {
            return;
        }
        self.in_battle = true;
        self.started = true;
        self.memory = BattleMemory {
            trainer: self.trainer,
            ..BattleMemory::default()
        };
        self.memory.catch.wanted = self.spare.clone();
        match self.plan {
            BattlePlan::Auto => {}
            // No identification, no catch: fight (or flee) on the HUD.
            BattlePlan::Fight => self.memory.catch.decided = true,
            BattlePlan::Flee => {
                self.memory.catch.decided = true;
                self.memory.catch.flee = true;
            }
        }
        events.push(GameEvent::BattleStarted);
    }

    /// Battle text to the battle's readers, once per frame.
    fn observe_battle_text(&mut self, o: &Observation, events: &mut Vec<GameEvent>) {
        if o.battle.is_none() || self.battle_text_frame == Some(o.frame_id) {
            return;
        }
        self.battle_text_frame = Some(o.frame_id);
        if let Some(page) = catch::observe(&mut self.memory, o) {
            battle::observe_page(&mut self.memory, &page, &self.party, &self.data);
            if let Some(status) = self
                .party
                .lead()
                .and_then(|lead| battle::lead_status_text(&page, &lead.display_name()))
                .filter(|s| self.lead_status != Some(*s))
            {
                self.lead_status = Some(status);
                events.push(GameEvent::PartyObserved {
                    slot: self.active.unwrap_or(0),
                    species: None,
                    nickname: None,
                    level: None,
                    hp: None,
                    status: Some(status),
                    held_item: None,
                });
            }
        }
    }
}

impl ToolStep for BattleStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        // The Pokémon out is the policy's lead: after a faint it is
        // another member (fleet worker 2: BUBBLE, the fainted SQUIRTLE's
        // move, chosen for RATTATA, whose menu has two moves).
        let party = Party::from_state(ctx.state);
        if let Some(slot) = o
            .battle
            .as_ref()
            .and_then(|b| b.player_name.as_deref())
            .filter(|n| !n.is_empty())
            .and_then(|n| party.battler_named(&self.data, n))
        {
            self.active = Some(slot);
        }
        self.party = match self.active {
            Some(slot) => party.with_first(slot),
            None => party,
        };
        let active = usize::from(self.active.unwrap_or(0));
        self.lead_status = ctx
            .state
            .party
            .value
            .as_ref()
            .and_then(|p| p.get(active))
            .and_then(|m| m.status.value);
        let data = Arc::clone(&self.data);

        // New moves and evolution: every finished page is read, and the
        // KNOWN MOVES list and questions about moves are answered.
        if let Some(d) = o.dialogue.as_ref().filter(|d| {
            d.ready_for_a() || o.menu.is_some() || d.stable_frames >= PAGE_PRINTED_FRAMES
        }) {
            let (events, log) =
                self.learning
                    .observe_page(&d.lines, &data, &self.party, &self.upcoming);
            ctx.events.extend(events);
            for detail in log {
                ctx.events.push(super::progress("Party", detail));
            }
            let tracked = self
                .tracker
                .observe_page(&d.lines, &data, self.memory.last_slot);
            ctx.events
                .extend(tracked.into_iter().filter(crate::track::tool_emits));
            if d.ready_for_a() && o.menu.is_none() && !self.tracker.applied(&d.lines) {
                let since = *self.unread_since.get_or_insert(o.frame_id);
                if o.frame_id.saturating_sub(since) < PAGE_READ_WAIT_FRAMES {
                    if o.battle.is_some() {
                        self.begin(ctx.events);
                        self.observe_battle_text(o, ctx.events);
                    }
                    return Decision::Wait("reading the page on a second frame".into());
                }
                self.unread_since = None;
            } else {
                self.unread_since = None;
            }
        }
        if let Some(list) = &o.move_list {
            let (decision, events) =
                self.learning
                    .on_move_list(list, &data, &self.party, &self.upcoming);
            ctx.events.extend(events);
            return decision;
        }
        if let (Some(d), Some(menu)) = (&o.dialogue, &o.menu) {
            if let Some(yes) = self.learning.answer(&d.lines) {
                return select(
                    menu,
                    u8::from(!yes),
                    if yes { "learning: YES" } else { "learning: NO" },
                );
            }
        }

        if let Some(b) = &o.battle {
            self.begin(ctx.events);
            ctx.events
                .extend(party::battle_events(&data, &self.party, b));
            if self.plan == BattlePlan::Auto {
                catch::identify(
                    o,
                    &data,
                    ctx.state,
                    &self.party,
                    &mut self.memory,
                    ctx.events,
                );
            }
            self.observe_battle_text(o, ctx.events);
            if let Some(species) = spares(self.spare.as_deref(), &self.memory.catch) {
                self.memory.catch.flee = true;
                ctx.events.push(super::progress(
                    "Catch",
                    format!("not catching {species} now: running rather than fainting it"),
                ));
            }
            // 0 HP read on one frame may be the drain's digits misread
            // (fleet worker 4: a full-HP MANKEY read 0/16 as a Lv3
            // RATTATA's TACKLE landed, and the cycle ended); a faint shows
            // 0 until the fainting page.
            let zero = b.player_hp_numbers.is_some_and(|(hp, _)| hp == 0);
            let since = match (zero, self.zero_hp_since) {
                (false, _) => None,
                (true, None) => Some(o.frame_id),
                (true, s) => s,
            };
            self.zero_hp_since = since;
            if since.is_some_and(|s| o.frame_id.saturating_sub(s) >= ZERO_HP_FRAMES)
                && !self.loss_ok
            {
                return Decision::Fail(format!(
                    "our Pokémon fainted ({})",
                    b.player_name.clone().unwrap_or_default()
                ));
            }
            // A trainer's battle can't be run from: a member at risk
            // against the foe out makes way for a safer one (once a battle:
            // the battle's party menu is reordered after a switch).
            if self.memory.trainer && self.shift_to.is_none() && !self.shifted {
                if let Some(BattleMenu::Command { .. }) = b.menu {
                    if let Some(slot) =
                        battle::defensive_switch(&data, &self.party, b, self.memory.our_stages)
                    {
                        ctx.events.push(super::progress(
                            "Battle",
                            format!("switching to slot {slot}: the one out is at risk"),
                        ));
                        self.shift_to = Some(slot);
                    }
                }
            }
            // Switch-training: the member being carried started the
            // battle; the carrier comes out at the first command menu.
            if let Some(slot) = self.shift_to.filter(|_| !self.shifted) {
                if self.active == Some(slot) {
                    self.shifted = true;
                    self.memory.our_stages = [0; 6];
                    self.memory.our_accuracy = 0;
                } else if let Some(BattleMenu::Command { column, row }) = b.menu {
                    return battle::step_toward(
                        (column, row),
                        (0, 1),
                        |c, r| BattleMenu::Command { column: c, row: r },
                        "POKéMON",
                        Expectation::ScreenIsNot(pokebot_state::ScreenState::BattleCommand),
                    );
                }
            }
            if let Some(decision) = battle::decide(
                o,
                &self.policy,
                &mut self.memory,
                &self.party,
                &data,
                ctx.events,
            ) {
                return decision;
            }
            if let (Some(d), Some(_)) = (&o.dialogue, &o.menu) {
                // The box may belong to the page before (the nickname
                // question's YES/NO stays while the next page prints):
                // read the question once it is printed (flash-5: "It"
                // of "It was placed in BOX…" taken for an unknown one).
                if !d.ready_for_a() && d.stable_frames < PAGE_PRINTED_FRAMES {
                    return Decision::Wait("the question is printing".into());
                }
                let page = d.lines.join(" ");
                // After a catch: "Give a nickname to the captured X?" → No.
                if catch::is_nickname_question(&page) {
                    return Decision::Act(Action::new(
                        "nickname: NO",
                        vec![ControllerCommand::Press(Button::B)],
                        Expectation::MenuClosed,
                        90,
                    ));
                }
                // "Will RED change POKéMON?" → No (the lead fights).
                if battle::is_switch_question(&page) {
                    return Decision::Act(Action::new(
                        "change Pokémon: NO",
                        vec![ControllerCommand::Press(Button::B)],
                        Expectation::MenuClosed,
                        90,
                    ));
                }
                // The PC transfer text after NO to the nickname, with the
                // YES/NO box still drawn: plain text to advance.
                if catch::is_pc_transfer_text(&page) {
                    return advance_or_wait(o.dialogue.as_ref(), "PC transfer text");
                }
                // A is YES: never answer a question we don't understand.
                return Decision::Fail(format!("unexpected question in battle: {:?}", d.lines));
            }
            if o.dialogue.is_some() {
                return advance_or_wait(o.dialogue.as_ref(), "battle text");
            }
            return Decision::Wait("battle animation".into());
        }
        if self.in_battle && o.player.is_some() {
            self.in_battle = false;
            // Where a catch went (party or PC) is the runtime's sensor's to
            // tell, from the same pages.
            ctx.events.push(GameEvent::BattleEnded);
            return Decision::Done(match self.caught() {
                Some(species) => format!("battle over: caught {species}"),
                None => "battle over".into(),
            });
        }
        // Battle screens without the battle HUD: the Pokédex page after a
        // first catch, and the battle bag during a throw.
        if self.in_battle {
            if let Some(decision) = catch::dismiss_pokedex(&mut self.memory.catch, o) {
                return decision;
            }
            if let (Some(party), Some(slot)) =
                (&o.party_menu, self.shift_to.filter(|_| !self.shifted))
            {
                let name = ctx
                    .state
                    .party
                    .value
                    .as_ref()
                    .and_then(|p| p.get(usize::from(slot)))
                    .and_then(|m| m.nickname.value.clone())
                    .or_else(|| {
                        self.party
                            .members
                            .iter()
                            .find(|m| m.slot == slot)
                            .map(|m| m.display_name())
                    });
                return self.shift_in_party_menu(party, slot, name.as_deref());
            }
            if o.bag.is_some() || self.memory.catch.thrower.is_some() {
                return catch::in_bag(o, &data, &mut self.memory.catch, ctx.events);
            }
            if o.dialogue.is_some() {
                return advance_or_wait(o.dialogue.as_ref(), "battle text");
            }
            return Decision::Wait("battle screen".into());
        }
        if !self.started {
            return Decision::Wait("waiting for the battle".into());
        }
        Decision::Wait("battle ending".into())
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        if action.label == "choose RUN" && outcome == Outcome::Confirmed {
            self.memory.run_attempts += 1;
        }
        if action.label.starts_with("choose move") && outcome == Outcome::Confirmed {
            if let Some((slot, move_slot)) = self.memory.last_slot {
                ctx.events.push(GameEvent::MoveUsed { slot, move_slot });
            }
            self.memory.catch.on_move_confirmed();
        }
    }

    fn expects(&self) -> Expects {
        Expects::BATTLE
    }
}

impl Tool for BattleTool {
    fn name(&self) -> &str {
        "Battle"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Battle { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Battle { policy } = intent else {
            return ToolOutcome::failed("not a Battle");
        };
        let loss_ok = loss_allowed(ctx);
        if loss_ok {
            ctx.info("battle: the script goes on if it is lost");
        }
        let mut step =
            BattleStep::new(Arc::clone(&ctx.data), *policy, ctx.after_dialogue()).loss_ok(loss_ok);
        ctx.drive(&mut step).map(|_| ()).into()
    }
}

/// The hunted species to RUN from: the foe is `spare`, and the catch was
/// decided against (declined, or the attempt given up). No species hunted
/// spares nothing, and neither does a foe never identified (Switch goal
/// run, Route 22: a `Train` hunt fights without identifying, so an unknown
/// foe equalled "no species hunted" and all 1666 battles were run from).
fn spares<'a>(spare: Option<&'a str>, c: &crate::catch::CatchMemory) -> Option<&'a str> {
    let spare = spare?;
    let foe = c.foe.as_ref().map(|(s, _)| s.as_str())?;
    (c.decided && c.attempt.is_none() && c.caught.is_none() && !c.flee && foe == spare)
        .then_some(spare)
}

/// Whether the battle starting now belongs to a script that goes on when
/// it is lost: the script being run, or an armed trigger under the player,
/// with a battle whose trainer has a victory line (the decomp's
/// `trainerbattle_earlyrival`: the rival wins, gloats, and the story goes
/// on without a white-out).
pub fn loss_allowed(ctx: &ToolContext<'_>) -> bool {
    use pokebot_world::predicate::{BeliefView, Truth};
    let Some(events) = ctx.world.events() else {
        return false;
    };
    let belief = crate::belief_view::StateBelief(ctx.state());
    let mut scripts: Vec<&str> = ctx.running_script.iter().map(String::as_str).collect();
    if let Some(pose) = ctx.pose() {
        for t in events
            .triggers
            .iter()
            .filter(|t| t.map == pose.map && (t.x, t.y) == (pose.x, pose.y))
        {
            let armed = pokebot_world::route::requirement_of(&t.when)
                .is_none_or(|req| !req.iter().any(|q| belief.eval(q) == Truth::False));
            if let (true, Some(s)) = (armed, t.script.as_deref()) {
                scripts.push(s);
            }
        }
    }
    scripts.iter().filter_map(|s| events.script(s)).any(|s| {
        s.paths.iter().any(|p| {
            p.does.iter().any(|e| {
                matches!(
                    e,
                    pokebot_world::events::Effect::Battle {
                        victory: Some(_),
                        ..
                    }
                )
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catch::CatchMemory;

    fn decided(foe: Option<&str>) -> CatchMemory {
        let mut c = CatchMemory::default();
        c.decided = true;
        c.foe = foe.map(|s| (s.to_owned(), 4));
        c
    }

    #[test]
    fn only_the_hunted_species_is_run_from() {
        let pidgey = Some("SPECIES_PIDGEY");
        assert_eq!(spares(pidgey, &decided(pidgey)), pidgey);
        assert_eq!(spares(pidgey, &decided(Some("SPECIES_RATTATA"))), None);
        // Not identified (yet): nothing to spare.
        assert_eq!(spares(pidgey, &decided(None)), None);
        // Being caught, or already fleeing: nothing more to do.
        let mut fleeing = decided(pidgey);
        fleeing.flee = true;
        assert_eq!(spares(pidgey, &fleeing), None);
    }

    /// In battle the party menu is in battle order: the member to SHIFT to
    /// is the row with its name (fleet worker 6's switches went to the row
    /// numbered like its party slot).
    #[test]
    fn the_shift_goes_to_the_row_with_the_members_name() {
        let Ok(data) = pokebot_gamedata::GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let row = |name: &str| pokebot_state::PartyRowObservation {
            nickname: Some(name.into()),
            level: None,
            hp: None,
            status: None,
        };
        let mut menu = pokebot_state::PartyMenuObservation {
            count: 3,
            selected: Some(0),
            members: vec![row("RATTATA"), row("BEEDRILL"), row("PIDGEY")],
            ..Default::default()
        };
        let mut step = BattleStep::new(Arc::new(data), BattlePlan::Fight, false).shifting_to(2);
        let label = |d: Decision| match d {
            Decision::Act(a) => a.label,
            _ => String::new(),
        };
        // BEEDRILL is party slot 2 but battle row 1.
        assert_eq!(
            label(step.shift_in_party_menu(&menu, 2, Some("BEEDRILL"))),
            "shift: Down toward slot 1"
        );
        menu.selected = Some(1);
        assert_eq!(
            label(step.shift_in_party_menu(&menu, 2, Some("BEEDRILL"))),
            "shift: open the member's actions"
        );
        // No name read: the slot.
        menu.selected = Some(0);
        assert_eq!(
            label(step.shift_in_party_menu(&menu, 2, None)),
            "shift: Down toward slot 2"
        );
    }

    /// Switch goal run, Route 22: `Train(BULBASAUR to Lv10)` fights with
    /// `BattlePlan::Fight`, which never identifies the foe and hunts no
    /// species; `None == None` made every wild battle a RUN ("not catching
    /// now: running rather than fainting it", 1666 times, no experience).
    #[test]
    fn a_training_battle_spares_nothing() {
        assert_eq!(spares(None, &decided(None)), None);
        assert_eq!(spares(None, &decided(Some("SPECIES_RATTATA"))), None);
    }

    fn member(species: &str, level: u8, hp: u16, mv: &[&str]) -> pokebot_state::PartyMon {
        use pokebot_state::{Knowledge, MoveSlot, PartyMon};
        let mut m = PartyMon {
            species: Knowledge::observed(species.to_owned(), 1),
            level: Knowledge::observed(level, 1),
            hp: Knowledge::observed((hp, hp), 1),
            ..PartyMon::default()
        };
        for (i, x) in mv.iter().enumerate() {
            m.moves[i] = Some(MoveSlot {
                mv: Knowledge::observed((*x).to_owned(), 1),
                pp: Knowledge::observed((20, 20), 1),
            });
        }
        m
    }

    /// Switch goal run, Route 4: `Train(PIDGEY to Lv9)` led with a Lv3
    /// PIDGEY for IVYSAUR (slot 1) to carry, but the first command menu
    /// went Down, Right, A: RUN ("Can't escape!" three times) until
    /// SPEAROW Lv12 fainted PIDGEY. The carrier comes out: POKéMON.
    #[test]
    fn a_switch_trained_lead_shifts_to_the_carrier_at_the_first_command_menu() {
        use pokebot_state::{Knowledge, Observation};
        use pokebot_vision::{FireRedPerception, PerceptionSystem};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(data), Ok(font)) = (
            GameData::load(root.join("data/world/gamedata.json")),
            pokebot_vision::text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let data = Arc::new(data);
        let mut vision = FireRedPerception::default().with_font(Arc::new(font));
        let mut observe = |name: &str, id: u64| -> Observation {
            let image =
                pokebot_video::png::load(root.join(format!("captures/fixtures/{name}.png")))
                    .unwrap();
            let frame =
                pokebot_core::NormalizedFrame::new(id, std::time::Instant::now(), image).unwrap();
            vision.observe(&frame)
        };
        let state = pokebot_state::GameState {
            party: Knowledge::observed(
                vec![
                    member("SPECIES_PIDGEY", 3, 16, &["MOVE_TACKLE"]),
                    member(
                        "SPECIES_IVYSAUR",
                        25,
                        68,
                        &[
                            "MOVE_TACKLE",
                            "MOVE_SLEEP_POWDER",
                            "MOVE_RAZOR_LEAF",
                            "MOVE_VINE_WHIP",
                        ],
                    ),
                ],
                1,
            ),
            ..pokebot_state::GameState::default()
        };
        let mut step = BattleStep::new(Arc::clone(&data), BattlePlan::Fight, false).shifting_to(1);
        let mut events = Vec::new();
        let mut run = |o: &Observation| {
            step.next(&mut StepContext {
                observation: o,
                state: &state,
                events: &mut events,
                quiet_frames: 0,
                frame: None,
                learned: &[],
            })
        };
        let label = |d: &Decision| match d {
            Decision::Act(a) => a.label.clone(),
            Decision::Wait(w) => format!("wait: {w}"),
            Decision::Done(w) => format!("done: {w}"),
            Decision::Fail(w) => format!("fail: {w}"),
        };
        let mut id = 0;
        for name in ["switch-shift-go", "switch-shift-go"] {
            id += 1;
            run(&observe(name, id));
        }
        let mut last = String::new();
        for name in ["switch-shift-command-fight", "switch-shift-command-fight"] {
            id += 1;
            last = label(&run(&observe(name, id)));
        }
        assert_eq!(last, "cursor to POKéMON: Down");
        for name in [
            "switch-shift-command-pokemon",
            "switch-shift-command-pokemon",
        ] {
            id += 1;
            last = label(&run(&observe(name, id)));
        }
        assert_eq!(last, "choose POKéMON");
    }

    /// Fleet worker 3, a trainer battle on Route 3: the belief still held a
    /// WEEDLE in slot 1, so the defensive switch went for it, but the
    /// battle's party showed BULBASAUR alone and no ▶ to read. It waited,
    /// pressed B and chose POKéMON again for 110k frames. A slot the menu
    /// doesn't show is given up: back to the fight.
    #[test]
    fn a_shift_to_a_member_the_party_menu_lacks_goes_back() {
        use pokebot_vision::{FireRedPerception, PerceptionSystem};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(data), Ok(font)) = (
            GameData::load(root.join("data/world/gamedata.json")),
            pokebot_vision::text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let mut vision = FireRedPerception::default().with_font(Arc::new(font));
        let image = pokebot_video::png::load(
            root.join("captures/fixtures/emu-battle-party-one-member.png"),
        )
        .unwrap();
        let frame =
            pokebot_core::NormalizedFrame::new(1, std::time::Instant::now(), image).unwrap();
        let o = vision.observe(&frame);
        let party = o.party_menu.expect("the battle's party menu");
        assert_eq!(party.count, 1);
        let mut step = BattleStep::new(Arc::new(data), BattlePlan::Fight, false).shifting_to(1);
        match step.shift_in_party_menu(&party, 1, None) {
            Decision::Act(a) => assert!(a.label.starts_with("shift: no slot 1"), "{}", a.label),
            _ => panic!("no press"),
        }
        assert!(step.shifted);
    }
}
