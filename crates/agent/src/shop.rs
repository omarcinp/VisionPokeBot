//! Buying at a Poké Mart (spec §2 Buying).
//!
//! Every step is closed-loop on what the mart shows: the BUY / SELL / SEE
//! YA! menu's rows are read to find BUY, the item's row is found by its
//! name, the quantity is stepped one Up or Down at a time until the box
//! reads the count, and the "…you want N. That will be ¥T. Okay?" text must
//! agree before YES. Back on the list, the money read confirms what was
//! paid. Money, quantities and prices count only once two frames read them
//! the same (a single misread digit on the Switch must never be acted on).

use std::collections::BTreeMap;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, Observation, PlayerPose, Pocket, ScreenState};
use pokebot_world::World;

use crate::bag::fits;
use crate::nav::{goal_tiles, nearest_reachable, Destination, Gone};
use crate::stock::{affordable, buy_count};
use crate::{Action, Decision, Expectation};

/// More retries than this in one phase fail the purchase.
const MAX_RETRIES: u32 = 12;
/// More retries than this in the whole purchase fail it.
const MAX_TOTAL_RETRIES: u32 = 40;
/// A wait (unreadable text or numbers) this long counts as one retry.
const WAIT_RETRY_FRAMES: u64 = 60;
/// Cursor steps and scrolls in the list before giving up on the item.
const MAX_LIST_MOVES: u32 = 40;
/// SEE YA! presses before giving up on leaving (an A that lands after the
/// last page closed talks to the clerk again).
const MAX_LEAVES: u32 = 8;
/// The quantity box never goes past this.
const MAX_QUANTITY: u16 = 99;
/// Frames to wait for each effect, before the executor's latency allowance.
/// Mart text prints slowly: about 240 frames after each A (probe §1).
const CURSOR_FRAMES: u64 = 45;
const LIST_FRAMES: u64 = 120;
const TEXT_FRAMES: u64 = 240;

/// Pages that end in a question whose box (quantity, YES/NO, the mart
/// menu) appears once they're printed: A there would answer it unchecked.
const QUESTION_PAGES: [&str; 5] = ["How many", "Okay?", "OK?", "help you", "anything else"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// BUY / SELL / SEE YA! over the clerk's text.
    Menu,
    /// The item list has focus (MONEY, rows, ▶).
    List,
    /// The quantity box has focus.
    Quantity,
    /// "…you want N. That will be ¥T. Okay?" with YES/NO.
    Confirm,
    /// Clerk text without a menu.
    Text,
    /// Back in the overworld.
    Outside,
    /// Anything else (fades, a frame between screens).
    Other,
}

/// The mart's BUY / SELL / SEE YA! menu is showing.
pub(crate) fn is_mart_menu(o: &Observation) -> bool {
    o.menu.is_some()
        && o.menu_lines.iter().any(|l| fits("BUY", l))
        && o.menu_lines.iter().any(|l| fits("SELL", l))
}

fn is_yes_no(o: &Observation) -> bool {
    matches!(o.menu_lines.as_slice(), [yes, no] if fits("YES", yes) && fits("NO", no))
}

/// The number right after `marker` in `text` (`,` separators skipped).
fn number_after(text: &str, marker: &str) -> Option<u32> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// A list row's identity however its name read: the item constant, or the
/// text itself when it doesn't resolve (CANCEL, a misread).
fn item_key(data: &GameData, name: &str) -> String {
    if fits("CANCEL", name) {
        "CANCEL".to_owned()
    } else {
        data.item_named(name).unwrap_or(name).to_owned()
    }
}

/// Rows as (item key, price).
type Rows = Vec<(String, Option<u32>)>;

/// Buys `count` of one item at the mart whose conversation is open: BUY →
/// item → quantity → YES → check the money → leave.
pub struct Purchase {
    item: String,
    /// Requested count; 0 = decide on the list from the money and `stock`.
    count: u16,
    /// Balls held (for `count == 0`).
    stock: Option<u16>,
    phase: Phase,
    /// The count being bought, once decided on the list.
    target: Option<u16>,
    /// Money on the list before buying (confirmed on two frames).
    money_before: Option<u32>,
    /// `MoneyObserved` was emitted.
    money_seen: bool,
    /// (count, total) the quantity box showed when A was pressed on it.
    quoted: Option<(u16, u32)>,
    /// (count, total) answered YES to: the next list read settles it.
    paid: Option<(u16, u32)>,
    /// The clerk's "Here you are!" was read after YES: the purchase went
    /// through whatever the money window reads.
    handed_over: bool,
    /// The quantity box's maximum, learned from an Up that wrapped.
    max_quantity: Option<u16>,
    /// The count an Up was pressed from (to spot a wrap).
    up_from: Option<u16>,
    /// Nothing more to buy: close the list and leave.
    finished: bool,
    /// SEE YA! was pressed.
    leaves: u32,
    list_moves: u32,
    summary: Vec<String>,
    retries: u32,
    total_retries: u32,
    /// The last unconfirmed reading: (frame, what it read).
    candidate: Option<(u64, String)>,
    pending: Option<Expectation>,
    /// List rows when the last list input was sent (a scroll is progress).
    last_rows: Option<Rows>,
    waiting_since: Option<u64>,
}

