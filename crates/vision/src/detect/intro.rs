//! Boot and new-game intro scenes without a text box: the copyright
//! notice, the Game Freak shooting star, and Professor Oak's stage
//! between his speech pages. Measured on the emulator (rec2, live-story)
//! and the Switch (switch-goal-14-230).

use pokebot_core::RgbImage;
use pokebot_state::Region;

use super::px;
use crate::color::{near, Rgb};

/// The copyright notice's four lines ("©2004 Pokémon." … "©1995-2004 GAME
/// FREAK inc.") occupy exactly these rows, between x 56 and 183.
const COPYRIGHT_LINES: [u32; 4] = [48, 64, 80, 96];
const COPYRIGHT_X: std::ops::Range<u32> = 56..184;

/// Whether `p` is within `tolerance` of `bg` on every channel.
fn is_bg(p: Rgb, bg: Rgb, tolerance: u8) -> bool {
    near(p, bg, tolerance)
}

/// The copyright notice: grey-scale text lines on a plain background. The
/// background is black once shown, and passes through greys while the
/// screen fades in from white (live-story-24, -28, -32; the text keeps
/// its contrast); only the four text rows have anything on them.
pub fn is_copyright(image: &RgbImage) -> bool {
    let bg = px(image, 0, 0);
    let grey = bg.iter().max().unwrap() - bg.iter().min().unwrap() <= 12;
    if !grey {
        return false;
    }
    let in_text = |x: u32, y: u32| {
        COPYRIGHT_X.contains(&x) && COPYRIGHT_LINES.iter().any(|&t| (t..t + 8).contains(&y))
    };
    // Everything outside the text lines is background (slack: 0.5% of the
    // 9600 pixels sampled).
    let slack = image.width() * image.height() / 4 / 200;
    let mut off = 0u32;
    for y in (0..image.height()).step_by(2) {
        for x in (0..image.width()).step_by(2) {
            if !in_text(x, y) && !is_bg(px(image, x, y), bg, 20) {
                off += 1;
                if off > slack {
                    return false;
                }
            }
        }
    }
    // Every line has ink.
    COPYRIGHT_LINES.iter().all(|&top| {
        let ink = (top..top + 8)
            .flat_map(|y| COPYRIGHT_X.map(move |x| (x, y)))
            .filter(|&(x, y)| !is_bg(px(image, x, y), bg, 12))
            .count();
        ink >= 40
    })
}

/// The shooting star's night band (rows 32–127) between black bars.
const STAR_BAND: Rgb = [24, 40, 74];

/// The Game Freak shooting star (and the logo it draws): a navy band at
/// rows 32–127 between black bars (live-story-224, rec2-260).
pub fn is_game_freak(image: &RgbImage) -> bool {
    // Cheap rejection first (this runs on every frame).
    if !near(px(image, 4, 40), STAR_BAND, 24) || !near(px(image, 236, 120), STAR_BAND, 24) {
        return false;
    }
    let black = |region: Region| super::share(image, region, [0, 0, 0], 2) >= 970;
    black(Region::new(0, 0, 240, 30))
        && black(Region::new(0, 130, 240, 30))
        && super::share(image, Region::new(0, 34, 240, 92), STAR_BAND, 2) >= 600
}

/// Oak's stage: the pale sky (231,243,239) down to row 83, a gradient,
/// and a teal floor from row 108 ((66,138,132); lighter, up to
/// (156,195,189), while a Pokémon is brought out: live-story-3012..3016),
/// with Oak, the player or the rival on the platform in the middle. Only
/// the sides are checked.
const OAK_SKY: Rgb = [231, 243, 239];

/// How bright Oak's stage is, in 1/256ths (256 = as drawn), when the
/// frame is his stage: the intro fades it in and out of black, scaling
/// every colour (live-story-1516 ×0.35, -1544 ×0.86).
pub fn oak_stage_brightness(image: &RgbImage) -> Option<u32> {
    let sky = [Region::new(0, 4, 60, 72), Region::new(180, 4, 60, 72)];
    let floor = [Region::new(0, 124, 40, 32), Region::new(200, 124, 40, 32)];
    // Brightness from the sky's green channel at the top-left corner.
    let k = u32::from(px(image, 2, 2)[1]) * 256 / u32::from(OAK_SKY[1]);
    if !(48..=272).contains(&k) {
        return None;
    }
    let sky_colour = OAK_SKY.map(|v| (u32::from(v) * k.min(256) / 256) as u8);
    // Cheap rejection first (this runs on every frame): both top corners.
    if !near(px(image, 2, 2), sky_colour, 24) || !near(px(image, 237, 2), sky_colour, 24) {
        return None;
    }
    let floor_colour = px(image, 10, 140);
    let [r, g, b] = floor_colour.map(u32::from);
    let teal = g >= r + 16 && g * 10 >= r * 12 && b * 10 >= r * 12;
    let share = |regions: &[Region], colour: Rgb| {
        let (mut hits, mut total) = (0u32, 0u32);
        for region in regions {
            for y in (region.y..region.y + region.height).step_by(2) {
                for x in (region.x..region.x + region.width).step_by(2) {
                    total += 1;
                    if near(px(image, x, y), colour, 24) {
                        hits += 1;
                    }
                }
            }
        }
        hits * 1000 / total.max(1)
    };
    (teal && share(&sky, sky_colour) >= 900 && share(&floor, floor_colour) >= 900).then_some(k)
}
