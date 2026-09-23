//! The naming screen: which key has focus and how many characters are typed.

use pokebot_core::RgbImage;
use pokebot_state::{KeyboardFocus, NamingObservation, Region};

use super::{px, share};
use crate::color::{near, KEYBOARD_PANEL, TEXT_GRAY};

/// Left edge of each key column and top of each key row.
pub const KEY_COLUMNS: [u32; 8] = [32, 44, 56, 88, 100, 112, 124, 155];
pub const KEY_ROWS: [u32; 4] = [80, 96, 112, 128];
pub const KEY_WIDTH: u32 = 12;
pub const KEY_HEIGHT: u32 = 16;
/// Name field: one 8-px slot per character.
const SLOT_X: u32 = 98;
const SLOT_WIDTH: u32 = 8;
const SLOT_Y: u32 = 50;
const SLOT_HEIGHT: u32 = 10;
pub const MAX_NAME_LENGTH: u8 = 7;

pub fn detect(image: &RgbImage) -> Option<NamingObservation> {
    // The keyboard panel fills most of its area between the keys, the
    // header bar is blue and the side margins show the cream/white stripes
    // (water in the overworld can look like the panel on its own).
    if share(image, Region::new(20, 76, 150, 72), KEYBOARD_PANEL, 2) < 450
        || share(
            image,
            Region::new(0, 0, 240, 8),
            crate::color::INFO_HEADER,
            2,
        ) < 600
        || share(image, Region::new(0, 60, 12, 90), [255, 251, 255], 1)
            + share(image, Region::new(0, 60, 12, 90), [247, 235, 198], 1)
            < 700
    {
        return None;
    }
    Some(NamingObservation {
        focus: focus(image),
        typed: typed(image),
    })
}

/// The focused key is outlined by a (pulsing) ring, so its border pixels are
/// not panel-coloured; unfocused keys have plain panel borders.
fn focus(image: &RgbImage) -> KeyboardFocus {
    let mut best = (0u32, KeyboardFocus::Buttons);
    for (row, &y) in KEY_ROWS.iter().enumerate() {
        for (column, &x) in KEY_COLUMNS.iter().enumerate() {
            let score = ring_pixels(image, x, y);
            if score > best.0 {
                best = (
                    score,
                    KeyboardFocus::Key {
                        column: column as u8,
                        row: row as u8,
                    },
                );
            }
        }
    }
    let perimeter = 2 * (KEY_WIDTH + KEY_HEIGHT) - 4;
    if best.0 * 2 >= perimeter {
        best.1
    } else {
        KeyboardFocus::Buttons
    }
}

fn ring_pixels(image: &RgbImage, x0: u32, y0: u32) -> u32 {
    let mut count = 0;
    for y in y0..y0 + KEY_HEIGHT {
        for x in x0..x0 + KEY_WIDTH {
            let edge = x == x0 || x == x0 + KEY_WIDTH - 1 || y == y0 || y == y0 + KEY_HEIGHT - 1;
            if edge && !near(px(image, x, y), KEYBOARD_PANEL, 20) {
                count += 1;
            }
        }
    }
    count
}

fn typed(image: &RgbImage) -> u8 {
    (0..u32::from(MAX_NAME_LENGTH))
        .take_while(|slot| {
            let x0 = SLOT_X + slot * SLOT_WIDTH;
            (SLOT_Y..SLOT_Y + SLOT_HEIGHT)
                .any(|y| (x0..x0 + SLOT_WIDTH).any(|x| near(px(image, x, y), TEXT_GRAY, 20)))
        })
        .count() as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::WHITE;
    use crate::detect::testing::fill;

    fn keyboard() -> RgbImage {
        let mut image = RgbImage::filled(240, 160, [247, 235, 198]);
        fill(&mut image, 0, 0, 240, 8, crate::color::INFO_HEADER);
        fill(&mut image, 20, 76, 150, 72, KEYBOARD_PANEL);
        image
    }

    fn ring(image: &mut RgbImage, column: usize, row: usize) {
        let (x, y) = (KEY_COLUMNS[column], KEY_ROWS[row]);
        fill(image, x, y, KEY_WIDTH, KEY_HEIGHT, [247, 195, 214]);
        fill(
            image,
            x + 1,
            y + 1,
            KEY_WIDTH - 2,
            KEY_HEIGHT - 2,
            KEYBOARD_PANEL,
        );
    }

    #[test]
    fn finds_focused_key_and_typed_count() {
        let mut image = keyboard();
        ring(&mut image, 4, 2);
        fill(&mut image, SLOT_X + 2, 52, 3, 6, TEXT_GRAY);
        fill(&mut image, SLOT_X + 10, 52, 3, 6, TEXT_GRAY);
        let naming = detect(&image).unwrap();
        assert_eq!(naming.focus, KeyboardFocus::Key { column: 4, row: 2 });
        assert_eq!(naming.typed, 2);
    }

    #[test]
    fn no_ring_means_button_column() {
        let mut image = keyboard();
        fill(&mut image, 36, 84, 4, 8, WHITE); // a letter, not a ring
        assert_eq!(detect(&image).unwrap().focus, KeyboardFocus::Buttons);
        assert!(detect(&RgbImage::filled(240, 160, WHITE)).is_none());
    }
}
