//! `PcSwap { deposit, withdraw }`: at the nearest Pokémon Center's PC,
//! store a party member and take a boxed Pokémon into the party, the way a
//! player does, closed-loop on every screen (`data/scripts/pc.inc`
//! `EventScript_PC`, `pokemon_storage_system*.c`):
//!
//! walk to face the PC (the `MB_PC` tile of the Center's 1F) → A → "<PLAYER>
//! booted up the PC." → "Which PC should be accessed?" (SOMEONE'S PC or
//! BILL'S PC, <PLAYER>'s PC, PROF. OAK's PC, LOG OFF) → the storage menu
//! (WITHDRAW POKéMON, DEPOSIT POKéMON, MOVE POKéMON, MOVE ITEMS, SEE YA!)
//! → DEPOSIT: the hand onto the member (PKMN DATA names the Pokémon under
//! it) → A → STORE → "Deposit in which BOX?" (the next box when this one
//! reads 30 /30) → A → B → "Continue BOX operations?" NO → WITHDRAW: the
//! box grid, every cell read under the hand until the species shows
//! (switching boxes on the title) → A → WITHDRAW → B, NO → SEE YA! → LOG
//! OFF.
//!
//! The party keeps between one and six members: the deposit goes first
//! unless the party has a single member. What changed is emitted as it is
//! confirmed on screen: `MonDeposited`, `BoxObserved` for a box read cell
//! by cell (a withdrawal needs its box known), `MonWithdrawn`; the party is
//! read afterwards to verify.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use pokebot_core::Button;
use pokebot_gamedata::GameData;
use pokebot_state::{
    BoxMon, GameEvent, GameState, Knowledge, MenuObservation, Observation, PcCursor, PcMode,
    PcStorageObservation,
};
use pokebot_world::World;

use super::go::GoStep;
use super::menu::{Retries, CURSOR_FRAMES, SCREEN_FRAMES};
use super::{
    progress, Expects, Intent, ProbeFact, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep,
};
use crate::bag::fits;
use crate::nav::Destination;
use crate::party::display_name;
use crate::{Action, Decision, Expectation, Outcome};

/// FireRed's boxes (`TOTAL_BOXES_COUNT`) and cells per box.
const BOXES: u8 = 14;
const CELLS: u8 = 30;
const COLUMNS: u8 = 6;
/// Frames PKMN DATA must read the same under the hand before it counts.
const READ_FRAMES: u32 = 8;

/// The tile of `map` whose behaviour is the PC (`MB_PC`): (11, 1) in every
/// Center 1F but One Island's (9, 1) and Indigo Plateau's (17, 9).
pub fn pc_tile(world: &World, map: &str) -> Option<(i32, i32)> {
    let m = world.map(map)?;
    (0..m.height)
        .flat_map(|y| (0..m.width).map(move |x| (x, y)))
        .find(|&(x, y)| {
            m.tile(x, y)
                .is_some_and(|t| t.behavior == pokebot_world::behavior::PC)
        })
}

/// The box a title names (`BOX1` → 0). The hand over the title hides its
/// first letters (`X1` with the hand on cell 2): the number after the
/// title's end of `BOX` is enough. Renamed boxes have no number.
pub fn box_number(title: &str) -> Option<u8> {
    let title = title.trim_end();
    let digits = title.len() - title.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (prefix, n) = title.split_at(title.len() - digits);
    let n: u8 = n.parse().ok()?;
    let prefix = prefix.trim_end();
    let named = ["BOX", "OX", "X", "?"]
        .iter()
        .any(|end| prefix.ends_with(end));
    (named && (1..=BOXES).contains(&n)).then(|| n - 1)
}

/// Whether a printed species name (`?` for unsure glyphs) is `species`.
/// Whether `read` names `species` or what it has evolved into: an
/// evolution the belief missed leaves its old name there (fleet workers 2
/// and 5: the farm stored the party's WEEDLE, a KAKUNA by then, and "no
/// party member reads WEEDLE" failed it every replan).
fn is_species(data: &GameData, read: &str, species: &str) -> bool {
    let mut line = vec![species.to_owned()];
    let mut at = 0;
    while at < line.len() {
        for next in data.evolutions_of(&line[at]) {
            if !line.contains(&next) {
                line.push(next);
            }
        }
        at += 1;
    }
    line.iter()
        .any(|s| data.species_named(read) == Some(s.as_str()) || fits(&display_name(s), read))
}

/// One storage operation, in the order they are done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Store a party member of this species: the one in the given slot
    /// (checked on PKMN DATA), else the first of the species.
    Deposit(String, Option<u8>),
    /// Take a Pokémon of this species from the boxes.
    Withdraw(String),
}

/// PKMN DATA as read under the hand.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reading {
    species: Option<String>,
    nickname: Option<String>,
    level: Option<u8>,
}

impl Reading {
    fn of(pc: &PcStorageObservation) -> Self {
        Self {
            species: pc.species.clone(),
            nickname: pc.nickname.clone(),
            level: pc.level,
        }
    }
}

/// Where the party panel's hand is, in Up/Down order (slot 0, 1…5, CANCEL).
fn party_index(c: PcCursor) -> Option<u8> {
    match c {
        PcCursor::Party(k) => Some(k),
        PcCursor::PartyCancel => Some(6),
        _ => None,
    }
}

/// The next place from `cur` toward `target` on the box grid: columns
/// first, then rows; from the title, down into the grid.
fn toward_cell(cur: PcCursor, target: u8) -> Option<(Button, Expectation)> {
    match cur {
        PcCursor::BoxTitle => Some((Button::Down, Expectation::PcCursorMoved(cur))),
        PcCursor::Cell(c) => {
            let (cc, cr, tc, tr) = (c % COLUMNS, c / COLUMNS, target % COLUMNS, target / COLUMNS);
            let (button, next) = if cc < tc {
                (Button::Right, c + 1)
            } else if cc > tc {
                (Button::Left, c - 1)
            } else if cr < tr {
                (Button::Down, c + COLUMNS)
            } else if cr > tr {
                (Button::Up, c - COLUMNS)
            } else {
                return None;
            };
            Some((button, Expectation::PcCursorAt(PcCursor::Cell(next))))
        }
        _ => None,
    }
}

