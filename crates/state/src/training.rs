//! A Pokémon's effort values and stat readings, kept from what the bot
//! sees (the game's rules: [`pokebot_gamedata::training`]).
//!
//! Every "X gained N EXP. Points!" is one defeated foe's EV yield for X.
//! EVs start at 0 for a Pokémon obtained while the bot watches; for one it
//! had before (or a defeat whose foe it didn't read) the EVs it may have
//! missed are kept as `unseen`, so each stat's EVs are a range. Stats are
//! computed only at a level-up, an evolution or a withdrawal from the PC,
//! so each stat reading is kept with the EV range of that computation.
//! The IVs that explain the readings are solved by the sensor, which has
//! the species' base stats, into [`Training::estimate`].

use pokebot_gamedata::training::{gain_evs, MAX_STAT_EVS, MAX_YIELD};
use serde::{Deserialize, Serialize};

/// Stat readings kept per Pokémon (a few per level up to Lv 100).
pub const SAMPLES_KEPT: usize = 256;
/// Recent defeats kept per Pokémon, for inspection.
pub const DEFEATS_KEPT: usize = 64;

/// EVs per stat (HP, Atk, Def, Spe, SpA, SpD), each within `low..=high`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvRange {
    pub low: [u16; 6],
    pub high: [u16; 6],
}

/// One defeated foe's EVs for this Pokémon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defeat {
    pub frame_id: u64,
    /// `None`: the foe wasn't read (its yield counts as unseen).
    pub species: Option<String>,
    pub level: Option<u8>,
    pub exp: u32,
    /// EVs added (after the game's caps).
    pub evs: [u16; 6],
}

/// Stats read at `level` (`None` = not read), computed from `evs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatSample {
    pub frame_id: u64,
    pub species: String,
    pub level: u8,
    pub stats: [Option<u16>; 6],
    pub evs: EvRange,
}

/// The IVs and natures that explain every stat sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IvEstimate {
    /// The [`Training::revision`] and read nature it was solved for.
    pub revision: u32,
    pub nature_read: Option<String>,
    /// Natures that fit (one when read, or when the stats tell).
    pub natures: Vec<String>,
    /// Lowest and highest possible IV per stat (HP, Atk, Def, Spe, SpA, SpD).
    pub ivs: [(u8, u8); 6],
    /// False when no IVs fit the counted EVs: an award was missed or a
    /// reading was wrong, and the EVs' upper bounds were dropped.
    pub consistent: bool,
    /// Every EV the Pokémon has was counted.
    pub exact_evs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Training {
    /// EVs counted from the defeats seen (HP, Atk, Def, Spe, SpA, SpD).
    pub evs: [u16; 6],
    /// The most EVs per stat gained without being counted (255: the
    /// Pokémon's history is unknown).
    pub unseen: [u16; 6],
    /// EVs the stats shown were computed from.
    pub stats_evs: EvRange,
    /// The highest level seen: the level the stats were computed at (a
    /// lower reading is a misread, and a Pokémon never loses levels).
    pub level: Option<u8>,
    /// Counted EVs when the current level was last seen, and after the
    /// first award since: a level-up's computation used at least those.
    pub level_evs: [u16; 6],
    pub first_award: Option<[u16; 6]>,
    pub defeats: u32,
    /// The last [`DEFEATS_KEPT`] defeats.
    pub recent: Vec<Defeat>,
    /// Total experience as last read on the summary, plus awards since
    /// (a reading above it means awards went unseen).
    pub exp: Option<u32>,
    pub samples: Vec<StatSample>,
    /// Bumped whenever the EVs or samples change.
    pub revision: u32,
    pub estimate: Option<IvEstimate>,
}

impl Default for Training {
    /// A Pokémon whose history is unknown: any EVs.
    fn default() -> Self {
        Self {
            evs: [0; 6],
            unseen: [MAX_STAT_EVS; 6],
            stats_evs: EvRange {
                low: [0; 6],
                high: [MAX_STAT_EVS; 6],
            },
            level: None,
            level_evs: [0; 6],
            first_award: None,
            defeats: 0,
            recent: Vec::new(),
            exp: None,
            samples: Vec::new(),
            revision: 0,
            estimate: None,
        }
    }
}

