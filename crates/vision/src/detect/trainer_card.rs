//! The Trainer Card's front: the badge row at the bottom.
//!
//! The card (measured on `emu-trainer-card.png`, Kanto card, front) has a
//! teal frame, a light-blue badge band with the BADGES label and eight
//! 16×16 badge slots at tilemap (4 + 3i, 16..17) (`trainer_card.c`
//! `DrawStarsAndBadgesOnCard`), so slot `i` is at x 32 + 24i, y 128. An
//! empty slot shows only the band's two blues; a badge covers about half of
//! it with its own palette (the Boulder Badge: 136 of 256 pixels).
//!
//! The POKéDEX row (`PrintPokedexOnCard`, window 1 at tile (1, 1)) prints
//! the caught count right-aligned to window x 136 at window y 72, i.e.
//! screen x 144, glyph cells from y 80 (ink rows 82..=90 on the fixture, the
//! `2` at x 138..=143); the row is blank until the Pokédex is received.

use pokebot_core::RgbImage;
use pokebot_state::{Region, TrainerCardObservation};

use super::{share, share_any};
use crate::color::Rgb;
use crate::text::Font;

/// Card frame (left, right and bottom borders, and the title plate).
const FRAME_TEAL: Rgb = [49, 154, 148];
const FRAME_TEAL_LIGHT: Rgb = [82, 203, 181];
/// Badge band and the empty slots' inner shade.
const BAND_BLUE: Rgb = [132, 186, 231];
const SLOT_LIGHT: Rgb = [214, 227, 247];

const LEFT_FRAME: Region = Region {
    x: 0,
    y: 0,
    width: 4,
    height: 160,
};
const BOTTOM_FRAME: Region = Region {
    x: 0,
    y: 150,
    width: 240,
    height: 10,
};
/// The band above the slots (y 120..=127), right of the BADGES label.
const BAND: Region = Region {
    x: 56,
    y: 120,
    width: 176,
    height: 8,
};

const SLOTS: u32 = 8;
const SLOT_X0: u32 = 32;
const SLOT_PITCH: u32 = 24;
const SLOT_Y: u32 = 128;
const SLOT_SIZE: u32 = 16;
/// Share of a slot not in the band's blues for a badge to be drawn there
/// (the Boulder Badge measures 531‰; an empty slot 0‰).
const BADGE_MIN_INK: u32 = 250;
/// Where the POKéDEX count's digits are: up to three 6 px cells ending at
/// x 144, with the glyph cell rows around y 80 (the MONEY row's ink ends
/// at y 74 and the TIME row's starts at y 98).
const POKEDEX_COUNT: Region = Region {
    x: 118,
    y: 76,
    width: 28,
    height: 20,
};

/// The card with its badges and, given the font, the POKéDEX count.
pub fn detect(image: &RgbImage, font: Option<&Font>) -> Option<TrainerCardObservation> {
    if !is_card(image) {
        return None;
    }
    let badges = (0..SLOTS)
        .filter(|&i| {
            let slot = Region::new(SLOT_X0 + SLOT_PITCH * i, SLOT_Y, SLOT_SIZE, SLOT_SIZE);
            1000 - share_any(image, slot, &[BAND_BLUE, SLOT_LIGHT], 1) >= BADGE_MIN_INK
        })
        .map(|i| i as u8 + 1)
        .collect();
    Some(TrainerCardObservation {
        badges,
        pokedex_count: font.and_then(|font| pokedex_count(image, font)),
    })
}

