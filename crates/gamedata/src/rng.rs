//! Generation III RNG as FireRed uses it (`src/random.c`), and what it
//! decides for a gift Pokémon such as the starter (`CreateBoxMon`).
//!
//! - The generator is the 32-bit LCG `s' = 0x41C64E6D·s + 0x6073`; each
//!   `Random()` returns the top 16 bits of the new state.
//! - The initial state is a 16-bit seed: Timer1 (the CPU clock, 16.78 MHz)
//!   counted from the title screen's set-up until the fade after Start/A
//!   (`SeedRngAndSetTrainerId`). One frame is 280 896 cycles, so a press one
//!   frame later moves the seed by 280 896 mod 65 536 = 18 752.
//! - The VBlank interrupt calls `Random()` once per frame; scripts, moving
//!   NPCs and menus add calls of their own.
//! - The starter is "method 1": `PID = r1 | r2 << 16`, then
//!   `r3 = HP | Atk << 5 | Def << 10` and `r4 = Spe | SpA << 5 | SpD << 10`.
//!   It's shiny when `TID ^ SID ^ PIDhi ^ PIDlo < 8`.
//!
//! An *advance* counts `Random()` calls since the seed: at advance `a` the
//! state is `a` steps past the seed and the next four calls make the Pokémon.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::training::stat;

pub const MULTIPLIER: u32 = 0x41C6_4E6D;
pub const INCREMENT: u32 = 0x6073;
/// GBA CPU cycles per frame (228 lines × 1232 cycles).
pub const CYCLES_PER_FRAME: u32 = 280_896;
/// Shiny when the XOR of both IDs and both PID halves is below this.
pub const SHINY_ODDS: u16 = 8;

#[inline]
pub fn next(state: u32) -> u32 {
    state.wrapping_mul(MULTIPLIER).wrapping_add(INCREMENT)
}

#[inline]
fn high(state: u32) -> u16 {
    (state >> 16) as u16
}

/// The state `steps` calls after `state`, in O(log steps).
pub fn jump(state: u32, steps: u64) -> u32 {
    // Compose the affine map x → m·x + c with itself (square-and-multiply).
    let (mut m, mut c) = (MULTIPLIER, INCREMENT);
    let (mut acc_m, mut acc_c) = (1u32, 0u32);
    let mut n = steps;
    while n > 0 {
        if n & 1 == 1 {
            acc_m = acc_m.wrapping_mul(m);
            acc_c = acc_c.wrapping_mul(m).wrapping_add(c);
        }
        c = c.wrapping_mul(m).wrapping_add(c);
        m = m.wrapping_mul(m);
        n >>= 1;
    }
    acc_m.wrapping_mul(state).wrapping_add(acc_c)
}

/// The seed a title press `frames` later (earlier if negative) would give,
/// when the timer runs at exactly one frame per 280 896 cycles. An emulator
/// that counts cycles exactly follows this; others show it by calibration.
pub fn seed_after_frames(seed: u16, frames: i64) -> u16 {
    let step = i64::from(CYCLES_PER_FRAME % 65_536);
    (i64::from(seed) + frames * step).rem_euclid(65_536) as u16
}

/// A Pokémon as method 1 makes it. IVs in the stat order of
/// [`crate::Species::base`]: HP, Atk, Def, Spe, SpA, SpD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Method1Mon {
    pub pid: u32,
    pub ivs: [u8; 6],
}

impl Method1Mon {
    /// Generated from the state at some advance (the next four calls).
    pub fn from_state(state: u32) -> Self {
        let s1 = next(state);
        let s2 = next(s1);
        let s3 = next(s2);
        let s4 = next(s3);
        Self::from_calls([high(s1), high(s2), high(s3), high(s4)])
    }

    fn from_calls(r: [u16; 4]) -> Self {
        let iv = |word: u16, shift: u16| ((word >> shift) & 31) as u8;
        Self {
            pid: u32::from(r[0]) | (u32::from(r[1]) << 16),
            ivs: [
                iv(r[2], 0),
                iv(r[2], 5),
                iv(r[2], 10),
                iv(r[3], 0),
                iv(r[3], 5),
                iv(r[3], 10),
            ],
        }
    }

    pub fn at(seed: u16, advance: u64) -> Self {
        Self::from_state(jump(u32::from(seed), advance))
    }

