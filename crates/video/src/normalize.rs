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
/// Each canonical pixel is the mean of the source pixels well inside its
/// cell: the pixels wholly inside it minus a one-pixel border, where scaling
/// blurs neighbours together and compression rings. On a 6.5× Switch capture
/// that is a 4×4 block, which averages out most JPEG noise; where the border
/// leaves nothing (up to 2×) it is the pixel under the cell's centre. Integer
/// scaling is reproduced exactly, and the arithmetic is integer only.
#[derive(Debug, Clone)]
pub struct Normalizer {
    locator: ViewportLocator,
}

impl Normalizer {
    pub fn new(locator: ViewportLocator) -> Self {
        Self { locator }
    }

    /// A fixed viewport inside a bigger capture (a capture card), as opposed
    /// to a source that is only the game (the emulator, images).
    pub fn letterboxed(&self) -> bool {
        matches!(self.locator, ViewportLocator::Fixed(_))
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

    /// Whether the whole captured frame is black: no HDMI signal (the Switch
    /// is off or asleep) or a fade. At least 99 % of the samples (every 16th
    /// pixel) must be darker than 24 in every channel, which JPEG noise on a
    /// black signal stays under.
    pub fn dark(frame: &CapturedFrame) -> bool {
        let image = &frame.image;
        let (mut lit, mut total) = (0u32, 0u32);
        for y in (0..image.height()).step_by(16) {
            for x in (0..image.width()).step_by(16) {
                total += 1;
                if image.pixel(x, y).iter().any(|&c| c > 24) {
                    lit += 1;
                }
            }
        }
        lit * 100 <= total
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
    let columns = cell_interiors(rect.width, CANONICAL_WIDTH);
    let rows = cell_interiors(rect.height, CANONICAL_HEIGHT);
    let (bytes, stride) = (image.as_bytes(), image.width() as usize * 3);
    let mut data = Vec::with_capacity((CANONICAL_WIDTH * CANONICAL_HEIGHT * 3) as usize);
    for &(y0, h) in &rows {
        for &(x0, w) in &columns {
            let mut sum = [0u32; 3];
            for sy in rect.y + y0..rect.y + y0 + h {
                let line = sy as usize * stride;
                let start = line + (rect.x + x0) as usize * 3;
                for p in bytes[start..start + w as usize * 3].chunks_exact(3) {
                    for c in 0..3 {
                        sum[c] += u32::from(p[c]);
                    }
                }
            }
            let n = w * h;
            data.extend(sum.map(|s| ((s + n / 2) / n) as u8));
        }
    }
    RgbImage::from_raw(CANONICAL_WIDTH, CANONICAL_HEIGHT, data)
}

/// Per destination cell along one axis, the source pixels averaged for it:
/// `(first, count)`. A cell covers `[i·src/dst, (i+1)·src/dst)`; the pixels
/// wholly inside it, minus one at each end, or else the centre pixel.
fn cell_interiors(src: u32, dst: u32) -> Vec<(u32, u32)> {
    (0..dst)
        .map(|i| {
            let (src64, dst64, i) = (u64::from(src), u64::from(dst), u64::from(i));
            let first_inside = (i * src64).div_ceil(dst64);
            let end_inside = ((i + 1) * src64) / dst64; // exclusive
            if end_inside >= first_inside + 3 {
                (
                    first_inside as u32 + 1,
                    (end_inside - first_inside - 2) as u32,
                )
            } else {
                (centre_sample(i as u32, src, dst), 1)
            }
        })
        .collect()
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
    fn integer_scales_round_trip_exactly() {
        let src = pattern();
        for scale in 1..=7 {
            let (w, h) = (CANONICAL_WIDTH * scale, CANONICAL_HEIGHT * scale);
            let big = letterbox(&src, scale, (w + 20, h + 10), 10, 5);
            let rect = Rect {
                x: 10,
                y: 5,
                width: w,
                height: h,
            };
            let out = Normalizer::new(ViewportLocator::Fixed(rect))
                .normalize(&captured(big))
                .unwrap();
            assert_eq!(out.image(), &src, "scale {scale}");
        }
    }

    #[test]
    fn cells_average_their_interior() {
        // 6.5× (the Switch viewport): cells alternate 6 and 7 source pixels
        // with a straddling pixel between them; 4 interior pixels each.
        assert_eq!(&cell_interiors(1560, 240)[..3], &[(1, 4), (8, 4), (14, 4)]);
        // Noise on one interior pixel moves the mean by a sixteenth of it,
        // and noise on the border doesn't count at all.
        let (w, h) = (1560, 1040);
        let mut big = RgbImage::filled(w, h, [100, 100, 100]);
        big.put_pixel(2, 2, [180, 100, 100]);
        big.put_pixel(0, 0, [255, 255, 255]);
        let rect = Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let out = Normalizer::new(ViewportLocator::Fixed(rect))
            .normalize(&captured(big))
            .unwrap();
        assert_eq!(out.image().pixel(0, 0), [105, 100, 100]);
        assert_eq!(out.image().pixel(1, 0), [100, 100, 100]);
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
