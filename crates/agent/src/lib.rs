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
pub mod belief_view;
pub mod catch;
pub mod checkpoint;
pub mod console;
mod executor;
pub mod keyboard;
pub mod learn;
pub mod motion;
pub mod moves;
pub mod nav;
pub mod new_game;
pub mod party;
pub mod progress;
pub mod save;
pub mod shop;
pub mod stock;
pub mod story;
pub mod tools;
pub mod track;

pub use action::{Action, Expectation};
pub use belief_view::StateBelief;
pub use executor::{
    Decision, Executor, ExecutorError, Outcome, OutsideRecovery, Task, TaskContext,
};
pub use motion::{InputKind, Syncer, SyncerHandle};
pub use new_game::{NewGameConfig, NewGameTask, RIVAL_PRESETS};
pub use progress::Progress;
pub use save::{ContinueTask, SaveGameTask};
pub use story::{
    all_milestones, opening, to_brock, to_cerulean, to_mt_moon, Milestone, Starter, StoryStep,
    StoryTask,
};
pub use tools::{Intent, Tool, ToolContext, ToolError, ToolOutcome, Toolbox};
