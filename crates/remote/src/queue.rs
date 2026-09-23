//! Device-side input queue (sans-IO). Commands are expanded into timed
//! controller states and played back to back; the caller ticks it with the
//! current time and sends the resulting state to the Switch.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use pokebot_core::{PressProfile, SwitchCommand, SwitchState};

/// Most timed inputs that may wait in the queue.
pub const CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Step {
    command: u64,
    state: SwitchState,
    duration: Duration,
    /// Last step of its command: the command finishes when it elapses.
    last: bool,
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
}

impl InputQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `command` behind everything already queued.
    /// [`SwitchCommand::Neutral`] instead cancels the queue and releases
    /// everything at once.
    pub fn push(
        &mut self,
        command: &SwitchCommand,
        profile: &PressProfile,
    ) -> Result<Accepted, Rejected> {
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
            });
        }
        for (i, t) in timeline.into_iter().enumerate() {
            self.steps.push_back(Step {
                command: id,
                state: t.state,
                duration: t.duration,
                last: i + 1 == count,
            });
        }
        Ok(Accepted { id, input })
    }

    /// Advances to `now`. Returns the controller state to present right now.
    pub fn tick(&mut self, now: Instant) -> SwitchState {
        loop {
            if let Some(active) = self.active {
                if now < active.ends_at {
                    return active.step.state;
                }
                self.active = None;
                if active.step.last {
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
            } else if !self.start_next(now) {
                return SwitchState::NEUTRAL;
            }
        }
    }

    fn start_next(&mut self, start: Instant) -> bool {
        match self.steps.pop_front() {
            Some(step) => {
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
            .map(|a| a.step.command)
            .into_iter()
            .collect();
        dropped.extend(self.steps.drain(..).map(|s| s.command));
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
