//! The field bag: open it from the Start menu, go to a pocket, read every
//! row, and close it again (spec §2).
//!
//! Every step is closed-loop on what the bag shows: the Start menu's rows are
//! read to find BAG, the pocket title is read to find the pocket (the bag
//! remembers the last pocket and row), and the rows are read until CANCEL.

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, ItemList, Observation, Pocket};

use crate::{Action, Decision, Expectation};

/// Pocket titles in bag order (Left/Right step through them).
const POCKETS: [(Pocket, &str); 3] = [
    (Pocket::Items, "ITEMS"),
    (Pocket::KeyItems, "KEY ITEMS"),
    (Pocket::PokeBalls, "POKé BALLS"),
];

/// `read` could be `name`: same length, `?` matches any character.
pub(crate) fn fits(name: &str, read: &str) -> bool {
    name.chars().count() == read.chars().count()
        && name
            .chars()
            .zip(read.chars())
            .all(|(a, b)| b == '?' || a == b)
}

/// The pocket whose title reads like `title` (`?` wildcards; unique match).
pub fn pocket_from_title(title: &str) -> Option<Pocket> {
    let mut found = POCKETS.iter().filter(|(_, name)| fits(name, title));
    let (pocket, _) = found.next()?;
    found.next().is_none().then_some(*pocket)
}

fn pocket_index(pocket: Pocket) -> Option<usize> {
    POCKETS.iter().position(|(p, _)| *p == pocket)
}

fn is_cancel(name: &str) -> bool {
    fits("CANCEL", name)
}

/// Bag rows: (name or item key, count).
type Rows = Vec<(String, Option<u16>)>;

/// A row's identity however its name read: `CANCEL`, the item constant, or
/// the text itself when it doesn't resolve.
fn item_key(data: &GameData, name: &str) -> String {
    if is_cancel(name) {
        "CANCEL".to_owned()
    } else {
        data.item_named(name).unwrap_or(name).to_owned()
    }
}

/// Rows as (item key, count): equal when they read the same items.
fn by_item(data: &GameData, rows: &[(String, Option<u16>)]) -> Vec<(String, Option<u16>)> {
    rows.iter()
        .map(|(name, count)| (item_key(data, name), *count))
        .collect()
}

/// Rows read from a pocket as (item constant, count), CANCEL left out.
/// `None` when any other row doesn't resolve to exactly one item, or has no
/// count (key items, which print none, count 1).
pub fn read_rows(data: &GameData, rows: &[(String, Option<u16>)]) -> Option<ItemList> {
    rows.iter()
        .filter(|(name, _)| !is_cancel(name))
        .map(|(name, count)| {
            let item = data.item_named(name)?;
            // Key items print no count: there is only ever one.
            let key = data.items[item].pocket.as_deref() == Some("POCKET_KEY_ITEMS");
            Some((item.to_owned(), count.or(key.then_some(1))?))
        })
        .collect()
}

/// More retries than this in one phase fail the audit.
const MAX_RETRIES: u32 = 12;
/// More retries than this in the whole audit fail it (phase changes don't
/// reset this count).
const MAX_TOTAL_RETRIES: u32 = 40;
/// Start menu rows are 15 px apart; the ▶ sits 4 px below its row's top.
const START_MENU_PITCH: u32 = 15;
/// A wait (unreadable text, no ▶) this long counts as one retry.
const WAIT_RETRY_FRAMES: u64 = 60;
/// Frames to wait for each effect, before the executor's latency allowance
/// (30 more on real hardware): menus open and close with fades.
const MENU_FRAMES: u64 = 60;
const BAG_OPEN_FRAMES: u64 = 120;
const POCKET_FRAMES: u64 = 60;
const CURSOR_FRAMES: u64 = 45;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// In the overworld: open the Start menu.
    Open,
    /// On the Start menu: move to BAG and open it.
    StartMenu,
    /// In the bag, on another pocket: step toward the wanted one.
    Pocket,
    /// On the wanted pocket: read every row.
    Read,
    /// Read: close the bag and the Start menu.
    Close,
}

