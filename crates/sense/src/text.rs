//! Facts in a page of text. Deltas (money won, items gained, balls
//! thrown) are owned here: nothing else may emit them for text, or they
//! would be counted twice. Party facts name a Pokémon, which is placed in
//! the party by [`crate::names`].

use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, Pocket};

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

/// What a page says about a named Pokémon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonFact {
    /// "BULBASAUR grew to LV. 12!"
    GrewTo { name: String, level: u8 },
    /// "… BULBASAUR evolved into IVYSAUR!"
    Evolved { name: String, into: String },
    /// "BULBASAUR fainted!" (not "Foe …" / "Wild …").
    Fainted { name: String },
}

/// Events for the bag, money, badges and heals a page implies.
pub fn page_events(page: &str, data: &GameData) -> Vec<GameEvent> {
    let mut events = Vec::new();
    if let Some(amount) = money_won(page) {
        events.push(GameEvent::MoneyChanged {
            delta: i64::from(amount),
            reason: "won a battle".into(),
        });
    }
    if let Some((name, reason)) = item_gained(page) {
        // "received 30 SAFARI BALLS": a count, and the plural.
        let (count, name) = match name.split_once(' ') {
            Some((n, rest)) if n.chars().all(|c| c.is_ascii_digit()) => {
                (n.parse().unwrap_or(1), rest.to_owned())
            }
            _ => (1, name),
        };
        let key = data.item_named(&name).or_else(|| {
            (count > 1)
                .then(|| name.strip_suffix('S'))
                .flatten()
                .and_then(|n| data.item_named(n))
        });
        if let Some(key) = key {
            if let Some(pocket) = data.items[key]
                .pocket
                .as_deref()
                .and_then(Pocket::from_decomp)
            {
                events.push(GameEvent::ItemsChanged {
                    pocket,
                    item: key.to_owned(),
                    delta: count,
                    reason,
                });
            }
        }
    }
    // A ball thrown in battle: "RED used POKé BALL!".
    if let Some(item) = ball_thrown(page, data) {
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
    if let Some(badge) = badge_received(page) {
        events.push(GameEvent::BadgeEarned {
            badge: badge.to_owned(),
        });
    }
    if page.contains("restored your POKéMON") {
        events.push(GameEvent::Healed);
    }
    if let Some(species) = caught(page).and_then(|name| data.species_named(&name)) {
        events.push(GameEvent::SpeciesCaught {
            species: species.to_owned(),
        });
    }
    events
}

/// "BULBASAUR is trying to learn VINE WHIP." → (`BULBASAUR`, `VINE WHIP`):
/// the KNOWN MOVES list that follows is that Pokémon's.
pub fn trying_to_learn(page: &str) -> Option<(String, String)> {
    let at = page.find(" is trying to learn ")?;
    let name = page[..at].rsplit(' ').next()?.to_owned();
    let rest = &page[at + " is trying to learn ".len()..];
    let mv = rest[..rest.find('.')?].trim().to_owned();
    (!name.is_empty() && !mv.is_empty()).then_some((name, mv))
}

/// Facts about named Pokémon on a page.
pub fn mon_facts(page: &str) -> Vec<MonFact> {
    let mut out = Vec::new();
    let before = |at: usize| page[..at].rsplit(' ').next().map(str::to_owned);
    if let Some(at) = page.find(" grew to ") {
        // The level glyph reads `Lv`; the game prints "LV. " or the glyph.
        let rest = page[at + " grew to ".len()..].trim_start();
        let rest = ["LV.", "Lv.", "Lv", "LV"]
            .iter()
            .find_map(|p| rest.strip_prefix(p))
            .unwrap_or(rest)
            .trim_start();
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let (Some(name), Ok(level)) = (before(at), digits.parse()) {
            out.push(MonFact::GrewTo { name, level });
        }
    }
    if let Some(at) = page.find(" evolved into ") {
        let rest = &page[at + " evolved into ".len()..];
        if let (Some(name), Some(end)) = (before(at), rest.find('!')) {
            out.push(MonFact::Evolved {
                name,
                into: rest[..end].trim().to_owned(),
            });
        }
    }
    if let Some(at) = page.find(" fainted!") {
        let prefix = &page[..at];
        let foe = prefix
            .rsplit(' ')
            .nth(1)
            .is_some_and(|w| w == "Foe" || w == "Wild");
        if let (false, Some(name)) = (foe, before(at)) {
            if !name.is_empty() {
                out.push(MonFact::Fainted { name });
            }
        }
    }
    out
}

/// "BULBASAUR gained 66 EXP. Points!" (or "gained a boosted 99 EXP.
/// Points!" for a traded Pokémon) → (`BULBASAUR`, 66). Only the whole
/// page: each one is one defeated foe's EVs for the Pokémon it names.
pub fn exp_gained(page: &str) -> Option<(String, u32)> {
    let at = page.find(" gained ")?;
    let name = page[..at].rsplit(' ').next()?.to_owned();
    let rest = &page[at + " gained ".len()..];
    let rest = rest.strip_prefix("a boosted ").unwrap_or(rest);
    let (digits, rest) = rest.split_once(' ')?;
    let exp = digits.replace(',', "").parse().ok()?;
    (rest.starts_with("EXP. Points!") && !name.is_empty() && !name.contains('?'))
        .then_some((name, exp))
}

/// "Wild PIDGEY fainted!" / "Foe GEODUDE fainted!" → the foe's name.
pub fn foe_fainted(page: &str) -> Option<String> {
    let at = page.find(" fainted!")?;
    let mut words = page[..at].rsplit(' ');
    let name = words.next()?;
    (matches!(words.next(), Some("Wild" | "Foe")) && !name.is_empty() && !name.contains('?'))
        .then(|| name.to_owned())
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

/// "RED received the BOULDERBADGE from BROCK." → `BOULDERBADGE`. Misty
/// prints no "received" page: her defeat page "You can have the
/// CASCADEBADGE to show you beat me." gives it.
fn badge_received(page: &str) -> Option<&'static str> {
    BADGES.into_iter().find(|b| {
        page.contains(&format!(" received the {b}"))
            || page.contains(&format!("You can have the {b}"))
    })
}

/// "Gotcha! PIDGEY was caught!" → `PIDGEY`.
fn caught(page: &str) -> Option<String> {
    let rest = &page[page.find("Gotcha! ")? + "Gotcha! ".len()..];
    let name = rest[..rest.find(" was caught!")?].trim();
    (!name.is_empty() && !name.contains('?')).then(|| name.to_owned())
}

/// The species name a page says was just caught: "Gotcha! X was caught!",
/// or the nickname question only a catch gets ("Give a nickname to the
/// captured X?"), for when the first page went by unread.
pub fn caught_name(page: &str) -> Option<String> {
    caught(page).or_else(|| {
        let rest = &page[page.find("Give a nickname to the captured ")?
            + "Give a nickname to the captured ".len()..];
        let name = rest[..rest.find('?')?].trim();
        (!name.is_empty()).then(|| name.to_owned())
    })
}

/// The pages after a catch when the party is full: "X was transferred to
/// Someone's PC." / "… to BOX “BOX2.”" / "BOX “BOX1” was full." / "It was
/// placed in …".
pub fn is_pc_transfer_text(page: &str) -> bool {
    page.contains("transferred to") || page.contains("placed in") || page.contains("was full")
}

/// "It was placed in BOX “BOX2.”" / "X was transferred to BOX “BOX2.”" →
/// 1 (0-based). The "BOX … was full." page names the full box, not the new
/// one.
pub fn box_from_text(page: &str) -> Option<u8> {
    if page.contains("was full") || !(page.contains("placed in") || page.contains("transferred to"))
    {
        return None;
    }
    // The last "BOX" followed (within two characters) by a number.
    page.match_indices("BOX")
        .filter_map(|(at, _)| {
            let digits: String = page[at + 3..]
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect();
            let gap = page[at + 3..]
                .chars()
                .take_while(|c| !c.is_ascii_digit())
                .count();
            (gap <= 2).then(|| digits.parse::<u8>().ok()).flatten()
        })
        .last()
        .filter(|n| *n >= 1)
        .map(|n| n - 1)
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

    fn data() -> Option<GameData> {
        GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok()
    }

    #[test]
    fn money_items_heal_badge_and_catch() {
        let Some(d) = data() else { return };
        assert_eq!(
            page_events("RED got ¥1,200 for winning!", &d),
            vec![GameEvent::MoneyChanged {
                delta: 1200,
                reason: "won a battle".into()
            }]
        );
        assert_eq!(
            page_events("RED found a POTION!", &d),
            vec![GameEvent::ItemsChanged {
                pocket: Pocket::Items,
                item: "ITEM_POTION".into(),
                delta: 1,
                reason: "found".into()
            }]
        );
        assert_eq!(
            page_events("RED received TM03 from MISTY.", &d)[0],
            GameEvent::ItemsChanged {
                pocket: Pocket::TmCase,
                item: "ITEM_TM03".into(),
                delta: 1,
                reason: "received".into()
            }
        );
        assert_eq!(
            page_events("RED used POKé BALL!", &d),
            vec![GameEvent::ItemsChanged {
                pocket: Pocket::PokeBalls,
                item: "ITEM_POKE_BALL".into(),
                delta: -1,
                reason: "thrown".into()
            }]
        );
        assert!(page_events("RED used POTION!", &d).is_empty());
        assert_eq!(
            page_events("RED received 30 SAFARI BALLS from the attendant.", &d),
            vec![GameEvent::ItemsChanged {
                pocket: Pocket::PokeBalls,
                item: "ITEM_SAFARI_BALL".into(),
                delta: 30,
                reason: "received".into()
            }]
        );
        assert_eq!(
            page_events("We've restored your POKéMON to full health.", &d),
            vec![GameEvent::Healed]
        );
        assert_eq!(
            page_events("RED received the BOULDERBADGE from BROCK.", &d),
            vec![GameEvent::BadgeEarned {
                badge: "BOULDERBADGE".into()
            }]
        );
        assert!(page_events("The BOULDERBADGE raises ATTACK.", &d).is_empty());
        assert_eq!(
            page_events("Gotcha! PIDGEY was caught!", &d),
            vec![GameEvent::SpeciesCaught {
                species: "SPECIES_PIDGEY".into()
            }]
        );
        assert!(page_events("Gotcha! PID?EY was caught!", &d).is_empty());
        assert_eq!(
            page_events("I'll throw in a PREMIER BALL, too.", &d).len(),
            1
        );
    }

    #[test]
    fn named_pokemon_facts() {
        assert_eq!(
            mon_facts("BULBASAUR grew to Lv12!"),
            vec![MonFact::GrewTo {
                name: "BULBASAUR".into(),
                level: 12
            }]
        );
        assert_eq!(
            mon_facts("Congratulations! Your BULBASAUR evolved into IVYSAUR!"),
            vec![MonFact::Evolved {
                name: "BULBASAUR".into(),
                into: "IVYSAUR".into()
            }]
        );
        assert_eq!(
            mon_facts("BULBASAUR fainted!"),
            vec![MonFact::Fainted {
                name: "BULBASAUR".into()
            }]
        );
        assert_eq!(
            mon_facts("BULBASAUR grew to LV. 9!"),
            vec![MonFact::GrewTo {
                name: "BULBASAUR".into(),
                level: 9
            }]
        );
        assert!(mon_facts("Foe PIDGEY fainted!").is_empty());
        assert_eq!(
            foe_fainted("Foe PIDGEY fainted!").as_deref(),
            Some("PIDGEY")
        );
        assert_eq!(
            foe_fainted("Wild RATTATA fainted!").as_deref(),
            Some("RATTATA")
        );
        assert_eq!(foe_fainted("BULBASAUR fainted!"), None);
        assert_eq!(
            exp_gained("BULBASAUR gained 66 EXP. Points!"),
            Some(("BULBASAUR".into(), 66))
        );
        assert_eq!(
            exp_gained("ABRA gained a boosted 1,203 EXP. Points!"),
            Some(("ABRA".into(), 1203))
        );
        // Half-printed: not yet the page.
        assert_eq!(exp_gained("BULBASAUR gained 67 EXP."), None);
        assert!(mon_facts("Wild PIDGEY fainted!").is_empty());
    }
}
