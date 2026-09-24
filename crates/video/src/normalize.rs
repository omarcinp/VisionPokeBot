use pokebot_core::{
    CapturedFrame, Error, NormalizedFrame, Result, RgbImage, CANONICAL_HEIGHT, CANONICAL_WIDTH,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Where the game viewport sits inside a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportLocator {
    /// The whole frame is the game (emulator output).
    FullFrame,
    /// A known crop, e.g. calibrated once for a capture card setup.
    Fixed(Rect),
}

/// Reduces any captured frame to the canonical 240×160 viewport.
///
/// Scaling samples the source pixel under the centre of each canonical
/// pixel, which reproduces integer-scaled output exactly and is fully
/// deterministic (integer arithmetic only).
#[derive(Debug, Clone)]
pub struct Normalizer {
    locator: ViewportLocator,
}

impl Normalizer {
    pub fn new(locator: ViewportLocator) -> Self {
        Self { locator }
    }

    /// Whether the letterbox around a fixed viewport is lit, i.e. the
    /// console is showing something other than the game (the Switch HOME
    /// menu, a system dialog). Always false for full-frame sources.
    pub fn outside_viewport_lit(&self, frame: &CapturedFrame) -> bool {
        let ViewportLocator::Fixed(rect) = self.locator else {
            return false;
        };
        let image = &frame.image;
        let (mut lit, mut total) = (0u32, 0u32);
        let mut sample = |x0: u32, x1: u32| {
            for y in (0..image.height()).step_by(8) {
                for x in (x0..x1).step_by(8) {
                    total += 1;
                    let [r, g, b] = image.pixel(x, y);
                    if u32::from(r) + u32::from(g) + u32::from(b) > 3 * 40 {
                        lit += 1;
                    }
                }
            }
        };
        sample(0, rect.x.min(image.width()));
        sample((rect.x + rect.width).min(image.width()), image.width());
        total > 0 && lit * 2 > total
    }

    pub fn normalize(&self, frame: &CapturedFrame) -> Result<NormalizedFrame> {
        let image = &frame.image;
        let rect = match self.locator {
            ViewportLocator::FullFrame => Rect {
                x: 0,
                y: 0,
                width: image.width(),
                height: image.height(),
            },
            ViewportLocator::Fixed(rect) => rect,
        };
        let canonical = resample(image, rect)?;
        NormalizedFrame::new(frame.frame_id, frame.captured_at, canonical)
    }
}

fn resample(image: &RgbImage, rect: Rect) -> Result<RgbImage> {
    let fits = rect.width > 0
        && rect.height > 0
        && rect
            .x
            .checked_add(rect.width)
            .is_some_and(|r| r <= image.width())
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|b| b <= image.height());
    if !fits {
        return Err(Error::InvalidImage(format!(
            "viewport {rect:?} does not fit in {}x{} frame",
            image.width(),
            image.height()
        )));
    }
    if rect.x == 0
        && rect.y == 0
        && image.width() == CANONICAL_WIDTH
        && image.height() == CANONICAL_HEIGHT
    {
        return Ok(image.clone());
    }
    let mut data = Vec::with_capacity((CANONICAL_WIDTH * CANONICAL_HEIGHT * 3) as usize);
    for y in 0..CANONICAL_HEIGHT {
        let sy = rect.y + centre_sample(y, rect.height, CANONICAL_HEIGHT);
        for x in 0..CANONICAL_WIDTH {
            let sx = rect.x + centre_sample(x, rect.width, CANONICAL_WIDTH);
            data.extend_from_slice(&image.pixel(sx, sy));
        }
    }
    RgbImage::from_raw(CANONICAL_WIDTH, CANONICAL_HEIGHT, data)
}

/// Source offset under the centre of destination cell `i` of `dst` cells.
fn centre_sample(i: u32, src: u32, dst: u32) -> u32 {
    let offset = ((2 * u64::from(i) + 1) * u64::from(src)) / (2 * u64::from(dst));
    offset as u32
}

