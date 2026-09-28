//! What the foe's moves and abilities forbid our battler, read from the
//! battle's pages (the game's own words, `battle_message.c` and
//! `strings.c`). The game refuses a forbidden choice with a page and puts
//! the same menu back, so choosing it again loops (the Switch in Diglett's
//! Cave: "Wild DIGLETT's ARENA TRAP prevents switching!" 45 times over a
//! switch-training SHIFT). The battle reads them and chooses otherwise:
//!
//! - a trap refuses SHIFT and RUN alike: the foe's ARENA TRAP, SHADOW TAG or
//!   MAGNET PULL (until it leaves); MEAN LOOK, SPIDER WEB and BLOCK ("… can't
//!   escape now!"), our INGRAIN, and the binding moves (BIND, WRAP, FIRE
//!   SPIN, CLAMP, WHIRLPOOL, SAND TOMB: until "… was freed from …!");
//! - TAUNT refuses status moves, TORMENT the move used last, IMPRISON the
//!   sealed moves, a CHOICE BAND every move but the chosen one (DISABLE is
//!   [`crate::battle::disable_text`]'s);
//! - ENCORE makes FIGHT use the encored move without the move menu.
//!
//! "Can't escape!" is both a RUN that failed for speed (the turn goes on:
//! the foe moves) and a RUN refused by a trap (the command menu is back at
//! once); only the second makes the battle one that can't be run from.

use pokebot_gamedata::GameData;

/// A page about what our battler may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    /// SHIFT and RUN are refused; `ability`: by the foe's ability (lasts
    /// while that foe is out), else by a move on our battler.
    Trapped {
        ability: bool,
    },
    /// A binding move holds our battler (ends with [`Page::Freed`]).
    Bound,
    Freed,
    Taunted,
    Tormented,
    /// IMPRISON: this move is sealed.
    Sealed(String),
    /// A CHOICE BAND allows only this move.
    Only(String),
    Encored,
    EncoreEnded,
    /// "Can't escape!": a failed or a refused RUN.
    CantEscape,
}

/// The page's restriction on our battler (`lead`: its printed name; the
/// foe's are prefixed "Wild "/"Foe "). The font's apostrophe reads as `’`.
pub fn read(page: &str, lead: &str, data: &GameData) -> Option<Page> {
    let page = page.replace('’', "'");
    let page = page.trim();
    if page == "Can't escape!" {
        return Some(Page::CantEscape);
    }
    // The party menu's and RUN's refusals by an ability, naming the foe.
    if page.ends_with(" prevents switching!") || page.contains(" prevents escape with ") {
        return Some(Page::Trapped { ability: true });
    }
    if lead.is_empty() {
        return None;
    }
    let ours = |text: &str| page == format!("{lead} {text}");
    if ours("can't be switched out!") || ours("can't escape now!") {
        return Some(Page::Trapped { ability: false });
    }
    if ours("was trapped in the vortex!")
        || ours("was trapped by SAND TOMB!")
        || (page.starts_with(&format!("{lead} was squeezed by ")) && page.ends_with("'s BIND!"))
        || (page.starts_with(&format!("{lead} was WRAPPED by ")) && page.ends_with('!'))
        || (page.ends_with(&format!(" CLAMPED {lead}!")) && !page.starts_with(lead))
    {
        return Some(Page::Bound);
    }
    if page.starts_with(&format!("{lead} was freed from ")) {
        return Some(Page::Freed);
    }
    if ours("fell for the TAUNT!")
        || (page.starts_with(&format!("{lead} can't use ")) && page.ends_with(" after the TAUNT!"))
    {
        return Some(Page::Taunted);
    }
    if ours("was subjected to TORMENT!")
        || ours("can't use the same move in a row due to the TORMENT!")
    {
        return Some(Page::Tormented);
    }
    if let Some(name) = page
        .strip_prefix(&format!("{lead} can't use the sealed "))
        .and_then(|r| r.strip_suffix('!'))
    {
        return data.move_named(name).map(|m| Page::Sealed(m.to_owned()));
    }
    // "<ITEM>'s effect allows only <MOVE> to be used!"
    if let Some(rest) = page.split_once("'s effect allows only ").map(|(_, r)| r) {
        let name = rest.strip_suffix(" to be used!")?;
        return data.move_named(name).map(|m| Page::Only(m.to_owned()));
    }
    if ours("got an ENCORE!") {
        return Some(Page::Encored);
    }
    if page == format!("{lead}'s ENCORE ended!") {
        return Some(Page::EncoreEnded);
    }
    None
}

