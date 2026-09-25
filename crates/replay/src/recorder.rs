use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use pokebot_core::{
    CapturedFrame, ControllerCommand, ControllerReceipt, Error, NormalizedFrame, Result,
};

use crate::{write_json, ControllerRecord, FrameRecord, SessionMetadata, FORMAT_VERSION};

/// Writes a session directory as the bot runs.
pub struct SessionRecorder {
    dir: PathBuf,
    started: Instant,
    record_raw: bool,
    frames: BufWriter<File>,
    controller: BufWriter<File>,
    events: BufWriter<File>,
    last_frame_id: Option<u64>,
}

impl SessionRecorder {
    /// Creates `dir` (which must not already contain a session).
    pub fn create(
        dir: impl AsRef<Path>,
        video_source: &str,
        controller: &str,
        record_raw: bool,
    ) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let metadata_path = dir.join("metadata.json");
        if metadata_path.exists() {
            return Err(Error::InvalidData(format!(
                "{} already contains a session",
                dir.display()
            )));
        }
        for sub in ["frames", "raw"] {
            if sub == "raw" && !record_raw {
                continue;
            }
            let path = dir.join(sub);
            std::fs::create_dir_all(&path).map_err(|e| Error::io(&path, e))?;
        }
        let created_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        write_json(
            &metadata_path,
            &SessionMetadata {
                format_version: FORMAT_VERSION,
                created_unix_ms,
                video_source: video_source.to_owned(),
                controller: controller.to_owned(),
            },
        )?;
        let open = |name: &str| -> Result<BufWriter<File>> {
            let path = dir.join(name);
            File::create(&path)
                .map(BufWriter::new)
                .map_err(|e| Error::io(&path, e))
        };
        Ok(Self {
            frames: open("frames.jsonl")?,
            controller: open("controller.jsonl")?,
            events: open("events.jsonl")?,
            dir,
            started: Instant::now(),
            record_raw,
            last_frame_id: None,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn record_frame(
        &mut self,
        captured: &CapturedFrame,
        normalized: &NormalizedFrame,
    ) -> Result<()> {
        let file = format!("frames/{:08}.png", normalized.frame_id);
        pokebot_video::png::save(normalized.image(), self.dir.join(&file))?;
        if self.record_raw {
            let raw = self.dir.join(format!("raw/{:08}.png", captured.frame_id));
            pokebot_video::png::save(&captured.image, raw)?;
        }
        let record = FrameRecord {
            frame_id: normalized.frame_id,
            delivered: Some(captured.delivered),
            elapsed_us: self.elapsed_us(normalized.captured_at),
            fingerprint: format!("{:016x}", normalized.image().fingerprint()),
            file,
        };
        self.last_frame_id = Some(normalized.frame_id);
        append(&mut self.frames, &record, &self.dir.join("frames.jsonl"))?;
        // Frames are frequent; flush about once a second so a killed process
        // loses little.
        if normalized.frame_id % 60 == 0 {
            flush(&mut self.frames, &self.dir.join("frames.jsonl"))?;
        }
        Ok(())
    }

    pub fn record_command(
        &mut self,
        command: &ControllerCommand,
        receipt: &ControllerReceipt,
    ) -> Result<()> {
        let record = ControllerRecord {
            command_id: receipt.command_id,
            elapsed_us: self.elapsed_us(receipt.issued_at),
            after_frame_id: self.last_frame_id,
            input_duration_us: receipt.input_duration.as_micros() as u64,
            command: command.clone(),
        };
        append(
            &mut self.controller,
            &record,
            &self.dir.join("controller.jsonl"),
        )
    }

    /// Appends a semantic event (any serializable record) to `events.jsonl`.
    pub fn record_event(&mut self, event: &impl serde::Serialize) -> Result<()> {
        let path = self.dir.join("events.jsonl");
        append(&mut self.events, event, &path)?;
        flush(&mut self.events, &path)
    }

    pub fn flush(&mut self) -> Result<()> {
        for (writer, name) in [
            (&mut self.frames, "frames.jsonl"),
            (&mut self.controller, "controller.jsonl"),
            (&mut self.events, "events.jsonl"),
        ] {
            writer
                .flush()
                .map_err(|e| Error::io(self.dir.join(name), e))?;
        }
        Ok(())
    }

    fn elapsed_us(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.started).as_micros() as u64
    }
}

impl Drop for SessionRecorder {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn flush(writer: &mut BufWriter<File>, path: &Path) -> Result<()> {
    writer.flush().map_err(|e| Error::io(path, e))
}

fn append(writer: &mut BufWriter<File>, record: &impl serde::Serialize, path: &Path) -> Result<()> {
    serde_json::to_writer(&mut *writer, record).map_err(|e| Error::InvalidData(e.to_string()))?;
    writer.write_all(b"\n").map_err(|e| Error::io(path, e))
}
