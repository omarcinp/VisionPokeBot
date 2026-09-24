//! `Probe`: open a screen to make a fact `Observed` (spec §3.2, §4.3).
//!
//! - a bag pocket: the field bag's pocket audit;
//! - the Trainer Card (Start → the trainer's name): the eight badge flags
//!   and the POKéDEX count (species caught);
//! - the Pokédex (Start → POKéDEX → the numerical list): every row's
//!   species and caught mark, read page by page, and the counts the list
//!   implies (it runs to the highest seen species, so named rows are the
//!   species seen and marked rows the species caught);
//! - the Fly map: the lit towns, when it is on screen. Opening it (the
//!   POKéMON menu → a mon → FLY) is not driven yet.
//!
//! Every step is verified on the next frames; a screen that never comes
//! fails the probe, and every probe closes what it opened with B until the
//! overworld shows again.

use std::sync::Arc;

use pokebot_core::Button;
use pokebot_gamedata::GameData;
use pokebot_state::{
    FlyMapObservation, GameEvent, GameState, Observation, Pocket, PokedexListObservation,
    ScreenState, TrainerCardObservation,
};

use super::menu::{
    open_start_menu, pick_row, Closer, MenuRow, Retries, MENU_FRAMES, SCREEN_FRAMES,
};
use super::{
    progress, Expects, Intent, ProbeFact, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep, SETTLE_FRAMES,
};
use crate::bag::PocketAudit;
use crate::{Action, Decision, Expectation, Outcome};

pub struct ProbeTool;

/// The pocket audit as a step: Start opens the menu only once the scene
/// has settled (pressed during a scripted pause it lands mid-cutscene).
struct AuditStep {
    audit: PocketAudit,
    data: Arc<GameData>,
}

impl ToolStep for AuditStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if !self.audit.observed()
            && o.bag.is_none()
            && o.menu.is_none()
            && ctx.quiet_frames < SETTLE_FRAMES
        {
            return Decision::Wait("letting the scene settle before the bag".into());
        }
        self.audit.next(o, &self.data, ctx.events)
    }

    fn expects(&self) -> Expects {
        Expects::MENUS
    }
}

/// Reads `pocket` through the Start menu and closes every menu again.
pub fn audit_pocket(ctx: &mut ToolContext<'_>, pocket: Pocket) -> Result<String, ToolError> {
    let mut step = AuditStep {
        audit: PocketAudit::new(pocket),
        data: Arc::clone(&ctx.data),
    };
    let summary = ctx.drive(&mut step)?;
    ctx.emit(progress("Bag", summary.clone()))?;
    Ok(summary)
}

/// The badge flags, in badge order.
pub const BADGE_FLAGS: [&str; 8] = [
    "FLAG_BADGE01_GET",
    "FLAG_BADGE02_GET",
    "FLAG_BADGE03_GET",
    "FLAG_BADGE04_GET",
    "FLAG_BADGE05_GET",
    "FLAG_BADGE06_GET",
    "FLAG_BADGE07_GET",
    "FLAG_BADGE08_GET",
];

/// What the Trainer Card's front establishes: every badge flag (drawn →
/// true, not drawn → false) and, when its POKéDEX row read, the caught
/// count (the card prints only that one; `seen` stays unknown).
pub fn trainer_card_events(card: &TrainerCardObservation) -> Vec<GameEvent> {
    let mut events: Vec<GameEvent> = BADGE_FLAGS
        .iter()
        .enumerate()
        .map(|(i, flag)| GameEvent::FlagObserved {
            flag: (*flag).to_owned(),
            value: card.badges.contains(&(i as u8 + 1)),
        })
        .collect();
    events.extend(pokedex_count_event(None, card.pokedex_count));
    events
}

/// `PokedexCountObserved` for the counts that were read. The event carries
/// both counts, so only a screen showing both (the Pokédex list) produces
/// it; the card's lone caught count is kept in the log rather than paired
/// with an invented `seen`.
pub fn pokedex_count_event(seen: Option<u16>, caught: Option<u16>) -> Option<GameEvent> {
    Some(GameEvent::PokedexCountObserved {
        seen: seen?,
        caught: caught?,
    })
}

/// Two views show the same rows (marks aside).
fn same_names(a: &[(String, bool)], b: &[(String, bool)]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|((x, _), (y, _))| x == y)
}

