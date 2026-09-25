//! The Quest Log recap FireRed plays after CONTINUE: "Previously on your
//! quest…" in a slate bar across the top (rows 0–15, quest_log.c's
//! `WIN_TOP_BAR`), a matching bar at the bottom (rows 144–159) and the
//! last actions replayed in grey between them (rec2-880,
//! switch-goal-14-880). When it ends the bars shrink to the screen's
//! edges over the map in colour again (rec2-960). B skips it.

use pokebot_core::RgbImage;
use pokebot_state::Region;

use super::px;
use crate::color::{near, Rgb};

/// The bars' fill: (66,89,107) on the emulator, (64,91,108) on the Switch.
const BAR: Rgb = [66, 89, 107];
/// Per-channel tolerance for the bars: tighter than the UI default, which
/// would take the plain grey (90,89,90) frames the emulator shows between
/// scenes (rec3) for bar.
const BAR_TOLERANCE: u8 = 12;

/// Share (per mille) of bar-coloured pixels in `region`.
fn bar_share(image: &RgbImage, region: Region) -> u32 {
    let (mut hits, mut total) = (0u32, 0u32);
    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            total += 1;
            if near(px(image, x, y), BAR, BAR_TOLERANCE) {
                hits += 1;
            }
        }
    }
    (hits * 1000).checked_div(total).unwrap_or(0)
}

/// Channel spread a grey pixel stays within (JPEG tints the Switch's
/// greys by a few levels).
const GREY_SPREAD: u8 = 12;

fn grey_share(image: &RgbImage, region: Region) -> u32 {
    let (mut grey, mut total) = (0u32, 0u32);
    for y in (region.y..region.y + region.height).step_by(2) {
        for x in (region.x..region.x + region.width).step_by(2) {
            let p = px(image, x, y);
            total += 1;
            if p.iter().max().unwrap() - p.iter().min().unwrap() <= GREY_SPREAD {
                grey += 1;
            }
        }
    }
    (grey * 1000).checked_div(total).unwrap_or(0)
}

/// Whether the frame is the Quest Log: the top bar (its text aside) over
/// a grey replay, or, as it ends, both bars' last rows at the edges.
pub fn detect(image: &RgbImage) -> bool {
    let row = |y: u32| bar_share(image, Region::new(0, y, 240, 1));
    if row(0) < 950 {
        return false;
    }
    let replay = bar_share(image, Region::new(0, 0, 240, 16)) >= 600
        && grey_share(image, Region::new(0, 20, 240, 88)) >= 970;
    // The bars retracting at the end leave a row or two each, over the
    // map. The main menu's slate background (switch-goal-15 frame 790)
    // fills the rows' ends too, and its left margin.
    let ending = row(image.height() - 1) >= 950
        && row(8) < 500
        && bar_share(image, Region::new(0, 8, 4, 140)) < 200;
    replay || ending
}
