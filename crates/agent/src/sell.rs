//! Selling at a Poké Mart (`item_menu.c` `Task_ItemContext_Sell`).
//!
//! From the clerk's BUY / SELL / SEE YA! menu: SELL opens the bag; the
//! item's pocket and row are found by name; A asks "How many would you
//! like to sell?" and opens the quantity box (`×NN ¥TOTAL`, the MONEY
//! window beside it), whose count steps by one with Up/Down (wrapping) and
//! by ten with Right/Left; A asks "I can pay ¥T. Would that be okay?",
//! answered YES only when T is the count at half the item's price; "Turned
//! over the X worth ¥T." ends one sale. A single item held skips the
//! quantity box. One sale is at most 99 (or all held), and the money never
//! goes past ¥999,999 (`AddMoney` caps it: the rest would be lost), so the
//! count is capped by the MONEY read. The sensor turns "Turned over…" into
//! the bag and money deltas (`pokebot_sense::text`).
//!
//! Every reading counts only once a later frame reads the same, as in
//! [`crate::shop::Purchase`].

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, Observation, Pocket, ScreenState};

use crate::bag::{fits, is_cancel, pocket_from_title, pocket_index};
use crate::shop::is_mart_menu;
use crate::{Action, Decision, Expectation};

/// The most money the game holds (`MAX_MONEY`).
pub const MAX_MONEY: u32 = 999_999;
/// The most one sale takes (`Task_ItemContext_Sell` caps the box at 99).
const MAX_PER_SALE: u16 = 99;
/// More retries than this in one phase fail the sale; in all, fail it too.
const MAX_RETRIES: u32 = 12;
const MAX_TOTAL_RETRIES: u32 = 40;
/// A wait (unreadable text or numbers) this long counts as one retry.
const WAIT_RETRY_FRAMES: u64 = 60;
/// Bag rows visible at once: fewer, ending in CANCEL, are the whole pocket.
const WHOLE_POCKET_ROWS: usize = 6;
const CURSOR_FRAMES: u64 = 45;
const POCKET_FRAMES: u64 = 60;
const TEXT_FRAMES: u64 = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// BUY / SELL / SEE YA! over the clerk's text.
    Menu,
    /// The bag, opened by SELL.
    Bag,
    /// The quantity box.
    Quantity,
    /// "I can pay ¥T. Would that be okay?" with YES/NO.
    Confirm,
    /// Text without a menu (the clerk's, or the sale's).
    Text,
    Outside,
    Other,
}

/// Sells up to `count` of one item at the mart whose conversation is open,
/// then leaves.
pub struct Sale {
    item: String,
    /// The pocket the item is in, from the game data.
    pocket: Option<Pocket>,
    /// Half the item's price.
    each: u32,
    /// Still to sell.
    left: u32,
    phase: Phase,
    /// Held, as the bag row last read it.
    held: Option<u16>,
    /// The count decided in the open quantity box (or for a single item).
    target: Option<u16>,
    /// (count, total) the box showed when A was pressed on it.
    quoted: Option<(u16, u32)>,
    /// (count, total) answered YES to: settled by "Turned over…".
    paid: Option<(u16, u32)>,
    /// Nothing more to sell: close the bag and leave.
    finished: bool,
    leaves: u32,
    /// What was sold, and why the sale stopped.
    pub sold: u32,
    pub earned: u32,
    summary: Vec<String>,
    retries: u32,
    total_retries: u32,
    candidate: Option<(u64, String)>,
    pending: Option<Expectation>,
    waiting_since: Option<u64>,
}

impl Sale {
    pub fn new(data: &GameData, item: &str, count: u32) -> Self {
        let info = data.items.get(item);
        Self {
            item: item.to_owned(),
            pocket: info
                .and_then(|i| i.pocket.as_deref())
                .and_then(Pocket::from_decomp),
            each: info.map_or(0, |i| i.price / 2),
            left: count,
            phase: Phase::Other,
            held: None,
            target: None,
            quoted: None,
            paid: None,
            finished: false,
            leaves: 0,
            sold: 0,
            earned: 0,
            summary: Vec::new(),
            retries: 0,
            total_retries: 0,
            candidate: None,
            pending: None,
            waiting_since: None,
        }
    }

