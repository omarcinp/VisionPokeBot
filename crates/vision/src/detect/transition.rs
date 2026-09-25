//! Screens between screens: fades, the battle intro's opening window, the
//! cave battle wipe and the dungeon map preview. None of them shows state
//! (the map is half hidden or dimmed, the text is last page's), so they
//! must never be localized or read; `ScreenState::Transition` makes the
//! sensor skip them and the agent wait.
//!
//! Dark maps are the hazard: Mt. Moon's rooms sit in black void and its
//! floor is dim, so every rule here needs geometry or a palette signature
//! a map cannot produce, not just "lots of black".

use pokebot_core::RgbImage;
use pokebot_state::Region;

use super::px;
use crate::color::{luma, Rgb};

/// Near-black as transitions draw it: GBA black, JPEG-softened on the
/// Switch (its black rows measure up to (4,2,3)).
fn is_black(p: Rgb) -> bool {
    p.iter().all(|&c| c <= 40)
}

/// Whether row `y` is black edge to edge (every other pixel, 97%).
fn black_row(image: &RgbImage, y: u32) -> bool {
    let n = (0..image.width())
        .step_by(2)
        .filter(|&x| is_black(px(image, x, y)))
        .count() as u32;
    n * 1000 >= image.width().div_ceil(2) * 970
}

/// Share (per mille) of near-black pixels in `region`, every other pixel.
fn black_share(image: &RgbImage, region: Region) -> u32 {
    let (mut black, mut total) = (0u32, 0u32);
    for y in (region.y..region.y + region.height).step_by(2) {
        for x in (region.x..region.x + region.width).step_by(2) {
            total += 1;
            if is_black(px(image, x, y)) {
                black += 1;
            }
        }
    }
    (black * 1000).checked_div(total).unwrap_or(0)
}

/// The battle scene opening from the middle after the wipe: black above
/// and below, the battle background showing in a band `[80 − k, 80 + k]`
/// that grows each frame (rows 57–103, 53–107, 71–89 … on the emulator,
/// the same rows on the Switch: the edges are crisp). Band edges always
/// add up to 160. A cave map framed by void can only have edges on its
/// 8-pixel tile grid, which with the camera at rest add up to 159, so
/// the exact sum tells the two apart.
pub fn is_battle_intro(image: &RgbImage) -> bool {
    let h = image.height();
    // The band shows the battle background edge to edge (the title
    // screen's flash across the Charizard silhouette is mostly black).
    if !black_row(image, 0)
        || !black_row(image, h - 1)
        || black_share(image, Region::new(0, h / 2, image.width(), 1)) > 100
    {
        return false;
    }
    let Some(top) = (0..h / 2).find(|&y| !black_row(image, y)) else {
        return false;
    };
    let Some(bottom) = (h / 2..h).rev().find(|&y| !black_row(image, y)) else {
        return false;
    };
    top >= 8 && top + bottom == h && (top..=bottom).all(|y| !black_row(image, y))
}

/// Centre of the clockwise wipe (the screen's).
const WIPE_CENTRE: (f32, f32) = (120.0, 80.0);
/// Radii the wipe is sampled on: outside the player sprite (which
/// reaches about 16 px from the centre), inside the screen's height.
const WIPE_RADII: [f32; 4] = [28.0, 44.0, 60.0, 76.0];
/// Angular samples per circle (5° each).
const WIPE_STEPS: usize = 72;

/// Where the black arc on the circle of radius `r` ends: the first of
/// the samples (clockwise from 12 o'clock) that is not black, if the arc
/// starts at 12 o'clock and the rest of the circle is mostly not black.
fn wipe_arc(image: &RgbImage, r: f32) -> Option<usize> {
    let black = |i: usize| {
        let a = (i as f32 + 0.5) * std::f32::consts::TAU / WIPE_STEPS as f32;
        let x = WIPE_CENTRE.0 + r * a.sin();
        let y = WIPE_CENTRE.1 - r * a.cos();
        is_black(px(image, x as u32, y as u32))
    };
    let end = (0..WIPE_STEPS).find(|&i| !black(i))?;
    if end < 2 {
        return None;
    }
    let rest = WIPE_STEPS - end;
    let lit = (end..WIPE_STEPS).filter(|&i| !black(i)).count();
    (lit * 4 >= rest * 3).then_some(end)
}

/// The cave battle wipe (`B_TRANSITION_CLOCKWISE_WIPE`, wild battles in
/// caves): black sweeps clockwise around the screen's centre from 12
/// o'clock, so on every circle around the centre the black is one arc
/// from 12 o'clock to the same angle (rec2-80720 a quarter, rec2-80740
/// three quarters). A cave's void is tile blocks: its edges don't run
/// along one ray from the centre on every circle. A half sweep is left
/// out: its edge is the vertical line x = 120, which a void edge can be
/// while the camera is mid-step.
pub fn is_clockwise_wipe(image: &RgbImage) -> bool {
    let mut ends = Vec::with_capacity(WIPE_RADII.len());
    for r in WIPE_RADII {
        match wipe_arc(image, r) {
            Some(end) => ends.push(end),
            None => return false,
        }
    }
    let (min, max) = (*ends.iter().min().unwrap(), *ends.iter().max().unwrap());
    let half = WIPE_STEPS / 2;
    min >= 2 && max - min <= 2 && !(half - 2..=half + 2).contains(&min)
}

