//! A tool's view of a page of text: whether the page was read (on two
//! observations) and so may be advanced, and the facts on it (money won,
//! items found or received, the Premier Ball bonus, balls thrown, the
//! nurse's heal, and an empty move) for the tool's own decisions. The
//! parsing is `pokebot_sense::text`; the runtime's sensor emits those facts
//! as events, so a tool emits only what [`tool_emits`] allows.

use pokebot_gamedata::GameData;
use pokebot_state::GameEvent;
#[cfg(test)]
use pokebot_state::Pocket;

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
        let mut events = pokebot_sense::text::page_events(&page, data);
        // Only the sensor sees the catch on every page; the tools here act
        // on the rest.
        events.retain(|e| !matches!(e, GameEvent::SpeciesCaught { .. }));
        if page.contains("no PP left for") {
            if let Some((slot, move_slot)) = last_move {
                events.push(GameEvent::MoveOutOfPp { slot, move_slot });
            }
        }
        events
    }
}

/// Whether a tool emits `event` itself: the runtime's sensor reads the
/// same pages on every frame and emits every other fact of a page (money,
/// items, badges, heals); emitting them here too would count them twice.
pub fn tool_emits(event: &GameEvent) -> bool {
    matches!(event, GameEvent::MoveOutOfPp { .. })
}

pub use pokebot_sense::text::BADGES;

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
