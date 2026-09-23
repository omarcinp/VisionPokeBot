//! Message boxes, information pages and the red "continue" arrow.

use pokebot_core::RgbImage;
use pokebot_state::{DialogueKind, DialogueObservation, Region};

use super::{px, share};
use crate::color::{luma, near, ARROW_RED, INFO_HEADER, MESSAGE_BORDER, TOLERANCE, WHITE};

/// Interior of the standard bottom message box.
pub const MESSAGE_TEXT: Region = Region {
    x: 8,
    y: 118,
    width: 224,
    height: 36,
};
/// Body of a full-screen information page (below the header bar).
pub const INFO_BODY: Region = Region {
    x: 0,
    y: 16,
    width: 240,
    height: 144,
};

pub fn detect(image: &RgbImage) -> Option<DialogueObservation> {
    let (kind, region) = if is_message_box(image) {
        (DialogueKind::MessageBox, MESSAGE_TEXT)
    } else if super::battle::is_battle_text_box(image) {
        (DialogueKind::BattleText, MESSAGE_TEXT)
    } else if is_info_page(image) {
        (DialogueKind::InfoPage, INFO_BODY)
    } else {
        return None;
    };
    let arrow = find_arrow(image, region);
    Some(DialogueObservation {
        kind,
        region,
        waiting_for_input: arrow.is_some(),
        arrow,
        stable_frames: 0,
        text_cells: text_cells(image, region, arrow),
    })
}

fn is_message_box(image: &RgbImage) -> bool {
    let top = share(image, Region::new(12, 115, 216, 1), MESSAGE_BORDER, 2);
    let bottom = share(image, Region::new(12, 156, 216, 1), MESSAGE_BORDER, 2);
    let left = share(image, Region::new(5, 126, 1, 20), MESSAGE_BORDER, 2);
    let right = share(image, Region::new(234, 126, 1, 20), MESSAGE_BORDER, 2);
    let interior = share(image, MESSAGE_TEXT, WHITE, 2);
    top >= 800 && bottom >= 800 && left >= 800 && right >= 800 && interior >= 500
}

fn is_info_page(image: &RgbImage) -> bool {
    share(image, Region::new(0, 1, 240, 6), INFO_HEADER, 2) >= 850
}

/// The downward red triangle: rows of red pixels shrinking by two each row
/// (9, 7, 5, 3, 1 at native size). Red text never forms this shape.
pub fn find_arrow(image: &RgbImage, region: Region) -> Option<Region> {
    let red = |x: u32, y: u32| region.contains(x, y) && near(px(image, x, y), ARROW_RED, TOLERANCE);
    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            // Candidate top-left of the arrow: a red run starting here.
            if !red(x, y) || red(x.wrapping_sub(1), y) || red(x, y.wrapping_sub(1)) {
                continue;
            }
            let run = |yy: u32, start: u32| (start..).take_while(|xx| red(*xx, yy)).count() as u32;
            let top = run(y, x);
            if !(7..=11).contains(&top) {
                continue;
            }
            let mut width = top;
            let mut row = 1;
            let mut shape_ok = true;
            while width > 2 {
                let start = x + row;
                let next = run(y + row, start);
                if next + 2 != width || red(start - 1, y + row) {
                    shape_ok = false;
                    break;
                }
                width = next;
                row += 1;
            }
            if shape_ok && row >= 4 {
                return Some(Region::new(x, y, top, row));
            }
        }
    }
    None
}

/// Mean luma of each 8×8 cell of `region`, with cells near the arrow zeroed
/// so its bouncing (a few pixels up and down) does not look like new text.
pub fn text_cells(image: &RgbImage, region: Region, arrow: Option<Region>) -> Vec<u8> {
    let exclude = arrow.map(|a| {
        Region::new(
            a.x.saturating_sub(2),
            a.y.saturating_sub(6),
            a.width + 4,
            a.height + 12,
        )
    });
    let mut cells = Vec::new();
    for cy in (region.y..region.y + region.height).step_by(8) {
        for cx in (region.x..region.x + region.width).step_by(8) {
            let cell = Region::new(
                cx,
                cy,
                8.min(region.x + region.width - cx),
                8.min(region.y + region.height - cy),
            );
            if exclude.is_some_and(|e| e.intersects(&cell)) {
                cells.push(0); // excluded
                continue;
            }
            let mut sum = 0u32;
            for y in cell.y..cell.y + cell.height {
                for x in cell.x..cell.x + cell.width {
                    sum += u32::from(luma(px(image, x, y)));
                }
            }
            // 0 marks exclusion; real cells in a text window are never black.
            cells.push(((sum / (cell.width * cell.height)) as u8).max(1));
        }
    }
    cells
}

/// Number of cells whose brightness changed noticeably. Cells excluded (0)
/// in either grid are ignored.
pub fn changed_cells(a: &[u8], b: &[u8]) -> usize {
    if a.len() != b.len() {
        return a.len().max(b.len());
    }
    a.iter()
        .zip(b)
        .filter(|(x, y)| **x != 0 && **y != 0 && x.abs_diff(**y) > 12)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::testing::*;

    fn scene() -> RgbImage {
        let mut image = RgbImage::filled(240, 160, [66, 138, 132]);
        draw_message_box(&mut image);
        image
    }

    #[test]
    fn finds_box_and_arrow() {
        let mut image = scene();
        assert!(!detect(&image).unwrap().waiting_for_input);
        draw_arrow(&mut image, 100, 140);
        let found = detect(&image).unwrap();
        assert_eq!(found.kind, DialogueKind::MessageBox);
        assert_eq!(found.arrow, Some(Region::new(100, 140, 9, 5)));
    }

    #[test]
    fn red_blocks_are_not_arrows() {
        let mut image = scene();
        fill(&mut image, 50, 125, 9, 5, ARROW_RED);
        assert!(!detect(&image).unwrap().waiting_for_input);
    }

    #[test]
    fn arrow_bounce_does_not_change_text_cells() {
        let mut a = scene();
        fill(&mut a, 20, 124, 40, 8, crate::color::TEXT_GRAY);
        let mut b = a.clone();
        draw_arrow(&mut a, 100, 140);
        draw_arrow(&mut b, 100, 142);
        let (da, db) = (detect(&a).unwrap(), detect(&b).unwrap());
        assert_eq!(changed_cells(&da.text_cells, &db.text_cells), 0);
        let mut c = b.clone();
        fill(&mut c, 20, 124, 40, 8, WHITE);
        assert!(changed_cells(&db.text_cells, &detect(&c).unwrap().text_cells) >= 3);
    }

    #[test]
    fn no_box_on_plain_frames() {
        assert!(detect(&RgbImage::filled(240, 160, [66, 138, 132])).is_none());
    }
}
