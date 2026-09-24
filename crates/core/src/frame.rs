use std::time::Instant;

use crate::{Error, Result, RgbImage};

/// FireRed's logical GBA viewport. Every ROI, template and detector works in
/// these coordinates regardless of how the frame was captured.
pub const CANONICAL_WIDTH: u32 = 240;
pub const CANONICAL_HEIGHT: u32 = 160;

/// A frame exactly as delivered by a video device (any resolution).
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// Monotonic per-source counter. Gaps mean the device produced frames the
    /// consumer did not read (e.g. a real-time source outpacing the bot).
    pub frame_id: u64,
    pub captured_at: Instant,
    pub image: RgbImage,
}

/// A frame reduced to the canonical 240×160 game viewport.
#[derive(Debug, Clone)]
pub struct NormalizedFrame {
    pub frame_id: u64,
    pub captured_at: Instant,
    image: RgbImage,
}

impl NormalizedFrame {
    pub fn new(frame_id: u64, captured_at: Instant, image: RgbImage) -> Result<Self> {
        if image.width() != CANONICAL_WIDTH || image.height() != CANONICAL_HEIGHT {
            return Err(Error::InvalidImage(format!(
                "normalized frame must be {CANONICAL_WIDTH}x{CANONICAL_HEIGHT}, got {}x{}",
                image.width(),
                image.height()
            )));
        }
        Ok(Self {
            frame_id,
            captured_at,
            image,
        })
    }

    pub fn image(&self) -> &RgbImage {
        &self.image
    }
}

/// The bot's only sensor. Implementations: emulator video output, capture
/// card, recorded session replay, image sequences.
pub trait VideoSource {
    /// Blocks until the next frame is available. Returns
    /// [`Error::EndOfStream`] when a finite source is exhausted.
    fn next_frame(&mut self) -> Result<CapturedFrame>;
}

impl<V: VideoSource + ?Sized> VideoSource for Box<V> {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        (**self).next_frame()
    }
}
