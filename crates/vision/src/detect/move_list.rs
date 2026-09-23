//! The "KNOWN MOVES" screen shown when a Pokémon with four moves learns a
//! new one: five rows (the four moves, then the new move), the selected row
//! framed in red.

use pokebot_core::RgbImage;
use pokebot_state::{MoveListObservation, Region};

use super::share;
use crate::color::Rgb;
use crate::text::Font;

/// Header bar: tan tab on the left, blue on the right.
const HEADER_TAN: Rgb = [231, 219, 156];
const HEADER_BLUE: Rgb = [0, 121, 198];
const SELECT_RED: Rgb = [231, 56, 0];
const ROWS: u32 = 5;
/// Row k's selection frame spans y 18+28k ..= 46+28k; its left edge is x 120.
const ROW_TOP: u32 = 18;
const ROW_PITCH: u32 = 28;
const FRAME_X: u32 = 120;
/// Where a row's move name is printed (capitals only: 12 rows keep the PP
/// line below out).
const NAME_X: u32 = 156;
const NAME_Y: u32 = 20;
const NAME_WIDTH: u32 = 82;

pub fn detect(image: &RgbImage, font: Option<&Font>) -> Option<MoveListObservation> {
    let header = share(image, Region::new(0, 0, 144, 4), HEADER_TAN, 1) >= 900
        && share(image, Region::new(150, 0, 90, 4), HEADER_BLUE, 1) >= 900;
    if !header {
        return None;
    }
    let selected = (0..ROWS)
        .find(|k| {
            share(
                image,
                Region::new(FRAME_X, ROW_TOP + 2 + ROW_PITCH * k, 1, 24),
                SELECT_RED,
                1,
            ) >= 900
        })
        .map(|k| k as u8);
    // The move list page is the only one with the selection frame.
    selected?;
    let moves = font.map_or_else(Vec::new, |font| {
        (0..ROWS)
            .map(|k| {
                let region = Region::new(NAME_X, NAME_Y + ROW_PITCH * k, NAME_WIDTH, 12);
                font.read(image, region, &[]).join(" ")
            })
            .collect()
    });
    Some(MoveListObservation { moves, selected })
}
