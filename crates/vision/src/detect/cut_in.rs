//! The field move cut-in: after "<mon> used CUT!" (and every other field
//! move), a black band crosses the screen (y 40..=119) with white streaks
//! and the Pokémon sliding through it. The map is hidden in the middle, so
//! it is a transition, not a place to localize; the obstacle's own
//! animation follows it.

use pokebot_core::RgbImage;

use super::px;
use crate::color::{near, WHITE};

const BAND: std::ops::RangeInclusive<u32> = 40..=119;

fn black_share(image: &RgbImage, y: u32) -> u32 {
    let n = (0..image.width())
        .step_by(2)
        .filter(|&x| px(image, x, y).iter().all(|&c| c < 24))
        .count() as u32;
    n * 1000 / image.width().div_ceil(2)
}

pub fn detect(image: &RgbImage) -> bool {
    // Cheap rejection first (this runs on every frame): the band's middle.
    if black_share(image, 80) < 700 {
        return false;
    }
    // The band opens from the middle: its rows are black edge to edge
    // (the streaks and the Pokémon aside), the rows around it are not.
    let rows: Vec<u32> = BAND.step_by(2).collect();
    let black = rows
        .iter()
        .filter(|&&y| black_share(image, y) >= 700)
        .count();
    if black * 2 < rows.len() {
        return false;
    }
    if black_share(image, BAND.start() - 4) >= 300 || black_share(image, BAND.end() + 3) >= 300 {
        return false;
    }
    // The white streaks: nothing a dark cave's void has.
    let white = BAND
        .clone()
        .step_by(2)
        .flat_map(|y| (0..image.width()).step_by(2).map(move |x| (x, y)))
        .filter(|&(x, y)| near(px(image, x, y), WHITE, 8))
        .count();
    white >= 20
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Option<RgbImage> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        pokebot_video::png::load(root.join("captures/fixtures").join(name)).ok()
    }

    #[test]
    fn the_cut_in_band_is_found_and_nothing_else() {
        if let Some(image) = fixture("emu-cut-in-band.png") {
            assert!(detect(&image));
        }
        for name in [
            "emu-cut-used.png",
            "emu-cut-question.png",
            "mtmoon-1f.png",
            "mtmoon-battle-wipe.png",
            "mtmoon-intro-fade.png",
            "emu-tools-overworld.png",
            "emu-trainer-battle-wipe-black.png",
        ] {
            if let Some(image) = fixture(name) {
                assert!(!detect(&image), "{name}");
            }
        }
    }
}
