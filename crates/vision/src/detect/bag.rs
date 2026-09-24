//! The bag screen: pocket title, item rows (name, count), the ▶ cursor and
//! the USE/…/CANCEL prompt opened from a battle. The field bag and the
//! battle bag share the same frame, colours and row geometry (see the probe
//! note); only the bottom panel and the cursor colour differ.

use pokebot_core::RgbImage;
use pokebot_state::{BagObservation, Region};

use super::share;
use crate::color::Rgb;
use crate::text::Font;

/// Outer bag frame and pocket title plate.
const FRAME_ORANGE: Rgb = [247, 203, 115];
/// Pocket title plate's underline.
const TITLE_UNDERLINE: Rgb = [222, 138, 74];

const TITLE_REGION: Region = Region {
    x: 8,
    y: 4,
    width: 76,
    height: 16,
};

/// Item rows: 6 visible, pitch 16, cell top at `8 + 16r`.
const ROWS: u32 = 6;
const ROW_TOP: u32 = 8;
const ROW_PITCH: u32 = 16;
const ROW_HEIGHT: u32 = 15;
const NAME_X: u32 = 96;
const NAME_WIDTH: u32 = 94;
const COUNT_X: u32 = 190;
const COUNT_WIDTH: u32 = 42;

/// Cursor column: the ▶ (or, in the USE/CANCEL window, the ▶) starts here.
const LIST_CURSOR_X: std::ops::Range<u32> = 88..96;
const LIST_CURSOR_Y0: u32 = 12;

/// USE/CANCEL prompt (battle bag only).
const PROMPT_MESSAGE_PROBE: (u32, u32) = (50, 120);
const PROMPT_OPTIONS: Region = Region {
    x: 182,
    y: 118,
    width: 50,
    height: 36,
};
const PROMPT_CURSOR_X: std::ops::Range<u32> = 170..185;
const PROMPT_CURSOR_Y0: u32 = 124;
const PROMPT_ROW_PITCH: u32 = 16;

pub fn detect(image: &RgbImage, font: &Font, small_font: &Font) -> Option<BagObservation> {
    if !is_bag_frame(image) {
        return None;
    }
    let pocket = font.read(image, TITLE_REGION, &[]).join(" ");
    let mut rows = Vec::new();
    for r in 0..ROWS {
        let name_region = Region::new(NAME_X, ROW_TOP + ROW_PITCH * r, NAME_WIDTH, ROW_HEIGHT);
        let name = font.read(image, name_region, &[]).join(" ");
        if name.is_empty() {
            break;
        }
        let count_region = Region::new(COUNT_X, ROW_TOP + ROW_PITCH * r, COUNT_WIDTH, ROW_HEIGHT);
        let count_text = small_font.read(image, count_region, &[]).join(" ");
        rows.push((name, parse_count(&count_text)));
    }
    let found = super::menu::find_cursor(image);
    let cursor = cursor_row(found, LIST_CURSOR_X, LIST_CURSOR_Y0, ROW_PITCH, rows.len());
    let prompt = if crate::color::near(
        super::px(image, PROMPT_MESSAGE_PROBE.0, PROMPT_MESSAGE_PROBE.1),
        crate::color::WHITE,
        crate::color::TOLERANCE,
    ) {
        let opts: Vec<String> = font
            .read(image, PROMPT_OPTIONS, &[])
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        let row = cursor_row(
            found,
            PROMPT_CURSOR_X,
            PROMPT_CURSOR_Y0,
            PROMPT_ROW_PITCH,
            opts.len(),
        );
        row.map(|row| (opts, row))
    } else {
        None
    };
    Some(BagObservation {
        pocket,
        rows,
        cursor,
        prompt,
    })
}

/// A cursor hit at `(x, y)` turned into a row index: `x` must fall in
/// `x_range`, `y` must be at or below the first row's top `y0`, and the
/// resulting row must be within `count` rows (a misread option/row list
/// must not produce an out-of-range index).
fn cursor_row(
    found: Option<(u32, u32)>,
    x_range: std::ops::Range<u32>,
    y0: u32,
    pitch: u32,
    count: usize,
) -> Option<u8> {
    let (x, y) = found?;
    if !x_range.contains(&x) || y < y0 {
        return None;
    }
    let row = (y - y0) / pitch;
    (row < count as u32).then_some(row as u8)
}

fn is_bag_frame(image: &RgbImage) -> bool {
    share(image, Region::new(6, 4, 230, 1), FRAME_ORANGE, 2) >= 900
        && share(image, Region::new(9, 21, 60, 2), TITLE_UNDERLINE, 1) >= 900
}

