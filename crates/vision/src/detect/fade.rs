//! Palette fades. Opening or closing a menu screen (Start menu → party,
//! bag, Trainer Card, summary) fades it to or from black: every
//! background palette colour is blended toward black in sixteenths
//! (`BeginNormalPaletteFade` → `BlendPalette`), so a fading frame is the
//! screen itself with its colours scaled by one factor. Measured on the
//! Switch: the Start menu's white window at 224, 190, 124 and 90 (14, 12,
//! 8 and 6 sixteenths), the party menu's teal (68, 176, 160) at (31, 83,
//! 79) and its grey prompt box 213 → 106 in the same frame (~7/16). The
//! sprites (Poké Ball, icons) fade on their own schedule and stay
//! brighter, so the scale is not simply "the brightest pixel is white".
//!
//! Such a frame is scaled back up and handed to the screen detectors; the
//! candidate factors are the sixteenths around the one that makes the
//! frame's brightest large surface white.

use pokebot_core::RgbImage;

/// A surface this large (px) sets the brightness bound; the sprites that
/// fade late are smaller (the party menu's Poké Ball: ~130 px).
const SURFACE_PIXELS: u32 = 600;
/// Frames whose brightest surface reaches this are not faded (a white
/// window at full brightness measures 250–255 on either source; the first
/// fade step, 15/16, puts it near 239).
const UNFADED_LEVEL: u8 = 244;
/// Deeper fades are left alone: scaling a frame up by more than 16/5
/// amplifies capture noise beyond the detectors' colour tolerance.
const DARKEST_SIXTEENTHS: u32 = 5;

/// The level (brightest channel) of the frame's brightest large surface.
pub fn surface_level(image: &RgbImage) -> u8 {
    let mut histogram = [0u32; 256];
    for p in image.as_bytes().chunks_exact(3) {
        histogram[usize::from(p[0].max(p[1]).max(p[2]))] += 1;
    }
    let mut above = 0;
    (0..=255u8)
        .rev()
        .find(|&v| {
            above += histogram[usize::from(v)];
            above >= SURFACE_PIXELS
        })
        .unwrap_or(0)
}

/// The fade factors (in sixteenths, most likely first) that could have
/// produced a frame whose brightest large surface is at `level`; empty
/// when the frame is not faded or too dark to restore.
pub fn candidates(level: u8) -> Vec<u32> {
    if level >= UNFADED_LEVEL {
        return Vec::new();
    }
    let level = u32::from(level);
    // The brightest surface as white gives the likeliest factor. The
    // menu screens' brightest surfaces are white or nearly (the party
    // menu's prompt box, 214, is the darkest: one sixteenth more), and
    // capture noise pushes the measured level up a little (the Start
    // menu's window at 190 measures 194): the step above, then the step
    // below. Only three, since each costs a pass of the screen detectors.
    let likeliest = (level * 16 + 127) / 255;
    if likeliest < DARKEST_SIXTEENTHS {
        return Vec::new();
    }
    [likeliest, likeliest + 1, likeliest - 1]
        .into_iter()
        .filter(|k| (DARKEST_SIXTEENTHS..16).contains(k))
        .collect()
}

/// `image` with its colours scaled by 16/`sixteenths` (rounded, clamped).
pub fn brighten(image: &RgbImage, sixteenths: u32) -> RgbImage {
    let k = sixteenths.max(1);
    let mut table = [0u8; 256];
    for (c, out) in table.iter_mut().enumerate() {
        *out = ((c as u32 * 16 + k / 2) / k).min(255) as u8;
    }
    let data = image
        .as_bytes()
        .iter()
        .map(|&c| table[usize::from(c)])
        .collect();
    RgbImage::from_raw(image.width(), image.height(), data).unwrap_or_else(|_| image.clone())
}

/// `image` as a fade at `sixteenths` would show it (test images).
#[cfg(test)]
pub fn darken(image: &RgbImage, sixteenths: u32) -> RgbImage {
    let mut out = image.clone();
    let scale = |c: u8| (u32::from(c) * sixteenths / 16) as u8;
    for y in 0..image.height() {
        for x in 0..image.width() {
            let p = image.pixel(x, y);
            out.put_pixel(x, y, [scale(p[0]), scale(p[1]), scale(p[2])]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfaded_and_black_frames_have_no_candidates() {
        let white = RgbImage::filled(240, 160, crate::color::WHITE);
        assert!(candidates(surface_level(&white)).is_empty());
        assert!(candidates(surface_level(&RgbImage::filled(240, 160, [8, 8, 8]))).is_empty());
    }

    #[test]
    fn a_faded_white_window_is_tried_as_white_first() {
        let mut image = RgbImage::filled(240, 160, [0, 90, 150]);
        crate::detect::testing::fill(&mut image, 170, 2, 60, 110, crate::color::WHITE);
        let faded = darken(&image, 12);
        let c = candidates(surface_level(&faded));
        assert_eq!(c.first(), Some(&12), "{c:?}");
        assert_eq!(brighten(&faded, 12).pixel(200, 50), crate::color::WHITE);
    }
}