/// Audits one bag pocket: Start → BAG → pocket → read → close.
pub struct PocketAudit {
    pocket: Pocket,
    phase: Phase,
    /// Rows read so far, top to bottom (each one resolves to an item).
    rows: Vec<(String, Option<u16>)>,
    /// The list's top was on screen (the ▶ on row 0: the list can't be
    /// scrolled then), so the rows collected start at the first item.
    top_seen: bool,
    /// The rows on screen when the last input was sent.
    last_view: Option<Vec<(String, Option<u16>)>>,
    /// The pocket was read and `PocketObserved` emitted (with this summary).
    observed: Option<String>,
    retries: u32,
    /// Retries over the whole audit.
    total_retries: u32,
    /// The last unconfirmed reading of the list: (frame, ▶ row, rows by
    /// item). A reading counts only once a later frame reads the same.
    candidate: Option<(u64, u8, Rows)>,
    /// What the last input was meant to show.
    pending: Option<Expectation>,
    waiting_since: Option<u64>,
}

impl PocketAudit {
    pub fn new(pocket: Pocket) -> Self {
        Self {
            pocket,
            phase: Phase::Open,
            rows: Vec::new(),
            top_seen: false,
            last_view: None,
            observed: None,
            retries: 0,
            total_retries: 0,
            candidate: None,
            pending: None,
            waiting_since: None,
        }
    }

    /// Next input, or Done once the pocket was read and every menu is closed.
    pub fn next(
        &mut self,
        o: &Observation,
        data: &GameData,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        let Some(target) = pocket_index(self.pocket) else {
            return Decision::Fail(format!("{:?} is not a bag pocket", self.pocket));
        };
        let phase = if self.observed.is_some() {
            Phase::Close
        } else if let Some(bag) = &o.bag {
            if pocket_from_title(&bag.pocket) == Some(self.pocket) {
                Phase::Read
            } else {
                Phase::Pocket
            }
        } else if o.menu.is_some() {
            Phase::StartMenu
        } else {
            Phase::Open
        };
        let pending = self.pending.take();
        if phase != self.phase {
            self.phase = phase;
            self.retries = 0;
            self.waiting_since = None;
            self.last_view = None;
        } else if let Some(expect) = pending {
            // New rows (a list scrolling under a ▶ that stays put) are
            // progress too; the same rows read differently are not.
            let scrolled = o
                .bag
                .as_ref()
                .zip(self.last_view.as_ref())
                .is_some_and(|(b, last)| by_item(data, &b.rows) != by_item(data, last));
            if !expect.met(o) && !scrolled {
                self.retry();
            }
        }
        if let Some(fail) = self.exhausted(&format!("no progress in {phase:?}")) {
            return fail;
        }
        match phase {
            Phase::Open => {
                if o.player.is_none() {
                    return self.wait(o, "locating before opening the Start menu");
                }
                self.act(
                    "open the Start menu",
                    Button::Start,
                    Expectation::MenuOpen,
                    MENU_FRAMES,
                )
            }
            Phase::StartMenu => self.start_menu(o),
            Phase::Pocket => {
                let bag = o.bag.as_ref().expect("bag phase");
                let Some(at) = pocket_from_title(&bag.pocket).and_then(pocket_index) else {
                    return self.wait(o, "reading the pocket title");
                };
                let (button, next) = if at < target {
                    (Button::Right, at + 1)
                } else {
                    (Button::Left, at - 1)
                };
                let title = POCKETS[next].1;
                self.act(
                    &format!("bag: {button:?} to {title}"),
                    button,
                    Expectation::BagPocket(title.to_owned()),
                    POCKET_FRAMES,
                )
            }
            Phase::Read => self.read(o, data, events),
            Phase::Close => {
                if o.bag.is_some() {
                    return self.act(
                        "close the bag",
                        Button::B,
                        Expectation::MenuOpen,
                        BAG_OPEN_FRAMES,
                    );
                }
                if o.menu.is_some() {
                    return self.act(
                        "close the Start menu",
                        Button::B,
                        Expectation::BagClosed,
                        MENU_FRAMES,
                    );
                }
                if o.player.is_none() {
                    return self.wait(o, "locating after closing the bag");
                }
                Decision::Done(self.observed.clone().unwrap_or_default())
            }
        }
    }