    fn phase_of(o: &Observation) -> Phase {
        if o.shop.as_ref().is_some_and(|s| s.quantity.is_some()) {
            return Phase::Quantity;
        }
        if o.menu.is_some() {
            if is_mart_menu(o) {
                return Phase::Menu;
            }
            if o.dialogue.is_some()
                && matches!(o.menu_lines.as_slice(), [yes, no] if fits("YES", yes) && fits("NO", no))
            {
                return Phase::Confirm;
            }
            return Phase::Other;
        }
        if o.dialogue.is_some() {
            return Phase::Text;
        }
        if o.bag.is_some() {
            return Phase::Bag;
        }
        if o.player.is_some() {
            return Phase::Outside;
        }
        Phase::Other
    }

    /// The count one sale takes with `money` held: what is left to sell,
    /// at most what is held and [`MAX_PER_SALE`], and no more than the
    /// money has room for.
    pub fn count_for(&self, money: u32, held: u16) -> u16 {
        let room = MAX_MONEY
            .saturating_sub(money)
            .checked_div(self.each)
            .unwrap_or(0);
        let n = self.left.min(u32::from(held.min(MAX_PER_SALE))).min(room);
        n as u16
    }

    /// Next input, or Done once the mart is left.
    pub fn next(
        &mut self,
        o: &Observation,
        data: &GameData,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        if self.each == 0 {
            return Decision::Fail(format!("sell: {} has no price", self.item));
        }
        let phase = Self::phase_of(o);
        let pending = self.pending.take();
        if phase != self.phase {
            self.phase = phase;
            self.retries = 0;
            self.waiting_since = None;
            self.candidate = None;
        } else if let Some(expect) = pending {
            if !expect.met(o) {
                self.retry();
            }
        }
        if let Some(fail) = self.exhausted(&format!("no progress in {phase:?}")) {
            return fail;
        }
        match phase {
            Phase::Menu => self.menu(o),
            Phase::Bag => self.bag(o, data, events),
            Phase::Quantity => self.quantity(o, events),
            Phase::Confirm => self.confirm(o, events),
            Phase::Text => self.text(o),
            Phase::Outside if self.finished && self.leaves > 0 => Decision::Done(self.summary()),
            Phase::Outside => self.wait(o, "waiting for the mart menu"),
            Phase::Other => self.wait(o, "waiting for a mart screen"),
        }
    }

    pub fn summary(&self) -> String {
        let mut s = format!("sold {} {} for ¥{}", self.sold, self.item, self.earned);
        for line in &self.summary {
            s.push_str("; ");
            s.push_str(line);
        }
        s
    }

    /// SELL, or SEE YA! (B) when done.
    fn menu(&mut self, o: &Observation) -> Decision {
        let menu = o.menu.expect("menu phase");
        if self.finished || self.left == 0 {
            self.finished = true;
            self.leaves += 1;
            if self.leaves > 8 {
                return Decision::Fail("sell: could not leave the clerk".into());
            }
            return self.act(
                "mart: SEE YA!",
                Button::B,
                Expectation::MenuClosed,
                TEXT_FRAMES,
            );
        }
        if o.menu_lines.len() != usize::from(menu.rows) {
            return self.wait(o, "reading the mart menu");
        }
        let Some(sell) = o.menu_lines.iter().position(|l| fits("SELL", l)) else {
            return self.wait(o, "looking for SELL");
        };
        let sell = sell as u8;
        if menu.cursor_row == sell {
            return self.act(
                "mart: SELL",
                Button::A,
                Expectation::ScreenIs(ScreenState::Bag),
                TEXT_FRAMES,
            );
        }
        let (button, next) = if menu.cursor_row > sell {
            (Button::Up, menu.cursor_row - 1)
        } else {
            (Button::Down, menu.cursor_row + 1)
        };
        self.act(
            &format!("mart menu: {button:?} toward SELL"),
            button,
            Expectation::MenuCursorAt(next),
            CURSOR_FRAMES,
        )
    }

