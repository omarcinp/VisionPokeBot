use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use pokebot_core::{Button, ControllerCommand};
use pokebot_runtime::Runtime;
use pokebot_state::{GameEvent, GameState, Observation, PlayerPose, ScreenState};

use crate::motion::{HoldTracker, InputKind, SyncerHandle, Track, FRAME_MS};
use crate::{Action, Expectation};

/// Consecutive frames an expectation must hold to count as confirmed.
const CONFIRM_FRAMES: u32 = 2;
/// Frames the console may show something other than the game (HOME menu,
/// a system dialog) before the executor tries to get back into it.
const OUTSIDE_GAME_FRAMES: u64 = 90;

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("{task} failed: {reason}")]
    TaskFailed { task: String, reason: String },
    #[error("stopped by user")]
    Stopped,
    #[error(transparent)]
    Device(#[from] pokebot_core::Error),
}

/// What the task sees when deciding.
pub struct TaskContext<'a> {
    pub observation: &'a Observation,
    pub state: &'a GameState,
    /// Events the task wants recorded (phase changes, confirmed choices).
    pub events: &'a mut Vec<GameEvent>,
}

pub enum Decision {
    Act(Action),
    /// Nothing to do yet (text printing, animation, transition).
    Wait(String),
    Done(String),
    Fail(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Confirmed,
    TimedOut,
    /// An interruptible action was cancelled because something came up.
    Interrupted,
    /// An interruptible walk was cancelled because the player fell behind
    /// the tile the timing model predicted (blocked, or slower than
    /// modelled).
    Stalled,
}

pub trait Task {
    fn name(&self) -> &str;
    fn next(&mut self, ctx: &mut TaskContext<'_>) -> Decision;
    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut TaskContext<'_>);
}

/// A moment on the frame clock and the wall clock.
#[derive(Debug, Clone, Copy)]
struct Moment {
    frame: u64,
    at: Instant,
}

/// The action in flight and what has been seen of it.
struct Pending {
    action: Action,
    issued: Moment,
    /// Where the player stood when the inputs were issued.
    issued_pose: Option<PlayerPose>,
    /// First frame at which the controller had applied all inputs.
    idle_since: Option<u64>,
    confirmations: u32,
    /// First frame at which the action showed an effect: its expectation
    /// held, or the player left the tile it was on.
    first_effect: Option<Moment>,
    /// First frame of the run of frames on which the expectation held.
    met_since: Option<Moment>,
    /// Predicts the player's tile during a walking hold.
    tracker: Option<HoldTracker>,
}

impl Pending {
    fn new(action: Action, issued: Moment, observation: &Observation) -> Self {
        Self {
            action,
            issued,
            issued_pose: observation.player.as_ref().map(|p| p.pose.clone()),
            idle_since: None,
            confirmations: 0,
            first_effect: None,
            met_since: None,
            tracker: None,
        }
    }

    fn issued_frame(&self) -> u64 {
        self.issued.frame
    }

    /// Notes what this frame shows: the first effect, the run of confirming
    /// frames, and whether a walking hold is stalling.
    fn note_frame(
        &mut self,
        now: Moment,
        observation: &Observation,
        idle: bool,
        frame_clock: bool,
    ) -> Track {
        let met = self.action.expect.met(observation);
        if self.first_effect.is_none() {
            let moved = match (&self.issued_pose, &observation.player) {
                (Some(from), Some(seen)) => seen.pose != *from,
                _ => false,
            };
            if met || moved {
                self.first_effect = Some(now);
            }
        }
        if idle && self.idle_since.is_none() {
            self.idle_since = Some(now.frame);
        }
        if met {
            self.met_since.get_or_insert(now);
        } else {
            self.met_since = None;
        }
        self.confirmations = if idle && met {
            self.confirmations + 1
        } else {
            0
        };
        match &mut self.tracker {
            // Once the buttons are released the timeout takes over.
            Some(tracker) if !idle => {
                let elapsed = elapsed_ms(self.issued, now, frame_clock);
                tracker.track(elapsed, observation.player.as_ref().map(|p| &p.pose))
            }
            _ => Track::OnTrack {
                predicted: 0,
                observed: None,
            },
        }
    }

