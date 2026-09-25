//! The Pokédex: the entry page shown after catching a new species, and the
//! numerical list with its caught marks.

use pokebot_core::RgbImage;
use pokebot_state::{PokedexListObservation, Region};

use super::{px, share};
use crate::color::{luma, near, Rgb, TOLERANCE, WHITE};
use crate::text::Font;

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

/// The list (`pokedex_screen.c` `sWindowTemplate_OrderedListMenu`, tiles
/// (2, 2) 23×16, `maxShowed` 9, rows 14 px apart from y 18, item text at
/// x 72, the caught mark blitted 12×12 at x 56): tan title and footer bands
/// like the entry page, a white list window and a white side panel (the
/// TABLE OF CONTENTS page has its green counter there, and is told apart by
/// its title too).
const LIST_TITLE: Region = Region {
    x: 0,
    y: 0,
    width: 240,
    height: 16,
};
const LIST_FOOTER: Region = Region {
    x: 0,
    y: 146,
    width: 240,
    height: 14,
};
const LIST_WINDOW: Region = Region {
    x: 16,
    y: 16,
    width: 184,
    height: 128,
};
const LIST_SIDE: Region = Region {
    x: 204,
    y: 20,
    width: 32,
    height: 120,
};
const LIST_ROWS: u32 = 9;
const LIST_ROW_TOP: u32 = 18;
const LIST_ROW_PITCH: u32 = 14;
/// Name column: from the item text's x to the type icons at x 136.
const LIST_NAME_X: u32 = 72;
const LIST_NAME_WIDTH: u32 = 62;
const CAUGHT_MARK_X: u32 = 56;
const CAUGHT_MARK_SIZE: u32 = 12;
/// The mark's Poké Ball red (43 of the 144 pixels on `emu-pokedex.png`).
const CAUGHT_RED: Rgb = [239, 48, 0];
const CAUGHT_MIN_RED: u32 = 20;
/// The ▶: 9 rows from 2 px below the row top, in the cursor column.
const LIST_CURSOR_X: u32 = 20;
const LIST_CURSOR_WIDTH: u32 = 6;
const LIST_CURSOR_ROWS: u32 = 9;
/// Dark pixels for the ▶ (25 when drawn, 0 otherwise).
const CURSOR_MIN_DARK: u32 = 15;
const DARK_LUMA: u8 = 80;

/// The numerical list: every visible row's name and caught mark, and the
/// ▶ row.
pub fn list(image: &RgbImage, font: &Font) -> Option<PokedexListObservation> {
    if !is_list_frame(image) {
        return None;
    }
    let title = font.read(image, LIST_TITLE, &[]).join(" ");
    if !title.contains("LIST") {
        return None;
    }
    let rows = (0..LIST_ROWS)
        .map(|r| {
            let top = LIST_ROW_TOP + LIST_ROW_PITCH * r;
            let name = font
                .read(
                    image,
                    Region::new(LIST_NAME_X, top, LIST_NAME_WIDTH, LIST_ROW_PITCH),
                    &[],
                )
                .join(" ");
            let mark = Region::new(CAUGHT_MARK_X, top, CAUGHT_MARK_SIZE, CAUGHT_MARK_SIZE);
            (
                name,
                count(image, mark, |p| near(p, CAUGHT_RED, TOLERANCE)) >= CAUGHT_MIN_RED,
            )
        })
        .collect();
    let cursor = (0..LIST_ROWS).find(|&r| {
        let cell = Region::new(
            LIST_CURSOR_X,
            LIST_ROW_TOP + LIST_ROW_PITCH * r + 2,
            LIST_CURSOR_WIDTH,
            LIST_CURSOR_ROWS,
        );
        count(image, cell, |p| luma(p) <= DARK_LUMA) >= CURSOR_MIN_DARK
    });
    Some(PokedexListObservation {
        rows,
        cursor: cursor.map(|r| r as u8),
    })
}

