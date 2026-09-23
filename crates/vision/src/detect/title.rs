//! The title screen: orange band along the top, dark red band at the bottom.

use pokebot_core::RgbImage;
use pokebot_state::Region;

use super::share;
use crate::color::{TITLE_BOTTOM, TITLE_TOP};

pub fn is_title(image: &RgbImage) -> bool {
    share(image, Region::new(0, 0, 240, 6), TITLE_TOP, 2) >= 800
        && share(image, Region::new(0, 152, 240, 8), TITLE_BOTTOM, 2) >= 400
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::testing::fill;

    #[test]
    fn recognises_title_bands() {
        let mut image = RgbImage::filled(240, 160, [0, 0, 0]);
        assert!(!is_title(&image));
        fill(&mut image, 0, 0, 240, 6, TITLE_TOP);
        fill(&mut image, 0, 152, 240, 8, TITLE_BOTTOM);
        assert!(is_title(&image));
    }
}
