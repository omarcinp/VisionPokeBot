//! One walking leg: holds along straight runs, taps for single tiles, and
//! predictive tracking of a hold while it runs.

use std::time::{Duration, Instant};

use pokebot_state::{Direction, Observation, PlayerPose};
use pokebot_world::path::Step;

use super::{InputKind, Syncer};
use crate::nav::straight_run;
use crate::{Action, Expectation, Outcome};

/// Shortest time the located pose may lag the prediction before a hold is
/// cancelled (the spread of the model when that is larger).
const STALL_MIN_MS: f64 = 100.0;

/// What the walker wants next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkStep {
    /// Hold `dir` for `duration` to walk `tiles` tiles, ending on `target`.
    Hold {
        dir: Direction,
        tiles: usize,
        target: PlayerPose,
        duration: Duration,
        timeout_frames: u64,
    },
    /// Tap `dir` once to step onto `to` (the player already faces `dir`).
    Tap {
        dir: Direction,
        to: (i32, i32),
        timeout_frames: u64,
    },
    /// Turn to face `dir` before stepping: a short press in a new
    /// direction only turns the player, so the step needs its own press.
    Turn {
        dir: Direction,
        timeout_frames: u64,
    },
    /// The active hold is stalling: release the buttons and replan.
    Cancel,
    Arrived,
    /// The active hold is going as predicted (or the player isn't located).
    Walking {
        predicted: usize,
        observed: Option<usize>,
    },
}

/// Where a hold's prediction and the located pose stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    OnTrack {
        predicted: usize,
        observed: Option<usize>,
    },
    /// The pose lagged the prediction by more than a tile for too long.
    Stalled { predicted: usize, observed: usize },
}

/// Predicts the player's tile during a hold and notices when the located
/// pose falls behind it (blocked, or slower than modelled).
#[derive(Debug, Clone)]
pub struct HoldTracker {
    from: PlayerPose,
    tiles: usize,
    latency_ms: f64,
    unit_ms: f64,
    stall_after_ms: f64,
    /// Since when (ms after issue) the pose has lagged by more than a tile.
    lag_since_ms: Option<f64>,
}

impl HoldTracker {
    /// A hold of `tiles` tiles issued from `from`, judged by `sync`'s
    /// estimate of a walking tile.
    pub fn new(from: PlayerPose, tiles: usize, sync: &Syncer) -> Self {
        Self::with_turn(from, tiles, sync, 0.0)
    }

    /// As [`HoldTracker::new`], for a hold that first turns the player
    /// (`turn_ms` before the first tile).
    pub fn with_turn(from: PlayerPose, tiles: usize, sync: &Syncer, turn_ms: f64) -> Self {
        let e = sync.estimate(InputKind::WalkTile);
        Self {
            from,
            tiles,
            latency_ms: e.latency_ms + turn_ms,
            unit_ms: e.unit_ms.max(1.0),
            stall_after_ms: e.spread_ms.max(STALL_MIN_MS),
            lag_since_ms: None,
        }
    }

    /// Tiles the game should have finished `elapsed_ms` after the issue.
    pub fn predicted(&self, elapsed_ms: f64) -> usize {
        let done = ((elapsed_ms - self.latency_ms) / self.unit_ms).floor();
        (done.max(0.0) as usize).min(self.tiles)
    }

    /// Tiles the located `pose` is from the start (a straight run), if it
    /// is on the same map.
    pub fn observed(&self, pose: &PlayerPose) -> Option<usize> {
        (pose.map == self.from.map)
            .then(|| ((pose.x - self.from.x).abs() + (pose.y - self.from.y).abs()) as usize)
    }

    /// One frame `elapsed_ms` after the issue, with the located pose if any.
    pub fn track(&mut self, elapsed_ms: f64, pose: Option<&PlayerPose>) -> Track {
        let predicted = self.predicted(elapsed_ms);
        let observed = pose.and_then(|p| self.observed(p));
        let Some(seen) = observed else {
            // No evidence either way: an unlocated moving sprite is normal.
            return Track::OnTrack {
                predicted,
                observed,
            };
        };
        if predicted > seen + 1 {
            let since = *self.lag_since_ms.get_or_insert(elapsed_ms);
            if elapsed_ms - since >= self.stall_after_ms {
                return Track::Stalled {
                    predicted,
                    observed: seen,
                };
            }
        } else {
            self.lag_since_ms = None;
        }
        Track::OnTrack {
            predicted,
            observed,
        }
    }
}

/// What a finished step tells the navigator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepDone {
    /// The direction that was pushed.
    pub dir: Direction,
    /// Whether the player now faces `dir` (a move or a turn happened).
    pub faced: bool,
    /// A tile learned to be blocked: (map, tile).
    pub blocked: Option<(String, (i32, i32))>,
}

