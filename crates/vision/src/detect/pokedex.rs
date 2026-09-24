//! The Pokédex entry page shown after catching a new species.

use pokebot_core::RgbImage;
use pokebot_state::Region;

use super::share;
use crate::color::{Rgb, WHITE};

const TAN: Rgb = [198, 178, 140];
const DIVIDER: Rgb = [123, 97, 57];
const CREAM: Rgb = [231, 219, 198];

/// The brown divider between the upper and lower page (y 89..=90).
const DIVIDER_ROWS: Region = Region {
    x: 3,
    y: 89,
    width: 234,
    height: 2,
};
/// The tan title bar (habitat text in white on it).
const TITLE_BAR: Region = Region {
    x: 0,
    y: 0,
    width: 240,
    height: 16,
};
/// White upper page (number, name, category, HT/WT, sprite in black).
const UPPER_PAGE: Region = Region {
    x: 3,
    y: 19,
    width: 234,
    height: 69,
};
/// Cream lower page (the description in black).
const LOWER_PAGE: Region = Region {
    x: 3,
    y: 92,
    width: 234,
    height: 49,
};

/// The Pokédex entry page: brown divider, tan title bar, white upper page
/// and cream lower page. Measured shares on `pokedex-page.png`: divider
/// 1000‰, title 861‰, upper 894‰, lower 786‰; the thresholds leave room
/// for text and capture blur.
pub fn is_page(image: &RgbImage) -> bool {
    share(image, DIVIDER_ROWS, DIVIDER, 2) >= 800
        && share(image, TITLE_BAR, TAN, 2) >= 600
        && share(image, UPPER_PAGE, WHITE, 2) >= 600
        && share(image, LOWER_PAGE, CREAM, 2) >= 500
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn fixture(name: &str) -> Option<RgbImage> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        pokebot_video::png::load(root.join("captures/fixtures").join(name)).ok()
    }

    #[test]
    fn the_entry_page_is_found() {
        let Some(image) = fixture("pokedex-page.png") else {
            return;
        };
        assert!(is_page(&image));
    }

    #[test]
    fn other_screens_are_not_the_entry_page() {
        for name in [
            "battle-gotcha.png",
            "mart-list.png",
            "battle-wild-caught.png",
            "nickname-prompt.png",
            "bag-items.png",
            "mtmoon-1f.png",
            "mtmoon-entry-intro.png",
        ] {
            let Some(image) = fixture(name) else {
                continue;
            };
            assert!(!is_page(&image), "{name}");
        }
    }
}