/// The unread cell nearest `from` (grid steps; ties to the lowest cell).
fn nearest_unread(from: u8, read: &dyn Fn(u8) -> bool) -> Option<u8> {
    let (fc, fr) = (i32::from(from % COLUMNS), i32::from(from / COLUMNS));
    (0..CELLS).filter(|c| !read(*c)).min_by_key(|c| {
        let (cc, cr) = (i32::from(c % COLUMNS), i32::from(c / COLUMNS));
        ((cc - fc).abs() + (cr - fr).abs(), *c)
    })
}

/// The step: from the overworld beside the Center's PC, through the
/// storage system, back to the overworld.
pub struct PcSwapStep {
    data: Arc<GameData>,
    go: Option<GoStep>,
    ops: VecDeque<Op>,
    /// The party slot the deposit is looked for in, and those that read
    /// as another Pokémon.
    deposit_slot: u8,
    wrong_slots: BTreeSet<u8>,
    /// The next deposit's slot is taken afresh (a deposit before it moved
    /// the members after it up).
    retarget: bool,
    /// The party slot STORE was chosen for.
    storing: Option<u8>,
    /// The box cell WITHDRAW was chosen for.
    taking: Option<(u8, u8)>,
    /// Box cells read: (box, cell) → what PKMN DATA showed (`None` empty).
    cells: BTreeMap<(u8, u8), Option<Reading>>,
    /// Boxes emitted as `BoxObserved`, and boxes searched without luck.
    reported: BTreeSet<u8>,
    searched: BTreeSet<u8>,
    /// Boxes whose belief was found wrong at a cell: read in full.
    distrusted: BTreeSet<u8>,
    /// The reading under the hand and the frames in a row it has held: it
    /// counts after [`READ_FRAMES`] (the panel redraws a few frames after
    /// the hand arrives: cell 2 read RATTATA, cell 1's, for 4 frames).
    last_reading: Option<(PcCursor, Reading, u32)>,
    /// The box last named by its title (the hand can hide it all).
    current_box: Option<u8>,
    booted: bool,
    retries: Retries,
    failure: Option<String>,
    /// What was done, for the log.
    pub done: Vec<String>,
}

impl PcSwapStep {
    /// `go` walks to face the PC first (`None`: already facing it).
    pub fn new(
        data: Arc<GameData>,
        go: Option<GoStep>,
        deposit: &str,
        withdraw: &str,
        state: &GameState,
    ) -> Self {
        let party = state.party.value.as_deref();
        let mut ops = vec![
            Op::Deposit(deposit.to_owned(), None),
            Op::Withdraw(withdraw.to_owned()),
        ];
        // A lone member can't be stored: take the other one first.
        if party.is_some_and(|p| p.len() <= 1) {
            ops.rotate_left(1);
        }
        Self::with_ops(data, go, ops, state)
    }

    /// Any operations, in order (the party must keep a member throughout).
    pub fn with_ops(
        data: Arc<GameData>,
        go: Option<GoStep>,
        ops: Vec<Op>,
        state: &GameState,
    ) -> Self {
        let deposit_slot = ops
            .iter()
            .find_map(|op| match op {
                Op::Deposit(species, slot) => {
                    Some(slot.or_else(|| super::teach::member_slot(state, species)))
                }
                Op::Withdraw(_) => None,
            })
            .flatten()
            .unwrap_or(0);
        Self {
            data,
            go,
            ops: ops.into(),
            deposit_slot,
            wrong_slots: BTreeSet::new(),
            retarget: false,
            storing: None,
            taking: None,
            cells: BTreeMap::new(),
            reported: BTreeSet::new(),
            searched: BTreeSet::new(),
            distrusted: BTreeSet::new(),
            last_reading: None,
            current_box: None,
            booted: false,
            retries: Retries::default(),
            failure: None,
            done: Vec::new(),
        }
    }

    fn fail(&mut self, why: String) {
        if self.failure.is_none() {
            self.failure = Some(why);
        }
        self.ops.clear();
    }

    /// The reading under the hand, once it has held [`READ_FRAMES`].
    fn stable(&mut self, pc: &PcStorageObservation) -> Option<Reading> {
        let cursor = pc.cursor?;
        let reading = Reading::of(pc);
        let held = match &self.last_reading {
            Some((c, r, n)) if *c == cursor && *r == reading => n + 1,
            _ => 1,
        };
        self.last_reading = Some((cursor, reading.clone(), held));
        (held >= READ_FRAMES).then_some(reading)
    }

    fn act(&mut self, label: impl Into<String>, button: Button, expect: Expectation) -> Decision {
        self.retries.act(label, button, expect, CURSOR_FRAMES)
    }

    /// The Center's menus: which PC, and the storage menu.
    fn pc_menu(&mut self, o: &Observation, menu: &MenuObservation) -> Decision {
        let lines = &o.menu_lines;
        if lines.len() != usize::from(menu.rows) {
            return self.retries.wait(o, "reading the PC menu");
        }
        let row = |text: &str| lines.iter().position(|l| fits(text, l));
        if row("LOG OFF").is_some() {
            self.retries.enter("which PC");
            self.booted = true;
            let (target, label) = if self.ops.is_empty() {
                (row("LOG OFF"), "LOG OFF")
            } else {
                // SOMEONE'S PC until Bill is met, BILL'S PC after: the
                // storage system is always the first row.
                let storage = lines
                    .iter()
                    .position(|l| l.starts_with("BILL") || l.starts_with("SOMEONE"))
                    .or(Some(0));
                (storage, "the storage system")
            };
            return match target {
                Some(r) => crate::new_game::select(menu, r as u8, label),
                None => self.retries.wait(o, "looking for LOG OFF"),
            };
        }
        if row("SEE YA!").is_some() {
            self.retries.enter("storage menu");
            let (text, label) = match self.ops.front() {
                Some(Op::Deposit(..)) => ("DEPOSIT POKéMON", "DEPOSIT POKéMON"),
                Some(Op::Withdraw(_)) => ("WITHDRAW POKéMON", "WITHDRAW POKéMON"),
                None => ("SEE YA!", "SEE YA!"),
            };
            return match row(text) {
                Some(r) => crate::new_game::select(menu, r as u8, label),
                None => self.retries.wait(o, "reading the storage menu"),
            };
        }
        // Any other question at the PC: NO.
        if let Some(no) = row("NO") {
            self.retries.enter("question");
            return crate::new_game::select(menu, no as u8, "answer NO");
        }
        self.retries.wait(o, "an unknown menu at the PC")
    }

