//! FireRed palette colours (as rendered by the GBA, 5-bit expanded) and
//! tolerant colour matching for noisy capture sources.

pub type Rgb = [u8; 3];

/// Per-channel tolerance for matching UI colours.
pub const TOLERANCE: u8 = 24;

pub const WHITE: Rgb = [255, 251, 255];
pub const TEXT_GRAY: Rgb = [99, 97, 99];
pub const ARROW_RED: Rgb = [231, 8, 8];
pub const MESSAGE_BORDER: Rgb = [74, 113, 165];
/// Frame of the message box signs open (grey instead of blue).
pub const SIGN_BORDER: Rgb = [107, 115, 123];
pub const MESSAGE_INNER: Rgb = [165, 211, 231];
pub const INFO_HEADER: Rgb = [0, 121, 198];
pub const TITLE_TOP: Rgb = [255, 89, 0];
pub const TITLE_BOTTOM: Rgb = [140, 0, 0];
pub const KEYBOARD_PANEL: Rgb = [123, 170, 198];

pub fn near(pixel: Rgb, color: Rgb, tolerance: u8) -> bool {
    pixel
        .iter()
        .zip(color)
        .all(|(a, b)| a.abs_diff(b) <= tolerance)
}

/// Strongly red: the continue arrow in any of its shades (it pulses between
/// about 198 and 231 red, and compressed captures soften its edges). Other
/// red UI never forms the arrow's shape, so hue alone is enough here.
pub fn is_arrow_red(p: Rgb) -> bool {
    p[0] >= 150 && p[1] <= 64 && p[2] <= 64
}

pub fn luma(p: Rgb) -> u8 {
    ((299 * u32::from(p[0]) + 587 * u32::from(p[1]) + 114 * u32::from(p[2])) / 1000) as u8
}
