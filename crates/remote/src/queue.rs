//! Device-side input queue (sans-IO). Commands are expanded into timed
//! controller states and played back to back; the caller ticks it with the
//! current time and sends the resulting state to the Switch.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use pokebot_core::{PressProfile, Stick, SwitchCommand, SwitchState, TimedSwitchInput};

/// Most timed inputs that may wait in the queue.
pub const CAPACITY: usize = 256;

/// Input the device plays by itself after a stretch without any, so the
/// Switch never dims its picture (Screen Burn-In Reduction: after 5 minutes
/// without input) or goes to sleep (TV mode Auto-Sleep: after 1–12 hours).
#[derive(Debug, Clone, PartialEq)]
pub struct Keepalive {
    /// Idle time before the routine plays (and again after each run).
    pub after: Duration,
    pub routine: SwitchCommand,
}

impl Keepalive {
    /// Four minutes, under the five of the Switch's dimming.
    pub const DEFAULT_AFTER: Duration = Duration::from_secs(240);

    /// Right stick up, then down. FireRed ignores the right stick, so this
    /// is safe on any screen (the D-pad would walk, B would answer NO), yet
    /// it counts as input for the Switch.
    pub fn right_stick_nudge() -> SwitchCommand {
        let right = |stick: Stick, ms: u64| TimedSwitchInput {
            state: SwitchState {
                right_stick: stick,
                ..SwitchState::NEUTRAL
            },
            duration: Duration::from_millis(ms),
        };
        SwitchCommand::Sequence(vec![
            right(Stick::UP, 100),
            right(Stick::CENTER, 100),
            right(Stick::DOWN, 100),
            right(Stick::CENTER, 100),
        ])
    }
}

impl Default for Keepalive {
    fn default() -> Self {
        Self {
            after: Self::DEFAULT_AFTER,
            routine: Self::right_stick_nudge(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Step {
    command: u64,
    state: SwitchState,
    duration: Duration,
    /// Last step of its command: the command finishes when it elapses.
    last: bool,
    /// Played by the device itself (keepalive): never reported to clients,
    /// and dropped as soon as a client sends input.
    internal: bool,
}

#[derive(Debug, Clone, Copy)]
struct Active {
    step: Step,
    ends_at: Instant,
}

/// How a command left the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    pub id: u64,
    /// Dropped by a later `Neutral` before it finished.
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    pub id: u64,
    /// Time the command occupies the input line once it starts.
    pub input: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    QueueFull,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejected::QueueFull => write!(f, "queue full ({CAPACITY} inputs)"),
        }
    }
}

#[derive(Debug, Default)]
pub struct InputQueue {
    steps: VecDeque<Step>,
    active: Option<Active>,
    next_id: u64,
    finished: Vec<Finished>,
    keepalive: Option<Keepalive>,
    /// When the queue last went idle (`None` while playing input).
    idle_since: Option<Instant>,
    keepalives: u32,
}

impl InputQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Plays `keepalive` whenever the queue has been idle for its `after`;
    /// `None` turns it off.
    pub fn set_keepalive(&mut self, keepalive: Option<Keepalive>) {
        self.keepalive = keepalive;
        self.idle_since = None;
    }

    pub fn keepalive(&self) -> Option<&Keepalive> {
        self.keepalive.as_ref()
    }

    /// Keepalive routines played so far.
    pub fn keepalives(&self) -> u32 {
        self.keepalives
    }

    /// Queues `command` behind everything already queued.
    /// [`SwitchCommand::Neutral`] instead cancels the queue and releases
    /// everything at once.
    pub fn push(
        &mut self,
        command: &SwitchCommand,
        profile: &PressProfile,
    ) -> Result<Accepted, Rejected> {
        // Client input takes over from a keepalive at once.
        self.drop_internal();
        let timeline = command.timeline(profile);
        if self.steps.len() + timeline.len() > CAPACITY {
            return Err(Rejected::QueueFull);
        }
        let id = self.next_id;
        self.next_id += 1;
        if matches!(command, SwitchCommand::Neutral) {
            self.cancel();
        }
        let input = timeline.iter().map(|t| t.duration).sum();
        let count = timeline.len();
        if count == 0 {
            // Nothing to hold: finishes on the next tick, in order.
            self.steps.push_back(Step {
                command: id,
                state: SwitchState::NEUTRAL,
                duration: Duration::ZERO,
                last: true,
                internal: false,
            });
        }
        self.queue_timeline(id, timeline, false);
        Ok(Accepted { id, input })
    }