    /// The storage screen (party panel or box).
    fn storage(
        &mut self,
        o: &Observation,
        pc: &PcStorageObservation,
        state: &GameState,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        self.booted = true;
        let message = pc.message.join(" ");
        if message.contains("last POK") {
            self.fail("the game won't store the last Pokémon".into());
        } else if message.contains("party's full") || message.contains("Can't take any more") {
            self.fail("the party is full".into());
        }
        if let Some(menu) = &o.menu {
            let lines = &o.menu_lines;
            if lines.len() != usize::from(menu.rows) {
                return self.retries.wait(o, "reading the storage window");
            }
            let row = |text: &str| lines.iter().position(|l| fits(text, l));
            if let (Some(_), Some(no)) = (row("YES"), row("NO")) {
                // "Continue BOX operations?": only asked when leaving.
                self.retries.enter("continue?");
                return crate::new_game::select(menu, no as u8, "leave the box: NO");
            }
            self.retries.enter("storage window");
            let wanted = match (self.ops.front(), pc.mode, pc.cursor) {
                (Some(Op::Deposit(s, _)), PcMode::Party, Some(PcCursor::Party(k)))
                    if self.panel_is(pc, s) =>
                {
                    self.storing = Some(k);
                    "STORE"
                }
                (Some(Op::Withdraw(s)), PcMode::Box, Some(PcCursor::Cell(c)))
                    if self.panel_is(pc, s) =>
                {
                    self.taking = self.current_box.map(|b| (b, c));
                    if self.taking.is_some() {
                        "WITHDRAW"
                    } else {
                        "CANCEL"
                    }
                }
                _ => "CANCEL",
            };
            return match row(wanted) {
                Some(r) => crate::new_game::select(menu, r as u8, wanted),
                None => self.retries.wait(o, "reading the storage window"),
            };
        }
        if let Some((name, count)) = &pc.picker {
            self.retries.enter("deposit picker");
            let Some(b) = box_number(name) else {
                return self.retries.wait(o, "reading the box picker");
            };
            if *count == Some(CELLS) {
                self.searched.insert(b);
                if self.searched.len() >= usize::from(BOXES) {
                    self.fail("every box is full".into());
                    return self.act("cancel the picker", Button::B, Expectation::PcPickerClosed);
                }
                return self.act(
                    format!("BOX{} is full: the next box", b + 1),
                    Button::Right,
                    Expectation::InputsDone,
                );
            }
            return self.act(
                format!("deposit into BOX{}", b + 1),
                Button::A,
                Expectation::PcPickerClosed,
            );
        }
        match (self.ops.front().cloned(), pc.mode) {
            (Some(Op::Deposit(s, slot)), PcMode::Party) => {
                if std::mem::take(&mut self.retarget) {
                    self.deposit_slot = slot
                        .or_else(|| super::teach::member_slot(state, &s))
                        .unwrap_or(0);
                    self.wrong_slots.clear();
                }
                self.find_member(o, pc, &s)
            }
            (Some(Op::Withdraw(s)), PcMode::Box) => self.find_in_box(o, pc, &s, state, events),
            _ => {
                self.retries.enter("leave the box");
                self.retries.act(
                    "leave the box",
                    Button::B,
                    Expectation::MenuOpen,
                    SCREEN_FRAMES,
                )
            }
        }
    }

    fn panel_is(&self, pc: &PcStorageObservation, species: &str) -> bool {
        pc.species
            .as_deref()
            .is_some_and(|read| is_species(&self.data, read, species))
    }

    /// DEPOSIT: the hand onto the member, by the slot the belief gives,
    /// then by reading the others.
    fn find_member(
        &mut self,
        o: &Observation,
        pc: &PcStorageObservation,
        species: &str,
    ) -> Decision {
        self.retries.enter("party panel");
        let Some(cur) = pc.cursor.and_then(party_index) else {
            return self.retries.wait(o, "finding the hand");
        };
        let target = self.deposit_slot;
        if cur == target {
            let Some(reading) = self.stable(pc) else {
                return Decision::Wait("reading PKMN DATA".into());
            };
            match reading.species.as_deref() {
                Some(read) if is_species(&self.data, read, species) => {
                    return self.retries.act(
                        format!("choose slot {target} ({read})"),
                        Button::A,
                        Expectation::MenuOpen,
                        SCREEN_FRAMES,
                    );
                }
                read => {
                    self.wrong_slots.insert(target);
                    match (0..6u8).find(|k| !self.wrong_slots.contains(k)) {
                        Some(next) => self.deposit_slot = next,
                        None => self.fail(format!(
                            "no party member reads {} (slot {target}: {read:?})",
                            display_name(species)
                        )),
                    }
                    return Decision::Wait("looking at another slot".into());
                }
            }
        }
        let (button, next) = if cur < target {
            (Button::Down, cur + 1)
        } else {
            (Button::Up, cur - 1)
        };
        let next = if next == 6 {
            PcCursor::PartyCancel
        } else {
            PcCursor::Party(next)
        };
        self.act(
            format!("party: {button:?} toward slot {target}"),
            button,
            Expectation::PcCursorAt(next),
        )
    }

    /// The cell of box `b` the belief holds `species` in, unless that
    /// belief was found wrong.
    fn believed_cell(&self, state: &GameState, b: u8, species: &str) -> Option<u8> {
        if self.distrusted.contains(&b) {
            return None;
        }
        state
            .pc
            .boxes
            .get(usize::from(b))?
            .value
            .as_ref()?
            .iter()
            .filter(|m| m.species.value.as_deref() == Some(species))
            .map(|m| m.slot)
            .min()
    }