/// Measured on `emu-pokedex.png`: title 896‰, footer 844‰, window 821‰,
/// side 993‰ (the contents page: 790, 914, 773, and the counter's green).
fn is_list_frame(image: &RgbImage) -> bool {
    share(image, LIST_TITLE, TAN, 2) >= 600
        && share(image, LIST_FOOTER, TAN, 2) >= 600
        && share(image, LIST_WINDOW, WHITE, 2) >= 600
        && share(image, LIST_SIDE, WHITE, 2) >= 800
}

fn count(image: &RgbImage, region: Region, hit: impl Fn(Rgb) -> bool) -> u32 {
    let mut n = 0;
    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            if x < image.width() && y < image.height() && hit(px(image, x, y)) {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn fixture(name: &str) -> Option<RgbImage> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        pokebot_video::png::load(root.join("captures/fixtures").join(name)).ok()
    }

    /// Fixture sources: the emulator's (`captures/fixtures/`) and the
    /// physical Switch's (`captures/fixtures/switch/`).
    const SOURCES: [&str; 2] = ["", "switch/"];

    fn font() -> Option<Font> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Font::load(root.join("data/world/font_normal.json")).ok()
    }

    #[test]
    fn the_list_names_and_caught_marks_are_read() {
        let (Some(image), Some(font)) = (fixture("emu-pokedex.png"), font()) else {
            return;
        };
        let list = list(&image, &font).unwrap();
        assert_eq!(list.cursor, Some(0));
        assert_eq!(list.rows.len(), 9);
        assert_eq!(list.rows[0], ("BULBASAUR".to_owned(), true));
        assert_eq!(list.rows[1], ("IVYSAUR".to_owned(), true));
        assert_eq!(list.rows[3], ("CHARMANDER".to_owned(), false));
        // Species never seen show dashes, never a mark.
        assert!(list.rows[2..].iter().all(|(_, caught)| !caught));
        assert!(
            list.rows[2].0.chars().all(|c| c == '-' || c == '?'),
            "{:?}",
            list.rows[2]
        );
    }

    #[test]
    fn the_scrolled_list_is_read() {
        let (Some(image), Some(font)) = (fixture("emu-pokedex-scrolled.png"), font()) else {
            return;
        };
        let list = list(&image, &font).unwrap();
        assert_eq!(list.cursor, Some(5));
        assert_eq!(list.rows[4], ("CATERPIE".to_owned(), false));
        assert_eq!(list.rows[7], ("WEEDLE".to_owned(), false));
        assert_eq!(list.rows[8], ("KAKUNA".to_owned(), false));
        assert!(list.rows.iter().all(|(_, caught)| !caught));
    }

    #[test]
    fn other_screens_are_not_the_list() {
        let Some(font) = font() else {
            return;
        };
        for name in [
            "emu-pokedex-contents.png",
            "emu-trainer-card.png",
            "emu-bag-key-items.png",
            "pokedex-page.png",
            "bag-items.png",
            "mart-list.png",
            "start-menu.png",
        ] {
            let Some(image) = fixture(name) else {
                continue;
            };
            assert!(list(&image, &font).is_none(), "{name}");
        }
    }

    #[test]
    fn the_entry_page_is_found() {
        let Some(image) = fixture("pokedex-page.png") else {
            return;
        };
        assert!(is_page(&image));
    }

    #[test]
    fn switch_the_entry_page_is_found() {
        let Some(image) = fixture("switch/pokedex-page.png") else {
            return;
        };
        assert!(is_page(&image));
    }

    #[test]
    fn other_screens_are_not_the_entry_page() {
        for dir in SOURCES {
            for name in [
                "battle-gotcha.png",
                "mart-list.png",
                "battle-wild-caught.png",
                "nickname-prompt.png",
                "bag-items.png",
                "mtmoon-1f.png",
                "mtmoon-entry-intro.png",
            ] {
                let Some(image) = fixture(&format!("{dir}{name}")) else {
                    continue;
                };
                assert!(!is_page(&image), "{dir}{name}");
            }
        }
    }
}