    /// Moves the Start menu's ▶ to BAG (found by reading the rows) and opens it.
    fn start_menu(&mut self, o: &Observation) -> Decision {
        let menu = o.menu.expect("start menu phase");
        let rows = (menu.window.height + START_MENU_PITCH / 2) / START_MENU_PITCH;
        // Every row must be read: a missing one would shift the ▶'s row.
        if o.menu_lines.len() != rows as usize {
            return self.wait(o, "reading the Start menu");
        }
        let Some(bag) = o.menu_lines.iter().position(|l| fits("BAG", l)) else {
            return self.wait(o, "looking for BAG in the Start menu");
        };
        let cursor = (menu.cursor_y.saturating_sub(menu.window.y) / START_MENU_PITCH) as usize;
        if cursor == bag {
            return self.act(
                "open the BAG",
                Button::A,
                Expectation::BagPocket(String::new()),
                BAG_OPEN_FRAMES,
            );
        }
        let up = cursor > bag;
        let button = if up { Button::Up } else { Button::Down };
        self.act(
            &format!("Start menu: {button:?} toward BAG"),
            button,
            Expectation::MenuCursorMoved {
                from_y: menu.cursor_y,
                up,
            },
            CURSOR_FRAMES,
        )
    }

    /// Reads the pocket from its first row down to CANCEL, then emits
    /// `PocketObserved` and starts closing.
    fn read(&mut self, o: &Observation, data: &GameData, events: &mut Vec<GameEvent>) -> Decision {
        let bag = o.bag.as_ref().expect("read phase");
        let Some(cursor) = bag.cursor else {
            return self.wait(o, "looking for the bag's ▶");
        };
        if bag.rows.is_empty() || read_rows(data, &bag.rows).is_none() {
            return self.wait(o, "reading the pocket's rows");
        }
        // Counts are facts only once two frames read them the same (one
        // misread digit on the Switch must not become a count).
        let view = by_item(data, &bag.rows);
        match self.candidate.take() {
            Some((frame, row, seen)) if row == cursor && frame < o.frame_id => {
                if seen != view {
                    self.candidate = Some((o.frame_id, cursor, view));
                    self.retry();
                    return self.wait(o, "re-reading rows that read differently");
                }
            }
            Some(same) if same.0 >= o.frame_id => {
                self.candidate = Some(same);
                return self.wait(o, "confirming the rows on a later frame");
            }
            _ => {
                self.candidate = Some((o.frame_id, cursor, view));
                return self.wait(o, "confirming the rows on a later frame");
            }
        }
        self.top_seen |= cursor == 0;
        if !self.top_seen {
            self.last_view = Some(bag.rows.clone());
            return self.act(
                "bag: Up to the top of the list",
                Button::Up,
                Expectation::BagCursorAt(cursor - 1),
                CURSOR_FRAMES,
            );
        }
        // Merge in order; a row is the same item however its name read.
        let key = |name: &str| item_key(data, name);
        // Rows seen before must read the same now (the overlap between views).
        let conflict = bag.rows.iter().any(|(name, count)| {
            self.rows
                .iter()
                .any(|(n, c)| key(n) == key(name) && c != count)
        });
        if conflict {
            self.retry();
            return self.wait(o, "re-reading rows that differ from the last view");
        }
        for row in &bag.rows {
            if !self.rows.iter().any(|(name, _)| key(name) == key(&row.0)) {
                self.rows.push(row.clone());
            }
        }
        // The last row of every pocket is CANCEL. Rows that repeat without
        // it are a Down that didn't land yet (a retry), not the end.
        if !bag.rows.iter().any(|(name, _)| is_cancel(name)) {
            self.last_view = Some(bag.rows.clone());
            return self.act(
                "bag: Down to see more rows",
                Button::Down,
                Expectation::BagCursorAt(cursor + 1),
                CURSOR_FRAMES,
            );
        }
        let Some(items) = read_rows(data, &self.rows) else {
            return Decision::Fail(format!("bag rows stopped resolving: {:?}", self.rows));
        };
        self.observed = Some(format!("{:?} pocket read: {items:?}", self.pocket));
        events.push(GameEvent::PocketObserved {
            pocket: self.pocket,
            items,
        });
        self.phase = Phase::Close;
        self.retries = 0;
        self.last_view = None;
        self.act(
            "close the bag",
            Button::B,
            Expectation::MenuOpen,
            BAG_OPEN_FRAMES,
        )
    }