    fn queue_timeline(&mut self, id: u64, timeline: Vec<TimedSwitchInput>, internal: bool) {
        let count = timeline.len();
        for (i, t) in timeline.into_iter().enumerate() {
            self.steps.push_back(Step {
                command: id,
                state: t.state,
                duration: t.duration,
                last: i + 1 == count,
                internal,
            });
        }
    }

    /// Removes keepalive input, active or queued, without reporting it.
    fn drop_internal(&mut self) {
        if self.active.is_some_and(|a| a.step.internal) {
            self.active = None;
        }
        self.steps.retain(|s| !s.internal);
    }

    /// Queues the keepalive routine once the queue has idled long enough.
    fn keep_alive(&mut self, now: Instant) -> bool {
        let Some(keepalive) = &self.keepalive else {
            return false;
        };
        let since = *self.idle_since.get_or_insert(now);
        if now.duration_since(since) < keepalive.after {
            return false;
        }
        let timeline = keepalive.routine.timeline(&PressProfile::default());
        let id = self.next_id;
        self.next_id += 1;
        self.keepalives = self.keepalives.wrapping_add(1);
        self.idle_since = None;
        self.queue_timeline(id, timeline, true);
        true
    }

    /// Advances to `now`. Returns the controller state to present right now.
    pub fn tick(&mut self, now: Instant) -> SwitchState {
        loop {
            if let Some(active) = self.active {
                if now < active.ends_at {
                    return active.step.state;
                }
                self.active = None;
                if active.step.last && !active.step.internal {
                    self.finished.push(Finished {
                        id: active.step.command,
                        cancelled: false,
                    });
                }
                // Chain from the scheduled end rather than `now` so durations
                // do not drift with the tick rate — unless we fell well behind,
                // in which case a short input would be skipped entirely.
                let start = if now - active.ends_at < Duration::from_millis(2) {
                    active.ends_at
                } else {
                    now
                };
                self.start_next(start);
            } else if !self.start_next(now) && !self.keep_alive(now) {
                return SwitchState::NEUTRAL;
            }
        }
    }

    fn start_next(&mut self, start: Instant) -> bool {
        match self.steps.pop_front() {
            Some(step) => {
                self.idle_since = None;
                self.active = Some(Active {
                    step,
                    ends_at: start + step.duration,
                });
                true
            }
            None => false,
        }
    }

    /// Drops every queued input. The dropped commands are reported as
    /// cancelled.
    fn cancel(&mut self) {
        let mut dropped: Vec<u64> = self
            .active
            .take()
            .filter(|a| !a.step.internal)
            .map(|a| a.step.command)
            .into_iter()
            .collect();
        dropped.extend(
            self.steps
                .drain(..)
                .filter(|s| !s.internal)
                .map(|s| s.command),
        );
        dropped.dedup();
        self.finished.extend(dropped.into_iter().map(|id| Finished {
            id,
            cancelled: true,
        }));
    }

    /// Commands that finished since the last call, in order.
    pub fn take_finished(&mut self) -> Vec<Finished> {
        std::mem::take(&mut self.finished)
    }

    pub fn is_idle(&self) -> bool {
        self.active.is_none() && self.steps.is_empty()
    }

    /// Timed inputs waiting behind the one being held.
    pub fn pending(&self) -> usize {
        self.steps.len()
    }
}

#[cfg(test)]
mod tests {
    use pokebot_core::{Stick, SwitchButton, TimedSwitchInput};

    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn pressed(button: SwitchButton) -> SwitchState {
        button.into()
    }

    const NEUTRAL: SwitchState = SwitchState::NEUTRAL;