/// The step in flight.
#[derive(Debug, Clone)]
struct Pending {
    from: PlayerPose,
    dir: Direction,
    to: (i32, i32),
    /// The player was believed to face `dir` when the step was issued, so
    /// a tap that doesn't move him is a block, not a turn.
    facing: bool,
}

/// Holds and taps along a path, with the bookkeeping of the step in
/// flight: which tile it aims at, how many taps failed in a row, and
/// whether to walk with taps for a while (after a hold fell short).
#[derive(Debug, Default)]
pub struct Walker {
    /// Moves left to make with taps instead of holds.
    single_steps: u32,
    /// Taps in a row that did not move the player.
    stalled: u32,
    /// The step in flight.
    pending: Option<Pending>,
    /// The hold in flight, and when it was issued.
    hold: Option<(HoldTracker, Instant)>,
}

impl Walker {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many taps in a row failed to move the player.
    pub fn stalled(&self) -> u32 {
        self.stalled
    }

    /// The next step along `path` from the located pose, or, while a hold
    /// runs, how it is going. `facing` is the direction the player is
    /// believed to face: a tap in another direction is preceded by a turn
    /// (a hold turns and walks by itself).
    pub fn next(
        &mut self,
        observation: &Observation,
        path: &[Step],
        facing: Option<Direction>,
        sync: &Syncer,
        now: Instant,
    ) -> WalkStep {
        let pose = observation.player.as_ref().map(|p| &p.pose);
        if let Some((tracker, issued)) = &mut self.hold {
            let elapsed_ms = now.saturating_duration_since(*issued).as_secs_f64() * 1000.0;
            return match tracker.track(elapsed_ms, pose) {
                Track::Stalled { .. } => {
                    self.hold = None;
                    self.single_steps = 2;
                    WalkStep::Cancel
                }
                Track::OnTrack {
                    predicted,
                    observed,
                } => WalkStep::Walking {
                    predicted,
                    observed,
                },
            };
        }
        let Some(pose) = pose else {
            return WalkStep::Walking {
                predicted: 0,
                observed: None,
            };
        };
        let Some(step) = path.first().copied() else {
            return WalkStep::Arrived;
        };
        // Keep hardware feedback frequent even across long straight roads.
        let run = straight_run((pose.x, pose.y), path).min(4);
        if run >= 2 && self.single_steps == 0 {
            let end = path[run - 1].to;
            let target = PlayerPose {
                map: pose.map.clone(),
                x: end.0,
                y: end.1,
            };
            // A hold in a new direction turns the player first (flash-7:
            // an 8-tile hold after a turn walked 7 tiles, released half a
            // tile early minus the turn); hold that much longer.
            let turn_ms = if facing == Some(step.dir) {
                0.0
            } else {
                sync.estimate(InputKind::Turn).unit_ms
            };
            self.pending = Some(Pending {
                from: pose.clone(),
                dir: step.dir,
                to: step.to,
                facing: facing == Some(step.dir),
            });
            self.hold = Some((
                HoldTracker::with_turn(pose.clone(), run, sync, turn_ms),
                now,
            ));
            return WalkStep::Hold {
                dir: step.dir,
                tiles: run,
                target,
                duration: sync.hold_for(InputKind::WalkTile, run)
                    + Duration::from_millis(turn_ms.round() as u64),
                timeout_frames: sync.timeout_frames(InputKind::WalkTile, run),
            };
        }
        if facing != Some(step.dir) {
            self.pending = None;
            return WalkStep::Turn {
                dir: step.dir,
                timeout_frames: sync.timeout_frames(InputKind::Turn, 1),
            };
        }
        self.single_steps = self.single_steps.saturating_sub(1);
        self.pending = Some(Pending {
            from: pose.clone(),
            dir: step.dir,
            to: step.to,
            facing: true,
        });
        WalkStep::Tap {
            dir: step.dir,
            to: step.to,
            timeout_frames: sync.timeout_frames(InputKind::WalkTile, 1),
        }
    }

    /// A single press toward `to` issued by the navigator itself (warps,
    /// map edges): tracked like a tap.
    pub fn note_tap(&mut self, from: PlayerPose, dir: Direction, to: (i32, i32), facing: bool) {
        self.hold = None;
        self.pending = Some(Pending {
            from,
            dir,
            to,
            facing,
        });
    }

    /// Nothing in flight (a turn in place, a warp push).
    pub fn clear_pending(&mut self) {
        self.hold = None;
        self.pending = None;
    }

