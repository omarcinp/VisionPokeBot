//! Shiny starter hunt, the thinking half: what each attempt showed, which
//! (seed, advance) made it, and which timing to try next.
//!
//! An attempt is two numbers of frames ([`Timing`]): from the reset to the
//! title-screen press (`title`: it picks the seed) and from that press to
//! the A that closes "This POKéMON is really quite energetic!" (`last`: the
//! starter is made right after it, so it picks the advance). The summary
//! then shows a [`Reading`] (nature and stats) that pins down the
//! (seed, advance) the attempt hit.
//!
//! What was measured on mGBA (hard resets, stepped):
//! - **Advances.** `advance = last + c` with `c` the same for every seed
//!   (−103 or −104): the RNG runs once a frame from the seed on, and the lab's
//!   wandering aides stand off screen (despawned, they call nothing).
//! - **Seeds.** One `title` always gives the same seed, but seeds don't
//!   follow the timer's 18 752-per-frame line from one `title` to another:
//!   the read lands a variable number of cycles (and sometimes a frame)
//!   later, as the title screen's flames vary its work. Each `title` is
//!   learned: with `c` known one reading leaves about ten seeds, a second
//!   reading at the same `title` leaves one.
//!
//! So the plan is: a known `title` with a shiny frame in reach → aim at it;
//! a `title` with one reading → read it again, aiming at a shiny frame of
//! one of its candidate seeds if any has one; else a new `title`.
//!
//! Real hardware lands presses a frame or two off (`jitter`); this module
//! only widens its searches by that much. Characterizing the Switch comes
//! first (see `docs/shiny-starter.md`).

use std::collections::BTreeSet;

use pokebot_gamedata::rng::{self, Hit, Method1Mon, Reading, TrainerIds};
use serde::{Deserialize, Serialize};

/// Frames from the reset to the title press (`title`) and from the title
/// press to the last A (`last`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timing {
    pub title: u32,
    pub last: u32,
}

/// What one attempt showed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sighting {
    pub timing: Timing,
    /// Nature and stats; `None` when the summary couldn't be read.
    pub reading: Option<Reading>,
    pub shiny: bool,
    /// The (seed, advance) that made it, once identified.
    #[serde(default)]
    pub hit: Option<Hit>,
}

impl Sighting {
    fn offset(&self) -> Option<i64> {
        Some(i64::from(self.hit?.advance) - i64::from(self.timing.last))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HuntConfig {
    /// Needed to aim at shiny frames; without them timings are spread
    /// blindly (full odds).
    pub ids: Option<TrainerIds>,
    /// The starter's base stats and level (5).
    pub base: [u16; 6],
    pub level: u8,
    /// Frames a press may land off its target (0 on the stepped emulator).
    pub jitter: u32,
    /// Allowed `Timing::title` (the title screen must be in its RUN state).
    pub title_min: u32,
    pub title_max: u32,
    /// Allowed `Timing::last` (the text must have printed by `last_min`).
    pub last_min: u32,
    pub last_max: u32,
}

/// Where to look for the first attempts' advances, relative to `last`: the
/// seed is read ~110 frames after the title press.
const BOOT_WINDOW: (i64, i64) = (-240, 400);
/// Advances a reading may sit off `last + c` (one frame of timer-read
/// slip, plus the jitter).
const OFFSET_SLACK: i64 = 2;
/// `last` offsets of the bootstrap attempts, all at one `title`.
const BOOT_STEPS: [u32; 6] = [0, 1, 17, 41, 3, 29];
/// Frames between the `title`s tried when a new one is needed.
const TITLE_STEP: u32 = 7;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunt {
    pub config: HuntConfig,
    pub sightings: Vec<Sighting>,
}

/// The next timing to try, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub timing: Timing,
    pub why: String,
    /// The shiny (seed, advance) aimed at, if any.
    pub target: Option<Hit>,
}

impl Hunt {
    pub fn new(config: HuntConfig) -> Self {
        Self {
            config,
            sightings: Vec::new(),
        }
    }