    /// The confirmed action's intervals for the timing model, in ms:
    /// (issue → first effect, first effect → expectation met).
    fn timing_sample(&self, frame_clock: bool) -> Option<(InputKind, usize, f64, f64)> {
        let (kind, units) = self.action.timing?;
        let done = self.met_since?;
        let first_effect = self.first_effect.unwrap_or(done);
        Some((
            kind,
            units,
            elapsed_ms(self.issued, first_effect, frame_clock),
            elapsed_ms(first_effect, done, frame_clock),
        ))
    }
}

/// Milliseconds from `from` to `to`: on the frame clock when the video is
/// stepped by the bot (wall time means nothing then), else on the frames'
/// capture times.
fn elapsed_ms(from: Moment, to: Moment, frame_clock: bool) -> f64 {
    if frame_clock {
        to.frame.saturating_sub(from.frame) as f64 * FRAME_MS
    } else {
        to.at.saturating_duration_since(from.at).as_secs_f64() * 1000.0
    }
}

/// Runs a task to completion against the runtime's devices.
pub struct Executor {
    /// Longest the task may wait without acting before it counts as stuck.
    pub max_wait_frames: u64,
    /// Hard cap on the whole task.
    pub max_frames: u64,
    /// Extra frames every action may take to show its effect: 0 for an
    /// emulator stepped by the bot, more for real hardware (controller, game
    /// and capture latency). Superseded by the syncer's estimate when one
    /// is attached.
    pub latency_frames: u64,
    /// The timing model: fed by every confirmed action with a `timing`,
    /// consulted for the allowance and to predict walking holds.
    pub syncer: Option<SyncerHandle>,
    /// Measure time in frames (at the GBA's rate) instead of wall time:
    /// for an in-process emulator stepped by the bot.
    pub frame_clock: bool,
}

impl Default for Executor {
    fn default() -> Self {
        Self {
            max_wait_frames: 45 * 60,
            max_frames: 60 * 60 * 20,
            latency_frames: 0,
            syncer: None,
            frame_clock: false,
        }
    }
}

/// Bookkeeping of the executor's way back into the game when the console
/// shows something else (HOME menu, a system dialog).
#[derive(Debug, Default)]
pub struct OutsideRecovery {
    since: Option<u64>,
    presses: u32,
}

/// Name under which actions run outside a task are recorded.
const TOOL_TASK: &str = "tool";

impl Executor {
    pub fn run(
        &self,
        runtime: &mut Runtime,
        task: &mut dyn Task,
        stop: &AtomicBool,
    ) -> Result<String, ExecutorError> {
        let name = task.name().to_owned();
        let fail = |reason: String| ExecutorError::TaskFailed {
            task: name.clone(),
            reason,
        };
        let mut first_frame = None;
        let mut waiting_since: Option<(u64, String)> = None;
        // Recovery presses since the task last acted on its own.
        let mut nudged = false;
        let mut outside = OutsideRecovery::default();
        loop {
            if stop.load(Ordering::Relaxed) {
                let _ = runtime.execute(ControllerCommand::Neutral);
                return Err(ExecutorError::Stopped);
            }
            runtime.observe()?;
            let observation = runtime
                .observation()
                .cloned()
                .expect("observe() sets the observation");
            let frame = observation.frame_id;
            let first = *first_frame.get_or_insert(frame);
            if frame - first > self.max_frames {
                let reason = format!("gave up after {} frames", self.max_frames);
                self.finish(runtime, &name, false, &reason)?;
                return Err(fail(reason));
            }
            // Outside the game (HOME menu, system dialog): nothing the task
            // decides can help.
            if self.recover_outside(runtime, &mut outside, &name)? {
                continue;
            }

            let mut events = Vec::new();
            let state = runtime.state().clone();
            let decision = task.next(&mut TaskContext {
                observation: &observation,
                state: &state,
                events: &mut events,
            });
            for event in events {
                runtime.emit(event)?;
            }
            match decision {
                Decision::Act(action) => {
                    waiting_since = None;
                    nudged = false;
                    let (action, outcome) = match self.run_pending(
                        runtime,
                        &name,
                        action,
                        stop,
                        &observation,
                        first + self.max_frames,
                    )? {
                        Ok(done) => done,
                        Err(reason) => {
                            self.finish(runtime, &name, false, &reason)?;
                            return Err(fail(reason));
                        }
                    };
                    let observation = runtime
                        .observation()
                        .cloned()
                        .expect("observe() sets the observation");
                    let state = runtime.state().clone();
                    let mut events = Vec::new();
                    task.on_outcome(
                        &action,
                        outcome,
                        &mut TaskContext {
                            observation: &observation,
                            state: &state,
                            events: &mut events,
                        },
                    );
                    for event in events {
                        runtime.emit(event)?;
                    }
                }
                Decision::Wait(reason) => {
                    let (since, _) = waiting_since.get_or_insert((frame, reason.clone()));
                    let waited = frame - *since;
                    // Halfway to giving up, try one B: it closes menus and
                    // pages the task doesn't know, advances text, and is
                    // the safe answer (NO) to an unexpected question.
                    if !nudged && waited > self.max_wait_frames / 2 {
                        nudged = true;
                        self.nudge(runtime, &name, &reason)?;
                        continue;
                    }
                    if waited > self.max_wait_frames {
                        let reason = format!("stuck waiting: {reason}");
                        self.finish(runtime, &name, false, &reason)?;
                        return Err(fail(reason));
                    }
                }
                Decision::Done(summary) => {
                    self.finish(runtime, &name, true, &summary)?;
                    return Ok(summary);
                }
                Decision::Fail(reason) => {
                    self.finish(runtime, &name, false, &reason)?;
                    return Err(fail(reason));
                }
            }
        }
    }