    fn act(&mut self, label: &str, button: Button, expect: Expectation, timeout: u64) -> Decision {
        self.waiting_since = None;
        self.candidate = None;
        self.pending = Some(expect.clone());
        Decision::Act(Action::new(
            label,
            vec![ControllerCommand::Press(button)],
            expect,
            timeout,
        ))
    }

    /// Waits; every [`WAIT_RETRY_FRAMES`] of waiting counts as a retry.
    fn wait(&mut self, o: &Observation, reason: &str) -> Decision {
        let since = *self.waiting_since.get_or_insert(o.frame_id);
        if o.frame_id.saturating_sub(since) >= WAIT_RETRY_FRAMES {
            self.retry();
            self.waiting_since = Some(o.frame_id);
        }
        if let Some(fail) = self.exhausted(reason) {
            return fail;
        }
        Decision::Wait(reason.to_owned())
    }

    fn retry(&mut self) {
        self.retries += 1;
        self.total_retries += 1;
    }

    /// Fails once a phase, or the whole audit, has used up its retries.
    fn exhausted(&self, reason: &str) -> Option<Decision> {
        if self.retries > MAX_RETRIES {
            return Some(Decision::Fail(format!(
                "bag audit ({:?}): {reason} after {MAX_RETRIES} retries in {:?}",
                self.pocket, self.phase
            )));
        }
        if self.total_retries > MAX_TOTAL_RETRIES {
            return Some(Decision::Fail(format!(
                "bag audit ({:?}): {reason}; {MAX_TOTAL_RETRIES} retries used in all",
                self.pocket
            )));
        }
        None
    }