    /// `c = advance − last`, the most common among identified attempts.
    pub fn offset(&self) -> Option<i64> {
        let mut counts = std::collections::BTreeMap::new();
        for c in self.sightings.iter().filter_map(Sighting::offset) {
            *counts.entry(c).or_insert(0u32) += 1;
        }
        counts
            .into_iter()
            .max_by_key(|&(c, n)| (n, std::cmp::Reverse(c)))
            .map(|(c, _)| c)
    }

    /// The seed `title` gave, once an attempt there is identified.
    pub fn title_seed(&self, title: u32) -> Option<(u16, i64)> {
        self.sightings
            .iter()
            .rev()
            .filter(|s| s.timing.title == title)
            .find_map(|s| Some((s.hit?.seed, s.offset()?)))
    }

    fn search(&self, reading: &Reading, seeds: &[u16], from: i64, to: i64) -> Vec<Hit> {
        let (from, to) = (from.max(0) as u32, to.max(0) as u32);
        rng::search(
            reading,
            &self.config.base,
            self.config.level,
            seeds,
            from..to.max(from),
        )
    }

    fn slack(&self) -> i64 {
        OFFSET_SLACK + i64::from(self.config.jitter)
    }

    /// Seeds (with their advance) that explain every unidentified reading at
    /// `title`, given the offset `c`.
    fn title_candidates(&self, title: u32, c: i64) -> Vec<Hit> {
        let pending: Vec<&Sighting> = self
            .sightings
            .iter()
            .filter(|s| s.timing.title == title && s.hit.is_none() && s.reading.is_some())
            .collect();
        let Some((first, rest)) = pending.split_first() else {
            return Vec::new();
        };
        let slack = self.slack();
        let at = |s: &Sighting| i64::from(s.timing.last) + c;
        let mut hits = self.search(
            first.reading.as_ref().expect("filtered"),
            &rng::all_seeds(),
            at(first) - slack,
            at(first) + slack + 1,
        );
        for s in rest {
            let seeds: Vec<u16> = hits
                .iter()
                .map(|h| h.seed)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let theirs = self.search(
                s.reading.as_ref().expect("filtered"),
                &seeds,
                at(s) - slack,
                at(s) + slack + 1,
            );
            // Same seed, and the same offset (to the jitter).
            hits.retain(|h| {
                let ch = i64::from(h.advance) - i64::from(first.timing.last);
                theirs.iter().any(|o| {
                    o.seed == h.seed
                        && (i64::from(o.advance) - i64::from(s.timing.last) - ch).abs()
                            <= i64::from(self.config.jitter)
                })
            });
        }
        hits
    }

    /// Records an attempt, then identifies what can be. Returns its hit.
    pub fn record(&mut self, sighting: Sighting) -> Option<Hit> {
        self.sightings.push(sighting);
        self.identify();
        self.sightings.last().and_then(|s| s.hit)
    }