impl Purchase {
    /// `count == 0` means "decide on the list": the count comes from
    /// [`crate::stock::buy_count`] once the money is read, and nothing is
    /// bought when it is 0.
    pub fn new(item: &str, count: u16) -> Self {
        Self {
            item: item.to_owned(),
            count,
            stock: None,
            phase: Phase::Other,
            target: None,
            money_before: None,
            money_seen: false,
            quoted: None,
            paid: None,
            handed_over: false,
            max_quantity: None,
            up_from: None,
            finished: false,
            leaves: 0,
            list_moves: 0,
            summary: Vec::new(),
            retries: 0,
            total_retries: 0,
            candidate: None,
            pending: None,
            last_rows: None,
            waiting_since: None,
        }
    }

    /// The balls held, for a `count == 0` purchase (unknown: buy nothing).
    pub fn with_stock(mut self, stock: Option<u16>) -> Self {
        self.stock = stock;
        self
    }

    /// Something was decided (bought, or found nothing to buy).
    pub fn finished(&self) -> bool {
        self.finished
    }

    fn phase_of(o: &Observation) -> Phase {
        if let Some(shop) = &o.shop {
            return if shop.cursor.is_some() && shop.quantity.is_none() {
                Phase::List
            } else {
                Phase::Quantity
            };
        }
        if o.menu.is_some() {
            if is_mart_menu(o) {
                return Phase::Menu;
            }
            if o.dialogue.is_some() && is_yes_no(o) {
                return Phase::Confirm;
            }
            return Phase::Other;
        }
        if o.dialogue.is_some() {
            return Phase::Text;
        }
        if o.player.is_some() {
            return Phase::Outside;
        }
        Phase::Other
    }

    /// Next input, or Done once the mart is left.
    pub fn next(
        &mut self,
        o: &Observation,
        data: &GameData,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        let phase = Self::phase_of(o);
        let pending = self.pending.take();
        if phase != self.phase {
            self.phase = phase;
            self.retries = 0;
            self.waiting_since = None;
            self.candidate = None;
            self.last_rows = None;
        } else if let Some(expect) = pending {
            // New rows under a ▶ that stays put (the list scrolled) are
            // progress too.
            let scrolled = o
                .shop
                .as_ref()
                .zip(self.last_rows.as_ref())
                .is_some_and(|(s, last)| rows_of(data, &s.items) != *last);
            if !expect.met(o) && !scrolled {
                self.retry();
            }
        }
        if let Some(fail) = self.exhausted(&format!("no progress in {phase:?}")) {
            return fail;
        }
        match phase {
            Phase::Menu => self.menu(o),
            Phase::List => self.list(o, data, events),
            Phase::Quantity => self.quantity(o, events),
            Phase::Confirm => self.confirm(o, events),
            Phase::Text => self.text(o),
            // Only after SEE YA!: a dropped frame between the list closing
            // and the menu coming back can show the overworld.
            Phase::Outside if self.finished && self.leaves > 0 => {
                Decision::Done(if self.summary.is_empty() {
                    "left the mart".to_owned()
                } else {
                    self.summary.join("; ")
                })
            }
            Phase::Outside => self.wait(o, "waiting for the mart menu"),
            Phase::Other => self.wait(o, "waiting for a mart screen"),
        }
    }

    /// BUY (or, when done, B = SEE YA!).
    fn menu(&mut self, o: &Observation) -> Decision {
        let menu = o.menu.expect("menu phase");
        if self.finished {
            self.leaves += 1;
            if self.leaves > MAX_LEAVES {
                return Decision::Fail("mart: could not leave the clerk".into());
            }
            return self.act(
                "mart: SEE YA!",
                Button::B,
                Expectation::MenuClosed,
                TEXT_FRAMES,
            );
        }
        // Every row must be read: a missing one would shift BUY's row.
        if o.menu_lines.len() != usize::from(menu.rows) {
            return self.wait(o, "reading the mart menu");
        }
        let Some(buy) = o.menu_lines.iter().position(|l| fits("BUY", l)) else {
            return self.wait(o, "looking for BUY");
        };
        let buy = buy as u8;
        if menu.cursor_row == buy {
            return self.act("mart: BUY", Button::A, Expectation::MenuClosed, LIST_FRAMES);
        }
        let (button, next) = if menu.cursor_row > buy {
            (Button::Up, menu.cursor_row - 1)
        } else {
            (Button::Down, menu.cursor_row + 1)
        };
        self.act(
            &format!("mart menu: {button:?} toward BUY"),
            button,
            Expectation::MenuCursorAt(next),
            CURSOR_FRAMES,
        )
    }