    /// The item's pocket and row; A on it. None left (or nothing more to
    /// sell): close the bag.
    fn bag(&mut self, o: &Observation, data: &GameData, events: &mut Vec<GameEvent>) -> Decision {
        let bag = o.bag.as_ref().expect("bag phase");
        let close = |s: &mut Self| {
            s.act(
                "sell: close the bag",
                Button::B,
                Expectation::ScreenIsNot(ScreenState::Bag),
                TEXT_FRAMES,
            )
        };
        if self.finished || self.left == 0 {
            self.finished = true;
            return close(self);
        }
        // The ▶ is hidden while a message prints in the bag ("How many
        // would you like to sell?"): the quantity box comes by itself.
        let Some(cursor) = bag.cursor else {
            return self.wait(o, "the bag's message");
        };
        let Some(want) = self.pocket.and_then(pocket_index) else {
            return Decision::Fail(format!("sell: {} is in no bag pocket", self.item));
        };
        let Some(at) = pocket_from_title(&bag.pocket).and_then(pocket_index) else {
            return self.wait(o, "reading the pocket title");
        };
        if at != want {
            let button = if at < want {
                Button::Right
            } else {
                Button::Left
            };
            let title = crate::bag::POCKETS[if at < want { at + 1 } else { at - 1 }].1;
            return self.act(
                &format!("sell: pocket {button:?}"),
                button,
                Expectation::BagPocket(title.to_owned()),
                POCKET_FRAMES,
            );
        }
        let row = bag
            .rows
            .iter()
            .position(|(name, _)| data.item_named(name) == Some(self.item.as_str()));
        let reading = format!(
            "{} ▶{cursor} {row:?} {:?}",
            bag.pocket,
            row.map(|r| &bag.rows[r])
        );
        if let Some(wait) = self.confirmed(o, reading) {
            return wait;
        }
        let Some(row) = row else {
            let cancel = bag.rows.iter().any(|(name, _)| is_cancel(name));
            if cancel && bag.rows.len() < WHOLE_POCKET_ROWS {
                self.log(events, format!("no {} left in the bag", self.item));
                self.finished = true;
                return close(self);
            }
            return self.act(
                "sell: scroll toward the item",
                if cancel { Button::Up } else { Button::Down },
                Expectation::InputsDone,
                CURSOR_FRAMES,
            );
        };
        self.held = bag.rows[row].1;
        if usize::from(cursor) != row {
            let (button, next) = if usize::from(cursor) < row {
                (Button::Down, cursor + 1)
            } else {
                (Button::Up, cursor - 1)
            };
            return self.act(
                &format!("sell: {button:?} toward {}", self.item),
                button,
                Expectation::BagCursorAt(next),
                CURSOR_FRAMES,
            );
        }
        // A single item skips the quantity box: its count is decided now.
        self.target = (self.held == Some(1)).then_some(1);
        self.act(
            &format!("sell: choose {}", self.item),
            Button::A,
            Expectation::InputsDone,
            TEXT_FRAMES,
        )
    }

    /// Steps the count to the target and presses A once two frames read
    /// exactly it; nothing to sell (the money is full): B.
    fn quantity(&mut self, o: &Observation, events: &mut Vec<GameEvent>) -> Decision {
        let shop = o.shop.as_ref().expect("quantity phase");
        let (Some((count, total)), Some(money)) = (shop.quantity, shop.money) else {
            return self.wait(o, "reading the quantity");
        };
        if let Some(wait) = self.confirmed(o, format!("×{count} ¥{total} ¥{money}")) {
            return wait;
        }
        let target = match self.target {
            Some(t) => t,
            None => {
                events.push(GameEvent::MoneyObserved { amount: money });
                let held = self.held.unwrap_or(MAX_PER_SALE);
                let t = self.count_for(money, held);
                if t == 0 {
                    self.log(
                        events,
                        format!(
                            "¥{money}: no room for more money, {} left unsold",
                            self.left
                        ),
                    );
                    self.finished = true;
                    return self.act(
                        "sell: cancel the quantity",
                        Button::B,
                        Expectation::InputsDone,
                        TEXT_FRAMES,
                    );
                }
                self.target = Some(t);
                t
            }
        };
        if count == target {
            if total != u32::from(count) * self.each {
                return self.wait(o, "the total doesn't match the count");
            }
            self.quoted = Some((count, total));
            return self.act(
                &format!("sell: {count} for ¥{total}"),
                Button::A,
                Expectation::Question,
                TEXT_FRAMES,
            );
        }
        let max = self.held.unwrap_or(MAX_PER_SALE).min(MAX_PER_SALE);
        let (button, next) = if count == 1 && target == max {
            // Down from 1 wraps to the most the box takes.
            (Button::Down, max)
        } else if target >= count + 10 {
            (Button::Right, (count + 10).min(max))
        } else if count >= target + 10 {
            (Button::Left, count - 10)
        } else if count < target {
            (Button::Up, count + 1)
        } else {
            (Button::Down, count - 1)
        };
        self.act(
            &format!("sell: quantity {button:?} to {next}"),
            button,
            Expectation::ShopQuantity(next),
            CURSOR_FRAMES,
        )
    }