    fn identify(&mut self) {
        let Some(c) = self.offset() else {
            self.bootstrap();
            return;
        };
        let slack = self.slack();
        let titles: BTreeSet<u32> = self
            .sightings
            .iter()
            .filter(|s| s.hit.is_none() && s.reading.is_some())
            .map(|s| s.timing.title)
            .collect();
        for title in titles {
            if let Some((seed, c_title)) = self.title_seed(title) {
                for i in 0..self.sightings.len() {
                    let s = &self.sightings[i];
                    if s.timing.title != title || s.hit.is_some() {
                        continue;
                    }
                    let Some(reading) = s.reading.as_ref() else {
                        continue;
                    };
                    let at = i64::from(s.timing.last) + c_title;
                    let hit = self
                        .search(reading, &[seed], at - slack, at + slack + 1)
                        .into_iter()
                        .min_by_key(|h| (i64::from(h.advance) - at).abs());
                    self.sightings[i].hit = hit;
                }
                continue;
            }
            let hits = self.title_candidates(title, c);
            let seeds: BTreeSet<u16> = hits.iter().map(|h| h.seed).collect();
            let readings = self
                .sightings
                .iter()
                .filter(|s| s.timing.title == title && s.hit.is_none() && s.reading.is_some())
                .count();
            // One reading leaves ~10 seeds: only a lone candidate counts.
            if seeds.len() == 1 && (readings >= 2 || hits.len() == 1) {
                let seed = *seeds.iter().next().expect("one");
                let c_title = i64::from(hits[0].advance)
                    - i64::from(
                        self.sightings
                            .iter()
                            .find(|s| s.timing.title == title && s.hit.is_none())
                            .expect("pending")
                            .timing
                            .last,
                    );
                for s in self.sightings.iter_mut() {
                    if s.timing.title != title || s.hit.is_some() || s.reading.is_none() {
                        continue;
                    }
                    let advance = i64::from(s.timing.last) + c_title;
                    s.hit = Some(Hit {
                        seed,
                        advance: advance.max(0) as u32,
                    });
                }
            }
        }
    }

    /// Without any identified attempt, `c` is unknown: attempts at one
    /// `title` are searched over a wide window, and a seed whose offsets
    /// agree for all of them is the answer.
    fn bootstrap(&mut self) {
        let Some(title) = self
            .sightings
            .iter()
            .find(|s| s.reading.is_some())
            .map(|s| s.timing.title)
        else {
            return;
        };
        let pending: Vec<usize> = (0..self.sightings.len())
            .filter(|&i| {
                let s = &self.sightings[i];
                s.timing.title == title && s.reading.is_some() && s.hit.is_none()
            })
            .collect();
        if pending.len() < 2 {
            return;
        }
        let jitter = i64::from(self.config.jitter);
        let all = rng::all_seeds();
        let first = &self.sightings[pending[0]];
        let last0 = i64::from(first.timing.last);
        let mut hits = self.search(
            first.reading.as_ref().expect("filtered"),
            &all,
            last0 + BOOT_WINDOW.0,
            last0 + BOOT_WINDOW.1,
        );
        for &i in &pending[1..] {
            let s = &self.sightings[i];
            let seeds: Vec<u16> = hits
                .iter()
                .map(|h| h.seed)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let last = i64::from(s.timing.last);
            let theirs = self.search(
                s.reading.as_ref().expect("filtered"),
                &seeds,
                last + BOOT_WINDOW.0,
                last + BOOT_WINDOW.1,
            );
            hits.retain(|h| {
                let c = i64::from(h.advance) - last0;
                theirs
                    .iter()
                    .any(|o| o.seed == h.seed && (i64::from(o.advance) - last - c).abs() <= jitter)
            });
        }
        if let [hit] = hits[..] {
            let c = i64::from(hit.advance) - last0;
            for &i in &pending {
                let advance = i64::from(self.sightings[i].timing.last) + c;
                self.sightings[i].hit = Some(Hit {
                    seed: hit.seed,
                    advance: advance.max(0) as u32,
                });
            }
        }
    }

    fn tried(&self, timing: Timing) -> bool {
        self.sightings.iter().any(|s| s.timing == timing)
    }

    /// The first shiny frame of `seed` in reach, as `(last, advance)`.
    fn shiny_last(&self, seed: u16, c: i64, ids: TrainerIds) -> Option<(u32, u32)> {
        let cfg = &self.config;
        let from = (i64::from(cfg.last_min) + c).max(0) as u32;
        let to = (i64::from(cfg.last_max) + c).max(0) as u32;
        rng::shiny_advances(seed, from..to, ids)
            .into_iter()
            .map(|a| ((i64::from(a) - c) as u32, a))
            .next()
    }

