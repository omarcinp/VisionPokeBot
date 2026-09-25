//! Motion: the timing model of one device ([`Syncer`]) and the walker that
//! turns a planned path into holds and taps and watches a hold as it runs.
//!
//! Walking is the one place open-loop holds are used for speed, so the
//! hold lengths and the timeouts around them come from measurements of the
//! device in use (per profile: `emulator`, `switch`) instead of constants.
//! Without samples every value is today's constant, so behaviour is
//! unchanged until the model has learned something.

mod syncer;
mod walker;

pub use syncer::{Estimate, InputKind, Syncer, SyncerHandle, TimingReport, FRAME_MS, TILE_MS};
pub use walker::{HoldTracker, StepDone, Track, WalkStep, Walker};
