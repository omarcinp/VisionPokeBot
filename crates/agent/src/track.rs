//! Facts in dialogue text that change the bag, money or party: money won,
//! items found or received (and the mart's Premier Ball bonus), balls
//! thrown, the nurse's heal, and an empty move.

use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, Pocket};

#[derive(Debug, Default)]
pub struct TextTracker {
    /// The last page applied.
    last_page: String,
    /// The page read on the previous observation.
    candidate: Option<String>,
}

impl TextTracker {
    /// Whether `lines` is the page applied last (or empty): it was read on
    /// two observations and may be advanced.
    pub fn applied(&self, lines: &[String]) -> bool {
        let page = lines.join(" ");
        page.is_empty() || page == self.last_page
    }

    /// Events for a fully printed page. A page is applied once the same
    /// text has read on two consecutive observations, and not again while
    /// it equals the last page applied: a one-frame misread inside a stable
    /// page (A, B, A) neither applies B nor re-applies A. `last_move` is the
    /// (party slot, move slot) chosen last in battle.
    pub fn observe_page(
        &mut self,
        lines: &[String],
        data: &GameData,
        last_move: Option<(u8, u8)>,
    ) -> Vec<GameEvent> {
        let page = lines.join(" ");
        if page.is_empty() {
            return Vec::new();
        }
        let confirmed = self.candidate.as_deref() == Some(page.as_str());
        self.candidate = Some(page.clone());
        if !confirmed || page == self.last_page {
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
        // A ball thrown in battle: "RED used POKé BALL!".
        if let Some(item) = ball_thrown(&page, data) {
            events.push(GameEvent::ItemsChanged {
                pocket: Pocket::PokeBalls,
                item,
                delta: -1,
                reason: "thrown".into(),
            });
        }
        // Buying 10+ Poké Balls at once: "I'll throw in a PREMIER BALL, too."
        if page.contains("PREMIER BALL, too") {
            events.push(GameEvent::ItemsChanged {
                pocket: Pocket::PokeBalls,
                item: "ITEM_PREMIER_BALL".into(),
                delta: 1,
                reason: "bonus".into(),
            });
        }
        if let Some(badge) = badge_received(&page) {
            events.push(GameEvent::BadgeEarned {
                badge: badge.to_owned(),
            });
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

/// "RED used POKé BALL!" → `ITEM_POKE_BALL`; only items of the Poké Balls
/// pocket (moves and other items used are not balls thrown).
fn ball_thrown(page: &str, data: &GameData) -> Option<String> {
    let rest = &page[page.find(" used ")? + " used ".len()..];
    let name = rest[..rest.find('!')?].trim();
    let key = data.item_named(name)?;
    (data.items[key]
        .pocket
        .as_deref()
        .and_then(Pocket::from_decomp)
        == Some(Pocket::PokeBalls))
    .then(|| key.to_owned())
}

/// Kanto's gym badges in gym order, as the game spells them.
pub const BADGES: [&str; 8] = [
    "BOULDERBADGE",
    "CASCADEBADGE",
    "THUNDERBADGE",
    "RAINBOWBADGE",
    "SOULBADGE",
    "MARSHBADGE",
    "VOLCANOBADGE",
    "EARTHBADGE",
];

/// "RED received the BOULDERBADGE from BROCK." → `BOULDERBADGE`. Misty
/// prints no "received" page: her defeat page "You can have the
/// CASCADEBADGE to show you beat me." gives it.
fn badge_received(page: &str) -> Option<&'static str> {
    BADGES.into_iter().find(|b| {
        page.contains(&format!(" received the {b}"))
            || page.contains(&format!("You can have the {b}"))
    })
}

/// "RED found a POTION!" / "received the TOWN MAP." / "received TM03 from
/// MISTY." / "obtained a X!", and
/// the Mt. Moon fossil's "Obtained the HELIX FOSSIL!" (no name before it).
fn item_gained(page: &str) -> Option<(String, String)> {
    for (verb, reason) in [
        (" found ", "found"),
        (" received ", "received"),
        (" obtained ", "obtained"),
    ] {
        let leading = format!("{}{}", verb[1..2].to_uppercase(), &verb[2..]);
        let at = page
            .find(verb)
            .map(|at| at + verb.len())
            .or_else(|| page.starts_with(&leading).then_some(leading.len()));
        if let Some(at) = at {
            let rest = &page[at..];
            let rest = ["a ", "an ", "the ", "one "]
                .iter()
                .find_map(|p| rest.strip_prefix(p))
                .unwrap_or(rest);
            let end = rest.find(['!', '.'])?;
            // "RED received TM03 from MISTY.": the giver follows the item.
            let name = rest[..end].split(" from ").next().unwrap_or_default();
            return Some((name.trim().to_owned(), reason.to_owned()));
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

    /// A page read on two consecutive observations (the first applies
    /// nothing).
    fn read(
        t: &mut TextTracker,
        page: &str,
        d: &GameData,
        last_move: Option<(u8, u8)>,
    ) -> Vec<GameEvent> {
        let first = t.observe_page(&lines(page), d, last_move);
        assert!(first.is_empty(), "{page}: applied on its first reading");
        t.observe_page(&lines(page), d, last_move)
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
            read(&mut t, "RED got ¥1,200\nfor winning!", &d, None),
            vec![GameEvent::MoneyChanged {
                delta: 1200,
                reason: "won a battle".into()
            }]
        );
        assert_eq!(
            read(&mut t, "RED found a POTION!", &d, None),
            vec![GameEvent::ItemsChanged {
                pocket: Pocket::Items,
                item: "ITEM_POTION".into(),
                delta: 1,
                reason: "found".into()
            }]
        );
        // Live, the Helix Fossil: the page starts with the verb.
        assert_eq!(
            read(&mut t, "Obtained the HELIX FOSSIL!", &d, None),
            vec![GameEvent::ItemsChanged {
                pocket: Pocket::KeyItems,
                item: "ITEM_HELIX_FOSSIL".into(),
                delta: 1,
                reason: "obtained".into()
            }]
        );
        // Its follow-up page adds nothing.
        assert!(read(
            &mut t,
            "RED put the HELIX FOSSIL\nin the KEY ITEMS POCKET.",
            &d,
            None
        )
        .is_empty());
        assert_eq!(
            read(
                &mut t,
                "We've restored your POKéMON\nto full health.",
                &d,
                None
            ),
            vec![GameEvent::Healed]
        );
        // The same page is read once.
        assert!(read(
            &mut t,
            "We've restored your POKéMON\nto full health.",
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
            read(
                &mut t,
                "There's no PP left for\nthis move!",
                &d,
                Some((0, 2))
            ),
            vec![GameEvent::MoveOutOfPp {
                slot: 0,
                move_slot: 2
            }]
        );
    }

    #[test]
    fn premier_ball_bonus_page_is_tracked() {
        let Some(d) = data() else { return };
        let mut t = TextTracker::default();
        let bonus = GameEvent::ItemsChanged {
            pocket: Pocket::PokeBalls,
            item: "ITEM_PREMIER_BALL".into(),
            delta: 1,
            reason: "bonus".into(),
        };
        assert_eq!(
            read(&mut t, "I'll throw in a PREMIER\nBALL, too.", &d, None),
            vec![bonus.clone()]
        );
        // Another purchase later: the same page is a new bonus.
        assert!(read(&mut t, "Here you are!\nThank you!", &d, None).is_empty());
        assert_eq!(
            read(&mut t, "I'll throw in a\nPREMIER BALL, too.", &d, None),
            vec![bonus]
        );
        // The purchase's own pages change nothing.
        for page in [
            "Here you are!\nThank you!",
            "POKé BALL, and you want 10.\nThat will be ¥2000. Okay?",
        ] {
            assert!(read(&mut t, page, &d, None).is_empty(), "{page}");
        }
    }

    #[test]
    fn thrown_ball_is_counted() {
        let Some(d) = data() else { return };
        let mut t = TextTracker::default();
        let thrown = GameEvent::ItemsChanged {
            pocket: Pocket::PokeBalls,
            item: "ITEM_POKE_BALL".into(),
            delta: -1,
            reason: "thrown".into(),
        };
        assert_eq!(
            read(&mut t, "RED used\nPOKé BALL!", &d, None),
            vec![thrown.clone()]
        );
        // The next throw of the same battle, after the broke-free page.
        assert!(read(&mut t, "Shoot!\nIt was so close, too!", &d, None).is_empty());
        assert_eq!(read(&mut t, "RED used\nPOKé BALL!", &d, None), vec![thrown]);
        // Other items and moves are not balls thrown.
        for page in [
            "RED used\nPOTION!",
            "PIDGEY used\nTACKLE!",
            "RED used\nPOKé DOLL!",
        ] {
            assert!(read(&mut t, page, &d, None).is_empty(), "{page}");
        }
    }

    #[test]
    fn badge_page_becomes_badge_event() {
        assert_eq!(
            badge_received("RED received the BOULDERBADGE from BROCK."),
            Some("BOULDERBADGE")
        );
        assert_eq!(badge_received("The BOULDERBADGE raises ATTACK."), None);
    }

    /// Live (Task 13): Misty never prints "received the CASCADEBADGE"; her
    /// defeat page, read in battle, is the only page that gives it.
    #[test]
    fn mistys_defeat_page_gives_the_cascade_badge() {
        let Some(d) = data() else { return };
        let mut t = TextTracker::default();
        assert_eq!(
            read(
                &mut t,
                "You can have the CASCADEBADGE to\nshow you beat me.",
                &d,
                None
            ),
            vec![GameEvent::BadgeEarned {
                badge: "CASCADEBADGE".into()
            }]
        );
        // Her explanation afterwards names the badge but gives nothing.
        assert!(read(
            &mut t,
            "The CASCADEBADGE makes all\nPOKéMON up to Lv. 30 obey.",
            &d,
            None
        )
        .is_empty());
    }

    /// Live (Task 13): "RED received TM03\nfrom MISTY." names the giver
    /// after the item.
    #[test]
    fn an_item_received_from_someone_is_tracked() {
        let Some(d) = data() else { return };
        let mut t = TextTracker::default();
        assert_eq!(
            read(&mut t, "RED received TM03\nfrom MISTY.", &d, None),
            vec![GameEvent::ItemsChanged {
                pocket: Pocket::TmCase,
                item: "ITEM_TM03".into(),
                delta: 1,
                reason: "received".into()
            }]
        );
    }

    fn thrown() -> GameEvent {
        GameEvent::ItemsChanged {
            pocket: Pocket::PokeBalls,
            item: "ITEM_POKE_BALL".into(),
            delta: -1,
            reason: "thrown".into(),
        }
    }

    /// Feeds pages one observation each; returns every event.
    fn feed(t: &mut TextTracker, d: &GameData, pages: &[&str]) -> Vec<GameEvent> {
        pages
            .iter()
            .flat_map(|p| t.observe_page(&lines(p), d, None))
            .collect()
    }

    #[test]
    fn a_page_applies_once_after_two_readings() {
        let Some(d) = data() else { return };
        let a = "RED used\nPOKé BALL!";
        let b = "RED used\nPOKé BA?L!";
        // A, A → once.
        assert_eq!(
            feed(&mut TextTracker::default(), &d, &[a, a]),
            vec![thrown()]
        );
        // A once → nothing.
        assert!(feed(&mut TextTracker::default(), &d, &[a]).is_empty());
        // A, A, B, A, A: a one-frame misread doesn't re-apply A.
        assert_eq!(
            feed(&mut TextTracker::default(), &d, &[a, a, b, a, a]),
            vec![thrown()]
        );
        // A, A, B, B: a page that reads twice is a new page.
        let won = "RED got ¥120\nfor winning!";
        assert_eq!(
            feed(&mut TextTracker::default(), &d, &[a, a, won, won]),
            vec![
                thrown(),
                GameEvent::MoneyChanged {
                    delta: 120,
                    reason: "won a battle".into()
                }
            ]
        );
    }
}