/// Digits after `×`, mapping the small font's shared `0`/`O` bitmap back to
/// `0` (see the probe note's digit caveat). `None` when no digit is found
/// (CANCEL has no count).
fn parse_count(text: &str) -> Option<u16> {
    let digits: String = text
        .chars()
        .map(|c| if c == 'O' { '0' } else { c })
        .filter(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::OnceLock;

    use super::*;
    use crate::color::{TEXT_GRAY, WHITE};
    use crate::detect::testing::fill;

    /// Loads a fixture PNG plus the normal and small fonts; skips the test
    /// (returns `None`) when either is missing, as the repo's fixtures are
    /// gitignored.
    fn fixture(name: &str) -> Option<(RgbImage, Font, Font)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let image = pokebot_video::png::load(root.join("captures/fixtures").join(name)).ok()?;
        let font = Font::load(root.join("data/world/font_normal.json")).ok()?;
        let small_font = Font::load(root.join("data/world/font_small.json")).ok()?;
        Some((image, font, small_font))
    }

    #[test]
    fn reads_the_poke_balls_pocket() {
        let Some((image, font, small_font)) = fixture("bag-pokeballs.png") else {
            return;
        };
        let bag = detect(&image, &font, &small_font).expect("bag");
        assert_eq!(bag.pocket, "POKé BALLS");
        assert_eq!(bag.rows.first(), Some(&("POKé BALL".to_owned(), Some(8))));
        assert_eq!(bag.rows.last().map(|r| r.0.as_str()), Some("CANCEL"));
        assert_eq!(bag.cursor, Some(0));
    }

    #[test]
    fn cursor_row_follows_the_arrow() {
        let Some((image, font, small_font)) = fixture("bag-pokeballs-cursor1.png") else {
            return;
        };
        let bag = detect(&image, &font, &small_font).expect("bag");
        assert_eq!(bag.cursor, Some(1));
    }

    #[test]
    fn use_prompt_is_read() {
        let Some((image, font, small_font)) = fixture("bag-use-prompt.png") else {
            return;
        };
        let bag = detect(&image, &font, &small_font).expect("bag");
        let (opts, row) = bag.prompt.expect("prompt");
        assert_eq!(opts.first().map(String::as_str), Some("USE"));
        assert_eq!(opts.last().map(String::as_str), Some("CANCEL"));
        assert_eq!(row, 0);
    }

    #[test]
    fn other_screens_are_not_bags() {
        for name in ["move-select.png", "learn-list-row0.png", "mart-list.png"] {
            let Some((image, font, small_font)) = fixture(name) else {
                continue;
            };
            assert!(detect(&image, &font, &small_font).is_none(), "{name}");
        }
    }

    fn fonts() -> Option<&'static (Font, Font)> {
        static FONTS: OnceLock<Option<(Font, Font)>> = OnceLock::new();
        FONTS
            .get_or_init(|| {
                let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
                let font = Font::load(root.join("data/world/font_normal.json")).ok()?;
                let small_font = Font::load(root.join("data/world/font_small.json")).ok()?;
                Some((font, small_font))
            })
            .as_ref()
    }

    /// A synthetic bag window, drawn with the probe note's colours: the
    /// orange frame/title plate, the cream list, the row dash, and a
    /// `POKé BALL ×12` / `CANCEL` list with the cursor on row 0.
    #[test]
    fn synthetic_bag_window_reads_its_rows() {
        let Some((font, small_font)) = fonts() else {
            return;
        };
        let mut image = RgbImage::filled(240, 160, [107, 203, 198]);
        fill(&mut image, 6, 4, 230, 103, FRAME_ORANGE);
        fill(&mut image, 8, 4, 76, 16, FRAME_ORANGE);
        fill(&mut image, 8, 19, 71, 4, TITLE_UNDERLINE);
        fill(&mut image, 88, 8, 143, 95, [255, 251, 206]);
        for r in 0..2u32 {
            fill(&mut image, 97, 23 + 16 * r, 125, 1, [239, 227, 173]);
        }
        font.render(&mut image, 8, 4, "POKé BALLS", WHITE, [214, 211, 206]);
        font.render(&mut image, 96, 8, "POKé BALL", TEXT_GRAY, [214, 211, 206]);
        small_font.render(&mut image, 190, 8, "×12", TEXT_GRAY, [214, 211, 206]);
        font.render(&mut image, 96, 24, "CANCEL", TEXT_GRAY, [214, 211, 206]);
        crate::detect::testing::draw_cursor(&mut image, 90, 12);

        let bag = detect(&image, font, small_font).expect("bag");
        assert_eq!(bag.pocket, "POKé BALLS");
        assert_eq!(
            bag.rows,
            vec![
                ("POKé BALL".to_owned(), Some(12)),
                ("CANCEL".to_owned(), None),
            ]
        );
        assert_eq!(bag.cursor, Some(0));
        assert_eq!(bag.prompt, None);
    }

    /// A misread options list (only "USE" recognised, no "CANCEL") with the
    /// cursor sitting on the second row must not report a row index past
    /// the end of `opts` — the prompt reads as absent rather than panicking
    /// or handing back an out-of-range row.
    #[test]
    fn prompt_cursor_past_the_read_options_is_not_reported() {
        let Some((font, small_font)) = fonts() else {
            return;
        };
        let mut image = RgbImage::filled(240, 160, [107, 203, 198]);
        fill(&mut image, 6, 4, 230, 103, FRAME_ORANGE);
        fill(&mut image, 8, 4, 76, 16, FRAME_ORANGE);
        fill(&mut image, 8, 19, 71, 4, TITLE_UNDERLINE);
        fill(&mut image, 88, 8, 143, 95, [255, 251, 206]);
        font.render(&mut image, 8, 4, "POKé BALLS", WHITE, [214, 211, 206]);
        font.render(&mut image, 96, 8, "CANCEL", TEXT_GRAY, [214, 211, 206]);
        // The prompt's message window (white interior) marks the bag as a
        // battle bag with the USE/CANCEL prompt open.
        fill(&mut image, 45, 117, 118, 37, WHITE);
        // Only "USE" is legible; "CANCEL" is left unrendered to simulate a
        // misread that finds one option where the game shows two.
        font.render(&mut image, 182, 118, "USE", TEXT_GRAY, [214, 211, 206]);
        // The ▶ sits on the (missing) second row.
        crate::detect::testing::draw_cursor(&mut image, 177, 140);

        let bag = detect(&image, font, small_font).expect("bag");
        assert_eq!(bag.prompt, None);
    }
}