    fn aimed(&self, timing: Timing, seed: u16, advance: u32, why: String) -> Plan {
        Plan {
            timing,
            why: format!(
                "{why}: seed {seed:04X} is shiny at advance {advance} (PID {:08X})",
                Method1Mon::at(seed, u64::from(advance)).pid
            ),
            target: Some(Hit { seed, advance }),
        }
    }

    /// The next timing (see the module docs).
    pub fn next_plan(&self) -> Plan {
        let cfg = &self.config;
        let Some(ids) = cfg.ids else {
            return self.blind_plan();
        };
        let Some(c) = self.offset() else {
            let n = self
                .sightings
                .iter()
                .filter(|s| s.timing.title == cfg.title_min)
                .count();
            let step = BOOT_STEPS[n % BOOT_STEPS.len()] + 60 * (n / BOOT_STEPS.len()) as u32;
            return Plan {
                timing: Timing {
                    title: cfg.title_min,
                    last: (cfg.last_min + step).min(cfg.last_max),
                },
                why: format!("calibrating: reading {} at one title timing", n + 1),
                target: None,
            };
        };
        let titles: BTreeSet<u32> = self.sightings.iter().map(|s| s.timing.title).collect();
        // 1. A known seed with a shiny frame in reach.
        let exact = titles
            .iter()
            .filter_map(|&title| {
                let (seed, c_title) = self.title_seed(title)?;
                let (last, advance) = self.shiny_last(seed, c_title, ids)?;
                let timing = Timing { title, last };
                (!self.tried(timing)).then_some((timing, seed, advance))
            })
            .min_by_key(|(t, _, _)| t.title + t.last);
        if let Some((timing, seed, advance)) = exact {
            return self.aimed(timing, seed, advance, "exact".into());
        }
        // 2. A title read once: read it again, at a candidate's shiny frame
        //    if one has it.
        for &title in &titles {
            if self.title_seed(title).is_some() {
                continue;
            }
            let hits = self.title_candidates(title, c);
            if hits.is_empty() {
                continue;
            }
            let seeds: BTreeSet<u16> = hits.iter().map(|h| h.seed).collect();
            let aim = seeds
                .iter()
                .filter_map(|&seed| {
                    let h = hits.iter().find(|h| h.seed == seed)?;
                    let first_last = self
                        .sightings
                        .iter()
                        .find(|s| s.timing.title == title && s.hit.is_none())?
                        .timing
                        .last;
                    let c_seed = i64::from(h.advance) - i64::from(first_last);
                    let (last, advance) = self.shiny_last(seed, c_seed, ids)?;
                    let timing = Timing { title, last };
                    (!self.tried(timing)).then_some((timing, seed, advance))
                })
                .min_by_key(|(t, _, _)| t.last);
            if let Some((timing, seed, advance)) = aim {
                return self.aimed(
                    timing,
                    seed,
                    advance,
                    format!("one of {} candidate seeds", seeds.len()),
                );
            }
            let used = self
                .sightings
                .iter()
                .filter(|s| s.timing.title == title)
                .map(|s| s.timing.last)
                .max()
                .unwrap_or(cfg.last_min);
            return Plan {
                timing: Timing {
                    title,
                    last: (used + 1).min(cfg.last_max),
                },
                why: format!(
                    "identifying title {title}: {} candidate seeds, none shiny in reach",
                    seeds.len()
                ),
                target: None,
            };
        }
        // 3. A new title.
        let title = (0..)
            .map(|k| cfg.title_min + k * TITLE_STEP)
            .take_while(|&t| t <= cfg.title_max)
            .find(|t| !titles.contains(t))
            .unwrap_or(cfg.title_max);
        Plan {
            timing: Timing {
                title,
                last: cfg.last_min,
            },
            why: format!("new title {title}: its seed is unknown"),
            target: None,
        }
    }