    #[test]
    fn press_holds_then_releases_then_finishes() {
        let mut q = InputQueue::new();
        let t0 = Instant::now();
        let a = q
            .push(
                &SwitchCommand::Press(SwitchButton::A),
                &PressProfile::default(),
            )
            .unwrap();
        assert_eq!(a.input, ms(160));
        assert_eq!(q.tick(t0), pressed(SwitchButton::A));
        assert_eq!(q.tick(t0 + ms(79)), pressed(SwitchButton::A));
        assert_eq!(q.tick(t0 + ms(80)), NEUTRAL);
        assert!(q.take_finished().is_empty());
        assert!(!q.is_idle());
        assert_eq!(q.tick(t0 + ms(160)), NEUTRAL);
        assert_eq!(
            q.take_finished(),
            vec![Finished {
                id: a.id,
                cancelled: false
            }]
        );
        assert!(q.is_idle());
    }

    #[test]
    fn commands_run_back_to_back_without_drift() {
        let mut q = InputQueue::new();
        let profile = PressProfile::default();
        let t0 = Instant::now();
        q.push(&SwitchCommand::Press(SwitchButton::A), &profile)
            .unwrap();
        q.push(&SwitchCommand::Press(SwitchButton::Home), &profile)
            .unwrap();
        q.tick(t0);
        // Ticks 1 ms late each time; Home still starts exactly at 160 ms.
        q.tick(t0 + ms(81));
        assert_eq!(q.tick(t0 + ms(161)), pressed(SwitchButton::Home));
        assert_eq!(q.tick(t0 + ms(240)), NEUTRAL);
        assert_eq!(q.take_finished().len(), 1);
        q.tick(t0 + ms(320));
        assert_eq!(q.take_finished().len(), 1);
        assert!(q.is_idle());
    }

    #[test]
    fn late_tick_finishes_every_elapsed_step() {
        let mut q = InputQueue::new();
        let t0 = Instant::now();
        let seq = SwitchCommand::Sequence(vec![
            TimedSwitchInput {
                state: pressed(SwitchButton::Up),
                duration: ms(10),
            },
            TimedSwitchInput {
                state: pressed(SwitchButton::Down),
                duration: ms(10),
            },
        ]);
        q.push(&seq, &PressProfile::default()).unwrap();
        q.tick(t0);
        assert_eq!(q.tick(t0 + ms(50)), pressed(SwitchButton::Down));
        assert_eq!(q.tick(t0 + ms(61)), NEUTRAL);
        assert_eq!(q.take_finished().len(), 1);
    }

    #[test]
    fn sticks_are_held_like_buttons() {
        let mut q = InputQueue::new();
        let t0 = Instant::now();
        let tilt = SwitchState {
            left_stick: Stick::UP,
            ..SwitchState::NEUTRAL
        };
        q.push(
            &SwitchCommand::Hold {
                state: tilt,
                duration: ms(250),
            },
            &PressProfile::default(),
        )
        .unwrap();
        assert_eq!(q.tick(t0), tilt);
        assert_eq!(q.tick(t0 + ms(249)), tilt);
        assert_eq!(q.tick(t0 + ms(250)), NEUTRAL);
    }

    #[test]
    fn neutral_cancels_and_releases_immediately() {
        let mut q = InputQueue::new();
        let profile = PressProfile::default();
        let t0 = Instant::now();
        let hold = q
            .push(
                &SwitchCommand::Hold {
                    state: pressed(SwitchButton::Right),
                    duration: Duration::from_secs(5),
                },
                &profile,
            )
            .unwrap();
        let queued = q
            .push(&SwitchCommand::Press(SwitchButton::A), &profile)
            .unwrap();
        assert_eq!(q.tick(t0), pressed(SwitchButton::Right));
        let neutral = q.push(&SwitchCommand::Neutral, &profile).unwrap();
        assert_eq!(neutral.input, Duration::ZERO);
        assert_eq!(
            q.take_finished(),
            vec![
                Finished {
                    id: hold.id,
                    cancelled: true
                },
                Finished {
                    id: queued.id,
                    cancelled: true
                },
            ]
        );
        assert_eq!(q.tick(t0 + ms(1)), NEUTRAL);
        assert_eq!(
            q.take_finished(),
            vec![Finished {
                id: neutral.id,
                cancelled: false
            }]
        );
        assert!(q.is_idle());
    }