    /// YES only when the price asked is the count at the selling price.
    fn confirm(&mut self, o: &Observation, events: &mut Vec<GameEvent>) -> Decision {
        let menu = o.menu.expect("confirm phase");
        let page = o
            .dialogue
            .as_ref()
            .map(|d| d.lines.join(" "))
            .unwrap_or_default();
        let Some(total) = number_after(&page, "pay ¥") else {
            return self.wait(o, "reading the price");
        };
        if let Some(wait) = self.confirmed(o, format!("¥{total}")) {
            return wait;
        }
        // A single item: its sale was decided on the bag row; the money
        // is only shown now.
        let expected = self
            .quoted
            .or_else(|| (self.target == Some(1)).then_some((1, self.each)));
        let agrees = expected.is_some_and(|(_, t)| t == total);
        if !agrees {
            self.log(
                events,
                format!("the clerk offers ¥{total}, expected {expected:?}: answering NO"),
            );
            self.quoted = None;
            self.finished = true;
            return self.act("sell: NO", Button::B, Expectation::MenuClosed, TEXT_FRAMES);
        }
        if menu.cursor_row != 0 {
            return self.act(
                "sell: Up to YES",
                Button::Up,
                Expectation::MenuCursorAt(0),
                CURSOR_FRAMES,
            );
        }
        self.paid = expected;
        self.act("sell: YES", Button::A, Expectation::MenuClosed, TEXT_FRAMES)
    }

    /// "Turned over…" settles the sale; the clerk's other pages are read
    /// on, never a page whose question box is still to come.
    fn text(&mut self, o: &Observation) -> Decision {
        let d = o.dialogue.as_ref().expect("text phase");
        let page = d.lines.join(" ");
        if page.starts_with("Turned over") {
            if let Some((count, total)) = self.paid.take() {
                self.sold += u32::from(count);
                self.earned += total;
                self.left = self.left.saturating_sub(u32::from(count));
                self.target = None;
                self.quoted = None;
            }
        }
        if page.contains("can't buy") {
            return Decision::Fail(format!("sell: the clerk won't buy {}", self.item));
        }
        if ["How many", "okay?", "Okay?", "help you", "anything else"]
            .iter()
            .any(|q| page.contains(q))
        {
            return self.wait(o, "a question: waiting for its box");
        }
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
        self.act("sell: advance text", Button::A, expect, TEXT_FRAMES)
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
                Some(self.wait(o, "re-reading what read differently"))
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
                "sell ({}): {reason} after {MAX_RETRIES} retries in {:?}",
                self.item, self.phase
            )));
        }
        if self.total_retries > MAX_TOTAL_RETRIES {
            return Some(Decision::Fail(format!(
                "sell ({}): {reason}; {MAX_TOTAL_RETRIES} retries used in all",
                self.item
            )));
        }
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Option<GameData> {
        GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok()
    }

    /// One sale takes what is left, at most 99 and all held, and never
    /// more than the money has room for (¥999,999 caps it).
    #[test]
    fn the_count_is_capped_by_the_box_and_the_money() {
        let Some(d) = data() else { return };
        let sale = Sale::new(&d, "ITEM_NUGGET", 200);
        assert_eq!(sale.count_for(10_000, 200), 99);
        assert_eq!(sale.count_for(10_000, 7), 7);
        // ¥600,000 has room for 79 NUGGETs at ¥5000.
        assert_eq!(sale.count_for(600_000, 200), 79);
        assert_eq!(sale.count_for(996_000, 200), 0);
        let few = Sale::new(&d, "ITEM_NUGGET", 3);
        assert_eq!(few.count_for(0, 200), 3);
    }

    #[test]
    fn the_price_asked_is_read() {
        assert_eq!(
            number_after("I can pay ¥495,000. Would that be okay?", "pay ¥"),
            Some(495_000)
        );
    }
}