    /// Runs one action to its outcome: issues its inputs, then observes
    /// until its expectation holds, it times out, or (when interruptible)
    /// something comes up or the walk stalls. The runtime's observation is
    /// the frame the outcome was decided on. Tools use this to act without
    /// a [`Task`].
    pub fn run_action(
        &self,
        runtime: &mut Runtime,
        action: Action,
        stop: &AtomicBool,
    ) -> Result<Outcome, ExecutorError> {
        if runtime.observation().is_none() {
            runtime.observe()?;
        }
        let observation = runtime
            .observation()
            .cloned()
            .expect("observe() sets the observation");
        let deadline = observation.frame_id + self.max_frames;
        match self.run_pending(runtime, TOOL_TASK, action, stop, &observation, deadline)? {
            Ok((_, outcome)) => Ok(outcome),
            Err(reason) => Err(ExecutorError::TaskFailed {
                task: TOOL_TASK.to_owned(),
                reason,
            }),
        }
    }

    /// The pending half of the loop: issues `action` from `issued_on` and
    /// observes until it has an outcome. `Err(reason)` in the inner result
    /// when `deadline` (a frame id) passes first.
    #[allow(clippy::type_complexity)]
    fn run_pending(
        &self,
        runtime: &mut Runtime,
        name: &str,
        action: Action,
        stop: &AtomicBool,
        issued_on: &Observation,
        deadline: u64,
    ) -> Result<Result<(Action, Outcome), String>, ExecutorError> {
        runtime.explain(format!("{name}: {}", action.label), &action);
        for command in &action.commands {
            runtime.execute(command.clone())?;
        }
        let issued = Moment {
            frame: issued_on.frame_id,
            at: Instant::now(),
        };
        let mut p = Pending::new(action, issued, issued_on);
        p.tracker = self.hold_tracker(&p);
        loop {
            if stop.load(Ordering::Relaxed) {
                let _ = runtime.execute(ControllerCommand::Neutral);
                return Err(ExecutorError::Stopped);
            }
            runtime.observe()?;
            let observation = runtime
                .observation()
                .cloned()
                .expect("observe() sets the observation");
            let frame = observation.frame_id;
            let now = Moment {
                frame,
                at: runtime
                    .last_captured()
                    .map_or_else(Instant::now, |c| c.captured_at),
            };
            if frame > deadline {
                return Ok(Err(format!("gave up after {} frames", self.max_frames)));
            }
            // Outside the game: the action can't show its effect; wait for
            // the game to be back (the caller recovers between actions).
            if runtime.outside_game() {
                continue;
            }
            let idle = runtime.is_idle()?;
            // Something came up mid-hold (a wild battle, a trainer, an
            // NPC talking): stop the inputs now, let the task replan.
            let interrupted = p.action.interruptible
                && !idle
                && (observation.dialogue.is_some()
                    || observation.battle.is_some()
                    || observation.menu.is_some()
                    || observation.screen.value == ScreenState::Transition);
            let track = p.note_frame(now, &observation, idle, self.frame_clock);
            // Release a directional hold as soon as its destination is
            // visible. Waiting for idle here lets an overlong hold walk
            // past the target and then teaches that delay as tile speed.
            if !idle && p.tracker.is_some() && p.action.expect.met(&observation) {
                runtime.execute(ControllerCommand::Neutral)?;
            }
            // A walking hold whose player fell behind the prediction:
            // it is blocked; release now instead of finishing the hold.
            let stalled = !interrupted && matches!(track, Track::Stalled { .. });
            if interrupted || stalled {
                runtime.execute(ControllerCommand::Neutral)?;
            }
            let allowance = self.allowance(p.action.timing.map(|(kind, _)| kind));
            let outcome = if interrupted {
                Some(Outcome::Interrupted)
            } else if stalled {
                Some(Outcome::Stalled)
            } else if p.confirmations >= CONFIRM_FRAMES {
                Some(Outcome::Confirmed)
            } else if p.idle_since.is_some_and(|since| {
                frame - since > p.action.timeout_frames.max(CONFIRM_FRAMES.into()) + allowance
            }) {
                Some(Outcome::TimedOut)
            } else {
                None
            };
            let Some(outcome) = outcome else {
                continue;
            };
            // Timing data for tuning inputs on slower devices.
            runtime.record(
                "ActionOutcome",
                serde_json::json!({
                    "task": name,
                    "label": p.action.label,
                    "confirmed": outcome == Outcome::Confirmed,
                    "outcome": format!("{outcome:?}"),
                    "frames": frame - p.issued_frame(),
                    "after_idle": p.idle_since.map(|i| frame - i),
                    "timeout": p.action.timeout_frames,
                    "track": format!("{track:?}"),
                }),
            )?;
            if outcome == Outcome::TimedOut {
                runtime.explain(
                    format!(
                        "{name}: \"{}\" not confirmed after {} frames",
                        p.action.label,
                        frame - p.issued_frame()
                    ),
                    &p.action,
                );
            }
            if outcome == Outcome::Confirmed {
                self.learn_timing(runtime, &p)?;
            }
            return Ok(Ok((p.action, outcome)));
        }
    }