    /// The executor's verdict on the step in flight.
    pub fn on_outcome(&mut self, action: &Action, outcome: Outcome) -> Option<StepDone> {
        self.hold = None;
        let Pending {
            from,
            dir,
            to: target,
            facing,
        } = self.pending.take()?;
        let mut faced = false;
        let mut blocked = None;
        match (outcome, &action.expect) {
            (Outcome::Confirmed, _) => {
                faced = true;
                self.stalled = 0;
            }
            // A hold that fell short, stalled or was cut off: replan from
            // wherever we are, with taps for a bit (they learn what blocks
            // the way).
            (_, Expectation::PlayerAt(_)) => {
                faced = true;
                if matches!(outcome, Outcome::TimedOut | Outcome::Stalled) {
                    self.single_steps = 2;
                }
            }
            (Outcome::TimedOut, Expectation::PlayerMovedFrom(_)) => {
                // A tap the player already faced along: something is in
                // the way; avoid that tile. Otherwise the first miss
                // probably just turned him and the second is the block.
                faced = true;
                self.stalled += 1;
                if facing || self.stalled >= 2 {
                    blocked = Some((from.map, target));
                    self.stalled = 0;
                }
            }
            _ => self.stalled += 1,
        }
        Some(StepDone {
            dir,
            faced,
            blocked,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_state::{Observed, PoseObservation, ScreenState};

    fn pose(x: i32, y: i32) -> PlayerPose {
        PlayerPose {
            map: "Route1".into(),
            x,
            y,
        }
    }

    fn observation(frame: u64, at: Option<(i32, i32)>) -> Observation {
        let mut o = Observation::bare(
            frame,
            Observed {
                value: ScreenState::Overworld,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.player = at.map(|(x, y)| PoseObservation {
            pose: pose(x, y),
            score: 1000,
        });
        o
    }

    fn path_right(from: (i32, i32), tiles: usize) -> Vec<Step> {
        (1..=tiles as i32)
            .map(|i| Step {
                dir: Direction::Right,
                to: (from.0 + i, from.1),
            })
            .collect()
    }

    #[test]
    fn a_straight_run_is_a_hold_with_the_syncers_timing() {
        let sync = Syncer::new("emulator");
        let mut w = Walker::new();
        let t0 = Instant::now();
        // Right×5: hold for 4 (the last step stays a tap).
        let step = w.next(
            &observation(1, Some((3, 7))),
            &path_right((3, 7), 5),
            Some(Direction::Right),
            &sync,
            t0,
        );
        assert_eq!(
            step,
            WalkStep::Hold {
                dir: Direction::Right,
                tiles: 4,
                target: pose(7, 7),
                duration: Duration::from_millis(268 * 4 - 134),
                timeout_frames: 30 + 16 + 8,
            }
        );
        // Facing another way: the hold also covers the turn (8 frames).
        let mut w = Walker::new();
        let step = w.next(
            &observation(1, Some((3, 7))),
            &path_right((3, 7), 5),
            Some(Direction::Down),
            &sync,
            t0,
        );
        assert!(
            matches!(step, WalkStep::Hold { duration, .. } if duration == Duration::from_millis(268 * 4 - 134 + 134)),
            "{step:?}"
        );
    }

    #[test]
    fn poses_arriving_late_cancel_the_hold() {
        let sync = Syncer::new("emulator");
        let mut w = Walker::new();
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);
        assert!(matches!(
            w.next(
                &observation(1, Some((3, 7))),
                &path_right((3, 7), 6),
                Some(Direction::Right),
                &sync,
                t0
            ),
            WalkStep::Hold { tiles: 4, .. }
        ));
        // Tile 1 done at 268 ms, tile 2 at 536 ms: the player is seen on
        // tile 1 at 300 ms, fine.
        assert_eq!(
            w.next(&observation(2, Some((4, 7))), &[], None, &sync, ms(300)),
            WalkStep::Walking {
                predicted: 1,
                observed: Some(1)
            }
        );
        // Not located for a while: no verdict.
        assert_eq!(
            w.next(&observation(3, None), &[], None, &sync, ms(600)),
            WalkStep::Walking {
                predicted: 2,
                observed: None
            }
        );
        // Still on tile 1 at 820 ms (prediction: 3): lagging by 2, but
        // not yet for 100 ms.
        assert_eq!(
            w.next(&observation(4, Some((4, 7))), &[], None, &sync, ms(820)),
            WalkStep::Walking {
                predicted: 3,
                observed: Some(1)
            }
        );
        // Still there 100 ms later: cancel, then taps for two moves.
        assert_eq!(
            w.next(&observation(5, Some((4, 7))), &[], None, &sync, ms(925)),
            WalkStep::Cancel
        );
        let action = Action::new("walk", vec![], Expectation::PlayerAt(pose(8, 7)), 30);
        let done = w.on_outcome(&action, Outcome::Stalled).unwrap();
        assert!(done.faced && done.blocked.is_none());
        assert!(matches!(
            w.next(
                &observation(6, Some((4, 7))),
                &path_right((4, 7), 4),
                Some(Direction::Right),
                &sync,
                ms(1000)
            ),
            WalkStep::Tap {
                dir: Direction::Right,
                to: (5, 7),
                timeout_frames: 30
            }
        ));
    }

    #[test]
    fn a_pose_catching_up_clears_the_lag() {
        let sync = Syncer::new("emulator");
        let mut tracker = HoldTracker::new(pose(0, 0), 6, &sync);
        assert!(matches!(
            tracker.track(820.0, Some(&pose(1, 0))),
            Track::OnTrack { .. }
        ));
        // Caught up: the clock restarts.
        assert!(matches!(
            tracker.track(880.0, Some(&pose(3, 0))),
            Track::OnTrack { .. }
        ));
        assert!(matches!(
            tracker.track(1500.0, Some(&pose(3, 0))),
            Track::OnTrack { .. }
        ));
        assert_eq!(
            tracker.track(1600.0, Some(&pose(3, 0))),
            Track::Stalled {
                predicted: 5,
                observed: 3
            }
        );
        // Another map: no evidence.
        let elsewhere = PlayerPose {
            map: "Route2".into(),
            x: 0,
            y: 0,
        };
        assert!(matches!(
            tracker.track(1700.0, Some(&elsewhere)),
            Track::OnTrack { observed: None, .. }
        ));
    }

    #[test]
    fn a_tap_in_a_new_direction_turns_first() {
        // Flash-6: from MtMoon_1F (16, 17), facing Down after a walk, a
        // 160 ms press Left only turned the player and timed out (71
        // frames) before a second press could try the step.
        let sync = Syncer::new("emulator");
        let mut w = Walker::new();
        let t0 = Instant::now();
        let step = w.next(
            &observation(1, Some((3, 7))),
            &path_right((3, 7), 1),
            Some(Direction::Down),
            &sync,
            t0,
        );
        assert_eq!(
            step,
            WalkStep::Turn {
                dir: Direction::Right,
                timeout_frames: 6
            }
        );
        // A turn has no step in flight.
        let action = Action::new("turn", vec![], Expectation::InputsDone, 6);
        assert!(w.on_outcome(&action, Outcome::Confirmed).is_none());
        // Facing Right now: a tap.
        assert!(matches!(
            w.next(
                &observation(2, Some((3, 7))),
                &path_right((3, 7), 1),
                Some(Direction::Right),
                &sync,
                t0
            ),
            WalkStep::Tap { to: (4, 7), .. }
        ));
    }

    #[test]
    fn a_missed_tap_while_facing_the_way_learns_the_blocker() {
        let sync = Syncer::new("emulator");
        let mut w = Walker::new();
        let t0 = Instant::now();
        assert!(matches!(
            w.next(
                &observation(1, Some((3, 7))),
                &path_right((3, 7), 1),
                Some(Direction::Right),
                &sync,
                t0
            ),
            WalkStep::Tap { to: (4, 7), .. }
        ));
        let action = Action::new("walk", vec![], Expectation::PlayerMovedFrom(pose(3, 7)), 30);
        let done = w.on_outcome(&action, Outcome::TimedOut).unwrap();
        assert!(done.faced);
        assert_eq!(done.blocked, Some(("Route1".to_owned(), (4, 7))));
        assert_eq!(w.stalled(), 0);
        // Nothing in flight: no verdict.
        assert!(w.on_outcome(&action, Outcome::Confirmed).is_none());
    }

    #[test]
    fn two_missed_taps_learn_the_blocker_when_the_facing_is_unknown() {
        // Taps the navigator issues itself (toward warps) without a turn.
        let sync = Syncer::new("emulator");
        let _ = &sync;
        let mut w = Walker::new();
        let action = Action::new("walk", vec![], Expectation::PlayerMovedFrom(pose(3, 7)), 30);
        w.note_tap(pose(3, 7), Direction::Right, (4, 7), false);
        let first = w.on_outcome(&action, Outcome::TimedOut).unwrap();
        assert!(first.faced && first.blocked.is_none());
        assert_eq!(w.stalled(), 1);
        w.note_tap(pose(3, 7), Direction::Right, (4, 7), false);
        let second = w.on_outcome(&action, Outcome::TimedOut).unwrap();
        assert_eq!(second.blocked, Some(("Route1".to_owned(), (4, 7))));
        assert_eq!(w.stalled(), 0);
    }
}
