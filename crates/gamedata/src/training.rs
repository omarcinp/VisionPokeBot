//! Individual values (IVs), effort values (EVs) and natures, as
//! Generation III computes them (`CalculateMonStats`, `MonGainEVs` and
//! `ModifyStatByNature` in pret/pokefirered's `src/pokemon.c`).
//!
//! - IVs: 0–31 per stat, fixed when the Pokémon is created.
//! - EVs: 0–255 per stat and 510 in all, 0 when the Pokémon is created
//!   (wild, gift, starter, in-game trade). Every Pokémon that gains
//!   experience for a defeated foe ("X gained N EXP. Points!") gains the
//!   foe species' whole EV yield (not a share of it, as experience is);
//!   a Macho Brace doubles it, Pokérus doubles it (the Pokérus spread is
//!   stubbed out in FireRed, so only traded-in Pokémon can carry it). A
//!   level-100 Pokémon gains none. Vitamins (HP UP, PROTEIN, IRON,
//!   CARBOS, CALCIUM, ZINC) add 10 while the stat has under 100.
//! - Stats are computed only on a level-up (after the defeat's EVs were
//!   added), an evolution, a vitamin, or a withdrawal from the PC: the
//!   EVs gained in between don't show until the next of those.
//!
//! Stat = ⌊(⌊(2·Base + IV + ⌊EV/4⌋)·Lv/100⌋ + 5)·Nature⌋ (Nature 0.9, 1 or
//! 1.1), HP = ⌊(2·Base + IV + ⌊EV/4⌋)·Lv/100⌋ + Lv + 10 (Shedinja: 1).
//!
//! Every reading of a stat at a known level and EV count narrows its IV to
//! those that give that value; readings at higher levels narrow it most
//! (at Lv 100 each IV point is a stat point). [`solve`] intersects them for
//! each of the 25 natures and keeps the natures every stat agrees with.

/// Stat indices, in the game's order (`Species::base`, `ev_yield`).
pub const HP: usize = 0;
pub const ATTACK: usize = 1;
pub const DEFENSE: usize = 2;
pub const SPEED: usize = 3;
pub const SP_ATTACK: usize = 4;
pub const SP_DEFENSE: usize = 5;

pub const MAX_IV: u8 = 31;
pub const MAX_STAT_EVS: u16 = 255;
pub const MAX_TOTAL_EVS: u16 = 510;
/// Vitamins raise a stat's EVs only while they are under this.
pub const VITAMIN_LIMIT: u16 = 100;
/// The most EVs one defeated species yields in one stat (and in all).
pub const MAX_YIELD: u16 = 3;

/// Natures in the game's order (`NATURE_HARDY` = 0), as printed.
pub const NATURES: [&str; 25] = [
    "HARDY", "LONELY", "BRAVE", "ADAMANT", "NAUGHTY", "BOLD", "DOCILE", "RELAXED", "IMPISH", "LAX",
    "TIMID", "HASTY", "SERIOUS", "JOLLY", "NAIVE", "MODEST", "MILD", "QUIET", "BASHFUL", "RASH",
    "CALM", "GENTLE", "SASSY", "CAREFUL", "QUIRKY",
];

/// Every nature, as a [`solve`] mask.
pub const ALL_NATURES: u32 = (1 << 25) - 1;
/// Every IV, as a mask (bit `i` = IV `i` possible).
pub const ALL_IVS: u32 = u32::MAX;

pub fn nature_named(name: &str) -> Option<u8> {
    NATURES.iter().position(|n| *n == name).map(|i| i as u8)
}

/// +1 if `nature` raises stat `index`, −1 if it lowers it, else 0. Nature
/// `n` raises stat `n / 5 + 1` and lowers `n % 5 + 1` (Attack, Defense,
/// Speed, Sp. Atk, Sp. Def); when they are the same it does neither.
pub fn nature_effect(nature: u8, index: usize) -> i8 {
    if index == HP || nature >= 25 {
        return 0;
    }
    let (up, down) = (usize::from(nature / 5) + 1, usize::from(nature % 5) + 1);
    match (up == down, index == up, index == down) {
        (true, _, _) => 0,
        (_, true, _) => 1,
        (_, _, true) => -1,
        _ => 0,
    }
}

