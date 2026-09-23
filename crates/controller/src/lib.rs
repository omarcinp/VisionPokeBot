//! Controller-side building blocks that are independent of any device.

pub mod null;
pub mod schedule;

pub use null::NullController;
pub use schedule::{FrameRate, InputSchedule};