    /// Timings spread over the allowed ranges (a fixed pseudo-random walk:
    /// every attempt a different seed and advance).
    fn blind_plan(&self) -> Plan {
        let cfg = &self.config;
        let n = self.sightings.len() as u32;
        let state = rng::jump(0x5EED, u64::from(n) * 2);
        let span = |lo: u32, hi: u32, r: u32| lo + r % (hi - lo + 1).max(1);
        Plan {
            timing: Timing {
                title: span(cfg.title_min, cfg.title_max, state >> 16),
                last: span(cfg.last_min, cfg.last_max, rng::next(state) >> 16),
            },
            why: "full odds: no trainer IDs".into(),
            target: None,
        }
    }

    /// The shiny attempt, if any.
    pub fn found(&self) -> Option<&Sighting> {
        self.sightings.iter().find(|s| s.shiny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHARMANDER: [u16; 6] = [39, 52, 43, 65, 60, 50];

    /// A pretend console: each title an arbitrary seed (as mGBA's are);
    /// advance = last + c.
    struct Console {
        c: i64,
    }

    impl Console {
        fn seed(title: u32) -> u16 {
            (rng::jump(title * 7919, 3) >> 16) as u16
        }

        fn attempt(&self, t: Timing, ids: TrainerIds) -> Sighting {
            let seed = Self::seed(t.title);
            let mon = Method1Mon::at(seed, (i64::from(t.last) + self.c) as u64);
            Sighting {
                timing: t,
                reading: Some(mon.reading(&CHARMANDER, 5)),
                shiny: mon.is_shiny(ids),
                hit: None,
            }
        }
    }

    fn config(ids: TrainerIds) -> HuntConfig {
        HuntConfig {
            ids: Some(ids),
            base: CHARMANDER,
            level: 5,
            jitter: 0,
            title_min: 64,
            title_max: 2464,
            last_min: 1200,
            last_max: 4800,
        }
    }

    #[test]
    fn a_stepped_console_is_found_shiny_in_a_few_attempts() {
        let ids = TrainerIds {
            tid: 16808,
            sid: 48577,
        };
        let console = Console { c: -103 };
        let mut hunt = Hunt::new(config(ids));
        for attempt in 1..=40 {
            let plan = hunt.next_plan();
            assert!(!hunt.tried(plan.timing), "{plan:?} was already played");
            let sighting = console.attempt(plan.timing, ids);
            let shiny = sighting.shiny;
            hunt.record(sighting);
            if shiny {
                assert!(attempt <= 20, "took {attempt} attempts");
                return;
            }
        }
        panic!("no shiny in 40 attempts");
    }

    #[test]
    fn identified_hits_are_the_real_frames() {
        let ids = TrainerIds { tid: 1, sid: 2 };
        let console = Console { c: -120 };
        let mut hunt = Hunt::new(config(ids));
        for _ in 0..8 {
            let plan = hunt.next_plan();
            hunt.record(console.attempt(plan.timing, ids));
        }
        let identified: Vec<_> = hunt.sightings.iter().filter(|s| s.hit.is_some()).collect();
        assert!(identified.len() >= 4, "{:?}", hunt.sightings);
        for s in identified {
            assert_eq!(
                s.hit,
                Some(Hit {
                    seed: Console::seed(s.timing.title),
                    advance: s.timing.last - 120
                })
            );
        }
        assert_eq!(hunt.offset(), Some(-120));
    }

    #[test]
    fn without_ids_timings_are_spread() {
        let mut cfg = config(TrainerIds { tid: 0, sid: 0 });
        cfg.ids = None;
        let mut hunt = Hunt::new(cfg.clone());
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..20 {
            let t = hunt.next_plan().timing;
            assert!((cfg.title_min..=cfg.title_max).contains(&t.title));
            assert!((cfg.last_min..=cfg.last_max).contains(&t.last));
            seen.insert((t.title, t.last));
            hunt.sightings.push(Sighting {
                timing: t,
                reading: None,
                shiny: false,
                hit: None,
            });
        }
        assert_eq!(seen.len(), 20);
    }
}