/// The dungeon map preview shown on first entry (map_preview_screen.c:
/// "MT. MOON" in a window at the top left, the artwork below): black to
/// the right of the name (x 112.., y 0..=21), black rows 22–23 and 136–159
/// around the artwork, the artwork itself not dark (rec2-75760, and while
/// fading in, rec2-75740). A map with its name popup has void in the
/// middle rows, not a bright band.
pub fn is_map_preview(image: &RgbImage) -> bool {
    [22, 23, 136, 147, 159].iter().all(|&y| black_row(image, y))
        && black_share(image, Region::new(112, 0, 128, 22)) >= 970
        && black_share(image, Region::new(0, 24, 240, 112)) <= 100
        && black_share(image, Region::new(4, 2, 96, 16)) <= 100
}

/// Luma a fade leaves every pixel under, and how many pixels may still
/// reach it. Every pixel counts: in a dark cave the player sprite's white
/// and skin (luma 208–238, some 20–40 pixels) may be all there is.
const FADE_CEILING: u8 = 176;
const FADE_BRIGHT_MAX: u32 = 8;

/// A frame dimmed by a fade: nothing reaches the brightness every
/// in-game screen has somewhere (white UI, the player sprite's
/// highlights: Mt. Moon's floor is dim but the player reaches luma 238;
/// rec2-202060 has 179 pixels at luma ≥ 200). Fading frames have none:
/// the dim Pokémon Center (switch-goal-14-27520) peaks at 109, Viridian
/// City under a fade at 166 (switch-goal-15-11430), Oak's stage at 90–156
/// (live-story-1516, -1530). Uniform frames are caught before this.
pub fn is_faded(image: &RgbImage) -> bool {
    let bytes = image.as_bytes();
    let bright = bytes
        .chunks_exact(3)
        .filter(|p| luma([p[0], p[1], p[2]]) >= FADE_CEILING)
        .take(FADE_BRIGHT_MAX as usize)
        .count() as u32;
    bright < FADE_BRIGHT_MAX
}

/// Rows per band of the Poké Ball trail (five bands, one ball each).
const TRAIL_BAND: u32 = 32;

/// Black run from the left (`from_left`) or right edge of row `y`.
fn black_run(image: &RgbImage, y: u32, from_left: bool) -> u32 {
    let w = image.width();
    (0..w)
        .map(|i| if from_left { i } else { w - 1 - i })
        .take_while(|&x| is_black(px(image, x, y)))
        .count() as u32
}

/// Whether band `b` of the Poké Ball trail has a ball rolling through it
/// from one side: black behind the ball, ending on the ball's round
/// front (the run is shortest at the band's middle row, 8–32 px shorter
/// at its top and bottom rows, and never grows toward the middle), with
/// the ball's red just past it.
fn trail_band(image: &RgbImage, b: u32, from_left: bool) -> bool {
    let (w, top) = (image.width(), b * TRAIL_BAND);
    // Cheap rejections first (this runs on every frame): the middle row's
    // run stops short of the far side, the top row's run reaches beyond
    // it, and the ball's red is just past the middle row's.
    let middle = black_run(image, top + TRAIL_BAND / 2 - 1, from_left);
    if !(8..=w - 40).contains(&middle) || black_run(image, top + 1, from_left) < middle + 8 {
        return false;
    }
    let red = (0..36).any(|d| {
        let x = if from_left {
            middle + d
        } else {
            w.saturating_sub(middle + d + 1)
        };
        x < w && {
            let p = px(image, x, top + 8 + d % 16);
            p[0] >= 150 && p[1] <= 80 && p[2] <= 80
        }
    });
    if !red {
        return false;
    }
    let runs: Vec<u32> = (top + 1..top + TRAIL_BAND - 1)
        .step_by(2)
        .map(|y| black_run(image, y, from_left))
        .collect();
    let mid = runs.len() / 2;
    let centre = runs[mid];
    let edge = runs[0].min(runs[runs.len() - 1]);
    (centre + 8..=centre + 32).contains(&edge)
        && runs[..=mid].windows(2).all(|w| w[1] <= w[0] + 1)
        && runs[mid..].windows(2).all(|w| w[1] + 1 >= w[0])
}

/// The Poké Ball trail into a trainer battle (`B_TRANSITION_POKEBALLS_TRAIL`):
/// five 32-row bands, a Poké Ball rolling through each from alternate
/// sides and leaving black behind it (switch-goal-14 frames 66670–66700).
/// A void's edges are straight tile edges; the ball's front is round.
pub fn is_pokeballs_trail(image: &RgbImage) -> bool {
    (0..image.height() / TRAIL_BAND)
        .any(|b| trail_band(image, b, true) || trail_band(image, b, false))
}