/// The POKéDEX row's number, when every glyph read as a digit (a blank row,
/// before the Pokédex, reads nothing).
pub fn pokedex_count(image: &RgbImage, font: &Font) -> Option<u16> {
    let text = font.read(image, POKEDEX_COUNT, &[]).join("");
    let digits = text.trim();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Teal frame on the left and bottom, blue band above the slots (measured
/// 1000‰, 626‰ and 994‰).
fn is_card(image: &RgbImage) -> bool {
    let teal = [FRAME_TEAL, FRAME_TEAL_LIGHT];
    share_any(image, LEFT_FRAME, &teal, 1) >= 800
        && share_any(image, BOTTOM_FRAME, &teal, 2) >= 500
        && share(image, BAND, BAND_BLUE, 1) >= 800
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::detect::testing::fill;

    fn fixture(name: &str) -> Option<RgbImage> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        pokebot_video::png::load(root.join("captures/fixtures").join(name)).ok()
    }

    fn font() -> Option<Font> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Font::load(root.join("data/world/font_normal.json")).ok()
    }

    /// A card with the frame, the band and badges drawn (as a dark disc)
    /// in the given slots.
    fn draw_card(badges: &[u8]) -> RgbImage {
        let mut image = RgbImage::filled(240, 160, crate::color::WHITE);
        fill(&mut image, 0, 0, 240, 160, FRAME_TEAL);
        fill(&mut image, 4, 16, 232, 134, [231, 243, 247]);
        fill(&mut image, 4, 114, 232, 36, BAND_BLUE);
        for i in 0..SLOTS {
            let x = SLOT_X0 + SLOT_PITCH * i;
            fill(&mut image, x + 4, SLOT_Y + 4, 8, 8, SLOT_LIGHT);
            if badges.contains(&(i as u8 + 1)) {
                fill(&mut image, x + 2, SLOT_Y + 2, 12, 12, [0, 0, 0]);
                fill(&mut image, x + 4, SLOT_Y + 4, 8, 8, [214, 138, 49]);
            }
        }
        image
    }

    #[test]
    fn the_boulder_badge_is_read_from_the_card() {
        let Some(image) = fixture("emu-trainer-card.png") else {
            return;
        };
        let card = detect(&image, None).unwrap();
        assert_eq!(card.badges, vec![1]);
        assert_eq!(card.pokedex_count, None, "no font, no count");
    }

    /// The fixture (Route 3 save) prints `POKéDEX 2`.
    #[test]
    fn the_pokedex_count_is_read_from_the_card() {
        let (Some(image), Some(font)) = (fixture("emu-trainer-card.png"), font()) else {
            return;
        };
        assert_eq!(pokedex_count(&image, &font), Some(2));
        assert_eq!(detect(&image, Some(&font)).unwrap().pokedex_count, Some(2));
    }

    /// Counts of one to three digits, right-aligned to x 144 like the game
    /// prints them; a blank row (no Pokédex yet) reads no count.
    #[test]
    fn counts_of_every_width_are_read() {
        let Some(font) = font() else { return };
        let ink = [82, 82, 82];
        let shadow = [181, 181, 181];
        for (count, text) in [(7u16, "7"), (42, "42"), (151, "151")] {
            let mut image = draw_card(&[1]);
            let x = 144 - 6 * text.len() as u32;
            font.render(&mut image, x, 80, text, ink, shadow);
            assert_eq!(pokedex_count(&image, &font), Some(count), "{text}");
            assert_eq!(
                detect(&image, Some(&font)).unwrap().pokedex_count,
                Some(count)
            );
        }
        assert_eq!(pokedex_count(&draw_card(&[]), &font), None);
    }

    #[test]
    fn every_slot_is_read() {
        assert_eq!(
            detect(&draw_card(&[]), None).unwrap().badges,
            Vec::<u8>::new()
        );
        assert_eq!(
            detect(&draw_card(&[3, 8]), None).unwrap().badges,
            vec![3, 8]
        );
        assert_eq!(
            detect(&draw_card(&[1, 2, 3, 4, 5, 6, 7, 8]), None)
                .unwrap()
                .badges,
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn other_screens_are_not_the_card() {
        for name in [
            "emu-pokedex.png",
            "emu-pokedex-contents.png",
            "emu-bag-key-items.png",
            "synthetic-fly-map.png",
            "start-menu.png",
            "bag-items.png",
            "mart-list.png",
            "pokedex-page.png",
            "mtmoon-1f.png",
        ] {
            let Some(image) = fixture(name) else {
                continue;
            };
            assert!(detect(&image, None).is_none(), "{name}");
        }
        assert!(detect(&RgbImage::filled(240, 160, crate::color::WHITE), None).is_none());
    }
}