    /// WITHDRAW: read the box cell by cell (the belief's cell first) until
    /// the species shows; a box without it gives way to the next.
    fn find_in_box(
        &mut self,
        o: &Observation,
        pc: &PcStorageObservation,
        species: &str,
        state: &GameState,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        self.retries.enter("box");
        let Some(cur) = pc.cursor else {
            return self.retries.wait(o, "finding the hand");
        };
        // The title is taken only with the hand in the grid: on the title
        // the hand hides it, and a box scrolling in shows the old one.
        if matches!(cur, PcCursor::Cell(_)) {
            if let Some(b) = pc.box_title.as_deref().and_then(box_number) {
                self.current_box = Some(b);
            }
        }
        let Some(b) = self.current_box else {
            if cur == PcCursor::BoxTitle {
                return self.act(
                    "box: down from the title",
                    Button::Down,
                    Expectation::PcCursorMoved(cur),
                );
            }
            // The hand over the top row covers the title (Switch, an
            // empty BOX2: the hand came down from the title to cell 2,
            // "BOX2" stayed unread and the swap waited until it failed
            // "no progress in box"): a row down uncovers it.
            if let PcCursor::Cell(c) = cur {
                if c < COLUMNS {
                    return self.act(
                        "box: down to uncover the title",
                        Button::Down,
                        Expectation::PcCursorAt(PcCursor::Cell(c + COLUMNS)),
                    );
                }
            }
            return self.retries.wait(o, "reading the box title");
        };
        if let PcCursor::Cell(c) = cur {
            if !self.cells.contains_key(&(b, c)) {
                let Some(reading) = self.stable(pc) else {
                    return Decision::Wait("reading PKMN DATA".into());
                };
                let reading = reading.species.is_some().then_some(reading);
                self.cells.insert((b, c), reading);
            }
        }
        let read = |c: u8| self.cells.contains_key(&(b, c));
        let complete = (0..CELLS).all(read);
        if complete && self.reported.insert(b) {
            events.push(GameEvent::BoxObserved {
                box_index: b,
                mons: self.box_mons(b, o.frame_id),
            });
        }
        let found = (0..CELLS).find(|c| {
            matches!(self.cells.get(&(b, *c)), Some(Some(r))
                if r.species.as_deref().is_some_and(|s| is_species(&self.data, s, species)))
        });
        // The belief's cell read as something else: the box is read in
        // full instead.
        if let Some(t) = self.believed_cell(state, b, species) {
            let holds = |r: &Option<Reading>| {
                r.as_ref()
                    .and_then(|r| r.species.as_deref())
                    .is_some_and(|s| is_species(&self.data, s, species))
            };
            if self.cells.get(&(b, t)).is_some_and(|r| !holds(r)) {
                self.distrusted.insert(b);
            }
        }
        let believed = self.believed_cell(state, b, species);
        // Take it when the box is read in full, or the belief holds the box
        // and agrees (a withdrawal moves a Pokémon the belief knows).
        let target = match found {
            Some(t) if complete || believed == Some(t) => Some(t),
            Some(_) => None,
            None => believed.filter(|t| !read(*t)),
        };
        if let Some(t) = target {
            if cur == PcCursor::Cell(t) && found == Some(t) {
                return self.retries.act(
                    format!("choose BOX{} cell {t}", b + 1),
                    Button::A,
                    Expectation::MenuOpen,
                    SCREEN_FRAMES,
                );
            }
            if let Some((button, expect)) = toward_cell(cur, t) {
                return self.act(format!("box: {button:?} toward cell {t}"), button, expect);
            }
        }
        if complete {
            // Not in this box: the next one, from the title. Boxes fill in
            // order (a full one sends deposits to the next): past an empty
            // one there is nothing (Switch: KANGASKHAN, in the Pokédex but
            // not in BOX1, would have been looked for in all fourteen).
            self.searched.insert(b);
            let empty = (0..CELLS).all(|c| matches!(self.cells.get(&(b, c)), Some(None)));
            if empty {
                self.fail(format!(
                    "no {} in the boxes: BOX{} is empty",
                    display_name(species),
                    b + 1
                ));
                return Decision::Wait("giving up".into());
            }
            if self.searched.len() >= usize::from(BOXES) {
                self.fail(format!("no {} in any box", display_name(species)));
                return Decision::Wait("giving up".into());
            }
            return match cur {
                PcCursor::BoxTitle => {
                    // Which box scrolled in is read from the grid.
                    self.current_box = None;
                    self.retries.act(
                        format!("BOX{}: the next box", b + 1),
                        Button::Right,
                        Expectation::InputsDone,
                        SCREEN_FRAMES,
                    )
                }
                PcCursor::Cell(c) if c < COLUMNS => self.act(
                    "box: up to the title",
                    Button::Up,
                    Expectation::PcCursorAt(PcCursor::BoxTitle),
                ),
                PcCursor::Cell(c) => self.act(
                    "box: up toward the title",
                    Button::Up,
                    Expectation::PcCursorAt(PcCursor::Cell(c - COLUMNS)),
                ),
                _ => self.retries.wait(o, "finding the hand"),
            };
        }
        let from = match cur {
            PcCursor::Cell(c) => c,
            _ => 2,
        };
        match nearest_unread(from, &read).and_then(|t| toward_cell(cur, t).map(|m| (t, m))) {
            Some((t, (button, expect))) => {
                self.act(format!("box: {button:?} to read cell {t}"), button, expect)
            }
            None => self.retries.wait(o, "reading the box"),
        }
    }

    /// Box `b` as read.
    fn box_mons(&self, b: u8, frame: u64) -> Vec<BoxMon> {
        (0..CELLS)
            .filter_map(|c| {
                let r = self.cells.get(&(b, c))?.as_ref()?;
                let species = r
                    .species
                    .as_deref()
                    .and_then(|s| self.data.species_named(s))
                    .map_or_else(Knowledge::unknown, |s| {
                        Knowledge::observed(s.to_owned(), frame)
                    });
                Some(BoxMon {
                    slot: c,
                    species,
                    level: r
                        .level
                        .map_or_else(Knowledge::unknown, |l| Knowledge::observed(l, frame)),
                    nickname: r
                        .nickname
                        .clone()
                        .map_or_else(Knowledge::unknown, |n| Knowledge::observed(n, frame)),
                    training: Default::default(),
                })
            })
            .collect()
    }
}