    pub fn nature(&self) -> u8 {
        (self.pid % 25) as u8
    }

    pub fn shiny_value(&self, ids: TrainerIds) -> u16 {
        ids.tid ^ ids.sid ^ (self.pid >> 16) as u16 ^ (self.pid & 0xFFFF) as u16
    }

    pub fn is_shiny(&self, ids: TrainerIds) -> bool {
        self.shiny_value(ids) < SHINY_ODDS
    }

    /// What its summary shows at `level` (no EVs).
    pub fn reading(&self, base: &[u16; 6], level: u8) -> Reading {
        let nature = self.nature();
        let mut stats = [0; 6];
        for (i, s) in stats.iter_mut().enumerate() {
            *s = stat(base[i], i, level, self.ivs[i], 0, nature);
        }
        Reading { nature, stats }
    }
}

/// The trainer's visible ID and the secret ID (both in the save file; only
/// the TID is shown in game).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainerIds {
    pub tid: u16,
    pub sid: u16,
}

/// The nature and the six stats (max HP first) read off the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    pub nature: u8,
    pub stats: [u16; 6],
}

impl Reading {
    /// For each stat, the IVs that print this value (bit `iv` set). `None`
    /// when a stat can't come from any IV: a misread, or another species.
    pub fn iv_masks(&self, base: &[u16; 6], level: u8) -> Option<[u32; 6]> {
        let mut masks = [0u32; 6];
        for (i, mask) in masks.iter_mut().enumerate() {
            for iv in 0..32u8 {
                if stat(base[i], i, level, iv, 0, self.nature) == self.stats[i] {
                    *mask |= 1 << iv;
                }
            }
            if *mask == 0 {
                return None;
            }
        }
        Some(masks)
    }
}

/// One (seed, advance) that makes the observed Pokémon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hit {
    pub seed: u16,
    pub advance: u32,
}

/// Every `(seed, advance)` with `seed` in `seeds` and `advance` in
/// `advances` that generates a Pokémon printing `reading` at `level`.
/// Brute force over the states, split across threads: the whole 16-bit seed
/// space over 10 000 advances takes about a second.
pub fn search(
    reading: &Reading,
    base: &[u16; 6],
    level: u8,
    seeds: &[u16],
    advances: Range<u32>,
) -> Vec<Hit> {
    let Some(masks) = reading.iv_masks(base, level) else {
        return Vec::new();
    };
    let ok = |word: u16, first: usize| {
        (0..3).all(|k| masks[first + k] & (1 << ((word >> (5 * k)) & 31)) != 0)
    };
    let scan = |seed: u16, out: &mut Vec<Hit>| {
        // r[i] is the (i+1)-th call after the current advance.
        let mut r = [0u16; 4];
        let mut s = jump(u32::from(seed), u64::from(advances.start));
        for slot in &mut r {
            s = next(s);
            *slot = high(s);
        }
        for advance in advances.clone() {
            if ok(r[2], 0)
                && ok(r[3], 3)
                && (u32::from(r[0]) | u32::from(r[1]) << 16) % 25 == u32::from(reading.nature)
            {
                out.push(Hit { seed, advance });
            }
            s = next(s);
            r = [r[1], r[2], r[3], high(s)];
        }
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let chunk = seeds.len().div_ceil(threads).max(1);
    let mut hits: Vec<Hit> = std::thread::scope(|scope| {
        let workers: Vec<_> = seeds
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    let mut out = Vec::new();
                    for &seed in part {
                        scan(seed, &mut out);
                    }
                    out
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("search worker"))
            .collect()
    });
    hits.sort_unstable();
    hits
}

/// Every 16-bit seed, for [`search`] when nothing narrows them.
pub fn all_seeds() -> Vec<u16> {
    (0..=u16::MAX).collect()
}