/// A Pokédex list row's name is the `-----` of a species never seen (or
/// blank, past the list's end).
fn is_unseen_row(name: &str) -> bool {
    name.trim().chars().all(|c| c == '-' || c == '?')
}

/// What a complete read of the numerical list establishes: `SpeciesCaught`
/// for marked rows, `SpeciesSeen` for the other named rows, and the counts
/// the list implies. Rows whose name doesn't resolve to one species are
/// returned apart (a progress note, not a fact).
pub fn pokedex_events(data: &GameData, rows: &[(String, bool)]) -> (Vec<GameEvent>, Vec<String>) {
    let mut events = Vec::new();
    let mut unresolved = Vec::new();
    let (mut seen, mut caught) = (0u16, 0u16);
    for (name, mark) in rows {
        if is_unseen_row(name) {
            continue;
        }
        seen += 1;
        if *mark {
            caught += 1;
        }
        let Some(species) = data.species_named(name) else {
            unresolved.push(name.clone());
            continue;
        };
        let species = species.to_owned();
        events.push(if *mark {
            GameEvent::SpeciesCaught { species }
        } else {
            GameEvent::SpeciesSeen { species }
        });
    }
    events.extend(pokedex_count_event(Some(seen), Some(caught)));
    (events, unresolved)
}

/// What the Fly map establishes: `MapVisited` for every lit fly spot. Dark
/// spots mean "not visited", which no event carries yet; they are returned
/// apart for the log.
pub fn fly_map_events(map: &FlyMapObservation) -> (Vec<GameEvent>, Vec<String>) {
    let events = map
        .lit
        .iter()
        .map(|map| GameEvent::MapVisited { map: map.clone() })
        .collect();
    (events, map.dark.clone())
}

/// A party Pokémon knows FLY.
pub fn party_has_fly(state: &GameState) -> bool {
    state.party.value.as_ref().is_some_and(|party| {
        party.iter().any(|mon| {
            mon.moves
                .iter()
                .flatten()
                .any(|slot| slot.mv.value.as_deref() == Some("MOVE_FLY"))
        })
    })
}

/// Start → the trainer's name → read the card → B until the overworld.
#[derive(Default)]
struct TrainerCardStep {
    retries: Retries,
    closer: Closer,
    /// The name row was confirmed: the card is on its way.
    chosen: bool,
    /// Times the row was confirmed.
    opens: u32,
    /// The card as first read, with its frame: a reading counts once a
    /// later frame reads the same.
    candidate: Option<(u64, TrainerCardObservation)>,
    observed: Option<String>,
}

impl ToolStep for TrainerCardStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        let phase = if self.observed.is_some() {
            "close"
        } else if o.trainer_card.is_some() {
            "card"
        } else if o.menu.is_some() && o.dialogue.is_none() {
            "menu"
        } else if self.chosen {
            "opening"
        } else {
            "open"
        };
        self.retries.enter(phase);
        if self.retries.exhausted() {
            return Decision::Fail(format!("trainer card: no progress in phase {phase}"));
        }
        match phase {
            "open" => open_start_menu(&mut self.retries, o, ctx.quiet_frames),
            "menu" => {
                let menu = o.menu.expect("menu phase");
                let (decision, chosen) = pick_row(
                    &mut self.retries,
                    o,
                    &menu,
                    &MenuRow::PlayerName,
                    ctx.state,
                    Expectation::MenuClosed,
                    SCREEN_FRAMES,
                );
                if chosen {
                    self.chosen = true;
                    self.opens += 1;
                }
                decision
            }
            "opening" => {
                if o.player.is_some() && ctx.quiet_frames > SETTLE_FRAMES {
                    // The menu closed to the overworld: the card never came.
                    if self.opens >= MAX_OPENS {
                        return Decision::Fail(format!(
                            "the trainer card did not open ({MAX_OPENS} tries)"
                        ));
                    }
                    self.chosen = false;
                    return self.retries.wait(o, "the trainer card did not open");
                }
                self.retries.wait(o, "waiting for the trainer card")
            }
            "card" => {
                let card = o.trainer_card.clone().expect("card phase");
                match self.candidate.take() {
                    Some((frame, first)) if frame < o.frame_id => {
                        if first != card {
                            self.candidate = Some((o.frame_id, card));
                            self.retries.failed();
                            return self
                                .retries
                                .wait(o, "re-reading a card that read differently");
                        }
                    }
                    _ => {
                        self.candidate = Some((o.frame_id, card));
                        return self.retries.wait(o, "confirming the card on a later frame");
                    }
                }
                let summary = format!(
                    "trainer card: badges {:?}, {} caught",
                    card.badges,
                    card.pokedex_count
                        .map_or("? (count not read)".to_owned(), |n| n.to_string())
                );
                ctx.events.extend(trainer_card_events(&card));
                ctx.events.push(progress("Probe", summary.clone()));
                self.observed = Some(summary.clone());
                self.closer.next(o, &summary)
            }
            _ => {
                let summary = self.observed.clone().unwrap_or_default();
                self.closer.next(o, &summary)
            }
        }
    }

    fn on_outcome(&mut self, _action: &Action, outcome: Outcome, _ctx: &mut StepContext<'_>) {
        if outcome != Outcome::Confirmed {
            self.retries.failed();
        }
    }

    fn expects(&self) -> Expects {
        Expects::MENUS
    }
}

