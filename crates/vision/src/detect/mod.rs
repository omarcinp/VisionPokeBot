//! Individual UI detectors. Coordinates are canonical 240×160.

pub mod bag;
pub mod battle;
pub mod dialogue;
pub mod fly_map;
pub mod hud;
pub mod main_menu;
pub mod menu;
pub mod move_list;
pub mod naming;
pub mod pokedex;
pub mod shop;
pub mod title;
pub mod trainer_card;

use pokebot_core::RgbImage;
use pokebot_state::Region;

use crate::color::{near, Rgb, TOLERANCE};

pub(crate) fn px(image: &RgbImage, x: u32, y: u32) -> Rgb {
    image.pixel(x, y)
}

/// Share (per mille) of pixels in `region`, sampled every `step` pixels,
/// that match `color`.
pub(crate) fn share(image: &RgbImage, region: Region, color: Rgb, step: u32) -> u32 {
    let (mut hits, mut total) = (0u32, 0u32);
    let mut y = region.y;
    while y < region.y + region.height {
        let mut x = region.x;
        while x < region.x + region.width {
            total += 1;
            if near(px(image, x, y), color, TOLERANCE) {
                hits += 1;
            }
            x += step;
        }
        y += step;
    }
    (hits * 1000).checked_div(total).unwrap_or(0)
}

/// Share (per mille) of pixels in `region`, sampled every `step` pixels,
/// that match any of `colors`.
pub(crate) fn share_any(image: &RgbImage, region: Region, colors: &[Rgb], step: u32) -> u32 {
    let (mut hits, mut total) = (0u32, 0u32);
    let mut y = region.y;
    while y < region.y + region.height {
        let mut x = region.x;
        while x < region.x + region.width {
            total += 1;
            let p = px(image, x, y);
            if colors.iter().any(|&c| near(p, c, TOLERANCE)) {
                hits += 1;
            }
            x += step;
        }
        y += step;
    }
    (hits * 1000).checked_div(total).unwrap_or(0)
}

/// Test helpers that draw UI elements with the game's colours and shapes.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use pokebot_core::RgbImage;

    use crate::color::*;

    pub fn fill(image: &mut RgbImage, x0: u32, y0: u32, w: u32, h: u32, c: Rgb) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                image.put_pixel(x, y, c);
            }
        }
    }

    /// The standard bottom message box (outer border at x 5..=234, y 115..=156).
    pub fn draw_message_box(image: &mut RgbImage) {
        fill(image, 5, 115, 230, 42, MESSAGE_BORDER);
        fill(image, 6, 116, 228, 40, MESSAGE_INNER);
        fill(image, 8, 118, 224, 36, WHITE);
    }

    /// The red "continue" arrow with its top-left corner at (x, y).
    pub fn draw_arrow(image: &mut RgbImage, x: u32, y: u32) {
        for row in 0..5u32 {
            for col in row..9 - row {
                image.put_pixel(x + col, y + row, ARROW_RED);
            }
        }
    }

    /// The ▶ menu cursor with its top-left at (x, y).
    pub fn draw_cursor(image: &mut RgbImage, x: u32, y: u32) {
        for row in 0..9u32 {
            let width = row.min(8 - row) + 1;
            for col in 0..width {
                image.put_pixel(x + col, y + row, TEXT_GRAY);
            }
        }
    }
}
