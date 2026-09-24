//! Facts in dialogue text that change the bag, money or party: money won,
//! items found or received, the nurse's heal, and an empty move.

use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, Pocket};

#[derive(Debug, Default)]
pub struct TextTracker {
    last_page: String,
}

impl TextTracker {
    /// Events for a fully printed page (each page once). `last_move` is the
    /// (party slot, move slot) chosen last in battle.
    pub fn observe_page(
        &mut self,
        lines: &[String],
        data: &GameData,
        last_move: Option<(u8, u8)>,
    ) -> Vec<GameEvent> {
        let page = lines.join(" ");
        if page.is_empty() || page == self.last_page {
            return Vec::new();
        }
        self.last_page = page.clone();
        let mut events = Vec::new();
        if let Some(amount) = money_won(&page) {
            events.push(GameEvent::MoneyChanged {
                delta: i64::from(amount),
                reason: "won a battle".into(),
            });
        }
        if let Some((name, reason)) = item_gained(&page) {
            if let Some(key) = data.item_named(&name) {
                if let Some(pocket) = data.items[key]
                    .pocket
                    .as_deref()
                    .and_then(Pocket::from_decomp)
                {
                    events.push(GameEvent::ItemsChanged {
                        pocket,
                        item: key.to_owned(),
                        delta: 1,
                        reason,
                    });
                }
            }
        }
        if page.contains("restored your POKéMON") {
            events.push(GameEvent::Healed);
        }
        if page.contains("no PP left for") {
            if let Some((slot, move_slot)) = last_move {
                events.push(GameEvent::MoveOutOfPp { slot, move_slot });
            }
        }
        events
    }
}

/// "RED got ¥1,200 for winning!" → 1200.
fn money_won(page: &str) -> Option<u32> {
    let rest = &page[page.find(" got ¥")? + " got ¥".len()..];
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(|c| *c != ',')
        .collect();
    page.contains("for winning")
        .then(|| digits.parse().ok())
        .flatten()
}

/// "RED found a POTION!" / "received the TOWN MAP." / "obtained a X!"
fn item_gained(page: &str) -> Option<(String, String)> {
    for (verb, reason) in [
        (" found ", "found"),
        (" received ", "received"),
        (" obtained ", "obtained"),
    ] {
        if let Some(at) = page.find(verb) {
            let rest = &page[at + verb.len()..];
            let rest = ["a ", "an ", "the ", "one "]
                .iter()
                .find_map(|p| rest.strip_prefix(p))
                .unwrap_or(rest);
            let end = rest.find(['!', '.'])?;
            return Some((rest[..end].trim().to_owned(), reason.to_owned()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_owned).collect()
    }

    fn data() -> Option<GameData> {
        GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok()
    }

    #[test]
    fn text_becomes_money_item_and_heal_events() {
        let Some(d) = data() else { return };
        let mut t = TextTracker::default();
        assert_eq!(
            t.observe_page(&lines("RED got ¥1,200\nfor winning!"), &d, None),
            vec![GameEvent::MoneyChanged {
                delta: 1200,
                reason: "won a battle".into()
            }]
        );
        assert_eq!(
            t.observe_page(&lines("RED found a POTION!"), &d, None),
            vec![GameEvent::ItemsChanged {
                pocket: Pocket::Items,
                item: "ITEM_POTION".into(),
                delta: 1,
                reason: "found".into()
            }]
        );
        assert_eq!(
            t.observe_page(
                &lines("We've restored your POKéMON\nto full health."),
                &d,
                None
            ),
            vec![GameEvent::Healed]
        );
        // The same page is read once.
        assert!(t
            .observe_page(
                &lines("We've restored your POKéMON\nto full health."),
                &d,
                None
            )
            .is_empty());
    }

    #[test]
    fn no_pp_page_zeroes_move() {
        let Some(d) = data() else { return };
        let mut t = TextTracker::default();
        assert_eq!(
            t.observe_page(
                &lines("There's no PP left for\nthis move!"),
                &d,
                Some((0, 2))
            ),
            vec![GameEvent::MoveOutOfPp {
                slot: 0,
                move_slot: 2
            }]
        );
    }
}