impl ToolStep for PcSwapStep {
    fn expects(&self) -> Expects {
        if self.go.is_some() {
            Expects::NONE
        } else {
            Expects::MENUS
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        if let Some(go) = self.go.as_mut() {
            go.on_outcome(action, outcome, ctx);
            return;
        }
        if outcome != Outcome::Confirmed {
            self.retries.failed();
            return;
        }
        if action.label.starts_with("deposit into BOX") {
            let b = action
                .label
                .trim_start_matches("deposit into BOX")
                .parse::<u8>()
                .ok()
                .and_then(|n| n.checked_sub(1));
            if let (Some(slot), Some(b), Some(Op::Deposit(s, _))) =
                (self.storing.take(), b, self.ops.front())
            {
                self.done
                    .push(format!("stored {s} (slot {slot}) in BOX{}", b + 1));
                ctx.events.push(GameEvent::MonDeposited {
                    party_slot: slot,
                    box_index: b,
                });
                // The box now holds one more: read it again when needed.
                self.reported.remove(&b);
                self.cells.retain(|(bx, _), _| *bx != b);
                self.ops.pop_front();
                self.retarget = true;
            }
        } else if action.label == "WITHDRAW" {
            if let (Some((b, c)), Some(Op::Withdraw(s))) = (self.taking.take(), self.ops.front()) {
                self.done
                    .push(format!("took {s} from BOX{} cell {c}", b + 1));
                ctx.events.push(GameEvent::MonWithdrawn {
                    box_index: b,
                    box_slot: c,
                });
                self.cells.insert((b, c), None);
                self.ops.pop_front();
            }
        }
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        if let Some(go) = self.go.as_mut() {
            return match go.next(ctx) {
                Decision::Done(_) => {
                    self.go = None;
                    Decision::Wait("facing the PC".into())
                }
                d => d,
            };
        }
        let o = ctx.observation;
        if self.retries.exhausted() {
            let why = format!("PC: no progress in {}", self.retries.phase());
            if self.failure.is_none() && !self.ops.is_empty() {
                // Leave the PC the ordinary way, then fail.
                self.fail(why);
                self.retries = Retries::default();
            } else {
                return Decision::Fail(self.failure.clone().unwrap_or(why));
            }
        }
        if let Some(pc) = &o.pc_storage {
            let pc = pc.clone();
            return self.storage(o, &pc, ctx.state, ctx.events);
        }
        if let Some(menu) = &o.menu {
            if o.dialogue.is_some() || self.booted {
                let menu = *menu;
                return self.pc_menu(o, &menu);
            }
        }
        if let Some(d) = &o.dialogue {
            self.retries.enter("text");
            let page = d.lines.join(" ");
            if page.contains("booted up the PC") {
                self.booted = true;
            }
            return crate::new_game::advance_or_wait(Some(d), "reading");
        }
        if o.player.is_none() || o.menu.is_some() {
            return self.retries.wait(o, "the PC's screens fading");
        }
        if self.booted && self.ops.is_empty() {
            return match self.failure.take() {
                Some(why) => Decision::Fail(why),
                None => Decision::Done(self.done.join("; ")),
            };
        }
        if ctx.quiet_frames < super::SETTLE_FRAMES {
            return Decision::Wait("letting the scene settle".into());
        }
        self.retries.enter("boot");
        self.retries.act(
            "boot up the PC",
            Button::A,
            Expectation::DialogueOpen,
            SCREEN_FRAMES,
        )
    }
}

/// The Center to use: the healer the scheduler would walk to when it is a
/// Center with a PC, else the Center fewest maps away.
fn nearest_pc(ctx: &mut ToolContext<'_>) -> Result<(String, (i32, i32)), ToolError> {
    if let Ok((map, _)) = super::heal::nurse_for(ctx, None) {
        if let Some(tile) = pc_tile(&ctx.world, &map) {
            return Ok((map, tile));
        }
    }
    let pose = ctx
        .pose()
        .ok_or_else(|| ToolError::Failed("player not located".into()))?;
    let (map, _) = super::lookup::nearest_nurse(&ctx.world, &pose.map)
        .ok_or_else(|| ToolError::Failed("no Pokémon Center found".into()))?;
    let tile =
        pc_tile(&ctx.world, &map).ok_or_else(|| ToolError::Failed(format!("{map} has no PC")))?;
    Ok((map, tile))
}

/// How many party members are `species` (the belief).
fn party_count(state: &GameState, species: &str) -> usize {
    state.party.value.as_deref().map_or(0, |p| {
        p.iter()
            .filter(|m| m.species.value.as_deref() == Some(species))
            .count()
    })
}

/// The party read, when the belief has none.
fn know_party(ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
    if ctx
        .state()
        .party
        .value
        .as_ref()
        .is_none_or(|p| p.is_empty())
    {
        ctx.invoke(&Intent::Probe {
            fact: ProbeFact::Party,
        })
        .result?;
    }
    Ok(())
}

/// Walks to face the nearest Center's PC and runs the step `make` builds
/// there; the party is read afterwards (what the screens confirmed changed
/// the belief; a withdrawn Pokémon's HP and moves were never seen).
fn at_pc(
    ctx: &mut ToolContext<'_>,
    make: impl FnOnce(&mut ToolContext<'_>, GoStep) -> PcSwapStep,
    describe: impl FnOnce(&PcSwapStep) -> String,
) -> Result<String, ToolError> {
    let (map, (x, y)) = nearest_pc(ctx)?;
    super::go::reach_map(ctx, &map)?;
    let go = GoStep::new(
        ctx,
        Destination::Facing {
            map: map.clone(),
            x,
            y,
        },
    );
    let mut step = make(ctx, go);
    ctx.info(format!("pc: {map} PC at ({x}, {y}): {}", describe(&step)));
    let summary = ctx.drive(&mut step)?;
    ctx.invoke(&Intent::Probe {
        fact: ProbeFact::Party,
    })
    .result?;
    Ok(summary)
}

pub fn pc_swap(
    ctx: &mut ToolContext<'_>,
    deposit: &str,
    withdraw: &str,
) -> Result<String, ToolError> {
    know_party(ctx)?;
    if party_count(ctx.state(), deposit) == 0 {
        return Err(ToolError::Failed(format!(
            "no {} in the party to store",
            display_name(deposit)
        )));
    }
    let (had_deposit, had_withdraw) = (
        party_count(ctx.state(), deposit),
        party_count(ctx.state(), withdraw),
    );
    let summary = at_pc(
        ctx,
        |ctx, go| {
            PcSwapStep::new(
                Arc::clone(&ctx.data),
                Some(go),
                deposit,
                withdraw,
                ctx.state(),
            )
        },
        |step| {
            format!(
                "store {} (slot {}), take {}",
                display_name(deposit),
                step.deposit_slot,
                display_name(withdraw)
            )
        },
    )?;
    let (has_deposit, has_withdraw) = (
        party_count(ctx.state(), deposit),
        party_count(ctx.state(), withdraw),
    );
    let same = usize::from(deposit == withdraw);
    let stored = has_deposit + 1 == had_deposit + same;
    let taken = has_withdraw + same == had_withdraw + 1;
    if !stored || !taken {
        return Err(ToolError::Failed(format!(
            "pc: the party read afterwards has {has_deposit} {} (had {had_deposit}) and \
             {has_withdraw} {} (had {had_withdraw})",
            display_name(deposit),
            display_name(withdraw)
        )));
    }
    ctx.emit(progress("PcSwap", summary.clone()))?;
    Ok(summary)
}

/// Stores the party members in `slots` (the party as the belief holds it),
/// keeping the others: from the last slot up, so every slot still names
/// its member when its turn comes.
pub fn deposit(ctx: &mut ToolContext<'_>, slots: &[u8]) -> Result<String, ToolError> {
    know_party(ctx)?;
    let party: Vec<String> = ctx
        .state()
        .party
        .value
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|m| m.species.value.clone().unwrap_or_default())
        .collect();
    let mut slots: Vec<u8> = slots.to_vec();
    slots.sort_unstable_by(|a, b| b.cmp(a));
    slots.dedup();
    let mut ops = Vec::new();
    for slot in &slots {
        match party.get(usize::from(*slot)).filter(|s| !s.is_empty()) {
            Some(species) => ops.push(Op::Deposit(species.clone(), Some(*slot))),
            None => {
                return Err(ToolError::Failed(format!(
                    "pc: party slot {slot} is not known ({} members)",
                    party.len()
                )))
            }
        }
    }
    if ops.len() >= party.len() {
        return Err(ToolError::Failed(
            "pc: the party must keep one member".into(),
        ));
    }
    if ops.is_empty() {
        return Ok("nothing to store".into());
    }
    let had = party.len();
    let names = ops
        .iter()
        .map(|op| match op {
            Op::Deposit(s, _) | Op::Withdraw(s) => display_name(s),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let summary = at_pc(
        ctx,
        |ctx, go| PcSwapStep::with_ops(Arc::clone(&ctx.data), Some(go), ops, ctx.state()),
        |_| format!("store {names}"),
    )?;
    let has = ctx.state().party.value.as_ref().map_or(0, Vec::len);
    if has + slots.len() != had {
        return Err(ToolError::Failed(format!(
            "pc: the party read afterwards has {has} members (had {had}, stored {})",
            slots.len()
        )));
    }
    ctx.emit(progress("PcSwap", summary.clone()))?;
    Ok(summary)
}

/// Takes a Pokémon of each species in `species` from the boxes into the
/// party (six at most in all).
pub fn withdraw(ctx: &mut ToolContext<'_>, species: &[String]) -> Result<String, ToolError> {
    know_party(ctx)?;
    let had = ctx.state().party.value.as_ref().map_or(0, Vec::len);
    if species.is_empty() {
        return Ok("nothing to take".into());
    }
    if had + species.len() > 6 {
        return Err(ToolError::Failed(format!(
            "pc: {had} members and {} to take: more than six",
            species.len()
        )));
    }
    let ops: Vec<Op> = species.iter().map(|s| Op::Withdraw(s.clone())).collect();
    let names = species
        .iter()
        .map(|s| display_name(s))
        .collect::<Vec<_>>()
        .join(", ");
    let summary = at_pc(
        ctx,
        |ctx, go| PcSwapStep::with_ops(Arc::clone(&ctx.data), Some(go), ops, ctx.state()),
        |_| format!("take {names}"),
    )?;
    let has = ctx.state().party.value.as_ref().map_or(0, Vec::len);
    if has != had + species.len() {
        return Err(ToolError::Failed(format!(
            "pc: the party read afterwards has {has} members (had {had}, took {})",
            species.len()
        )));
    }
    ctx.emit(progress("PcSwap", summary.clone()))?;
    Ok(summary)
}

pub struct PcSwapTool;

impl Tool for PcSwapTool {
    fn name(&self) -> &str {
        "PcSwap"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::PcSwap { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::PcSwap { deposit, withdraw } = intent else {
            return ToolOutcome::failed("not a PcSwap");
        };
        pc_swap(ctx, deposit, withdraw).map(|_| ()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_core::ControllerCommand;
    use pokebot_state::{Observed, PartyMon, Region, ScreenState};

    /// Fleet workers 2 and 5: the belief's WEEDLE was a KAKUNA on the PC's
    /// panel. A later form reads as the species; an earlier one doesn't.
    #[test]
    fn an_evolved_member_reads_as_its_species() {
        let Some(data) = data() else { return };
        assert!(is_species(&data, "WEEDLE", "SPECIES_WEEDLE"));
        assert!(is_species(&data, "KAKUNA", "SPECIES_WEEDLE"));
        assert!(is_species(&data, "BEEDRILL", "SPECIES_WEEDLE"));
        assert!(!is_species(&data, "WEEDLE", "SPECIES_KAKUNA"));
        assert!(!is_species(&data, "PIDGEY", "SPECIES_WEEDLE"));
    }

    fn data() -> Option<Arc<GameData>> {
        GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok()
        .map(Arc::new)
    }

    fn state(party: &[&str], box1: Option<&[(u8, &str)]>) -> GameState {
        let mut s = GameState {
            party: Knowledge::observed(
                party
                    .iter()
                    .map(|sp| PartyMon {
                        species: Knowledge::observed((*sp).to_owned(), 1),
                        ..PartyMon::default()
                    })
                    .collect(),
                1,
            ),
            ..GameState::default()
        };
        if let Some(mons) = box1 {
            s.pc.boxes[0] = Knowledge::observed(
                mons.iter()
                    .map(|(slot, sp)| BoxMon {
                        slot: *slot,
                        species: Knowledge::observed((*sp).to_owned(), 1),
                        level: Knowledge::unknown(),
                        nickname: Knowledge::unknown(),
                        training: Default::default(),
                    })
                    .collect(),
                1,
            );
        }
        s
    }

    fn storage(frame: u64, mode: PcMode, cursor: PcCursor, species: Option<&str>) -> Observation {
        let mut o = Observation::bare(
            frame,
            Observed {
                value: ScreenState::PcStorage,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.pc_storage = Some(PcStorageObservation {
            mode,
            cursor: Some(cursor),
            nickname: species.map(str::to_owned),
            species: species.map(str::to_owned),
            level: species.map(|_| 10),
            box_title: (mode == PcMode::Box).then(|| "BOX1".to_owned()),
            picker: None,
            message: Vec::new(),
        });
        o
    }

    fn with_menu(mut o: Observation, lines: &[&str], cursor_row: u8) -> Observation {
        o.menu = Some(MenuObservation {
            window: Region::new(150, 30, 70, 80),
            rows: lines.len() as u8,
            cursor_row,
            cursor_y: 44,
        });
        o.menu_lines = lines.iter().map(|s| (*s).to_owned()).collect();
        o
    }

    struct Run {
        state: GameState,
        events: Vec<GameEvent>,
    }

    impl Run {
        fn next(&mut self, step: &mut PcSwapStep, o: &Observation) -> Decision {
            step.next(&mut StepContext {
                observation: o,
                state: &self.state,
                events: &mut self.events,
                quiet_frames: 0,
                frame: None,
                learned: &[],
            })
        }

        /// The decision once PKMN DATA has held long enough to count.
        fn settled(&mut self, step: &mut PcSwapStep, o: &Observation) -> Decision {
            let mut o = o.clone();
            for _ in 0..READ_FRAMES {
                let d = self.next(step, &o);
                if !matches!(d, Decision::Wait(_)) {
                    return d;
                }
                o.frame_id += 1;
            }
            self.next(step, &o)
        }

        fn confirm(&mut self, step: &mut PcSwapStep, d: &Decision, o: &Observation) {
            let Decision::Act(a) = d else {
                panic!("expected an input, got {}", show(d))
            };
            step.on_outcome(
                a,
                Outcome::Confirmed,
                &mut StepContext {
                    observation: o,
                    state: &self.state,
                    events: &mut self.events,
                    quiet_frames: 0,
                    frame: None,
                    learned: &[],
                },
            );
        }
    }

    fn show(d: &Decision) -> String {
        match d {
            Decision::Act(a) => format!("act {}", a.label),
            Decision::Wait(w) => format!("wait {w}"),
            Decision::Done(w) => format!("done {w}"),
            Decision::Fail(w) => format!("fail {w}"),
        }
    }

    fn pressed(d: &Decision) -> (Button, String) {
        match d {
            Decision::Act(a) => match a.commands.first() {
                Some(ControllerCommand::Press(b)) => (*b, a.label.clone()),
                c => panic!("{c:?}"),
            },
            d => panic!("expected a press, got {}", show(d)),
        }
    }

    #[test]
    fn box_titles_name_their_box_even_half_hidden() {
        assert_eq!(box_number("BOX1"), Some(0));
        assert_eq!(box_number("? BOX1"), Some(0));
        assert_eq!(box_number("X1"), Some(0), "the hand over BO");
        assert_eq!(box_number("BOX14"), Some(13));
        assert_eq!(box_number("BOX15"), None);
        assert_eq!(box_number("PIKACHU"), None);
        assert_eq!(box_number("TEAM2"), None);
    }

    #[test]
    fn every_center_has_its_pc_tile() {
        let Ok(world) =
            World::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world"))
        else {
            return;
        };
        assert_eq!(
            pc_tile(&world, "CeruleanCity_PokemonCenter_1F"),
            Some((11, 1))
        );
        assert_eq!(pc_tile(&world, "OneIsland_PokemonCenter_1F"), Some((9, 1)));
        assert_eq!(pc_tile(&world, "PalletTown_PlayersHouse_1F"), None);
    }

    /// The emulator's first swap (Cerulean Center, `tests/pc_emulator.rs`)
    /// as observations: the hand down to PARAS in slot 4, STORE, the box
    /// picker (BOX1 full: the next), `MonDeposited`; then the box read
    /// cell by cell, `BoxObserved`, back to EKANS in cell 0, WITHDRAW,
    /// `MonWithdrawn`.
    #[test]
    fn a_member_is_stored_and_a_boxed_one_found_and_taken() {
        let Some(data) = data() else { return };
        let party = [
            "SPECIES_GEODUDE",
            "SPECIES_ZUBAT",
            "SPECIES_CHARMELEON",
            "SPECIES_MANKEY",
            "SPECIES_PARAS",
            "SPECIES_CLEFAIRY",
        ];
        let mut run = Run {
            state: state(&party, None),
            events: Vec::new(),
        };
        let mut step = PcSwapStep::new(
            Arc::clone(&data),
            None,
            "SPECIES_PARAS",
            "SPECIES_EKANS",
            &run.state,
        );
        assert_eq!(step.deposit_slot, 4);
        // The storage menu: DEPOSIT first.
        let mut o = with_menu(
            Observation::bare(
                1,
                storage(1, PcMode::Party, PcCursor::Party(0), None).screen,
                Default::default(),
            ),
            &[
                "WITHDRAW POKéMON",
                "DEPOSIT POKéMON",
                "MOVE POKéMON",
                "MOVE ITEMS",
                "SEE YA!",
            ],
            0,
        );
        o.dialogue = Some(pokebot_state::DialogueObservation {
            kind: pokebot_state::DialogueKind::MessageBox,
            region: Region::new(8, 118, 224, 36),
            waiting_for_input: false,
            arrow: None,
            stable_frames: 0,
            text_cells: Vec::new(),
            lines: vec!["You can withdraw a POKéMON if you".into()],
            help: false,
        });
        assert_eq!(pressed(&run.next(&mut step, &o)).0, Button::Down);
        // The party panel: the hand from slot 0 down to slot 4.
        let o = storage(2, PcMode::Party, PcCursor::Party(0), Some("GEODUDE"));
        let (b, _) = pressed(&run.next(&mut step, &o));
        assert_eq!(b, Button::Down);
        let o = storage(3, PcMode::Party, PcCursor::Party(4), Some("PARAS"));
        let d = run.settled(&mut step, &o);
        assert_eq!(pressed(&d), (Button::A, "choose slot 4 (PARAS)".into()));
        let o = with_menu(o, &["STORE", "SUMMARY", "MARK", "RELEASE", "CANCEL"], 0);
        assert_eq!(
            pressed(&run.next(&mut step, &o)),
            (Button::A, "STORE".into())
        );
        // The picker: BOX1 full, then BOX2.
        let mut o = storage(20, PcMode::Party, PcCursor::Party(4), Some("PARAS"));
        o.pc_storage.as_mut().unwrap().picker = Some(("BOX1".into(), Some(30)));
        assert_eq!(pressed(&run.next(&mut step, &o)).0, Button::Right);
        o.pc_storage.as_mut().unwrap().picker = Some(("BOX2".into(), Some(3)));
        let d = run.next(&mut step, &o);
        assert_eq!(pressed(&d), (Button::A, "deposit into BOX2".into()));
        run.confirm(&mut step, &d, &o);
        assert_eq!(
            run.events,
            [GameEvent::MonDeposited {
                party_slot: 4,
                box_index: 1
            }]
        );
        // Still on the party panel: leave (B, then NO).
        let o = storage(21, PcMode::Party, PcCursor::Party(4), Some("CLEFAIRY"));
        assert_eq!(pressed(&run.next(&mut step, &o)).0, Button::B);
        let o = with_menu(o, &["YES", "NO"], 0);
        assert_eq!(pressed(&run.next(&mut step, &o)).0, Button::Down);
        // BOX1: EKANS in cell 0, RATTATA in cell 1, the rest empty.
        let mut frame = 100;
        let mut cursor = PcCursor::Cell(0);
        let cell = |c: u8| match c {
            0 => Some("EKANS"),
            1 => Some("RATTATA"),
            _ => None,
        };
        for _ in 0..60 {
            let PcCursor::Cell(c) = cursor else {
                panic!("{cursor:?}")
            };
            let o = storage(frame, PcMode::Box, cursor, cell(c));
            frame += 100;
            let d = run.settled(&mut step, &o);
            let Decision::Act(a) = &d else {
                panic!("{}", show(&d))
            };
            if a.label.starts_with("choose") {
                assert_eq!(c, 0);
                break;
            }
            cursor = match a.expect {
                Expectation::PcCursorAt(next) => next,
                ref e => panic!("{e:?}"),
            };
        }
        let observed = run.events.iter().find_map(|e| match e {
            GameEvent::BoxObserved { box_index: 0, mons } => Some(mons.clone()),
            _ => None,
        });
        let observed = observed.expect("BOX1 read in full");
        assert_eq!(
            observed
                .iter()
                .map(|m| (m.slot, m.species.value.clone().unwrap()))
                .collect::<Vec<_>>(),
            [
                (0, "SPECIES_EKANS".to_owned()),
                (1, "SPECIES_RATTATA".to_owned())
            ]
        );
        let o = with_menu(
            storage(frame, PcMode::Box, PcCursor::Cell(0), Some("EKANS")),
            &["WITHDRAW", "SUMMARY", "MARK", "RELEASE", "CANCEL"],
            0,
        );
        let d = run.next(&mut step, &o);
        assert_eq!(pressed(&d), (Button::A, "WITHDRAW".into()));
        run.confirm(&mut step, &d, &o);
        assert_eq!(
            run.events.last(),
            Some(&GameEvent::MonWithdrawn {
                box_index: 0,
                box_slot: 0
            })
        );
        assert!(step.ops.is_empty());
    }

    /// Switch, Celadon: BOX1 searched, the next box (BOX2, empty) scrolled
    /// in and the hand came down from the title to cell 2, where it covers
    /// the title: nothing read, and the swap waited until it failed. A row
    /// down uncovers it.
    #[test]
    fn a_title_under_the_hand_is_uncovered_a_row_down() {
        let Some(data) = data() else { return };
        let mut run = Run {
            state: state(&["SPECIES_IVYSAUR", "SPECIES_PIDGEY"], None),
            events: Vec::new(),
        };
        let mut step = PcSwapStep::new(
            Arc::clone(&data),
            None,
            "SPECIES_PIDGEY",
            "SPECIES_PARAS",
            &run.state,
        );
        step.ops.pop_front();
        let mut o = storage(1, PcMode::Box, PcCursor::Cell(2), None);
        if let Some(pc) = o.pc_storage.as_mut() {
            pc.box_title = None;
        }
        let d = run.settled(&mut step, &o);
        let Decision::Act(a) = &d else {
            panic!("{}", show(&d))
        };
        assert_eq!(a.label, "box: down to uncover the title");
        assert!(matches!(
            a.expect,
            Expectation::PcCursorAt(PcCursor::Cell(8))
        ));
    }

    /// A box the belief holds is not read in full: the hand goes to the
    /// cell it names, and takes the Pokémon once PKMN DATA agrees. When it
    /// doesn't, the box is read.
    #[test]
    fn a_believed_cell_is_gone_to_directly_and_checked() {
        let Some(data) = data() else { return };
        let mut run = Run {
            state: state(
                &["SPECIES_IVYSAUR", "SPECIES_PIDGEY"],
                Some(&[(0, "SPECIES_RATTATA"), (2, "SPECIES_PARAS")]),
            ),
            events: Vec::new(),
        };
        let mut step = PcSwapStep::new(
            Arc::clone(&data),
            None,
            "SPECIES_PIDGEY",
            "SPECIES_PARAS",
            &run.state,
        );
        step.ops.pop_front();
        let o = storage(1, PcMode::Box, PcCursor::Cell(0), Some("RATTATA"));
        let d = run.settled(&mut step, &o);
        assert_eq!(pressed(&d).0, Button::Right);
        let o = storage(100, PcMode::Box, PcCursor::Cell(1), None);
        assert_eq!(pressed(&run.settled(&mut step, &o)).0, Button::Right);
        let o = storage(200, PcMode::Box, PcCursor::Cell(2), Some("PARAS"));
        let d = run.settled(&mut step, &o);
        assert_eq!(pressed(&d), (Button::A, "choose BOX1 cell 2".into()));
        assert!(run.events.is_empty(), "no box read in full");
        // Another belief: PARAS in cell 1, which reads empty: read it all.
        let mut run = Run {
            state: state(&["SPECIES_IVYSAUR"], Some(&[(1, "SPECIES_PARAS")])),
            events: Vec::new(),
        };
        let mut step = PcSwapStep::new(
            Arc::clone(&data),
            None,
            "SPECIES_IVYSAUR",
            "SPECIES_PARAS",
            &run.state,
        );
        assert_eq!(
            step.ops.front(),
            Some(&Op::Withdraw("SPECIES_PARAS".into())),
            "a lone member is stored after the other is taken"
        );
        let o = storage(1, PcMode::Box, PcCursor::Cell(1), None);
        let d = run.settled(&mut step, &o);
        assert!(step.distrusted.contains(&0));
        assert!(!pressed(&d).1.starts_with("choose"));
    }
}