    /// Reads the money (settling a purchase), decides the count, and moves
    /// the ▶ to the item's row.
    fn list(&mut self, o: &Observation, data: &GameData, events: &mut Vec<GameEvent>) -> Decision {
        let shop = o.shop.as_ref().expect("list phase");
        let (Some(money), Some(cursor)) = (shop.money, shop.cursor) else {
            return self.wait(o, "reading the money");
        };
        let rows = rows_of(data, &shop.items);
        // Only what the decision uses is confirmed: the money, the ▶ and the
        // item's row (flicker on other rows must not burn retries).
        let item_row = rows.iter().position(|(key, _)| *key == self.item);
        if let Some(wait) = self.confirmed(o, format!("list ¥{money} ▶{cursor} {item_row:?}")) {
            return wait;
        }
        if !self.money_seen {
            self.money_seen = true;
            events.push(GameEvent::MoneyObserved { amount: money });
        }
        if let Some(paid) = self.paid.take() {
            self.settle(money, paid, data, events);
            self.finished = true;
        }
        self.money_before = Some(money);
        if self.finished {
            return self.act(
                "mart: close the list",
                Button::B,
                Expectation::ScreenIsNot(ScreenState::Shop),
                TEXT_FRAMES,
            );
        }
        let target = match self.target {
            Some(t) => t,
            None => match self.decide(money, data, events) {
                Ok(0) => {
                    self.finished = true;
                    return self.act(
                        "mart: close the list",
                        Button::B,
                        Expectation::ScreenIsNot(ScreenState::Shop),
                        TEXT_FRAMES,
                    );
                }
                Ok(t) => {
                    self.target = Some(t);
                    t
                }
                Err(fail) => return fail,
            },
        };
        let row = rows.iter().position(|(key, _)| *key == self.item);
        if row == Some(usize::from(cursor)) {
            return self.act(
                &format!("mart: choose {} (buying {target})", self.item),
                Button::A,
                Expectation::ShopQuantity(1),
                TEXT_FRAMES,
            );
        }
        self.list_moves += 1;
        if self.list_moves > MAX_LIST_MOVES {
            return Decision::Fail(format!("mart: {} not found in the list", self.item));
        }
        let at_end = rows.iter().any(|(key, _)| key == "CANCEL");
        let up = match row {
            Some(r) => r < usize::from(cursor),
            // The whole list is on screen and the item isn't in it.
            None if at_end && cursor == 0 => {
                return Decision::Fail(format!("mart: this mart doesn't sell {}", self.item))
            }
            // Not visible: scroll down, or back up from the end.
            None => at_end,
        };
        self.last_rows = Some(rows);
        let (button, next) = if up {
            (Button::Up, cursor.saturating_sub(1))
        } else {
            (Button::Down, cursor + 1)
        };
        self.act(
            &format!("mart list: {button:?} toward {}", self.item),
            button,
            Expectation::ShopCursorAt(next),
            CURSOR_FRAMES,
        )
    }

    /// How many to buy with `money`: the requested count (or, for 0, the
    /// stock policy's), capped by what the money pays for.
    fn decide(
        &mut self,
        money: u32,
        data: &GameData,
        events: &mut Vec<GameEvent>,
    ) -> Result<u16, Decision> {
        let Some(price) = data
            .items
            .get(&self.item)
            .map(|i| i.price)
            .filter(|p| *p > 0)
        else {
            return Err(Decision::Fail(format!("mart: no price for {}", self.item)));
        };
        let want = if self.count > 0 {
            self.count
        } else if let Some(stock) = self.stock {
            buy_count(data, stock, money)
        } else {
            self.log(
                events,
                format!("buying 0 {}: the stock is unknown", self.item),
            );
            return Ok(0);
        };
        // An explicit count keeps the Potion money too, like the stock
        // policy's.
        let affordable = affordable(data, &self.item, money).min(MAX_QUANTITY);
        let count = want.min(affordable);
        if count == 0 {
            self.log(
                events,
                format!(
                    "buying 0 {} with ¥{money} (wanted {want}, ¥{price} each, Potion money kept)",
                    self.item
                ),
            );
        } else if count < want {
            self.log(
                events,
                format!("¥{money} buys only {count} of {want} {}", self.item),
            );
        }
        Ok(count)
    }

    /// Back on the list after YES: the money read must be the old amount
    /// minus the total, or the purchase is only partly known.
    fn settle(
        &mut self,
        money: u32,
        (count, total): (u16, u32),
        data: &GameData,
        events: &mut Vec<GameEvent>,
    ) {
        let before = self.money_before.unwrap_or(money);
        let pocket = data
            .items
            .get(&self.item)
            .and_then(|i| i.pocket.as_deref())
            .and_then(Pocket::from_decomp);
        // Money went down, or the clerk handed the items over: the purchase
        // went through.
        let handed_over = std::mem::take(&mut self.handed_over);
        if money < before || handed_over {
            if let Some(pocket) = pocket {
                events.push(GameEvent::ItemsChanged {
                    pocket,
                    item: self.item.clone(),
                    delta: i32::from(count),
                    reason: "bought".into(),
                });
            }
        }
        if before.checked_sub(total) == Some(money) {
            events.push(GameEvent::MoneyChanged {
                delta: -i64::from(total),
                reason: "bought".into(),
            });
            self.summary
                .push(format!("bought {count} {} for ¥{total}", self.item));
        } else {
            events.push(GameEvent::MoneyObserved { amount: money });
            self.log(
                events,
                format!(
                    "money after buying {count} {} reads ¥{money}, expected ¥{} (¥{before} − ¥{total})",
                    self.item,
                    i64::from(before) - i64::from(total)
                ),
            );
        }
    }

    /// Steps the quantity to the target, one Up or Down at a time, and
    /// presses A once two frames read exactly the target.
    fn quantity(&mut self, o: &Observation, events: &mut Vec<GameEvent>) -> Decision {
        let shop = o.shop.as_ref().expect("quantity phase");
        let Some((count, total)) = shop.quantity else {
            return self.wait(o, "reading the quantity");
        };
        if let Some(wait) = self.confirmed(o, format!("×{count} ¥{total}")) {
            return wait;
        }
        let Some(mut target) = self.target else {
            return Decision::Fail(
                "mart: the quantity box opened before a count was decided".into(),
            );
        };
        // An Up that went down wrapped past the box's maximum.
        if let Some(from) = self.up_from.take() {
            if count < from {
                self.max_quantity = Some(from);
            }
        }
        if let Some(max) = self.max_quantity.filter(|m| *m < target) {
            self.log(
                events,
                format!(
                    "the quantity box stops at {max}: buying {max} {}",
                    self.item
                ),
            );
            target = max;
            self.target = Some(max);
        }
        if count == target {
            self.quoted = Some((count, total));
            return self.act(
                &format!("mart: buy {count} for ¥{total}"),
                Button::A,
                Expectation::Question,
                TEXT_FRAMES,
            );
        }
        // Down from 1 wraps to the maximum, when that is the target.
        let wrap_down = count == 1 && self.max_quantity == Some(target);
        if count < target && !wrap_down {
            self.up_from = Some(count);
            return self.act(
                &format!("mart: quantity Up to {}", count + 1),
                Button::Up,
                Expectation::ShopQuantity(count + 1),
                CURSOR_FRAMES,
            );
        }
        let next = if wrap_down { target } else { count - 1 };
        self.act(
            &format!("mart: quantity Down to {next}"),
            Button::Down,
            Expectation::ShopQuantity(next),
            CURSOR_FRAMES,
        )
    }