    /// Outside the game (HOME menu, system dialog) on the current frame:
    /// after [`OUTSIDE_GAME_FRAMES`] presses Home (back to the game), then
    /// A, alternating. `true` while outside (the caller observes again).
    pub fn recover_outside(
        &self,
        runtime: &mut Runtime,
        recovery: &mut OutsideRecovery,
        name: &str,
    ) -> Result<bool, ExecutorError> {
        if !runtime.outside_game() {
            recovery.since = None;
            recovery.presses = 0;
            return Ok(false);
        }
        let frame = runtime.observation().map_or(0, |o| o.frame_id);
        let since = *recovery.since.get_or_insert(frame);
        if frame - since > OUTSIDE_GAME_FRAMES {
            let button = if recovery.presses % 2 == 0 {
                Button::Home
            } else {
                Button::A
            };
            recovery.presses += 1;
            recovery.since = Some(frame);
            let action = Action::new(
                format!("recover: the console left the game, press {button:?}"),
                vec![ControllerCommand::Press(button)],
                Expectation::InputsDone,
                30,
            );
            runtime.error(format!("{name}: {}", action.label));
            runtime.execute(action.commands[0].clone())?;
        }
        Ok(true)
    }

    /// One B press when a wait has gone on too long: it closes menus and
    /// pages the task doesn't know, advances text, and is the safe answer
    /// (NO) to an unexpected question.
    pub fn nudge(
        &self,
        runtime: &mut Runtime,
        name: &str,
        reason: &str,
    ) -> Result<(), ExecutorError> {
        let action = Action::new(
            format!("recover: stuck waiting ({reason}), press B"),
            vec![ControllerCommand::Press(Button::B)],
            Expectation::InputsDone,
            30,
        );
        runtime.error(format!("{name}: {}", action.label));
        runtime.execute(action.commands[0].clone())?;
        Ok(())
    }

    /// Extra frames an action of `kind` may take to show its effect.
    fn allowance(&self, kind: Option<InputKind>) -> u64 {
        match &self.syncer {
            Some(syncer) => lock(syncer).expect_frames(kind),
            None => self.latency_frames,
        }
    }