/// What our battler may not do now, from the pages read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Limits {
    /// SHIFT and RUN are refused: why.
    pub trapped: Option<String>,
    /// The trap is the foe's ability (ends when that foe goes), not a move
    /// on our battler (ends when our battler goes).
    trap_by_ability: bool,
    /// Held by a binding move (also refuses SHIFT and RUN).
    pub bound: bool,
    pub taunted: bool,
    pub tormented: bool,
    pub sealed: Vec<String>,
    pub only: Option<String>,
    pub encored: bool,
    /// "Can't escape!" was the last page: refused, if the command menu
    /// comes back before any other page.
    cant_escape_pending: bool,
}

impl Limits {
    /// Takes in battle page `page` (read once).
    pub fn observe(&mut self, page: &str, lead: &str, data: &GameData) {
        let read = read(page, lead, data);
        self.cant_escape_pending = read == Some(Page::CantEscape);
        match read {
            Some(Page::Trapped { ability }) => {
                if self.trapped.is_none() {
                    self.trapped = Some(page.replace('’', "'"));
                }
                self.trap_by_ability |= ability;
            }
            Some(Page::Bound) => self.bound = true,
            Some(Page::Freed) => self.bound = false,
            Some(Page::Taunted) => self.taunted = true,
            Some(Page::Tormented) => self.tormented = true,
            Some(Page::Sealed(m)) => {
                if !self.sealed.contains(&m) {
                    self.sealed.push(m);
                }
            }
            Some(Page::Only(m)) => self.only = Some(m),
            Some(Page::Encored) => self.encored = true,
            Some(Page::EncoreEnded) => self.encored = false,
            Some(Page::CantEscape) | None => {}
        }
        // Our battler or the foe changed: what they did to each other is
        // over ("Go! X!" after a faint or a SHIFT; a trainer's next one).
        if page.starts_with("Go! ") || page.contains(", come back!") {
            self.battler_changed();
        }
        if page.contains(" sent out ") {
            self.foe_changed();
        }
    }

    /// The command menu is up: a "Can't escape!" just before it, with no
    /// turn between (no foe move), was a refusal.
    pub fn at_command_menu(&mut self) {
        if std::mem::take(&mut self.cant_escape_pending) && self.trapped.is_none() {
            self.trapped = Some("Can't escape! (refused, no turn passed)".into());
        }
    }

    pub fn can_switch(&self) -> bool {
        self.trapped.is_none() && !self.bound
    }

    pub fn can_run(&self) -> bool {
        self.trapped.is_none() && !self.bound
    }

    /// Why SHIFT and RUN are refused, if they are.
    pub fn why(&self) -> Option<String> {
        self.trapped
            .clone()
            .or_else(|| self.bound.then(|| "held by a binding move".to_owned()))
    }

    /// Whether the game accepts move `mv` (DISABLE aside), `last` the move
    /// used last.
    pub fn allows(&self, data: &GameData, mv: &str, last: Option<&str>) -> bool {
        if self.only.as_deref().is_some_and(|only| only != mv) {
            return false;
        }
        if self.sealed.iter().any(|m| m == mv) {
            return false;
        }
        if self.tormented && last == Some(mv) && mv != "MOVE_STRUGGLE" {
            return false;
        }
        if self.taunted && data.move_(mv).is_some_and(|m| m.power == 0) {
            return false;
        }
        true
    }

    fn battler_changed(&mut self) {
        let by_ability = self.trap_by_ability.then(|| self.trapped.clone()).flatten();
        let sealed = std::mem::take(&mut self.sealed);
        *self = Limits {
            trapped: by_ability,
            trap_by_ability: self.trap_by_ability,
            sealed,
            ..Limits::default()
        };
    }

    fn foe_changed(&mut self) {
        if self.trap_by_ability {
            self.trapped = None;
            self.trap_by_ability = false;
        }
        self.sealed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Option<GameData> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        GameData::load(root.join("data/world/gamedata.json")).ok()
    }