/// One stat as the game computes it (Shedinja's HP is always 1: see
/// [`Sample::fixed_hp`]).
pub fn stat(base: u16, index: usize, level: u8, iv: u8, ev: u16, nature: u8) -> u16 {
    let l = u32::from(level);
    let core =
        (2 * u32::from(base) + u32::from(iv) + u32::from(ev.min(MAX_STAT_EVS)) / 4) * l / 100;
    if index == HP {
        return (core + l + 10) as u16;
    }
    let n = core + 5;
    (match nature_effect(nature, index) {
        1 => n * 110 / 100,
        -1 => n * 90 / 100,
        _ => n,
    }) as u16
}

/// Adds the EVs of one defeated Pokémon (`MonGainEVs`): the species' yield,
/// doubled by Pokérus and by a Macho Brace, stat by stat in the game's
/// order, capped at 255 per stat and 510 in all. Returns what was added.
pub fn gain_evs(evs: &mut [u16; 6], yields: [u8; 6], pokerus: bool, macho_brace: bool) -> [u16; 6] {
    let mut total: u16 = evs.iter().sum();
    let mut added = [0; 6];
    for i in 0..6 {
        if total >= MAX_TOTAL_EVS {
            break;
        }
        let mut gain = u16::from(yields[i]) * if pokerus { 2 } else { 1 };
        if macho_brace {
            gain *= 2;
        }
        gain = gain
            .min(MAX_TOTAL_EVS - total)
            .min(MAX_STAT_EVS.saturating_sub(evs[i]));
        evs[i] += gain;
        total += gain;
        added[i] = gain;
    }
    added
}

/// A vitamin's EVs for stat `index` (+10 up to 100, within the 510 total);
/// `None` when the game refuses it ("It won't have any effect.").
pub fn vitamin(evs: &mut [u16; 6], index: usize) -> Option<u16> {
    let total: u16 = evs.iter().sum();
    if total >= MAX_TOTAL_EVS || evs[index] >= VITAMIN_LIMIT {
        return None;
    }
    let gain = 10
        .min(VITAMIN_LIMIT - evs[index])
        .min(MAX_TOTAL_EVS - total);
    evs[index] += gain;
    Some(gain)
}

/// One reading of a Pokémon's stats: `stats` as shown (`None` = not read)
/// at `level`, computed from EVs somewhere in `ev_low..=ev_high` per stat
/// (equal when every defeat since the Pokémon was obtained is known).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The species' base stats (the species when read: evolving changes them).
    pub base: [u16; 6],
    /// Shedinja: HP is always 1 and says nothing about the IV.
    pub fixed_hp: bool,
    pub level: u8,
    pub stats: [Option<u16>; 6],
    pub ev_low: [u16; 6],
    pub ev_high: [u16; 6],
}

/// What the samples allow: bit `n` of `natures` for nature `n`, bit `i` of
/// `ivs[s]` for IV `i` of stat `s` under some allowed nature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub natures: u32,
    pub ivs: [u32; 6],
    /// Some IV and nature explain every reading with the EVs as given.
    /// When none does (an EV award or a reading was wrong), the estimate
    /// is solved again with the EVs' upper bounds dropped.
    pub consistent: bool,
}

impl Estimate {
    /// Lowest and highest possible IV of stat `index`.
    pub fn iv_range(&self, index: usize) -> Option<(u8, u8)> {
        let m = self.ivs[index];
        (m != 0).then(|| (m.trailing_zeros() as u8, 31 - m.leading_zeros() as u8))
    }

    /// The nature, when only one fits.
    pub fn nature(&self) -> Option<u8> {
        (self.natures.count_ones() == 1).then(|| self.natures.trailing_zeros() as u8)
    }