    fn right(stick: Stick) -> SwitchState {
        SwitchState {
            right_stick: stick,
            ..SwitchState::NEUTRAL
        }
    }

    /// A queue ticked every millisecond, like the device's executor.
    struct Ticked {
        q: InputQueue,
        t0: Instant,
        now: u64,
    }

    impl Ticked {
        fn new(keepalive: Option<Duration>) -> Self {
            let mut q = InputQueue::new();
            q.set_keepalive(keepalive.map(|after| Keepalive {
                after,
                routine: Keepalive::right_stick_nudge(),
            }));
            let t0 = Instant::now();
            q.tick(t0);
            Self { q, t0, now: 0 }
        }

        /// Ticks every millisecond up to `until`; the state at `until`.
        fn at(&mut self, until: u64) -> SwitchState {
            let mut state = NEUTRAL;
            while self.now < until {
                self.now += 1;
                state = self.q.tick(self.t0 + ms(self.now));
            }
            state
        }

        fn press_a(&mut self) -> Accepted {
            self.q
                .push(
                    &SwitchCommand::Press(SwitchButton::A),
                    &PressProfile::default(),
                )
                .unwrap()
        }
    }

    #[test]
    fn keepalive_nudges_the_right_stick_after_idling() {
        let mut t = Ticked::new(Some(ms(1000)));
        assert_eq!(t.at(999), NEUTRAL);
        assert_eq!(t.at(1000), right(Stick::UP));
        assert_eq!(t.at(1100), right(Stick::CENTER));
        assert_eq!(t.at(1200), right(Stick::DOWN));
        assert_eq!(t.at(1400), NEUTRAL);
        // Never reported: no client sent it.
        assert!(t.q.take_finished().is_empty());
        assert!(t.q.is_idle());
        assert_eq!(t.q.keepalives(), 1);
        // And again after another idle stretch.
        assert_eq!(t.at(2399), NEUTRAL);
        assert_eq!(t.at(2400), right(Stick::UP));
        assert_eq!(t.q.keepalives(), 2);
    }

    #[test]
    fn client_input_restarts_the_idle_clock() {
        let mut t = Ticked::new(Some(ms(1000)));
        t.at(900);
        t.press_a();
        assert_eq!(t.at(901), pressed(SwitchButton::A));
        // Press and release end at 1061: idle from there.
        assert_eq!(t.at(2060), NEUTRAL);
        assert_eq!(t.at(2061), right(Stick::UP));
    }

    #[test]
    fn client_input_takes_over_from_a_running_keepalive() {
        let mut t = Ticked::new(Some(ms(1000)));
        assert_eq!(t.at(1000), right(Stick::UP));
        let a = t.press_a();
        assert_eq!(t.at(1001), pressed(SwitchButton::A));
        assert_eq!(t.at(1161), NEUTRAL);
        assert_eq!(
            t.q.take_finished(),
            vec![Finished {
                id: a.id,
                cancelled: false
            }]
        );
        // A client's Neutral doesn't report a keepalive as cancelled.
        assert_eq!(t.at(2161), right(Stick::UP));
        t.q.push(&SwitchCommand::Neutral, &PressProfile::default())
            .unwrap();
        assert!(t.q.take_finished().is_empty());
        assert_eq!(t.at(2162), NEUTRAL);
    }

    #[test]
    fn no_keepalive_unless_configured() {
        let mut t = Ticked::new(None);
        assert_eq!(t.at(10_000), NEUTRAL);
        t.q.set_keepalive(Some(Keepalive::default()));
        t.q.set_keepalive(None);
        assert_eq!(t.at(20_000), NEUTRAL);
        assert_eq!(t.q.keepalives(), 0);
    }

    #[test]
    fn rejects_when_full() {
        let mut q = InputQueue::new();
        let profile = PressProfile::default();
        for _ in 0..CAPACITY / 2 {
            q.push(&SwitchCommand::Press(SwitchButton::A), &profile)
                .unwrap();
        }
        assert_eq!(
            q.push(&SwitchCommand::Press(SwitchButton::A), &profile),
            Err(Rejected::QueueFull)
        );
    }
}