    /// The pages as the Switch reads them (two lines joined, `’`).
    #[test]
    fn every_restriction_page_is_read() {
        let Some(data) = data() else { return };
        let r = |p: &str| read(p, "CATERPIE", &data);
        let trapped = Some(Page::Trapped { ability: true });
        assert_eq!(r("Wild DIGLETT’s ARENA TRAP prevents switching!"), trapped);
        assert_eq!(r("Foe WOBBUFFET prevents escape with SHADOW TAG!"), trapped);
        let held = Some(Page::Trapped { ability: false });
        assert_eq!(r("CATERPIE can’t be switched out!"), held);
        assert_eq!(r("CATERPIE can’t escape now!"), held);
        assert_eq!(r("Wild GASTLY can’t escape now!"), None);
        assert_eq!(
            r("CATERPIE was squeezed by Wild ONIX’s BIND!"),
            Some(Page::Bound)
        );
        assert_eq!(r("CATERPIE was WRAPPED by Wild EKANS!"), Some(Page::Bound));
        assert_eq!(r("CATERPIE was trapped in the vortex!"), Some(Page::Bound));
        assert_eq!(r("CATERPIE was trapped by SAND TOMB!"), Some(Page::Bound));
        assert_eq!(r("Foe CLOYSTER CLAMPED CATERPIE!"), Some(Page::Bound));
        assert_eq!(r("CATERPIE CLAMPED Foe SHELLDER!"), None);
        assert_eq!(r("Wild EKANS was WRAPPED by CATERPIE!"), None);
        assert_eq!(r("CATERPIE was freed from WRAP!"), Some(Page::Freed));
        assert_eq!(r("CATERPIE fell for the TAUNT!"), Some(Page::Taunted));
        assert_eq!(
            r("CATERPIE can’t use STRING SHOT after the TAUNT!"),
            Some(Page::Taunted)
        );
        assert_eq!(
            r("CATERPIE was subjected to TORMENT!"),
            Some(Page::Tormented)
        );
        assert_eq!(
            r("CATERPIE can’t use the same move in a row due to the TORMENT!"),
            Some(Page::Tormented)
        );
        assert_eq!(
            r("CATERPIE can’t use the sealed TACKLE!"),
            Some(Page::Sealed("MOVE_TACKLE".into()))
        );
        assert_eq!(
            r("CHOICE BAND’s effect allows only TACKLE to be used!"),
            Some(Page::Only("MOVE_TACKLE".into()))
        );
        assert_eq!(r("CATERPIE got an ENCORE!"), Some(Page::Encored));
        assert_eq!(r("CATERPIE’s ENCORE ended!"), Some(Page::EncoreEnded));
        assert_eq!(r("Can’t escape!"), Some(Page::CantEscape));
        assert_eq!(r("Wild DIGLETT used SCRATCH!"), None);
    }

    #[test]
    fn a_trap_refuses_shift_and_run_until_it_ends() {
        let Some(data) = data() else { return };
        let mut l = Limits::default();
        l.observe(
            "Wild DIGLETT’s ARENA TRAP prevents switching!",
            "CATERPIE",
            &data,
        );
        assert!(!l.can_switch() && !l.can_run());
        // The ability stays with the foe, whoever of ours is out.
        l.observe("Go! IVYSAUR!", "IVYSAUR", &data);
        assert!(!l.can_run());
        // A trainer's next Pokémon: gone.
        l.observe("COOLTRAINER sent out PIDGEY!", "IVYSAUR", &data);
        assert!(l.can_switch() && l.can_run());
        // A binding move: until freed.
        l.observe("IVYSAUR was WRAPPED by Foe EKANS!", "IVYSAUR", &data);
        assert!(!l.can_run());
        l.observe("IVYSAUR was freed from WRAP!", "IVYSAUR", &data);
        assert!(l.can_run());
        // MEAN LOOK: until our battler goes.
        l.observe("IVYSAUR can’t escape now!", "IVYSAUR", &data);
        assert!(!l.can_switch());
        l.observe("Go! PIDGEY!", "PIDGEY", &data);
        assert!(l.can_switch());
    }

    /// A RUN that failed for speed lets the foe move; one refused by a
    /// trap puts the command menu straight back.
    #[test]
    fn cant_escape_is_a_trap_only_when_no_turn_passes() {
        let Some(data) = data() else { return };
        let mut l = Limits::default();
        l.observe("Can’t escape!", "CATERPIE", &data);
        l.observe("Wild RATTATA used TACKLE!", "CATERPIE", &data);
        l.at_command_menu();
        assert!(l.can_run());
        l.observe("Can’t escape!", "CATERPIE", &data);
        l.at_command_menu();
        assert!(!l.can_run());
    }

    #[test]
    fn refused_moves_are_not_chosen() {
        let Some(data) = data() else { return };
        let mut l = Limits::default();
        assert!(l.allows(&data, "MOVE_STRING_SHOT", None));
        l.observe("CATERPIE fell for the TAUNT!", "CATERPIE", &data);
        assert!(!l.allows(&data, "MOVE_STRING_SHOT", None));
        assert!(l.allows(&data, "MOVE_TACKLE", None));
        l.observe("CATERPIE was subjected to TORMENT!", "CATERPIE", &data);
        assert!(!l.allows(&data, "MOVE_TACKLE", Some("MOVE_TACKLE")));
        assert!(l.allows(&data, "MOVE_TACKLE", Some("MOVE_STRING_SHOT")));
        l.observe("CATERPIE can’t use the sealed TACKLE!", "CATERPIE", &data);
        assert!(!l.allows(&data, "MOVE_TACKLE", None));
        let mut band = Limits::default();
        band.observe(
            "CHOICE BAND’s effect allows only TACKLE to be used!",
            "CATERPIE",
            &data,
        );
        assert!(band.allows(&data, "MOVE_TACKLE", None));
        assert!(!band.allows(&data, "MOVE_STRING_SHOT", None));
    }
}