/// Times the Start menu row is chosen without its screen appearing before
/// the probe fails (the phases would otherwise cycle with fresh retries).
const MAX_OPENS: u32 = 3;

/// Frames after choosing POKéDEX before the TABLE OF CONTENTS (which no
/// detector reads) is answered with A; the same again between A presses.
const CONTENTS_FRAMES: u64 = 90;
/// A presses on the unrecognised contents page before giving up.
const MAX_CONTENTS_PRESSES: u32 = 3;
/// Downs that change nothing with the ▶ on a named row before it is the
/// list's end (one could be a press that didn't land).
const LIST_END_STRIKES: u32 = 2;
/// Frames after a Down before an unchanged list means the Down changed
/// nothing (the console answers a press 30 frames later, and the list
/// scrolls with an animation).
const DOWN_LAND_FRAMES: u64 = 75;
/// A scrolled view must repeat at least this many rows of the last one
/// (single Downs scroll by one row; a view sharing fewer rows is a
/// misread, not a scroll).
const MIN_SCROLL_OVERLAP: usize = 5;
/// More rows than the Kanto Pokédex has: the list never ended.
const MAX_LIST_ROWS: usize = 160;

/// Start → POKéDEX → (contents: A) → the list, read page by page with
/// Down → B until the overworld.
struct PokedexStep {
    data: Arc<GameData>,
    retries: Retries,
    closer: Closer,
    chosen: bool,
    opens: u32,
    /// The list was on screen: frames without it are its redraw, not the
    /// contents page.
    list_seen: bool,
    /// Frame of the last input on the contents page (or of choosing
    /// POKéDEX), and how many A presses it took so far.
    contents_press: Option<u64>,
    contents_presses: u32,
    /// Rows read so far, top to bottom.
    rows: Vec<(String, bool)>,
    /// The ▶ was seen on row 0 (the list's top), so `rows` starts there.
    top_seen: bool,
    /// The last confirmed view: (▶ row, rows).
    last_view: Option<(u8, Vec<(String, bool)>)>,
    /// Frame of the last Down.
    pressed_at: Option<u64>,
    candidate: Option<(u64, PokedexListObservation)>,
    strikes: u32,
    observed: Option<String>,
}

impl PokedexStep {
    fn new(data: Arc<GameData>) -> Self {
        Self {
            data,
            retries: Retries::default(),
            closer: Closer::default(),
            chosen: false,
            opens: 0,
            list_seen: false,
            contents_press: None,
            contents_presses: 0,
            rows: Vec::new(),
            top_seen: false,
            last_view: None,
            pressed_at: None,
            candidate: None,
            strikes: 0,
            observed: None,
        }
    }

    /// Merges a view that scrolled by one or more rows past the last one:
    /// the smallest shift whose overlapping names agree. The overlap's
    /// marks are folded into the rows already read.
    fn merge_scrolled(&mut self, last: &[(String, bool)], view: &[(String, bool)]) -> bool {
        let n = view.len();
        if last.len() != n {
            return false;
        }
        for shift in 1..=n.saturating_sub(MIN_SCROLL_OVERLAP) {
            let overlap = n - shift;
            if same_names(&last[shift..], &view[..overlap]) {
                self.fold_marks(&view[..overlap]);
                self.rows.extend(view[overlap..].iter().cloned());
                return true;
            }
        }
        false
    }