    /// A predictor for a walking hold that may be cut short: an
    /// interruptible action of two or more tiles, issued from a located
    /// tile, with a timing model to predict from.
    fn hold_tracker(&self, p: &Pending) -> Option<HoldTracker> {
        let (InputKind::WalkTile, tiles) = p.action.timing? else {
            return None;
        };
        if tiles < 2 || !p.action.interruptible {
            return None;
        }
        let from = p.issued_pose.clone()?;
        let syncer = self.syncer.as_ref()?;
        Some(HoldTracker::new(from, tiles, &lock(syncer)))
    }

    /// Feeds the confirmed action's timing to the model.
    fn learn_timing(&self, runtime: &mut Runtime, p: &Pending) -> Result<(), ExecutorError> {
        let Some(syncer) = &self.syncer else {
            return Ok(());
        };
        let Some((kind, units, to_effect, effect_to_done)) = p.timing_sample(self.frame_clock)
        else {
            return Ok(());
        };
        let estimate = {
            let mut syncer = lock(syncer);
            syncer.observe_ms(kind, to_effect, effect_to_done, units);
            syncer.estimate(kind)
        };
        runtime.record(
            "TimingSample",
            serde_json::json!({
                "kind": format!("{kind:?}"),
                "units": units,
                "to_effect_ms": to_effect,
                "effect_to_done_ms": effect_to_done,
                "latency_ms": estimate.latency_ms,
                "unit_ms": estimate.unit_ms,
                "spread_ms": estimate.spread_ms,
                "samples": estimate.samples,
            }),
        )?;
        Ok(())
    }

    /// Releases the controller and records the goal result.
    fn finish(
        &self,
        runtime: &mut Runtime,
        name: &str,
        success: bool,
        detail: &str,
    ) -> Result<(), ExecutorError> {
        let _ = runtime.execute(ControllerCommand::Neutral);
        runtime.emit(GameEvent::GoalFinished {
            goal: name.to_owned(),
            success,
            detail: detail.to_owned(),
        })?;
        Ok(())
    }
}

fn lock(syncer: &SyncerHandle) -> std::sync::MutexGuard<'_, crate::motion::Syncer> {
    syncer.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pokebot_state::{Observed, PoseObservation};

    use super::*;
    use crate::motion::Syncer;

    fn pose(x: i32) -> PlayerPose {
        PlayerPose {
            map: "Route1".into(),
            x,
            y: 5,
        }
    }