    /// YES only when the text names the count and total the box showed.
    fn confirm(&mut self, o: &Observation, events: &mut Vec<GameEvent>) -> Decision {
        let menu = o.menu.expect("confirm phase");
        let page = o
            .dialogue
            .as_ref()
            .map(|d| d.lines.join(" "))
            .unwrap_or_default();
        let (Some(want), Some(total)) = (number_after(&page, "want "), number_after(&page, "¥"))
        else {
            return self.wait(o, "reading the price");
        };
        if let Some(wait) = self.confirmed(o, format!("want {want} ¥{total}")) {
            return wait;
        }
        let agrees = self
            .quoted
            .is_some_and(|(n, t)| u32::from(n) == want && t == total && self.target == Some(n));
        if !agrees {
            self.log(
                events,
                format!(
                    "the clerk asks ¥{total} for {want}, but the box showed {:?}: answering NO",
                    self.quoted
                ),
            );
            self.quoted = None;
            self.finished = true;
            return self.act("mart: NO", Button::B, Expectation::MenuClosed, TEXT_FRAMES);
        }
        if menu.cursor_row != 0 {
            return self.act(
                "mart: Up to YES",
                Button::Up,
                Expectation::MenuCursorAt(0),
                CURSOR_FRAMES,
            );
        }
        self.paid = self.quoted;
        self.act("mart: YES", Button::A, Expectation::MenuClosed, TEXT_FRAMES)
    }

    /// Advances the clerk's pages, but never a page whose question box is
    /// still to come.
    fn text(&mut self, o: &Observation) -> Decision {
        let d = o.dialogue.as_ref().expect("text phase");
        let page = d.lines.join(" ");
        if self.paid.is_some() && page.contains("Here you are") {
            self.handed_over = true;
        }
        if QUESTION_PAGES.iter().any(|q| page.contains(q)) {
            return self.wait(o, "a question: waiting for its box");
        }
        // After SEE YA! only the farewell is left: a settled box is it even
        // when its text wasn't read (it has no ▼ arrow).
        let farewell = self.finished && self.leaves > 0;
        let read = !d.lines.is_empty() || farewell;
        if !(d.waiting_for_input || (d.ready_for_a() && read)) {
            return self.wait(o, "mart text is printing");
        }
        let expect = if farewell {
            Expectation::ShopClosed
        } else {
            Expectation::TextAdvanced {
                kind: d.kind,
                baseline: d.text_cells.clone(),
            }
        };
        self.act("mart: advance text", Button::A, expect, TEXT_FRAMES)
    }

    /// `None` once a later frame read the same `reading`; otherwise a wait.
    fn confirmed(&mut self, o: &Observation, reading: String) -> Option<Decision> {
        match self.candidate.take() {
            Some((frame, seen)) if frame < o.frame_id && seen == reading => {
                self.candidate = Some((frame, seen));
                None
            }
            Some((frame, _)) if frame < o.frame_id => {
                self.candidate = Some((o.frame_id, reading));
                self.retry();
                Some(self.wait(o, "re-reading a number that read differently"))
            }
            Some(same) => {
                self.candidate = Some(same);
                Some(self.wait(o, "confirming on a later frame"))
            }
            None => {
                self.candidate = Some((o.frame_id, reading));
                Some(self.wait(o, "confirming on a later frame"))
            }
        }
    }