    /// ORs the marks of `tail` (the rows a view shows again) into the last
    /// rows read: the list prints its names first and blits the caught
    /// balls a few frames later, so a mark can show up on a later view.
    fn fold_marks(&mut self, tail: &[(String, bool)]) {
        let start = self.rows.len().saturating_sub(tail.len());
        for (row, (_, mark)) in self.rows[start..].iter_mut().zip(tail) {
            row.1 |= *mark;
        }
    }

    fn read(&mut self, o: &Observation, ctx_events: &mut Vec<GameEvent>) -> Decision {
        let list = o.pokedex_list.clone().expect("list phase");
        let Some(cursor) = list.cursor else {
            return self.retries.wait(o, "looking for the Pokédex list's ▶");
        };
        // A reading counts once a later frame reads the same.
        match self.candidate.take() {
            Some((frame, first)) if frame < o.frame_id => {
                if first != list {
                    self.candidate = Some((o.frame_id, list));
                    self.retries.failed();
                    return self
                        .retries
                        .wait(o, "re-reading rows that read differently");
                }
            }
            _ => {
                self.candidate = Some((o.frame_id, list));
                return self.retries.wait(o, "confirming the rows on a later frame");
            }
        }
        self.top_seen |= cursor == 0;
        if !self.top_seen {
            return self.retries.act(
                "Pokédex: Up to the top of the list",
                Button::Up,
                Expectation::InputsDone,
                MENU_FRAMES,
            );
        }
        match &self.last_view {
            None => {
                self.rows = list.rows.clone();
            }
            Some((last_cursor, last_rows))
                if *last_cursor == cursor && same_names(last_rows, &list.rows) =>
            {
                self.fold_marks(&list.rows);
                // Unchanged so far: the Down may not have landed yet.
                let since = self
                    .pressed_at
                    .map_or(u64::MAX, |f| o.frame_id.saturating_sub(f));
                if since < DOWN_LAND_FRAMES {
                    return self.retries.wait_for_input("waiting for the Down to land");
                }
                // The Down changed nothing. The list ends on the highest
                // seen species (`DexScreen_CountMonsInOrderedList`), so with
                // a named row under the ▶ this is the end. With an unseen
                // row under it the list scrolled one identical `-----` row
                // (the ▶ parks mid-window while the list scrolls under it;
                // a press that didn't land would read the same and cost one
                // phantom unseen row, which no fact depends on).
                let under_cursor = list.rows.get(usize::from(cursor)).map(|(n, _)| n.as_str());
                if under_cursor.is_some_and(|n| !is_unseen_row(n)) {
                    self.strikes += 1;
                    if self.strikes >= LIST_END_STRIKES {
                        return self.finish(ctx_events);
                    }
                    self.retries.failed();
                } else {
                    self.rows.push(list.rows[usize::from(cursor)].clone());
                }
            }
            Some((_, last_rows)) if same_names(last_rows, &list.rows) => {
                // The ▶ moved within the page.
                self.fold_marks(&list.rows);
                self.strikes = 0;
            }
            Some((_, last_rows)) => {
                self.strikes = 0;
                if !self.merge_scrolled(&last_rows.clone(), &list.rows) {
                    self.retries.failed();
                    return self
                        .retries
                        .wait(o, "rows that don't continue the last view");
                }
            }
        }
        if self.rows.len() > MAX_LIST_ROWS {
            return Decision::Fail(format!(
                "Pokédex: {} rows read and no end in sight",
                self.rows.len()
            ));
        }
        self.last_view = Some((cursor, list.rows.clone()));
        self.pressed_at = Some(o.frame_id);
        self.retries.act(
            "Pokédex: Down to the next row",
            Button::Down,
            Expectation::InputsDone,
            MENU_FRAMES,
        )
    }

    fn finish(&mut self, ctx_events: &mut Vec<GameEvent>) -> Decision {
        let (events, unresolved) = pokedex_events(&self.data, &self.rows);
        let (mut seen, mut caught) = (0usize, 0usize);
        for e in &events {
            match e {
                GameEvent::SpeciesSeen { .. } => seen += 1,
                GameEvent::SpeciesCaught { .. } => caught += 1,
                _ => {}
            }
        }
        let summary = format!(
            "pokédex: {} rows read, {caught} caught, {seen} seen",
            self.rows.len()
        );
        ctx_events.extend(events);
        if !unresolved.is_empty() {
            ctx_events.push(progress(
                "Probe",
                format!("pokédex rows that resolve to no species: {unresolved:?}"),
            ));
        }
        ctx_events.push(progress("Probe", summary.clone()));
        self.observed = Some(summary);
        Decision::Wait("pokédex read; closing".into())
    }
}