    fn observation(frame: u64, x: Option<i32>) -> Observation {
        let mut o = Observation::bare(
            frame,
            Observed {
                value: ScreenState::Overworld,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.player = x.map(|x| PoseObservation {
            pose: pose(x),
            score: 1000,
        });
        o
    }

    fn moment(t0: Instant, frame: u64) -> Moment {
        Moment {
            frame,
            at: t0 + Duration::from_millis(frame * 20),
        }
    }

    #[test]
    fn movement_timing_ends_at_arrival_not_controller_idle() {
        let t0 = Instant::now();
        let action = Action::new("walk", vec![], Expectation::PlayerAt(pose(4)), 30)
            .timed(InputKind::WalkTile, 4);
        let mut p = Pending::new(action, moment(t0, 0), &observation(0, Some(0)));
        p.note_frame(moment(t0, 14), &observation(14, Some(1)), false, false);
        p.note_frame(moment(t0, 54), &observation(54, Some(4)), false, false);
        p.note_frame(moment(t0, 200), &observation(200, Some(4)), true, false);
        p.note_frame(moment(t0, 201), &observation(201, Some(4)), true, false);
        assert_eq!(
            p.timing_sample(false),
            Some((InputKind::WalkTile, 4, 280.0, 800.0))
        );
        assert_eq!(p.confirmations, 2);
    }

    /// A hold of 4 tiles: the player leaves the start tile on frame 20 and
    /// stands on the target from frame 60 on; confirmed on frame 61.
    #[test]
    fn a_confirmed_hold_feeds_the_syncer() {
        let t0 = Instant::now();
        let action = Action::new("walk", vec![], Expectation::PlayerAt(pose(4)), 30)
            .interruptible()
            .timed(InputKind::WalkTile, 4);
        let mut p = Pending::new(action, moment(t0, 0), &observation(0, Some(0)));
        let executor = Executor {
            syncer: Some(SyncerHandle::new(Syncer::new("emulator").into())),
            ..Executor::default()
        };
        p.tracker = executor.hold_tracker(&p);
        assert!(p.tracker.is_some());
        let mut confirmed_at = None;
        for frame in 1..=61 {
            let x = match frame {
                0..=19 => Some(0),
                20..=39 => Some(1),
                40..=49 => Some(2),
                50..=59 => Some(3),
                _ => Some(4),
            };
            let idle = frame >= 55;
            let track = p.note_frame(moment(t0, frame), &observation(frame, x), idle, false);
            assert!(
                matches!(track, Track::OnTrack { .. }),
                "frame {frame}: {track:?}"
            );
            if p.confirmations >= CONFIRM_FRAMES {
                confirmed_at = Some(frame);
                break;
            }
        }
        assert_eq!(confirmed_at, Some(61));
        assert_eq!(p.first_effect.map(|m| m.frame), Some(20));
        assert_eq!(p.met_since.map(|m| m.frame), Some(60));
        // Wall clock: 20 ms per frame here.
        let (kind, units, to_effect, effect_to_done) = p.timing_sample(false).unwrap();
        assert_eq!((kind, units), (InputKind::WalkTile, 4));
        assert!((to_effect - 400.0).abs() < 1.0, "{to_effect}");
        assert!((effect_to_done - 800.0).abs() < 1.0, "{effect_to_done}");
        // Frame clock: the GBA's frame period instead.
        let (_, _, to_effect, _) = p.timing_sample(true).unwrap();
        assert!((to_effect - 20.0 * FRAME_MS).abs() < 0.01, "{to_effect}");
        let syncer = executor.syncer.clone().unwrap();
        let (kind, units, a, b) = p.timing_sample(false).unwrap();
        lock(&syncer).observe_ms(kind, a, b, units);
        // One sample, pulled toward 800 / 3 ≈ 267 ms per tile.
        let e = lock(&syncer).estimate(InputKind::WalkTile);
        assert_eq!(e.samples, 1);
        assert!(
            (e.unit_ms - (268.0 + 0.2 * (800.0 / 3.0 - 268.0))).abs() < 0.01,
            "{e:?}"
        );
    }

    #[test]
    fn a_hold_whose_player_stays_put_stalls() {
        let t0 = Instant::now();
        let action = Action::new("walk", vec![], Expectation::PlayerAt(pose(6)), 30)
            .interruptible()
            .timed(InputKind::WalkTile, 6);
        let mut p = Pending::new(action, moment(t0, 0), &observation(0, Some(0)));
        let executor = Executor {
            syncer: Some(SyncerHandle::new(Syncer::new("emulator").into())),
            ..Executor::default()
        };
        p.tracker = executor.hold_tracker(&p);
        let mut stalled_at = None;
        for frame in 1..=60 {
            // The player never moves (an NPC in the way); 20 ms per frame.
            let track = p.note_frame(
                moment(t0, frame),
                &observation(frame, Some(0)),
                false,
                false,
            );
            if matches!(track, Track::Stalled { .. }) {
                stalled_at = Some(frame);
                break;
            }
        }
        // Two tiles predicted at 536 ms (frame 27), lagging by 2 from then;
        // 100 ms later (frame 32) the hold is called stalled.
        assert_eq!(stalled_at, Some(32));
        assert!(p.timing_sample(false).is_none());
    }

    #[test]
    fn actions_without_timing_or_tracking_are_untouched() {
        let t0 = Instant::now();
        let executor = Executor::default();
        let action = Action::new("press A", vec![], Expectation::MenuOpen, 30);
        let mut p = Pending::new(action, moment(t0, 0), &observation(0, None));
        assert!(executor.hold_tracker(&p).is_none());
        assert_eq!(executor.allowance(Some(InputKind::MenuPress)), 0);
        let hw = Executor {
            latency_frames: 30,
            ..Executor::default()
        };
        assert_eq!(hw.allowance(None), 30);
        for frame in 1..=3 {
            p.note_frame(moment(t0, frame), &observation(frame, None), true, false);
        }
        assert!(p.timing_sample(false).is_none());
        assert_eq!(p.confirmations, 0);
    }
}