    fn log(&mut self, events: &mut Vec<GameEvent>, detail: String) {
        self.summary.push(detail.clone());
        events.push(GameEvent::GoalProgress {
            goal: "Story".into(),
            phase: "Mart".into(),
            detail,
        });
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

    fn exhausted(&self, reason: &str) -> Option<Decision> {
        if self.retries > MAX_RETRIES {
            return Some(Decision::Fail(format!(
                "mart ({}): {reason} after {MAX_RETRIES} retries in {:?}",
                self.item, self.phase
            )));
        }
        if self.total_retries > MAX_TOTAL_RETRIES {
            return Some(Decision::Fail(format!(
                "mart ({}): {reason}; {MAX_TOTAL_RETRIES} retries used in all",
                self.item
            )));
        }
        None
    }
}

fn rows_of(data: &GameData, items: &[(String, Option<u32>)]) -> Rows {
    items
        .iter()
        .map(|(name, price)| (item_key(data, name), *price))
        .collect()
}

/// The nearest mart selling `item` that can be walked to from `pose`
/// (tile-level search across maps, as navigation plans, to the tiles the
/// clerk is talked to from), and its clerk's local id. Ties: the map name.
/// A mart only reachable through blocked ground (a one-way ledge, a cave
/// not yet crossed) doesn't count, however few maps away it is.
pub fn nearest_mart(
    world: &World,
    data: &GameData,
    pose: &PlayerPose,
    item: &str,
    gone: &Gone,
) -> Option<(String, u32)> {
    let clerk = |name: &str| {
        world
            .map(name)?
            .objects
            .iter()
            .filter(|o| o.graphics.as_deref() == Some("OBJ_EVENT_GFX_CLERK"))
            .filter_map(|o| Some((o.local_id, o.x?, o.y?)))
            .min_by_key(|(id, _, _)| *id)
    };
    let mut clerks = BTreeMap::new();
    let mut goals = BTreeMap::new();
    for (name, items) in &data.marts {
        if !items.iter().any(|i| i == item) {
            continue;
        }
        let Some((id, x, y)) = clerk(name) else {
            continue;
        };
        let facing = Destination::Facing {
            map: name.clone(),
            x,
            y,
        };
        goals.insert(name.clone(), goal_tiles(world, &facing));
        clerks.insert(name.clone(), id);
    }
    let map = nearest_reachable(world, pose, &goals, gone)
        .into_iter()
        .next()?;
    let id = clerks[&map];
    Some((map, id))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pokebot_core::{Button, ControllerCommand};
    use pokebot_state::{
        DialogueKind, DialogueObservation, MenuObservation, Observed, PlayerPose, PoseObservation,
        Region, ScreenState, ShopObservation,
    };

    use super::*;
    use crate::{Action, Expectation};

    fn root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn data() -> Option<GameData> {
        GameData::load(root().join("data/world/gamedata.json")).ok()
    }

    fn screen(value: ScreenState) -> Observed<ScreenState> {
        Observed {
            value,
            detector: "test".into(),
        }
    }

    fn dialogue(lines: &[&str], waiting: bool) -> DialogueObservation {
        DialogueObservation {
            kind: DialogueKind::MessageBox,
            region: Region::new(8, 118, 224, 36),
            waiting_for_input: waiting,
            arrow: None,
            stable_frames: 60,
            text_cells: lines.iter().map(|l| l.len() as u8).collect(),
            lines: lines.iter().map(|l| (*l).to_owned()).collect(),
            help: false,
        }
    }

    /// "Hi, there! / May I help you?" (or "Is there anything else…") with
    /// the BUY / SELL / SEE YA! menu, ▶ on `cursor`.
    fn mart_menu(frame: u64, cursor: u8) -> Observation {
        let mut o = Observation::bare(frame, screen(ScreenState::Dialogue), Default::default());
        o.dialogue = Some(dialogue(&["Hi, there!", "May I help you?"], false));
        o.menu = Some(MenuObservation {
            window: Region::new(14, 6, 100, 52),
            rows: 3,
            cursor_row: cursor,
            cursor_y: 12 + 16 * u32::from(cursor),
        });
        o.menu_lines = ["BUY", "SELL", "SEE YA!"].map(String::from).to_vec();
        o
    }

    const PEWTER: [(&str, Option<u32>); 6] = [
        ("POKé BALL", Some(200)),
        ("POTION", Some(300)),
        ("ANTIDOTE", Some(100)),
        ("PARLYZ HEAL", Some(200)),
        ("AWAKENING", Some(250)),
        ("BURN HEAL", Some(250)),
    ];

    fn shop(frame: u64, shop: ShopObservation) -> Observation {
        let mut o = Observation::bare(frame, screen(ScreenState::Shop), Default::default());
        o.shop = Some(shop);
        o
    }

    fn list(frame: u64, money: u32, cursor: u8) -> Observation {
        shop(
            frame,
            ShopObservation {
                money: Some(money),
                items: PEWTER.iter().map(|(n, p)| ((*n).to_owned(), *p)).collect(),
                cursor: Some(cursor),
                quantity: None,
            },
        )
    }

    fn quantity(frame: u64, money: u32, count: u16) -> Observation {
        shop(
            frame,
            ShopObservation {
                money: Some(money),
                items: PEWTER.iter().map(|(n, p)| ((*n).to_owned(), *p)).collect(),
                cursor: None,
                quantity: Some((count, 200 * u32::from(count))),
            },
        )
    }

    /// "POKé BALL, and you want N. / That will be ¥T. Okay?" with YES/NO.
    fn confirm(frame: u64, count: u16, total: u32) -> Observation {
        let mut o = Observation::bare(frame, screen(ScreenState::Dialogue), Default::default());
        let want = format!("POKé BALL, and you want {count}.");
        let price = format!("That will be ¥{total}. Okay?");
        o.dialogue = Some(dialogue(&[&want, &price], false));
        o.menu = Some(MenuObservation {
            window: Region::new(166, 70, 52, 36),
            rows: 2,
            cursor_row: 0,
            cursor_y: 76,
        });
        o.menu_lines = ["YES", "NO"].map(String::from).to_vec();
        o
    }

    fn text(frame: u64, lines: &[&str]) -> Observation {
        let mut o = Observation::bare(frame, screen(ScreenState::Dialogue), Default::default());
        o.dialogue = Some(dialogue(lines, true));
        o
    }

    fn overworld(frame: u64) -> Observation {
        let mut o = Observation::bare(frame, screen(ScreenState::Unknown), Default::default());
        o.player = Some(PoseObservation {
            pose: PlayerPose {
                map: "PewterCity_Mart".into(),
                x: 4,
                y: 3,
            },
            score: 980,
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

    fn pressed(action: &Action) -> Button {
        match action.commands.as_slice() {
            [ControllerCommand::Press(b)] => *b,
            other => panic!("not one press: {other:?}"),
        }
    }

    /// Numbers count only once a later frame reads the same: the first
    /// frame waits, the second (`frame + 1`) decides.
    fn twice(
        p: &mut Purchase,
        o: Observation,
        data: &GameData,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        let first = p.next(&o, data, events);
        assert!(matches!(first, Decision::Wait(_)), "first frame acted");
        let mut again = o;
        again.frame_id += 1;
        p.next(&again, data, events)
    }

    fn leave(p: &mut Purchase, data: &GameData, events: &mut Vec<GameEvent>, frame: u64) {
        // "Is there anything else I can do?": B is SEE YA!.
        let a = act(p.next(&mart_menu(frame, 0), data, events));
        assert_eq!(pressed(&a), Button::B);
        let a = act(p.next(&text(frame + 1, &["Please come again!"]), data, events));
        assert_eq!(pressed(&a), Button::A);
        assert_eq!(a.expect, Expectation::ShopClosed);
        assert!(matches!(
            p.next(&overworld(frame + 2), data, events),
            Decision::Done(_)
        ));
    }

    fn at(map: &str, x: i32, y: i32) -> PlayerPose {
        PlayerPose {
            map: map.into(),
            x,
            y,
        }
    }

    #[test]
    fn nearest_mart_selling_balls() {
        let (Ok(world), Some(data)) = (World::load(root().join("data/world")), data()) else {
            return;
        };
        let gone = Gone::new();
        let nearest =
            |pose: PlayerPose, item: &str| nearest_mart(&world, &data, &pose, item, &gone);
        assert_eq!(
            nearest(at("PewterCity", 17, 26), "ITEM_POKE_BALL"),
            Some(("PewterCity_Mart".into(), 3))
        );
        assert_eq!(
            nearest(at("ViridianCity", 26, 27), "ITEM_POKE_BALL"),
            Some(("ViridianCity_Mart".into(), 1))
        );
        assert_eq!(
            nearest(at("CeruleanCity", 22, 20), "ITEM_POKE_BALL"),
            Some(("CeruleanCity_Mart".into(), 1))
        );
        assert_eq!(nearest(at("PewterCity", 17, 26), "ITEM_NOT_SOLD"), None);
    }

    /// Review: by map hops Cerulean is nearest from Route 4's west end, but
    /// it is only reached through Mt. Moon; Pewter's mart is the walk.
    #[test]
    fn nearest_mart_is_the_nearest_walk_not_the_fewest_maps() {
        let (Ok(world), Some(data)) = (World::load(root().join("data/world")), data()) else {
            return;
        };
        let gone = Gone::new();
        for pose in [at("Route4_PokemonCenter_1F", 7, 7), at("MtMoon_1F", 18, 36)] {
            assert_eq!(
                nearest_mart(&world, &data, &pose, "ITEM_POKE_BALL", &gone).map(|(map, _)| map),
                Some("PewterCity_Mart".into()),
                "from {pose:?}"
            );
        }
    }

    #[test]
    fn shop_expectations() {
        assert!(Expectation::ShopCursorAt(0).met(&list(1, 3000, 0)));
        assert!(!Expectation::ShopCursorAt(1).met(&list(1, 3000, 0)));
        assert!(Expectation::ShopQuantity(3).met(&quantity(1, 3000, 3)));
        assert!(!Expectation::ShopQuantity(2).met(&quantity(1, 3000, 3)));
        assert!(!Expectation::ShopQuantity(1).met(&list(1, 3000, 0)));
        assert!(Expectation::ShopClosed.met(&overworld(1)));
        assert!(!Expectation::ShopClosed.met(&list(1, 3000, 0)));
        assert!(!Expectation::ShopClosed.met(&text(1, &["Please come again!"])));
        assert!(!Expectation::ShopClosed.met(&mart_menu(1, 0)));
    }

    #[test]
    fn buys_three_poke_balls() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 3);
        let mut events = Vec::new();

        let a = act(p.next(&mart_menu(1, 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::A);
        assert_eq!(a.expect, Expectation::MenuClosed);

        // The list: money read on two frames, then A on POKé BALL.
        let a = act(twice(&mut p, list(2, 3000, 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::A);
        assert_eq!(a.expect, Expectation::ShopQuantity(1));
        assert_eq!(events, vec![GameEvent::MoneyObserved { amount: 3000 }]);

        // ×01 → Up → ×02 → Up → ×03 → A.
        let a = act(twice(&mut p, quantity(4, 3000, 1), &data, &mut events));
        assert_eq!(pressed(&a), Button::Up);
        assert_eq!(a.expect, Expectation::ShopQuantity(2));
        let a = act(twice(&mut p, quantity(6, 3000, 2), &data, &mut events));
        assert_eq!(pressed(&a), Button::Up);
        assert_eq!(a.expect, Expectation::ShopQuantity(3));
        let a = act(twice(&mut p, quantity(8, 3000, 3), &data, &mut events));
        assert_eq!(pressed(&a), Button::A);
        assert_eq!(a.expect, Expectation::Question);

        // "…you want 3. That will be ¥600. Okay?" → YES.
        let a = act(twice(&mut p, confirm(10, 3, 600), &data, &mut events));
        assert_eq!((a.label.as_str(), pressed(&a)), ("mart: YES", Button::A));
        let a = act(p.next(
            &text(12, &["Here you are!", "Thank you!"]),
            &data,
            &mut events,
        ));
        assert_eq!(pressed(&a), Button::A);

        // Back on the list: ¥2400 = ¥3000 − ¥600 confirms the purchase.
        let a = act(twice(&mut p, list(13, 2400, 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::B);
        assert_eq!(a.expect, Expectation::ScreenIsNot(ScreenState::Shop));
        assert_eq!(
            events,
            vec![
                GameEvent::MoneyObserved { amount: 3000 },
                GameEvent::ItemsChanged {
                    pocket: pokebot_state::Pocket::PokeBalls,
                    item: "ITEM_POKE_BALL".into(),
                    delta: 3,
                    reason: "bought".into(),
                },
                GameEvent::MoneyChanged {
                    delta: -600,
                    reason: "bought".into(),
                },
            ]
        );
        leave(&mut p, &data, &mut events, 20);
        assert_eq!(events.len(), 3);
    }

    /// Live (Switch): the settled farewell's text wasn't read; without a
    /// ▼ arrow the mart waited on it until it failed.
    #[test]
    fn an_unread_settled_farewell_is_advanced() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 0).with_stock(Some(3));
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        act(twice(&mut p, list(2, 700, 0), &data, &mut events));
        let a = act(p.next(&mart_menu(10, 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::B);
        let mut farewell = text(11, &[]);
        farewell.dialogue.as_mut().unwrap().waiting_for_input = false;
        let a = act(p.next(&farewell, &data, &mut events));
        assert_eq!(pressed(&a), Button::A);
        assert_eq!(a.expect, Expectation::ShopClosed);
    }

    #[test]
    fn zero_count_decides_on_the_list_and_may_buy_nothing() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 0).with_stock(Some(3));
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        // ¥700 − 2 Potions (¥600) leaves ¥100 < ¥200: nothing to buy.
        let a = act(twice(&mut p, list(2, 700, 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::B);
        assert_eq!(events[0], GameEvent::MoneyObserved { amount: 700 });
        assert!(
            matches!(&events[1], GameEvent::GoalProgress { detail, .. } if detail.contains("0"))
        );
        leave(&mut p, &data, &mut events, 10);
        assert!(!events.iter().any(|e| matches!(
            e,
            GameEvent::ItemsChanged { .. } | GameEvent::MoneyChanged { .. }
        )));
    }

    #[test]
    fn zero_count_buys_up_to_the_target() {
        let Some(data) = data() else { return };
        // 3 held, ¥3000: (3000 − 600) / 200 = 12 = 15 − 3.
        let mut p = Purchase::new("ITEM_POKE_BALL", 0).with_stock(Some(3));
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        act(twice(&mut p, list(2, 3000, 0), &data, &mut events));
        let a = act(twice(&mut p, quantity(4, 3000, 11), &data, &mut events));
        assert_eq!(a.expect, Expectation::ShopQuantity(12));
        let a = act(twice(&mut p, quantity(6, 3000, 12), &data, &mut events));
        assert_eq!(a.expect, Expectation::Question);
    }

    /// Review: an explicit count was capped by `money / price` only, so
    /// it could spend the 2-Potion reserve.
    #[test]
    fn an_explicit_count_keeps_the_potion_money() {
        let Some(data) = data() else { return };
        // ¥1000 − 2 Potions (¥600) = ¥400: 2 of the 3 asked for.
        let mut p = Purchase::new("ITEM_POKE_BALL", 3);
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        let a = act(twice(&mut p, list(2, 1000, 0), &data, &mut events));
        assert_eq!(a.expect, Expectation::ShopQuantity(1));
        assert!(events.iter().any(|e| matches!(
            e,
            GameEvent::GoalProgress { detail, .. } if detail.contains("buys only 2 of 3")
        )));
        let a = act(twice(&mut p, quantity(4, 1000, 1), &data, &mut events));
        assert_eq!(a.expect, Expectation::ShopQuantity(2));
        let a = act(twice(&mut p, quantity(6, 1000, 2), &data, &mut events));
        assert_eq!(a.expect, Expectation::Question);
    }

    #[test]
    fn a_misread_quantity_is_never_confirmed() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 3);
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        act(twice(&mut p, list(2, 3000, 0), &data, &mut events));
        // ×03 on one frame, ×08 on the next: no A until two frames agree.
        assert!(matches!(
            p.next(&quantity(4, 3000, 3), &data, &mut events),
            Decision::Wait(_)
        ));
        assert!(matches!(
            p.next(&quantity(5, 3000, 8), &data, &mut events),
            Decision::Wait(_)
        ));
        let a = act(p.next(&quantity(6, 3000, 8), &data, &mut events));
        // An overshoot (or a wrap past the maximum) goes back down.
        assert_eq!(pressed(&a), Button::Down);
        assert_eq!(a.expect, Expectation::ShopQuantity(7));
    }

    #[test]
    fn a_wrapped_quantity_is_corrected_with_down() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 3);
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        act(twice(&mut p, list(2, 3000, 0), &data, &mut events));
        act(twice(&mut p, quantity(4, 3000, 1), &data, &mut events));
        act(twice(&mut p, quantity(6, 3000, 2), &data, &mut events));
        // Up from ×02 wrapped to ×01: ×02 is the most the box allows.
        let a = act(twice(&mut p, quantity(8, 3000, 1), &data, &mut events));
        assert_eq!(pressed(&a), Button::Down);
        assert_eq!(a.expect, Expectation::ShopQuantity(2));
        let a = act(twice(&mut p, quantity(10, 3000, 2), &data, &mut events));
        assert_eq!(a.expect, Expectation::Question);
    }

    #[test]
    fn a_price_that_disagrees_is_answered_no() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 3);
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        act(twice(&mut p, list(2, 3000, 0), &data, &mut events));
        act(twice(&mut p, quantity(4, 3000, 1), &data, &mut events));
        act(twice(&mut p, quantity(6, 3000, 2), &data, &mut events));
        act(twice(&mut p, quantity(8, 3000, 3), &data, &mut events));
        let a = act(twice(&mut p, confirm(10, 8, 1600), &data, &mut events));
        assert_eq!((a.label.as_str(), pressed(&a)), ("mart: NO", Button::B));
    }

    #[test]
    fn a_money_mismatch_is_observed_not_tracked() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 1);
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        act(twice(&mut p, list(2, 3000, 0), &data, &mut events));
        act(twice(&mut p, quantity(4, 3000, 1), &data, &mut events));
        act(twice(&mut p, confirm(6, 1, 200), &data, &mut events));
        act(p.next(
            &text(8, &["Here you are!", "Thank you!"]),
            &data,
            &mut events,
        ));
        act(twice(&mut p, list(9, 2700, 0), &data, &mut events));
        // Money went down (the purchase went through), but not by ¥200.
        assert!(matches!(
            &events[1],
            GameEvent::ItemsChanged { delta: 1, .. }
        ));
        assert_eq!(events[2], GameEvent::MoneyObserved { amount: 2700 });
        assert!(matches!(&events[3], GameEvent::GoalProgress { .. }));
        assert!(!events
            .iter()
            .any(|e| matches!(e, GameEvent::MoneyChanged { .. })));
    }