impl ToolStep for PokedexStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        let phase = if self.observed.is_some() {
            "close"
        } else if o.pokedex_list.is_some() {
            self.list_seen = true;
            "list"
        } else if self.list_seen {
            "redraw"
        } else if o.menu.is_some() && o.dialogue.is_none() {
            "menu"
        } else if self.chosen {
            "contents"
        } else {
            "open"
        };
        self.retries.enter(phase);
        if self.retries.exhausted() {
            return Decision::Fail(format!("pokédex: no progress in phase {phase}"));
        }
        match phase {
            "open" => open_start_menu(&mut self.retries, o, ctx.quiet_frames),
            "redraw" => self
                .retries
                .wait(o, "waiting for the Pokédex list to redraw"),
            "menu" => {
                let menu = o.menu.expect("menu phase");
                let (decision, chosen) = pick_row(
                    &mut self.retries,
                    o,
                    &menu,
                    &MenuRow::Text("POKéDEX"),
                    ctx.state,
                    Expectation::MenuClosed,
                    SCREEN_FRAMES,
                );
                if chosen {
                    self.chosen = true;
                    self.opens += 1;
                    self.contents_press = Some(o.frame_id);
                    self.contents_presses = 0;
                }
                decision
            }
            "contents" => {
                if o.player.is_some() && ctx.quiet_frames > SETTLE_FRAMES {
                    // The menu closed to the overworld: no Pokédex came.
                    if self.opens >= MAX_OPENS {
                        return Decision::Fail(format!(
                            "the Pokédex did not open ({MAX_OPENS} tries; none yet?)"
                        ));
                    }
                    self.chosen = false;
                    return self.retries.wait(o, "the Pokédex did not open");
                }
                let since = self
                    .contents_press
                    .map_or(u64::MAX, |f| o.frame_id.saturating_sub(f));
                if since < CONTENTS_FRAMES || o.screen.value == ScreenState::Transition {
                    return self.retries.wait(o, "waiting for the Pokédex");
                }
                if self.contents_presses >= MAX_CONTENTS_PRESSES {
                    return Decision::Fail(
                        "the Pokédex list did not open from the TABLE OF CONTENTS".into(),
                    );
                }
                self.contents_presses += 1;
                self.contents_press = Some(o.frame_id);
                // The contents page opens with the ▶ on NUMERICAL MODE
                // (`sDexScreenDataInitialState`); the list showing is the
                // check that it was.
                self.retries.act(
                    "Pokédex contents: A on NUMERICAL MODE",
                    Button::A,
                    Expectation::InputsDone,
                    SCREEN_FRAMES,
                )
            }
            "list" => self.read(o, ctx.events),
            _ => {
                let summary = self.observed.clone().unwrap_or_default();
                self.closer.next(o, &summary)
            }
        }
    }

    fn on_outcome(&mut self, _action: &Action, outcome: Outcome, _ctx: &mut StepContext<'_>) {
        if outcome != Outcome::Confirmed {
            self.retries.failed();
        }
    }

    fn expects(&self) -> Expects {
        Expects::MENUS
    }
}

/// Opens the Trainer Card, records the badges and the caught count, and
/// closes it again.
pub fn probe_trainer_card(ctx: &mut ToolContext<'_>) -> Result<String, ToolError> {
    let mut step = TrainerCardStep::default();
    ctx.drive(&mut step)
}

/// Opens the Pokédex list, records every row, and closes it again.
pub fn probe_pokedex(ctx: &mut ToolContext<'_>) -> Result<String, ToolError> {
    let mut step = PokedexStep::new(Arc::clone(&ctx.data));
    ctx.drive(&mut step)
}