fn add(a: [u16; 6], b: [u16; 6]) -> [u16; 6] {
    std::array::from_fn(|i| (a[i] + b[i]).min(MAX_STAT_EVS))
}

impl Training {
    /// A Pokémon obtained at `level` while the bot watched: every Pokémon
    /// is created with no EVs (wild, gift, starter, in-game trade).
    pub fn obtained(level: u8) -> Self {
        Self {
            unseen: [0; 6],
            stats_evs: EvRange {
                low: [0; 6],
                high: [0; 6],
            },
            level: Some(level),
            ..Self::default()
        }
    }

    /// Every EV this Pokémon has was counted.
    pub fn exact(&self) -> bool {
        self.unseen == [0; 6]
    }

    /// The EVs now, each within `low..=high`.
    pub fn ev_range(&self) -> EvRange {
        EvRange {
            low: self.evs,
            high: add(self.evs, self.unseen),
        }
    }

    /// One defeat's EVs ("X gained N EXP. Points!"). `ev_yield` is the
    /// defeated species'; `None` counts the most any species yields as
    /// unseen.
    pub fn award(
        &mut self,
        frame_id: u64,
        species: Option<String>,
        level: Option<u8>,
        ev_yield: Option<[u8; 6]>,
        exp: u32,
        macho_brace: bool,
    ) {
        let evs = match ev_yield {
            // FireRed can't spread Pokérus: only a traded-in Pokémon has it.
            Some(y) => gain_evs(&mut self.evs, y, false, macho_brace),
            None => {
                let most = MAX_YIELD * if macho_brace { 2 } else { 1 };
                self.unseen = add(self.unseen, [most; 6]);
                [0; 6]
            }
        };
        self.first_award.get_or_insert(self.evs);
        self.exp = self.exp.map(|e| e.saturating_add(exp));
        self.defeats += 1;
        self.recent.push(Defeat {
            frame_id,
            species,
            level,
            exp,
            evs,
        });
        if self.recent.len() > DEFEATS_KEPT {
            self.recent.remove(0);
        }
        self.revision += 1;
    }

    /// The level was read as `level`: above the highest seen, the stats
    /// were computed again, from at least the EVs counted after the first
    /// award since that level was last seen (the award that raised it).
    pub fn level_seen(&mut self, level: u8) {
        match self.level {
            Some(top) if level < top => return,
            Some(top) if level > top => {
                self.stats_evs = EvRange {
                    low: self.first_award.unwrap_or(self.level_evs),
                    high: add(self.evs, self.unseen),
                };
                self.revision += 1;
            }
            _ => {}
        }
        self.level = Some(level);
        self.level_evs = self.evs;
        self.first_award = None;
    }

    /// The stats were computed again at an unseen moment since the level
    /// was last seen (an evolution, a withdrawal from the PC).
    pub fn recalculated(&mut self) {
        self.stats_evs = EvRange {
            low: self.level_evs,
            high: add(self.evs, self.unseen),
        };
        self.revision += 1;
    }

    /// Stats read at `level` (HP, Atk, Def, Spe, SpA, SpD; `None` = not
    /// read). Readings of the same computation fill one sample; a new
    /// value replaces a misread one.
    pub fn observe_stats(
        &mut self,
        frame_id: u64,
        species: &str,
        level: u8,
        stats: [Option<u16>; 6],
    ) {
        if stats.iter().all(Option::is_none) {
            return;
        }
        let evs = self.stats_evs;
        let same = |s: &StatSample| s.species == species && s.level == level && s.evs == evs;
        if let Some(s) = self.samples.iter_mut().rev().find(|s| same(s)) {
            let merged = std::array::from_fn(|i| stats[i].or(s.stats[i]));
            if merged == s.stats {
                return;
            }
            s.stats = merged;
            s.frame_id = frame_id;
        } else {
            self.samples.push(StatSample {
                frame_id,
                species: species.to_owned(),
                level,
                stats,
                evs,
            });
            if self.samples.len() > SAMPLES_KEPT {
                self.samples.remove(0);
            }
        }
        self.revision += 1;
    }

