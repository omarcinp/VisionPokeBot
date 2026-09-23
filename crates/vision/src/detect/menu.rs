//! List menus: the ▶ cursor and the white window around it.

use pokebot_core::RgbImage;
use pokebot_state::{MenuObservation, Region};

use super::px;
use crate::color::{near, TEXT_GRAY, WHITE};

const CURSOR_ROWS: u32 = 9;
/// Menu items are 16 px apart.
const ROW_HEIGHT: u32 = 16;
const TOLERANCE: u8 = 20;

pub fn detect(image: &RgbImage) -> Option<MenuObservation> {
    let (cx, cy) = find_cursor(image)?;
    let white = |x: u32, y: u32| near(px(image, x, y), WHITE, TOLERANCE);
    // Walk out from the cursor through white window interior to the frame.
    let probe_x = cx.checked_sub(2)?;
    let mut top = cy;
    while top > 0 && white(probe_x, top - 1) {
        top -= 1;
    }
    let mut bottom = cy;
    while bottom + 1 < image.height() && white(probe_x, bottom + 1) {
        bottom += 1;
    }
    let mut left = probe_x;
    while left > 0 && white(left - 1, cy + 4) {
        left -= 1;
    }
    let mut right = cx;
    while right + 1 < image.width() && white(right + 1, top) {
        right += 1;
    }
    let height = bottom + 1 - top;
    let rows = (height / ROW_HEIGHT).max(1);
    let cursor_row = (cy.saturating_sub(top) / ROW_HEIGHT).min(rows - 1);
    Some(MenuObservation {
        window: Region::new(left, top, right + 1 - left, height),
        rows: rows.min(u32::from(u8::MAX)) as u8,
        cursor_row: cursor_row as u8,
        cursor_y: cy,
    })
}

/// Top-left of the ▶ cursor: a gray triangle whose rows are 1,2,3,4,5,4,3,2,1
/// pixels wide, with no gray immediately left or right of each row.
pub fn find_cursor(image: &RgbImage) -> Option<(u32, u32)> {
    let gray = |x: u32, y: u32| near(px(image, x, y), TEXT_GRAY, TOLERANCE);
    for y in 0..image.height().saturating_sub(CURSOR_ROWS) {
        for x in 1..image.width().saturating_sub(6) {
            let matches = (0..CURSOR_ROWS).all(|row| {
                let width = row.min(CURSOR_ROWS - 1 - row) + 1;
                (0..width).all(|col| gray(x + col, y + row))
                    && !gray(x + width, y + row)
                    && !gray(x - 1, y + row)
            });
            if matches {
                return Some((x, y));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::testing::*;

    /// A window like FireRed's YES/NO box: frame, white interior from y 14.
    fn window(rows: u32, cursor_row: u32) -> RgbImage {
        let mut image = RgbImage::filled(240, 160, [231, 243, 239]);
        fill(&mut image, 9, 9, 60, 16 * rows + 10, [41, 48, 49]);
        fill(&mut image, 14, 14, 50, 16 * rows + 4, WHITE);
        draw_cursor(&mut image, 17, 20 + 16 * cursor_row);
        image
    }

    #[test]
    fn reads_rows_and_cursor_position() {
        for (rows, cursor) in [(2, 0), (2, 1), (5, 3)] {
            let menu = detect(&window(rows, cursor)).unwrap();
            assert_eq!((menu.rows, menu.cursor_row), (rows as u8, cursor as u8));
            assert_eq!(menu.window.x, 14);
            assert_eq!(menu.window.y, 14);
        }
    }

    #[test]
    fn text_blocks_are_not_cursors() {
        let mut image = RgbImage::filled(240, 160, WHITE);
        fill(&mut image, 20, 20, 5, 9, TEXT_GRAY);
        assert!(find_cursor(&image).is_none());
    }
}
