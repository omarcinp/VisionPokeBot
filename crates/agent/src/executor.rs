use std::sync::atomic::{AtomicBool, Ordering};

use pokebot_core::ControllerCommand;
use pokebot_runtime::Runtime;
use pokebot_state::{GameEvent, GameState, Observation};

use crate::Action;

/// Consecutive frames an expectation must hold to count as confirmed.
const CONFIRM_FRAMES: u32 = 2;

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
}

pub trait Task {
    fn name(&self) -> &str;
    fn next(&mut self, ctx: &mut TaskContext<'_>) -> Decision;
    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut TaskContext<'_>);
}

struct Pending {
    action: Action,
    issued_frame: u64,
    /// First frame at which the controller had applied all inputs.
    idle_since: Option<u64>,
    confirmations: u32,
}

/// Runs a task to completion against the runtime's devices.
pub struct Executor {
    /// Longest the task may wait without acting before it counts as stuck.
    pub max_wait_frames: u64,
    /// Hard cap on the whole task.
    pub max_frames: u64,
}

impl Default for Executor {
    fn default() -> Self {
        Self {
            max_wait_frames: 45 * 60,
            max_frames: 60 * 60 * 20,
        }
    }
}

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
        let mut pending: Option<Pending> = None;
        let mut first_frame = None;
        let mut waiting_since: Option<(u64, String)> = None;
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

            let mut events = Vec::new();
            if let Some(p) = &mut pending {
                let idle = runtime.is_idle()?;
                if idle && p.idle_since.is_none() {
                    p.idle_since = Some(frame);
                }
                if idle && p.action.expect.met(&observation) {
                    p.confirmations += 1;
                } else {
                    p.confirmations = 0;
                }
                let outcome = if p.confirmations >= CONFIRM_FRAMES {
                    Some(Outcome::Confirmed)
                } else if p.idle_since.is_some_and(|since| {
                    frame - since > p.action.timeout_frames.max(CONFIRM_FRAMES.into())
                }) {
                    Some(Outcome::TimedOut)
                } else {
                    None
                };
                if let Some(outcome) = outcome {
                    let p = pending.take().expect("pending");
                    if outcome == Outcome::TimedOut {
                        runtime.explain(
                            format!(
                                "{name}: \"{}\" not confirmed after {} frames",
                                p.action.label,
                                frame - p.issued_frame
                            ),
                            &p.action,
                        );
                    }
                    let state = runtime.state().clone();
                    task.on_outcome(
                        &p.action,
                        outcome,
                        &mut TaskContext {
                            observation: &observation,
                            state: &state,
                            events: &mut events,
                        },
                    );
                }
                for event in events {
                    runtime.emit(event)?;
                }
                continue;
            }

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
                    runtime.explain(format!("{name}: {}", action.label), &action);
                    for command in &action.commands {
                        runtime.execute(command.clone())?;
                    }
                    pending = Some(Pending {
                        action,
                        issued_frame: frame,
                        idle_since: None,
                        confirmations: 0,
                    });
                }
                Decision::Wait(reason) => {
                    let (since, _) = waiting_since.get_or_insert((frame, reason.clone()));
                    if frame - *since > self.max_wait_frames {
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
