use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pokebot_core::{CapturedFrame, Error, Result, VideoSource};
use serde::de::DeserializeOwned;

use crate::{ControllerRecord, FrameRecord, SessionMetadata, FORMAT_VERSION};

/// A recorded session loaded from disk.
#[derive(Debug, Clone)]
pub struct Session {
    pub dir: PathBuf,
    pub metadata: SessionMetadata,
    pub frames: Vec<FrameRecord>,
    pub commands: Vec<ControllerRecord>,
}

impl Session {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let metadata_path = dir.join("metadata.json");
        let metadata: SessionMetadata = serde_json::from_slice(
            &std::fs::read(&metadata_path).map_err(|e| Error::io(&metadata_path, e))?,
        )
        .map_err(|e| Error::InvalidData(format!("{}: {e}", metadata_path.display())))?;
        if metadata.format_version != FORMAT_VERSION {
            return Err(Error::InvalidData(format!(
                "unsupported session format {} (expected {FORMAT_VERSION})",
                metadata.format_version
            )));
        }
        Ok(Self {
            frames: read_jsonl(&dir.join("frames.jsonl"))?,
            commands: read_jsonl(&dir.join("controller.jsonl"))?,
            dir,
            metadata,
        })
    }

    /// Plays the recorded normalized frames, starting at the first frame
    /// whose id is `>= from_frame`.
    pub fn video_source(&self, from_frame: u64) -> ReplayVideoSource {
        let start = self.frames.partition_point(|f| f.frame_id < from_frame);
        ReplayVideoSource {
            dir: self.dir.clone(),
            frames: self.frames[start..].to_vec(),
            next: 0,
            epoch: Instant::now(),
        }
    }
}

/// Replays a session's normalized frames as a [`VideoSource`]. Frame ids and
/// relative timestamps match the recording.
pub struct ReplayVideoSource {
    dir: PathBuf,
    frames: Vec<FrameRecord>,
    next: usize,
    epoch: Instant,
}

impl VideoSource for ReplayVideoSource {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let record = self.frames.get(self.next).ok_or(Error::EndOfStream)?;
        let image = pokebot_video::png::load(self.dir.join(&record.file))?;
        self.next += 1;
        Ok(CapturedFrame {
            frame_id: record.frame_id,
            captured_at: self.epoch + Duration::from_micros(record.elapsed_us),
            image,
        })
    }
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            serde_json::from_str(line)
                .map_err(|e| Error::InvalidData(format!("{}:{}: {e}", path.display(), i + 1)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pokebot_core::{Button, ControllerCommand, ControllerReceipt, NormalizedFrame, RgbImage};

    use super::*;
    use crate::SessionRecorder;

    fn frame(id: u64, shade: u8) -> (CapturedFrame, NormalizedFrame) {
        let image = RgbImage::filled(240, 160, [shade, 0, 0]);
        let captured = CapturedFrame {
            frame_id: id,
            captured_at: Instant::now(),
            image: image.clone(),
        };
        let normalized = NormalizedFrame::new(id, captured.captured_at, image).unwrap();
        (captured, normalized)
    }

    #[test]
    fn recorded_session_replays_identically() {
        let dir = std::env::temp_dir().join(format!("pokebot-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut recorder = SessionRecorder::create(&dir, "test", "test", true).unwrap();
            for (id, shade) in [(0, 10), (1, 20), (3, 30)] {
                let (captured, normalized) = frame(id, shade);
                recorder.record_frame(&captured, &normalized).unwrap();
            }
            let command = ControllerCommand::Press(Button::A);
            let receipt = ControllerReceipt {
                command_id: 0,
                issued_at: Instant::now(),
                input_duration: Duration::from_millis(160),
            };
            recorder.record_command(&command, &receipt).unwrap();
        }

        let session = Session::open(&dir).unwrap();
        assert_eq!(session.frames.len(), 3);
        assert_eq!(session.commands.len(), 1);
        assert_eq!(session.commands[0].after_frame_id, Some(3));
        assert_eq!(
            session.commands[0].command,
            ControllerCommand::Press(Button::A)
        );

        let mut source = session.video_source(1);
        let ids: Vec<(u64, [u8; 3])> = std::iter::from_fn(|| source.next_frame().ok())
            .map(|f| (f.frame_id, f.image.pixel(0, 0)))
            .collect();
        assert_eq!(ids, vec![(1, [20, 0, 0]), (3, [30, 0, 0])]);
        assert!(dir.join("raw/00000003.png").exists());
        assert!(SessionRecorder::create(&dir, "test", "test", false).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