    /// Total experience read on the summary. More than the awards seen
    /// add up to means some went unseen: each missed defeat gave at least
    /// as much experience as the smallest award seen, and at most 3 EVs
    /// in a stat.
    pub fn observe_exp(&mut self, exp: u32) {
        if let Some(tracked) = self.exp.filter(|t| exp > *t) {
            let smallest = self.recent.iter().map(|d| d.exp).min().unwrap_or(1).max(1);
            let missed = (exp - tracked).div_ceil(smallest);
            let most =
                u16::try_from(missed.saturating_mul(u32::from(MAX_YIELD))).unwrap_or(u16::MAX);
            self.unseen = add(self.unseen, [most.min(MAX_STAT_EVS); 6]);
            self.revision += 1;
        }
        self.exp = Some(exp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIDGEY: [u8; 6] = [0, 0, 0, 1, 0, 0];

    #[test]
    fn awards_are_counted_and_a_level_up_uses_them() {
        let mut t = Training::obtained(5);
        t.award(
            1,
            Some("SPECIES_PIDGEY".into()),
            Some(3),
            Some(PIDGEY),
            8,
            false,
        );
        t.level_seen(5);
        t.award(
            2,
            Some("SPECIES_PIDGEY".into()),
            Some(3),
            Some(PIDGEY),
            8,
            false,
        );
        assert_eq!(t.evs, [0, 0, 0, 2, 0, 0]);
        t.level_seen(6);
        assert_eq!(t.stats_evs.low, [0, 0, 0, 2, 0, 0]);
        assert_eq!(t.stats_evs.high, [0, 0, 0, 2, 0, 0]);
        // A later award doesn't show until the next computation.
        t.award(
            3,
            Some("SPECIES_RATTATA".into()),
            Some(3),
            Some(PIDGEY),
            8,
            true,
        );
        assert_eq!(t.evs[3], 4);
        assert_eq!(t.stats_evs.high[3], 2);
        // A level misread low, then read right again: no new computation.
        t.level_seen(3);
        t.level_seen(6);
        assert_eq!(t.stats_evs.high[3], 2);
        assert_eq!(t.level, Some(6));
    }

    #[test]
    fn an_unread_foe_and_missed_experience_widen_the_range() {
        let mut t = Training::obtained(5);
        t.observe_exp(100);
        t.award(1, None, None, None, 10, false);
        assert_eq!(t.ev_range().high, [3; 6]);
        assert_eq!(t.exp, Some(110));
        // 25 more than the one 10-point award: up to 3 defeats missed.
        t.observe_exp(135);
        assert_eq!(t.unseen, [12; 6]);
        assert!(!t.exact());
    }

    #[test]
    fn readings_of_one_computation_share_a_sample() {
        let mut t = Training::obtained(9);
        t.observe_stats(
            1,
            "SPECIES_BULBASAUR",
            9,
            [Some(27), None, None, None, None, None],
        );
        let r = t.revision;
        t.observe_stats(
            2,
            "SPECIES_BULBASAUR",
            9,
            [Some(27), None, None, None, None, None],
        );
        assert_eq!(t.revision, r);
        t.observe_stats(
            3,
            "SPECIES_BULBASAUR",
            9,
            [None, Some(12), Some(14), Some(13), Some(17), Some(18)],
        );
        assert_eq!(t.samples.len(), 1);
        assert_eq!(t.samples[0].stats[0], Some(27));
        t.level_seen(10);
        t.observe_stats(
            4,
            "SPECIES_BULBASAUR",
            10,
            [Some(29), None, None, None, None, None],
        );
        assert_eq!(t.samples.len(), 2);
    }

    #[test]
    fn unknown_history_is_the_default() {
        let t: Training = serde_json::from_str("{}").unwrap();
        assert_eq!(t.unseen, [255; 6]);
        assert!(!t.exact());
    }
}