    #[test]
    fn an_overworld_frame_before_see_ya_does_not_finish() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 0).with_stock(Some(3));
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        let a = act(twice(&mut p, list(2, 700, 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::B);
        // A dropped frame between the list and "Is there anything else…".
        assert!(matches!(
            p.next(&overworld(4), &data, &mut events),
            Decision::Wait(_)
        ));
        leave(&mut p, &data, &mut events, 5);
    }

    #[test]
    fn here_you_are_proves_the_purchase_when_the_money_lags() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 3);
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        act(twice(&mut p, list(2, 3000, 0), &data, &mut events));
        act(twice(&mut p, quantity(4, 3000, 1), &data, &mut events));
        act(twice(&mut p, quantity(6, 3000, 2), &data, &mut events));
        act(twice(&mut p, quantity(8, 3000, 3), &data, &mut events));
        act(twice(&mut p, confirm(10, 3, 600), &data, &mut events));
        act(p.next(
            &text(12, &["Here you are!", "Thank you!"]),
            &data,
            &mut events,
        ));
        // The MONEY window still reads the old amount on two frames.
        act(twice(&mut p, list(13, 3000, 0), &data, &mut events));
        assert_eq!(
            events[1],
            GameEvent::ItemsChanged {
                pocket: pokebot_state::Pocket::PokeBalls,
                item: "ITEM_POKE_BALL".into(),
                delta: 3,
                reason: "bought".into(),
            }
        );
        assert_eq!(events[2], GameEvent::MoneyObserved { amount: 3000 });
        assert!(matches!(&events[3], GameEvent::GoalProgress { .. }));
        assert_eq!(events.len(), 4);
    }

    #[test]
    fn no_here_you_are_and_unchanged_money_is_no_purchase() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 1);
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        act(twice(&mut p, list(2, 3000, 0), &data, &mut events));
        act(twice(&mut p, quantity(4, 3000, 1), &data, &mut events));
        act(twice(&mut p, confirm(6, 1, 200), &data, &mut events));
        act(twice(&mut p, list(8, 3000, 0), &data, &mut events));
        assert!(!events
            .iter()
            .any(|e| matches!(e, GameEvent::ItemsChanged { .. })));
    }

    #[test]
    fn flicker_on_other_rows_is_not_a_misread() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 1);
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        assert!(matches!(
            p.next(&list(2, 3000, 0), &data, &mut events),
            Decision::Wait(_)
        ));
        // BURN HEAL reads differently on the next frame: the money, ▶ and
        // POKé BALL's row still agree, so A goes out.
        let mut flicker = list(3, 3000, 0);
        flicker.shop.as_mut().unwrap().items[5] = ("BURN H??L".into(), Some(25));
        let a = act(p.next(&flicker, &data, &mut events));
        assert_eq!(a.expect, Expectation::ShopQuantity(1));
        assert_eq!(p.total_retries, 0);
    }

    #[test]
    fn question_pages_are_never_pressed_through() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POKE_BALL", 3);
        let mut events = Vec::new();
        // A on these would confirm ×01 or YES before they were checked.
        for (frame, lines) in [
            (1, ["POKé BALL? Certainly.", "How many would you like?"]),
            (
                2,
                ["POKé BALL, and you want 3.", "That will be ¥600. Okay?"],
            ),
        ] {
            assert!(matches!(
                p.next(&text(frame, &lines), &data, &mut events),
                Decision::Wait(_)
            ));
        }
    }

    #[test]
    fn the_item_is_found_by_its_name() {
        let Some(data) = data() else { return };
        let mut p = Purchase::new("ITEM_POTION", 1);
        let mut events = Vec::new();
        act(p.next(&mart_menu(1, 0), &data, &mut events));
        let a = act(twice(&mut p, list(2, 3000, 0), &data, &mut events));
        assert_eq!(pressed(&a), Button::Down);
        assert_eq!(a.expect, Expectation::ShopCursorAt(1));
        let a = act(twice(&mut p, list(4, 3000, 1), &data, &mut events));
        assert_eq!(pressed(&a), Button::A);
    }
}
