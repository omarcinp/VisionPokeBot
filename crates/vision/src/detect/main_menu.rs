//! The title's main menu (CONTINUE / NEW GAME / ...): stacked boxes on a
//! blue background; the selected box is white, the others gray. There is no
//! ▶ cursor.

use pokebot_core::RgbImage;
use pokebot_state::{MenuObservation, Region};

use super::{px, share};
use crate::color::{near, Rgb, WHITE};

const BACKGROUND: Rgb = [74, 81, 148];
const UNSELECTED: Rgb = [140, 138, 148];
/// Column used to walk down through the boxes.
const PROBE_X: u32 = 120;

pub fn detect(image: &RgbImage) -> Option<MenuObservation> {
    // Background down both side edges.
    if share(image, Region::new(0, 0, 3, 160), BACKGROUND, 1) < 900
        || share(image, Region::new(237, 0, 3, 160), BACKGROUND, 1) < 900
    {
        return None;
    }
    // Box interiors along the probe column: runs of white or gray.
    let mut boxes: Vec<(u32, u32, bool)> = Vec::new();
    let mut y = 0;
    while y < image.height() {
        let p = px(image, PROBE_X, y);
        let selected = near(p, WHITE, 12);
        if selected || near(p, UNSELECTED, 12) {
            let start = y;
            while y < image.height() && near(px(image, PROBE_X, y), p, 12) {
                y += 1;
            }
            if y - start >= 10 {
                boxes.push((start, y - start, selected));
            }
        } else {
            y += 1;
        }
    }
    let selected = boxes.iter().position(|b| b.2)?;
    let (top, height, _) = boxes[selected];
    Some(MenuObservation {
        window: Region::new(8, top, 224, height),
        rows: boxes.len() as u8,
        cursor_row: selected as u8,
        cursor_y: top,
    })
}