    /// The pocket was read and `PocketObserved` emitted.
    pub fn observed(&self) -> bool {
        self.observed.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pokebot_state::{
        BagObservation, MenuObservation, Observed, PlayerPose, PoseObservation, Region, ScreenState,
    };

    use super::*;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    fn rows(list: &[(&str, Option<u16>)]) -> Vec<(String, Option<u16>)> {
        list.iter().map(|(n, c)| ((*n).to_owned(), *c)).collect()
    }

    #[test]
    fn pocket_titles() {
        assert_eq!(pocket_from_title("POKé BALLS"), Some(Pocket::PokeBalls));
        assert_eq!(pocket_from_title("?TEMS"), Some(Pocket::Items));
        assert_eq!(pocket_from_title("KEY ITEMS"), Some(Pocket::KeyItems));
        assert_eq!(pocket_from_title("KEY IT?MS"), Some(Pocket::KeyItems));
        assert_eq!(pocket_from_title("?????"), Some(Pocket::Items));
        assert_eq!(pocket_from_title("BERRIES"), None);
        assert_eq!(pocket_from_title(""), None);
    }

    #[test]
    fn rows_resolve_to_item_constants() {
        let Some(data) = data() else { return };
        assert_eq!(
            read_rows(&data, &rows(&[("POKé BALL", Some(3)), ("CANCEL", None)])),
            Some(vec![("ITEM_POKE_BALL".to_owned(), 3)])
        );
        assert_eq!(read_rows(&data, &rows(&[("CANCEL", None)])), Some(vec![]));
        assert_eq!(
            read_rows(&data, &rows(&[("TEACHY TV", None), ("CANCEL", None)])),
            Some(vec![("ITEM_TEACHY_TV".to_owned(), 1)])
        );
        // An unknown or ambiguous name, or an item without a count, is not
        // a reading.
        for name in ["QQQ", "???? BALL"] {
            assert_eq!(
                read_rows(&data, &rows(&[(name, Some(3)), ("CANCEL", None)])),
                None,
                "{name}"
            );
        }
        assert_eq!(
            read_rows(&data, &rows(&[("POKé BALL", None), ("CANCEL", None)])),
            None
        );
    }

    fn screen(value: ScreenState) -> Observed<ScreenState> {
        Observed {
            value,
            detector: "test".into(),
        }
    }

    fn overworld(frame: u64) -> Observation {
        let mut o = Observation::bare(frame, screen(ScreenState::Unknown), Default::default());
        o.player = Some(PoseObservation {
            pose: PlayerPose {
                map: "PewterCity".into(),
                x: 17,
                y: 26,
            },
            score: 980,
        });
        o
    }

    /// The Start menu as measured on the emulator: window (174,6) 60×108,
    /// rows 15 px apart, the ▶ 4 px below its row's top.
    fn start_menu(frame: u64, cursor: u32) -> Observation {
        let mut o = Observation::bare(frame, screen(ScreenState::Menu), Default::default());
        o.menu = Some(MenuObservation {
            window: Region::new(174, 6, 60, 108),
            rows: 6,
            cursor_row: cursor.min(5) as u8,
            cursor_y: 10 + 15 * cursor,
        });
        o.menu_lines = ["POKéDEX", "POKéMON", "BAG", "RED", "SAVE", "OPTION", "EXIT"]
            .map(String::from)
            .to_vec();
        o
    }

    fn bag(frame: u64, pocket: &str, list: &[(&str, Option<u16>)], cursor: u8) -> Observation {
        let mut o = Observation::bare(frame, screen(ScreenState::Bag), Default::default());
        o.bag = Some(BagObservation {
            pocket: pocket.into(),
            rows: rows(list),
            cursor: Some(cursor),
            prompt: None,
        });
        o
    }

    fn act(decision: Decision) -> Action {
        match decision {
            Decision::Act(action) => action,
            Decision::Wait(r) => panic!("waited: {r}"),
            Decision::Done(r) => panic!("done: {r}"),
            Decision::Fail(r) => panic!("failed: {r}"),
        }
    }

    /// A list reading counts only when a later frame reads the same: the
    /// first frame waits, the second (`frame + 1`) decides.
    fn twice(
        audit: &mut PocketAudit,
        o: Observation,
        data: &GameData,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        let first = audit.next(&o, data, events);
        assert!(matches!(first, Decision::Wait(_)), "first frame acted");
        let mut again = o;
        again.frame_id += 1;
        audit.next(&again, data, events)
    }

    fn pressed(action: &Action) -> Button {
        match action.commands.as_slice() {
            [ControllerCommand::Press(b)] => *b,
            other => panic!("not one press: {other:?}"),
        }
    }

    #[test]
    fn bag_expectations() {
        let balls = bag(1, "POKé BALLS", &[("CANCEL", None)], 0);
        assert!(Expectation::BagPocket(String::new()).met(&balls));
        assert!(Expectation::BagPocket("POKé BALLS".into()).met(&balls));
        // A title read with an unknown glyph is still the same pocket.
        assert!(Expectation::BagPocket("POK? BALLS".into()).met(&balls));
        assert!(!Expectation::BagPocket("KEY ITEMS".into()).met(&balls));
        assert!(Expectation::BagCursorAt(0).met(&balls));
        assert!(!Expectation::BagCursorAt(1).met(&balls));
        assert!(!Expectation::BagClosed.met(&balls));
        assert!(!Expectation::BagClosed.met(&start_menu(2, 2)));
        assert!(Expectation::BagClosed.met(&overworld(3)));
        assert!(!Expectation::BagPocket(String::new()).met(&overworld(3)));
    }

    #[test]
    fn audits_the_poke_balls_pocket() {
        let Some(data) = data() else { return };
        let mut audit = PocketAudit::new(Pocket::PokeBalls);
        let mut events = Vec::new();
        let balls = [("POKé BALL", Some(5)), ("CANCEL", None)];

        let a = act(audit.next(&overworld(1), &data, &mut events));
        assert_eq!(
            (a.label.as_str(), pressed(&a)),
            ("open the Start menu", Button::Start)
        );
        assert_eq!(a.expect, Expectation::MenuOpen);

        // ▶ on POKéDEX, BAG is the third row: Down, Down, A.
        let a = act(audit.next(&start_menu(2, 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::Down);
        assert_eq!(
            a.expect,
            Expectation::MenuCursorMoved {
                from_y: 10,
                up: false
            }
        );
        let a = act(audit.next(&start_menu(3, 1), &data, &mut events));
        assert_eq!(pressed(&a), Button::Down);
        let a = act(audit.next(&start_menu(4, 2), &data, &mut events));
        assert_eq!((a.label.as_str(), pressed(&a)), ("open the BAG", Button::A));
        assert_eq!(a.expect, Expectation::BagPocket(String::new()));

        // The bag opens on ITEMS: Right toward POKé BALLS, one pocket at a time.
        let a = act(audit.next(&bag(5, "ITEMS", &[("CANCEL", None)], 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::Right);
        assert_eq!(a.expect, Expectation::BagPocket("KEY ITEMS".into()));
        let key = [("TEACHY TV", None), ("TM CASE", None), ("CANCEL", None)];
        let a = act(audit.next(&bag(6, "KEY ITEMS", &key, 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::Right);
        assert_eq!(a.expect, Expectation::BagPocket("POKé BALLS".into()));
        assert!(events.is_empty());

        // CANCEL is visible: the pocket is read, then B closes the bag.
        let o = bag(7, "POKé BALLS", &balls, 0);
        let a = act(twice(&mut audit, o, &data, &mut events));
        assert_eq!(
            (a.label.as_str(), pressed(&a)),
            ("close the bag", Button::B)
        );
        assert_eq!(
            events,
            vec![GameEvent::PocketObserved {
                pocket: Pocket::PokeBalls,
                items: vec![("ITEM_POKE_BALL".into(), 5)],
            }]
        );
        // Back on the Start menu (▶ still on BAG): B closes it.
        let a = act(audit.next(&start_menu(9, 2), &data, &mut events));
        assert_eq!(
            (a.label.as_str(), pressed(&a)),
            ("close the Start menu", Button::B)
        );
        assert_eq!(a.expect, Expectation::BagClosed);
        assert!(matches!(
            audit.next(&overworld(10), &data, &mut events),
            Decision::Done(_)
        ));
        assert_eq!(events.len(), 1, "one PocketObserved");
    }

    #[test]
    fn remembered_pocket_and_row_are_read_from_the_top() {
        let Some(data) = data() else { return };
        let mut audit = PocketAudit::new(Pocket::Items);
        let mut events = Vec::new();
        // The bag reopens on POKé BALLS with the ▶ on CANCEL (row 1).
        let a = act(audit.next(
            &bag(
                1,
                "POKé BALLS",
                &[("POKé BALL", Some(5)), ("CANCEL", None)],
                1,
            ),
            &data,
            &mut events,
        ));
        assert_eq!(pressed(&a), Button::Left);
        assert_eq!(a.expect, Expectation::BagPocket("KEY ITEMS".into()));
        let items = [("POTION", Some(1)), ("ANTIDOTE", Some(2)), ("CANCEL", None)];
        let a = act(twice(
            &mut audit,
            bag(2, "ITEMS", &items, 2),
            &data,
            &mut events,
        ));
        assert_eq!(pressed(&a), Button::Up);
        assert_eq!(a.expect, Expectation::BagCursorAt(1));
        let a = act(twice(
            &mut audit,
            bag(4, "ITEMS", &items, 1),
            &data,
            &mut events,
        ));
        assert_eq!(pressed(&a), Button::Up);
        let a = act(twice(
            &mut audit,
            bag(6, "ITEMS", &items, 0),
            &data,
            &mut events,
        ));
        assert_eq!(pressed(&a), Button::B);
        assert_eq!(
            events,
            vec![GameEvent::PocketObserved {
                pocket: Pocket::Items,
                items: vec![("ITEM_POTION".into(), 1), ("ITEM_ANTIDOTE".into(), 2)],
            }]
        );
    }

    #[test]
    fn long_pockets_are_scrolled_and_merged() {
        let Some(data) = data() else { return };
        let mut audit = PocketAudit::new(Pocket::Items);
        let mut events = Vec::new();
        let top = [
            ("POTION", Some(1)),
            ("ANTIDOTE", Some(2)),
            ("PARLYZ HEAL", Some(3)),
            ("AWAKENING", Some(4)),
            ("BURN HEAL", Some(5)),
            ("ICE HEAL", Some(6)),
        ];
        let a = act(twice(
            &mut audit,
            bag(1, "ITEMS", &top, 0),
            &data,
            &mut events,
        ));
        assert_eq!(pressed(&a), Button::Down);
        assert_eq!(a.expect, Expectation::BagCursorAt(1));
        // The list scrolls under a ▶ that stays put: new rows count as progress.
        let scrolled = [
            ("ANTIDOTE", Some(2)),
            ("PARLYZ HEAL", Some(3)),
            ("AWAKENING", Some(4)),
            ("BURN HEAL", Some(5)),
            ("ICE HEAL", Some(6)),
            ("CANCEL", None),
        ];
        let a = act(twice(
            &mut audit,
            bag(3, "ITEMS", &scrolled, 3),
            &data,
            &mut events,
        ));
        assert_eq!(pressed(&a), Button::B);
        let GameEvent::PocketObserved { items, .. } = &events[0] else {
            panic!("{events:?}")
        };
        let names: Vec<&str> = items.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(
            names,
            [
                "ITEM_POTION",
                "ITEM_ANTIDOTE",
                "ITEM_PARALYZE_HEAL",
                "ITEM_AWAKENING",
                "ITEM_BURN_HEAL",
                "ITEM_ICE_HEAL"
            ]
        );
    }

    #[test]
    fn no_progress_fails_after_twelve_retries() {
        let Some(data) = data() else { return };
        let mut audit = PocketAudit::new(Pocket::PokeBalls);
        let mut events = Vec::new();
        // Start never opens the menu.
        for frame in 0..=12 {
            let a = act(audit.next(&overworld(frame), &data, &mut events));
            assert_eq!(pressed(&a), Button::Start);
        }
        assert!(matches!(
            audit.next(&overworld(13), &data, &mut events),
            Decision::Fail(_)
        ));
    }

    #[test]
    fn unreadable_start_menu_is_not_guessed() {
        let Some(data) = data() else { return };
        let mut audit = PocketAudit::new(Pocket::PokeBalls);
        let mut events = Vec::new();
        // A row missing from the reading shifts the rows: wait, never press.
        let mut o = start_menu(1, 0);
        o.menu_lines.remove(1);
        assert!(matches!(
            audit.next(&o, &data, &mut events),
            Decision::Wait(_)
        ));
        let mut o = start_menu(2, 0);
        o.menu_lines[2] = "B?Z".into();
        assert!(matches!(
            audit.next(&o, &data, &mut events),
            Decision::Wait(_)
        ));
    }

    fn observed(events: &[GameEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, GameEvent::PocketObserved { .. }))
            .count()
    }

    #[test]
    fn a_misread_count_is_re_read_until_two_frames_agree() {
        let Some(data) = data() else { return };
        let mut audit = PocketAudit::new(Pocket::PokeBalls);
        let mut events = Vec::new();
        let count = |frame, n| {
            bag(
                frame,
                "POKé BALLS",
                &[("POKé BALL", Some(n)), ("CANCEL", None)],
                0,
            )
        };
        // ×8, then a misread ×3, then ×8 again: no two frames agree yet.
        for (frame, n) in [(1, 8), (2, 3), (3, 8)] {
            assert!(matches!(
                audit.next(&count(frame, n), &data, &mut events),
                Decision::Wait(_)
            ));
        }
        assert_eq!(observed(&events), 0);
        let a = act(audit.next(&count(4, 8), &data, &mut events));
        assert_eq!(pressed(&a), Button::B);
        assert_eq!(
            events,
            vec![GameEvent::PocketObserved {
                pocket: Pocket::PokeBalls,
                items: vec![("ITEM_POKE_BALL".into(), 8)],
            }]
        );
        // Both disagreements were retries.
        assert_eq!(audit.total_retries, 2);
    }

    #[test]
    fn rows_that_disagree_with_the_last_view_are_never_merged() {
        let Some(data) = data() else { return };
        let mut audit = PocketAudit::new(Pocket::Items);
        let mut events = Vec::new();
        let top = [
            ("POTION", Some(1)),
            ("ANTIDOTE", Some(2)),
            ("PARLYZ HEAL", Some(3)),
            ("AWAKENING", Some(4)),
            ("BURN HEAL", Some(5)),
            ("ICE HEAL", Some(6)),
        ];
        let a = act(twice(
            &mut audit,
            bag(1, "ITEMS", &top, 0),
            &data,
            &mut events,
        ));
        assert_eq!(pressed(&a), Button::Down);
        // After scrolling, ANTIDOTE reads ×7 (it was ×2) on every frame.
        let scrolled = [
            ("ANTIDOTE", Some(7)),
            ("PARLYZ HEAL", Some(3)),
            ("AWAKENING", Some(4)),
            ("BURN HEAL", Some(5)),
            ("ICE HEAL", Some(6)),
            ("CANCEL", None),
        ];
        let mut failed = false;
        for frame in 3..40 {
            match audit.next(&bag(frame, "ITEMS", &scrolled, 3), &data, &mut events) {
                Decision::Wait(_) => {}
                Decision::Fail(_) => {
                    failed = true;
                    break;
                }
                Decision::Act(a) => panic!("acted on conflicting rows: {}", a.label),
                Decision::Done(r) => panic!("done: {r}"),
            }
        }
        assert!(failed, "a lasting conflict fails the audit");
        assert_eq!(observed(&events), 0);
    }

    #[test]
    fn reading_noise_is_not_progress() {
        let Some(data) = data() else { return };
        let mut audit = PocketAudit::new(Pocket::Items);
        let mut events = Vec::new();
        let top = |potion: &'static str| {
            [
                (potion, Some(1)),
                ("ANTIDOTE", Some(2)),
                ("PARLYZ HEAL", Some(3)),
                ("AWAKENING", Some(4)),
                ("BURN HEAL", Some(5)),
                ("ICE HEAL", Some(6)),
            ]
        };
        let a = act(twice(
            &mut audit,
            bag(1, "ITEMS", &top("POTION"), 0),
            &data,
            &mut events,
        ));
        assert_eq!(pressed(&a), Button::Down);
        // Down never lands; the same rows read with and without a `?`
        // must not count as the list scrolling.
        let mut failed = None;
        for frame in 3..80 {
            let name = if frame % 2 == 0 { "POTION" } else { "POT?ON" };
            if let Decision::Fail(r) =
                audit.next(&bag(frame, "ITEMS", &top(name), 0), &data, &mut events)
            {
                failed = Some(r);
                break;
            }
        }
        let reason = failed.expect("no progress fails");
        assert!(reason.contains("retries"), "{reason}");
        assert_eq!(observed(&events), 0);
    }

    #[test]
    fn total_retries_are_capped_across_phases() {
        let Some(data) = data() else { return };
        let mut audit = PocketAudit::new(Pocket::PokeBalls);
        let mut events = Vec::new();
        // Each cycle: Start fails to open the menu 11 times (under the
        // phase limit), then a partly drawn menu resets the phase.
        let mut frame = 0;
        let mut failed = None;
        'cycles: for _ in 0..5 {
            for _ in 0..12 {
                frame += 1;
                match audit.next(&overworld(frame), &data, &mut events) {
                    Decision::Act(_) => {}
                    Decision::Fail(r) => {
                        failed = Some(r);
                        break 'cycles;
                    }
                    _ => panic!("unexpected decision"),
                }
            }
            frame += 1;
            let mut partial = start_menu(frame, 0);
            partial.menu_lines.truncate(1);
            assert!(matches!(
                audit.next(&partial, &data, &mut events),
                Decision::Wait(_)
            ));
        }
        let reason = failed.expect("the total cap fails the audit");
        assert!(reason.contains("in all"), "{reason}");
        assert!(audit.total_retries > MAX_TOTAL_RETRIES);
        assert!(audit.retries <= MAX_RETRIES);
    }
}