/// Finds the game viewport in a letterboxed capture: the bounding box of
/// pixels brighter than `black_threshold`, widened to the nearest 3:2 box.
///
/// Intended for one-off calibration on a bright frame; dark scenes and fades
/// make per-frame detection unreliable, so feed the result into
/// [`ViewportLocator::Fixed`].
pub fn detect_viewport(image: &RgbImage, black_threshold: u8) -> Option<Rect> {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..image.height() {
        for x in 0..image.width() {
            if image.pixel(x, y).iter().any(|c| *c > black_threshold) {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if min_x == u32::MAX {
        return None;
    }
    let (mut width, mut height) = (max_x - min_x + 1, max_y - min_y + 1);
    let (mut x, mut y) = (min_x, min_y);
    // Widen the short side to 3:2 around the detected centre, clamped to the frame.
    if width * CANONICAL_HEIGHT < height * CANONICAL_WIDTH {
        let target = (height * CANONICAL_WIDTH)
            .div_ceil(CANONICAL_HEIGHT)
            .min(image.width());
        x = x
            .saturating_sub((target - width) / 2)
            .min(image.width() - target);
        width = target;
    } else if width * CANONICAL_HEIGHT > height * CANONICAL_WIDTH {
        let target = (width * CANONICAL_HEIGHT)
            .div_ceil(CANONICAL_WIDTH)
            .min(image.height());
        y = y
            .saturating_sub((target - height) / 2)
            .min(image.height() - target);
        height = target;
    }
    Some(Rect {
        x,
        y,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn pattern() -> RgbImage {
        let mut img = RgbImage::filled(CANONICAL_WIDTH, CANONICAL_HEIGHT, [0, 0, 0]);
        for y in 0..CANONICAL_HEIGHT {
            for x in 0..CANONICAL_WIDTH {
                img.put_pixel(x, y, [x as u8, y as u8, (x ^ y) as u8 | 0x80]);
            }
        }
        img
    }

    /// Integer upscale placed at (`ox`, `oy`) inside a black canvas.
    fn letterbox(src: &RgbImage, scale: u32, canvas: (u32, u32), ox: u32, oy: u32) -> RgbImage {
        let mut out = RgbImage::filled(canvas.0, canvas.1, [0, 0, 0]);
        for y in 0..src.height() * scale {
            for x in 0..src.width() * scale {
                out.put_pixel(ox + x, oy + y, src.pixel(x / scale, y / scale));
            }
        }
        out
    }

    fn captured(image: RgbImage) -> CapturedFrame {
        CapturedFrame {
            frame_id: 7,
            captured_at: Instant::now(),
            image,
        }
    }

    #[test]
    fn full_frame_passthrough_is_identity() {
        let src = pattern();
        let out = Normalizer::new(ViewportLocator::FullFrame)
            .normalize(&captured(src.clone()))
            .unwrap();
        assert_eq!(out.image(), &src);
        assert_eq!(out.frame_id, 7);
    }

    #[test]
    fn integer_scaled_letterbox_round_trips_exactly() {
        let src = pattern();
        let big = letterbox(&src, 4, (1280, 720), 160, 40);
        let rect = detect_viewport(&big, 0).unwrap();
        assert_eq!(
            rect,
            Rect {
                x: 160,
                y: 40,
                width: 960,
                height: 640
            }
        );
        let out = Normalizer::new(ViewportLocator::Fixed(rect))
            .normalize(&captured(big))
            .unwrap();
        assert_eq!(out.image(), &src);
    }

    #[test]
    fn lit_letterbox_means_outside_the_game() {
        let rect = Rect {
            x: 160,
            y: 40,
            width: 960,
            height: 640,
        };
        let normalizer = Normalizer::new(ViewportLocator::Fixed(rect));
        let game = letterbox(&pattern(), 4, (1280, 720), 160, 40);
        assert!(!normalizer.outside_viewport_lit(&captured(game)));
        let home_menu = RgbImage::filled(1280, 720, [45, 45, 45]);
        assert!(normalizer.outside_viewport_lit(&captured(home_menu)));
        let full = Normalizer::new(ViewportLocator::FullFrame);
        assert!(!full.outside_viewport_lit(&captured(RgbImage::filled(240, 160, [200, 200, 200]))));
    }

    #[test]
    fn rejects_viewport_outside_frame() {
        let rect = Rect {
            x: 100,
            y: 0,
            width: 240,
            height: 160,
        };
        let err = Normalizer::new(ViewportLocator::Fixed(rect)).normalize(&captured(pattern()));
        assert!(err.is_err());
    }

    #[test]
    fn detect_viewport_widens_to_three_by_two() {
        let mut img = RgbImage::filled(100, 100, [0, 0, 0]);
        for y in 20..80 {
            img.put_pixel(50, y, [255, 255, 255]);
        }
        let rect = detect_viewport(&img, 16).unwrap();
        assert_eq!((rect.width, rect.height), (90, 60));
        assert_eq!((rect.x, rect.y), (6, 20));
        assert!(detect_viewport(&RgbImage::filled(10, 10, [0, 0, 0]), 16).is_none());
    }
}