/// Advances in `advances` where `seed` gives a shiny for `ids`.
pub fn shiny_advances(seed: u16, advances: Range<u32>, ids: TrainerIds) -> Vec<u32> {
    let mut r = [0u16; 2];
    let mut t = jump(u32::from(seed), u64::from(advances.start));
    for slot in &mut r {
        t = next(t);
        *slot = high(t);
    }
    let mut out = Vec::new();
    for advance in advances {
        if ids.tid ^ ids.sid ^ r[0] ^ r[1] < SHINY_ODDS {
            out.push(advance);
        }
        t = next(t);
        r = [r[1], high(t)];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::training::nature_named;

    const CHARMANDER: [u16; 6] = [39, 52, 43, 65, 60, 50];

    fn named(nature: &str, stats: [u16; 6]) -> Reading {
        Reading {
            nature: nature_named(nature).unwrap(),
            stats,
        }
    }

    /// Measured on mGBA: four power-on attempts with the same title press
    /// and the last A 0, 1, 2 and 7 frames apart read these starters; only
    /// seed C53A explains them, at advances 1316 + offset.
    #[test]
    fn four_readings_pin_down_the_emulators_seed() {
        let readings = [
            (0, named("RASH", [18, 10, 10, 12, 13, 9])),
            (1, named("BASHFUL", [20, 11, 9, 12, 11, 10])),
            (2, named("CALM", [19, 9, 9, 12, 11, 11])),
            (7, named("CAREFUL", [20, 10, 10, 12, 10, 12])),
        ];
        let mut hits = search(&readings[0].1, &CHARMANDER, 5, &all_seeds(), 1200..1450);
        for (offset, reading) in &readings[1..] {
            hits.retain(|h| {
                Method1Mon::at(h.seed, u64::from(h.advance + offset)).reading(&CHARMANDER, 5)
                    == *reading
            });
        }
        assert_eq!(
            hits,
            vec![Hit {
                seed: 0xC53A,
                advance: 1316
            }]
        );
    }

    /// The emulator hunt's shiny: aimed at seed AD63, advance 2053, for TID
    /// 16808 / SID 48577; the summary read a NAUGHTY CHARMANDER with these
    /// stats and the star.
    #[test]
    fn the_emulator_hunts_shiny_is_what_the_model_says() {
        let mon = Method1Mon::at(0xAD63, 2053);
        assert!(mon.is_shiny(TrainerIds {
            tid: 16808,
            sid: 48577
        }));
        assert_eq!(
            mon.reading(&CHARMANDER, 5),
            named("NAUGHTY", [19, 12, 10, 11, 11, 9])
        );
    }

    #[test]
    fn jump_matches_stepping() {
        let mut s = 0x1234u32;
        for n in 0..2000u64 {
            assert_eq!(jump(0x1234, n), s, "n={n}");
            s = next(s);
        }
    }

    #[test]
    fn the_first_calls_from_seed_zero() {
        // 0 → 0x00006073 → 0xE97E7B6A: Random() returns 0x0000, then 0xE97E.
        assert_eq!(next(0), 0x6073);
        assert_eq!(next(0x6073), 0xE97E_7B6A);
        let mon = Method1Mon::at(0, 0);
        assert_eq!(mon.pid & 0xFFFF, 0);
        assert_eq!(mon.pid >> 16, 0xE97E);
    }

    #[test]
    fn search_finds_the_generating_frame() {
        let (seed, advance) = (0xBEEF, 1234);
        let reading = Method1Mon::at(seed, advance).reading(&CHARMANDER, 5);
        let hits = search(&reading, &CHARMANDER, 5, &[seed, seed ^ 1], 1000..1500);
        assert!(hits.contains(&Hit {
            seed,
            advance: advance as u32
        }));
        // A different reading at the same frame isn't matched there.
        let mut other = reading;
        other.nature = (other.nature + 1) % 25;
        assert!(!search(&other, &CHARMANDER, 5, &[seed], 1234..1235)
            .iter()
            .any(|h| h.advance == 1234));
    }

    #[test]
    fn shiny_advances_are_shiny() {
        let ids = TrainerIds {
            tid: 12345,
            sid: 54321,
        };
        let found = shiny_advances(0x4321, 0..40_000, ids);
        assert!(!found.is_empty());
        for &a in &found {
            assert!(Method1Mon::at(0x4321, u64::from(a)).is_shiny(ids));
        }
        // And nothing in between is.
        let first = found[0];
        assert!((0..first).all(|a| !Method1Mon::at(0x4321, u64::from(a)).is_shiny(ids)));
    }

    #[test]
    fn a_frame_later_moves_the_seed_by_18752() {
        assert_eq!(seed_after_frames(0, 1), 18_752);
        assert_eq!(seed_after_frames(18_752, -1), 0);
        assert_eq!(seed_after_frames(0, 1024), 0);
    }
}
