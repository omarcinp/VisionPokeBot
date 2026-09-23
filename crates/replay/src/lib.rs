//! Session recording and replay.
//!
//! Layout of a session directory:
//!
//! ```text
//! session/
//! ├── metadata.json
//! ├── frames.jsonl        one FrameRecord per recorded frame
//! ├── controller.jsonl    one ControllerRecord per issued command
//! ├── events.jsonl        semantic events derived by the bot
//! ├── frames/             normalized 240×160 PNGs, named by frame id
//! └── raw/                optional full-resolution captured PNGs
//! ```

mod reader;
mod recorder;
mod records;

pub use reader::{ReplayVideoSource, Session};
pub use recorder::SessionRecorder;
pub use records::{ControllerRecord, FrameRecord, SessionMetadata, FORMAT_VERSION};

use std::path::Path;

use pokebot_core::{Error, Result};

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let json = serde_json::to_vec_pretty(value).map_err(|e| Error::InvalidData(e.to_string()))?;
    std::fs::write(path, json).map_err(|e| Error::io(path, e))
}