    pub fn nature_names(&self) -> Vec<&'static str> {
        (0..25)
            .filter(|n| self.natures & (1 << n) != 0)
            .map(|n| NATURES[n])
            .collect()
    }
}

/// IVs of stat `index` that give `value` under `nature` with EVs in
/// `low..=high` (as a mask). One more EV quarter raises ⌊(2·Base + IV +
/// ⌊EV/4⌋)·Lv/100⌋ by at most 1, so over the range it takes every value
/// between its ends; so does ×0.9 after it, while ×1.1 skips some.
fn fitting_ivs(s: &Sample, index: usize, value: u16, nature: u8) -> u32 {
    if index == HP && s.fixed_hp {
        return ALL_IVS;
    }
    let (low, high) = (
        u32::from(s.ev_low[index].min(MAX_STAT_EVS) / 4),
        u32::from(s.ev_high[index].min(MAX_STAT_EVS) / 4),
    );
    let (l, v) = (u32::from(s.level), u32::from(value));
    let effect = nature_effect(nature, index);
    let mut mask = 0;
    for iv in 0..=MAX_IV {
        let core = |q: u32| (2 * u32::from(s.base[index]) + u32::from(iv) + q) * l / 100;
        let (lo, hi) = (core(low), core(high.max(low)));
        let fits = if index == HP {
            (lo + l + 10..=hi + l + 10).contains(&v)
        } else {
            let (lo, hi) = (lo + 5, hi + 5);
            match effect {
                1 => {
                    // The least n with ⌊1.1·n⌋ ≥ value gives exactly it, or nothing does.
                    let n = (v * 100).div_ceil(110);
                    n * 110 / 100 == v && (lo..=hi).contains(&n)
                }
                -1 => (lo * 90 / 100..=hi * 90 / 100).contains(&v),
                _ => (lo..=hi).contains(&v),
            }
        };
        if fits {
            mask |= 1 << iv;
        }
    }
    mask
}

/// IV masks per stat under `nature` (`None` if a stat has no fitting IV).
fn solve_nature(samples: &[Sample], nature: u8) -> Option<[u32; 6]> {
    let mut ivs = [ALL_IVS; 6];
    for s in samples {
        for (index, value) in s.stats.iter().enumerate() {
            if let Some(value) = value {
                ivs[index] &= fitting_ivs(s, index, *value, nature);
            }
        }
    }
    ivs.iter().all(|m| *m != 0).then_some(ivs)
}

fn solve_with(samples: &[Sample], natures: u32) -> Option<Estimate> {
    let mut out = Estimate {
        natures: 0,
        ivs: [0; 6],
        consistent: true,
    };
    for n in (0..25u8).filter(|n| natures & (1 << n) != 0) {
        if let Some(ivs) = solve_nature(samples, n) {
            out.natures |= 1 << n;
            for (all, m) in out.ivs.iter_mut().zip(ivs) {
                *all |= m;
            }
        }
    }
    (out.natures != 0).then_some(out)
}