/// Records the Fly map when it is on screen; opening it is not driven yet.
pub fn probe_fly_map(ctx: &mut ToolContext<'_>) -> Result<String, ToolError> {
    if !party_has_fly(ctx.state()) {
        return Err(ToolError::Unsupported("Fly not in the party".into()));
    }
    let Some(map) = ctx.observation().and_then(|o| o.fly_map.clone()) else {
        return Err(ToolError::Unsupported(
            "opening the Fly map through the POKéMON menu is not driven yet".into(),
        ));
    };
    let (events, dark) = fly_map_events(&map);
    let summary = format!("fly map: lit {:?}, dark {dark:?}", map.lit);
    for event in events {
        ctx.emit(event)?;
    }
    ctx.emit(progress("Probe", summary.clone()))?;
    Ok(summary)
}

impl Tool for ProbeTool {
    fn name(&self) -> &str {
        "Probe"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Probe { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Probe { fact } = intent else {
            return ToolOutcome::failed("not a Probe");
        };
        match fact {
            ProbeFact::Pocket { pocket } => audit_pocket(ctx, *pocket).map(|_| ()).into(),
            ProbeFact::TrainerCard => probe_trainer_card(ctx).map(|_| ()).into(),
            ProbeFact::FlyMap => probe_fly_map(ctx).map(|_| ()).into(),
            ProbeFact::Pokedex => probe_pokedex(ctx).map(|_| ()).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    #[test]
    fn the_card_gives_every_badge_flag_and_the_caught_count() {
        let card = TrainerCardObservation {
            badges: vec![1, 3],
            pokedex_count: Some(4),
        };
        let events = trainer_card_events(&card);
        let count = pokedex_count_event(None, Some(4));
        assert_eq!(events.len(), 8 + usize::from(count.is_some()));
        for (i, flag) in BADGE_FLAGS.iter().enumerate() {
            assert!(
                events.contains(&GameEvent::FlagObserved {
                    flag: (*flag).to_owned(),
                    value: i == 0 || i == 2
                }),
                "{flag}"
            );
        }
        assert_eq!(events.get(8), count.as_ref());
        let unread = TrainerCardObservation {
            badges: vec![],
            pokedex_count: None,
        };
        assert_eq!(trainer_card_events(&unread).len(), 8);
    }

    #[test]
    fn list_rows_become_seen_caught_and_counts() {
        let Some(data) = data() else { return };
        let rows = vec![
            ("BULBASAUR".to_owned(), true),
            ("IVYSAUR".to_owned(), true),
            ("-----".to_owned(), false),
            ("CHARMANDER".to_owned(), false),
            ("--?--".to_owned(), false),
            ("".to_owned(), false),
            ("QQQ".to_owned(), false),
        ];
        let (events, unresolved) = pokedex_events(&data, &rows);
        assert_eq!(unresolved, vec!["QQQ".to_owned()]);
        let mut expected = vec![
            GameEvent::SpeciesCaught {
                species: "SPECIES_BULBASAUR".into(),
            },
            GameEvent::SpeciesCaught {
                species: "SPECIES_IVYSAUR".into(),
            },
            GameEvent::SpeciesSeen {
                species: "SPECIES_CHARMANDER".into(),
            },
        ];
        // The list runs to the highest seen species: 4 named rows seen
        // (the unresolved one included), 2 marked.
        expected.extend(pokedex_count_event(Some(4), Some(2)));
        assert_eq!(events, expected);
    }

    #[test]
    fn lit_fly_spots_are_visited_maps() {
        let map = FlyMapObservation {
            lit: vec!["PalletTown".into(), "ViridianCity".into()],
            dark: vec!["PewterCity".into()],
        };
        let (events, dark) = fly_map_events(&map);
        assert_eq!(
            events,
            vec![
                GameEvent::MapVisited {
                    map: "PalletTown".into()
                },
                GameEvent::MapVisited {
                    map: "ViridianCity".into()
                }
            ]
        );
        assert_eq!(dark, vec!["PewterCity".to_owned()]);
    }

    #[test]
    fn fly_is_looked_for_in_the_party() {
        let mut state = GameState::default();
        assert!(!party_has_fly(&state));
        let mut mon = pokebot_state::PartyMon::default();
        mon.moves[1] = Some(pokebot_state::MoveSlot {
            mv: pokebot_state::Knowledge::observed("MOVE_FLY".into(), 1),
            pp: pokebot_state::Knowledge::unknown(),
        });
        state.party = pokebot_state::Knowledge::observed(vec![mon], 1);
        assert!(party_has_fly(&state));
    }
}
