use std::path::{Path, PathBuf};
use std::time::Instant;

use pokebot_core::{CapturedFrame, Error, Result, VideoSource};

/// Plays a directory of PNG files (sorted by file name) as a video stream.
/// Useful for perception fixtures and hand-captured screenshots.
pub struct ImageSequenceSource {
    files: Vec<PathBuf>,
    next: usize,
}

impl ImageSequenceSource {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| Error::io(dir, e))?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
            })
            .collect();
        files.sort();
        Ok(Self { files, next: 0 })
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

impl VideoSource for ImageSequenceSource {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let path = self.files.get(self.next).ok_or(Error::EndOfStream)?;
        let image = crate::png::load(path)?;
        let frame_id = self.next as u64;
        self.next += 1;
        Ok(CapturedFrame {
            frame_id,
            delivered: frame_id,
            captured_at: Instant::now(),
            image,
        })
    }
}