/// The IVs and natures that explain every sample. `natures` restricts the
/// natures (a read nature, or [`ALL_NATURES`]). When nothing explains the
/// samples with their EV ranges, the ranges are opened upwards (EVs
/// missed), then the readings are given up on (`None`).
pub fn solve(samples: &[Sample], natures: u32) -> Option<Estimate> {
    if let Some(e) = solve_with(samples, natures) {
        return Some(e);
    }
    let opened: Vec<Sample> = samples
        .iter()
        .map(|s| Sample {
            ev_high: [MAX_STAT_EVS; 6],
            ..s.clone()
        })
        .collect();
    solve_with(&opened, natures).map(|e| Estimate {
        consistent: false,
        ..e
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BULBASAUR: [u16; 6] = [45, 49, 49, 45, 65, 65];
    const IVYSAUR: [u16; 6] = [60, 62, 63, 60, 80, 80];

    fn sample(base: [u16; 6], level: u8, stats: [u16; 6], evs: [u16; 6]) -> Sample {
        Sample {
            base,
            fixed_hp: false,
            level,
            stats: stats.map(Some),
            ev_low: evs,
            ev_high: evs,
        }
    }

    #[test]
    fn natures_follow_the_game_table() {
        // LONELY: +Attack −Defense; BOLD: +Defense −Attack; LAX: +Defense
        // −Sp. Def; HARDY and SERIOUS change nothing.
        assert_eq!(nature_effect(1, ATTACK), 1);
        assert_eq!(nature_effect(1, DEFENSE), -1);
        assert_eq!(nature_effect(5, DEFENSE), 1);
        assert_eq!(nature_effect(5, ATTACK), -1);
        let lax = nature_named("LAX").unwrap();
        assert_eq!(
            (nature_effect(lax, DEFENSE), nature_effect(lax, SP_DEFENSE)),
            (1, -1)
        );
        for neutral in ["HARDY", "DOCILE", "SERIOUS", "BASHFUL", "QUIRKY"] {
            let n = nature_named(neutral).unwrap();
            assert!((0..6).all(|i| nature_effect(n, i) == 0), "{neutral}");
        }
        assert_eq!(nature_effect(3, HP), 0);
    }

    #[test]
    fn stats_follow_the_game_formula() {
        // Garchomp-like textbook case (Bulbapedia's worked example in Gen
        // III terms): base 108/130, Lv 78, IV 24/12, EV 74/190, Adamant.
        assert_eq!(stat(108, HP, 78, 24, 74, 3), 289);
        assert_eq!(stat(130, ATTACK, 78, 12, 190, 3), 278);
        // Bulbasaur Lv 5, IV 0: HP 19, Atk 9.
        assert_eq!(stat(45, HP, 5, 0, 0, 0), 19);
        assert_eq!(stat(49, ATTACK, 5, 0, 0, 0), 9);
    }

    #[test]
    fn evs_are_capped_per_stat_and_in_all() {
        let mut evs = [0; 6];
        assert_eq!(
            gain_evs(&mut evs, [0, 0, 0, 1, 0, 0], false, false),
            [0, 0, 0, 1, 0, 0]
        );
        assert_eq!(
            gain_evs(&mut evs, [0, 0, 0, 1, 0, 0], false, true),
            [0, 0, 0, 2, 0, 0]
        );
        let mut evs = [0, 0, 0, 254, 0, 0];
        gain_evs(&mut evs, [0, 0, 0, 3, 0, 0], false, false);
        assert_eq!(evs[SPEED], 255);
        // At 509 in all only one more EV fits, in the first yielding stat.
        let mut evs = [255, 254, 0, 0, 0, 0];
        assert_eq!(
            gain_evs(&mut evs, [0, 2, 1, 0, 0, 0], false, false),
            [0, 1, 0, 0, 0, 0]
        );
        assert_eq!(evs.iter().sum::<u16>(), 510);
        assert_eq!(gain_evs(&mut evs, [1; 6], true, true), [0; 6]);
    }

    #[test]
    fn vitamins_stop_at_one_hundred() {
        let mut evs = [0, 95, 0, 0, 0, 0];
        assert_eq!(vitamin(&mut evs, ATTACK), Some(5));
        assert_eq!(vitamin(&mut evs, ATTACK), None);
        assert_eq!(vitamin(&mut evs, HP), Some(10));
    }

    /// The emulator fixtures `emu-summary-info.png` / `emu-summary-skills.png`:
    /// IVYSAUR Lv 18, LAX nature, 54 HP, 33/30/34/37/32 (Atk/Def/Spe/SpA/SpD).
    #[test]
    fn a_read_nature_and_stats_bound_the_ivs() {
        let lax = nature_named("LAX").unwrap();
        let s = Sample {
            ev_low: [0; 6],
            ev_high: [MAX_STAT_EVS; 6],
            ..sample(IVYSAUR, 18, [54, 33, 30, 34, 37, 32], [0; 6])
        };
        let e = solve(std::slice::from_ref(&s), 1 << lax).unwrap();
        assert!(e.consistent);
        assert_eq!(e.nature(), Some(lax));
        for i in 0..6 {
            let (lo, hi) = e.iv_range(i).unwrap();
            assert!(lo <= hi && hi <= 31, "stat {i}: {lo}..={hi}");
        }
        // Unknown EVs only bound the IVs from above; with none, the true
        // IV set is the tightest.
        let exact = solve(
            &[sample(IVYSAUR, 18, [54, 33, 30, 34, 37, 32], [0; 6])],
            1 << lax,
        )
        .unwrap();
        for i in 0..6 {
            assert_eq!(exact.ivs[i] & !e.ivs[i], 0);
        }
    }

    /// A made-up Bulbasaur (IVs 20/9/27/14/31/3, BOLD) read every level
    /// from 6 to 30 while its EVs are counted: the stats pin every IV to
    /// a few values and the nature to one.
    #[test]
    fn readings_level_by_level_converge_on_the_ivs_and_nature() {
        let ivs = [20, 9, 27, 14, 31, 3];
        let bold = nature_named("BOLD").unwrap();
        let mut evs = [0u16; 6];
        let mut samples = Vec::new();
        for level in 6..=30u8 {
            // Two Pidgey (1 Speed) and a Rattata (1 Speed) per level.
            for _ in 0..3 {
                gain_evs(&mut evs, [0, 0, 0, 1, 0, 0], false, false);
            }
            let stats = std::array::from_fn(|i| stat(BULBASAUR[i], i, level, ivs[i], evs[i], bold));
            samples.push(sample(BULBASAUR, level, stats, evs));
        }
        let e = solve(&samples, ALL_NATURES).unwrap();
        assert!(e.consistent);
        assert_eq!(e.nature(), Some(bold));
        for (i, iv) in ivs.iter().enumerate() {
            let (lo, hi) = e.iv_range(i).unwrap();
            assert!((lo..=hi).contains(iv), "stat {i}: {lo}..={hi}");
            assert!(hi - lo <= 3, "stat {i}: {lo}..={hi}");
        }
    }

    /// The closed form against every EV in the range, for every nature
    /// effect and a spread of bases, levels and values.
    #[test]
    fn fitting_ivs_matches_trying_every_ev() {
        for (base, level) in [(45, 5), (130, 37), (255, 100), (5, 63), (80, 100)] {
            for nature in [0u8, 1, 5] {
                for (low, high) in [(0, 0), (0, 255), (37, 90), (252, 255)] {
                    let s = Sample {
                        base: [base; 6],
                        fixed_hp: false,
                        level,
                        stats: [None; 6],
                        ev_low: [low; 6],
                        ev_high: [high; 6],
                    };
                    for index in [HP, ATTACK, DEFENSE] {
                        for value in 1..=700u16 {
                            let brute = (0..=MAX_IV)
                                .filter(|&iv| {
                                    (low / 4..=high / 4).any(|q| {
                                        stat(base, index, level, iv, q * 4, nature) == value
                                    })
                                })
                                .fold(0u32, |m, iv| m | 1 << iv);
                            assert_eq!(
                                fitting_ivs(&s, index, value, nature),
                                brute,
                                "base {base} Lv {level} nature {nature} EVs {low}..={high} stat {index} = {value}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_wrong_ev_count_opens_the_ranges_instead_of_failing() {
        let adamant = nature_named("ADAMANT").unwrap();
        // 252 Attack EVs, but counted as none.
        let actual = std::array::from_fn(|i| {
            stat(
                BULBASAUR[i],
                i,
                50,
                31,
                if i == ATTACK { 252 } else { 0 },
                adamant,
            )
        });
        let e = solve(&[sample(BULBASAUR, 50, actual, [0; 6])], 1 << adamant).unwrap();
        assert!(!e.consistent);
        assert!(e.ivs[ATTACK] & (1 << 31) != 0);
    }
}
