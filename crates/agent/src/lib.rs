//! Goal-driven, closed-loop control.
//!
//! A [`Task`] looks at the current observation and state and decides the next
//! [`Action`]. The [`Executor`] issues the action's controller commands and
//! then waits until the action's [`Expectation`] is confirmed on screen (or it
//! times out) before asking the task again. No step assumes a button press
//! worked; every effect is checked from video.

mod action;
pub mod bag;
pub mod battle;
pub mod catch;
pub mod checkpoint;
mod executor;
pub mod keyboard;
pub mod learn;
pub mod moves;
pub mod nav;
pub mod new_game;
pub mod party;
pub mod progress;
pub mod save;
pub mod stock;
pub mod story;
pub mod track;

pub use action::{Action, Expectation};
pub use executor::{Decision, Executor, ExecutorError, Outcome, Task, TaskContext};
pub use new_game::{NewGameConfig, NewGameTask, RIVAL_PRESETS};
pub use progress::Progress;
pub use save::{ContinueTask, SaveGameTask};
pub use story::{all_milestones, opening, to_brock, Milestone, Starter, StoryStep, StoryTask};
