use pokebot_core::ControllerCommand;
use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub format_version: u32,
    pub created_unix_ms: u64,
    /// Free-form description of the devices, e.g. `"emulator:mgba_libretro"`.
    pub video_source: String,
    pub controller: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRecord {
    pub frame_id: u64,
    /// Microseconds since the recorder was created.
    pub elapsed_us: u64,
    /// [`pokebot_core::RgbImage::fingerprint`] of the normalized frame.
    pub fingerprint: String,
    /// Path of the normalized PNG relative to the session directory.
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerRecord {
    pub command_id: u64,
    pub elapsed_us: u64,
    /// Last frame the bot had observed when it issued the command.
    pub after_frame_id: Option<u64>,
    pub input_duration_us: u64,
    pub command: ControllerCommand,
}
