//! The map-name popup: on entering a map with a new name the game slides a
//! white box down at the top-left ("ROUTE 3", "MT. MOON", "PEWTER CITY"),
//! holds it for 120 frames and slides it back up
//! (`src/map_name_popup.c`: window at tile x 1, 14 tiles wide, 19 on maps
//! with a floor number, 22 on rooftops; `DrawTextBorderOuter` frame).
//!
//! Fully shown, the box spans x 2..=125 and y ..=21 (its top is off
//! screen): two dark-grey border columns each side with a light-grey one
//! inside, a light-grey row and two dark-grey rows at the bottom. It hides
//! the map below it, so its area is left out of localization (it cost the
//! Switch's Route 3 entry frames ~10% of their samples, enough to fail).
//! Measured on emulator and Switch frames (fixtures `*map-popup-*.png`).

use pokebot_core::RgbImage;
use pokebot_state::Region;

use super::px;
use crate::color::{near, Rgb, WHITE};
use crate::text::Font;

/// The frame's dark and light greys (text window palette 3).
const BORDER: Rgb = [99, 113, 123];
const INNER: Rgb = [206, 211, 214];
/// Switch captures soften the border's edges by up to ~20 per channel.
const TOLERANCE: u8 = 28;
/// Left border columns (dark, dark, light) and the first white column.
const LEFT: u32 = 2;
/// Last dark border row when the box is fully down.
const SHOWN_BOTTOM: u32 = 21;
/// Last dark border row while sliding: from here on the box shows the
/// light and white rows above its border (the part worth excluding).
const MIN_BOTTOM: u32 = 4;
/// Right dark border column pairs (x, x+1) for 14, 19 and 22 tile windows.
const RIGHT: [u32; 3] = [124, 164, 188];
/// Share (per mille) of a row or column's samples that must match.
const MATCH: u32 = 900;

/// A map-name popup on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapPopup {
    /// The screen area the box hides (with a pixel of margin).
    pub covers: Region,
    /// Last dark row of the bottom border: 21 when fully down.
    pub bottom: u32,
    /// First dark column of the right border.
    pub right: u32,
}

impl MapPopup {
    /// Fully down (not sliding): its text is all on screen.
    pub fn shown(&self) -> bool {
        self.bottom == SHOWN_BOTTOM
    }

    /// The white interior the name is printed in.
    fn interior(&self) -> Region {
        Region::new(LEFT + 3, 0, self.right - 1 - (LEFT + 3), self.bottom - 2)
    }
}

fn share(pixels: impl Iterator<Item = (u32, u32)>, image: &RgbImage, color: Rgb) -> u32 {
    let (mut hits, mut total) = (0u32, 0u32);
    for (x, y) in pixels {
        total += 1;
        if near(px(image, x, y), color, TOLERANCE) {
            hits += 1;
        }
    }
    (hits * 1000).checked_div(total).unwrap_or(0)
}

fn row(image: &RgbImage, y: u32, x0: u32, x1: u32, color: Rgb) -> bool {
    share((x0..=x1).step_by(2).map(|x| (x, y)), image, color) >= MATCH
}

fn column(image: &RgbImage, x: u32, y0: u32, y1: u32, color: Rgb) -> bool {
    share((y0..=y1).map(|y| (x, y)), image, color) >= MATCH
}

/// The popup, fully down or sliding, if one is on screen.
pub fn detect(image: &RgbImage) -> Option<MapPopup> {
    // Cheap rejection first (this runs on every overworld frame): the
    // left border's dark columns next to the light one, at the top row.
    if !(near(px(image, LEFT, 0), BORDER, TOLERANCE)
        && near(px(image, LEFT + 2, 0), INNER, TOLERANCE))
    {
        return None;
    }
    let bottom = (MIN_BOTTOM..=SHOWN_BOTTOM).rev().find(|&b| {
        // Bottom border: two dark rows under a light one, inside the
        // rounded corners.
        row(image, b, LEFT + 2, RIGHT[0] - 2, BORDER)
            && row(image, b - 1, LEFT + 2, RIGHT[0] - 2, BORDER)
            && row(image, b - 2, LEFT + 3, RIGHT[0] - 2, INNER)
            && column(image, LEFT, 0, b - 1, BORDER)
            && column(image, LEFT + 1, 0, b - 1, BORDER)
            && column(image, LEFT + 2, 0, b - 2, INNER)
            // The first interior column (text never starts this far left).
            && column(image, LEFT + 3, 0, b - 3, WHITE)
    })?;
    let right = RIGHT.into_iter().find(|&r| {
        column(image, r, 0, bottom - 1, BORDER)
            && column(image, r + 1, 0, bottom - 2, BORDER)
            && column(image, r - 1, 0, bottom - 2, INNER)
            && row(image, bottom, LEFT + 2, r - 1, BORDER)
    })?;
    Some(MapPopup {
        covers: Region::new(0, 0, right + 4, bottom + 2),
        bottom,
        right,
    })
}

/// The map name, once the box is fully down and something was read.
pub fn read(image: &RgbImage, popup: &MapPopup, font: &Font) -> Option<String> {
    if !popup.shown() {
        return None;
    }
    let name = font.read(image, popup.interior(), &[]).join(" ");
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn fixture(name: &str) -> Option<RgbImage> {
        pokebot_video::png::load(root().join("captures/fixtures").join(name)).ok()
    }

    /// Live popups: Switch (JPEG-softened) and emulator captures of Route 3
    /// from Pewter, Mt. Moon, Pewter City and Pallet Town (new game).
    #[test]
    fn popup_names_are_read() {
        let Ok(font) = Font::load(root().join("data/world/font_normal.json")) else {
            return;
        };
        for (name, text) in [
            ("switch-map-popup-route3.png", "ROUTE 3"),
            ("switch-map-popup-route3-b.png", "ROUTE 3"),
            ("emu-map-popup-mt-moon.png", "MT. MOON"),
            ("emu-map-popup-pewter.png", "PEWTER CITY"),
            ("emu-map-popup-pallet.png", "PALLET TOWN"),
        ] {
            let Some(image) = fixture(name) else {
                continue;
            };
            let popup = detect(&image).unwrap_or_else(|| panic!("{name}: no popup"));
            assert!(popup.shown(), "{name}: {popup:?}");
            assert_eq!(popup.covers, Region::new(0, 0, 128, 23), "{name}");
            assert_eq!(read(&image, &popup, &font).as_deref(), Some(text), "{name}");
        }
    }

    /// Sliding in or out, the box is found at its height but not read.
    #[test]
    fn a_sliding_popup_is_found_but_not_read() {
        let Some(image) = fixture("emu-map-popup-sliding.png") else {
            return;
        };
        let popup = detect(&image).expect("popup");
        assert!(!popup.shown(), "{popup:?}");
        assert!(popup.bottom < SHOWN_BOTTOM);
    }

    /// Plain overworld frames, a message box and the Start menu have none.
    #[test]
    fn frames_without_a_popup() {
        for name in [
            "emu-tools-overworld.png",
            "switch-forest-north-gate.png",
            "start-menu.png",
            "switch-sign.png",
            "mtmoon-1f.png",
        ] {
            let Some(image) = fixture(name) else {
                continue;
            };
            assert_eq!(detect(&image), None, "{name}");
        }
        assert_eq!(detect(&RgbImage::filled(240, 160, WHITE)), None);
        assert_eq!(detect(&RgbImage::filled(240, 160, [0, 0, 0])), None);
    }

    /// The drawn geometry, independent of fixtures: fully down and 6 px up.
    #[test]
    fn synthetic_popup_geometry() {
        for (bottom, right) in [(21, 124), (15, 124), (21, 164)] {
            let mut image = RgbImage::filled(240, 160, [57, 146, 49]);
            let fill = |image: &mut RgbImage, x0: u32, y0: u32, x1: u32, y1: u32, c: Rgb| {
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        image.put_pixel(x, y, c);
                    }
                }
            };
            fill(&mut image, LEFT, 0, right + 1, bottom, BORDER);
            fill(&mut image, LEFT + 2, 0, right - 1, bottom - 2, INNER);
            fill(&mut image, LEFT + 3, 0, right - 2, bottom - 3, WHITE);
            let popup = detect(&image).expect("popup");
            assert_eq!((popup.bottom, popup.right), (bottom, right));
            assert_eq!(popup.shown(), bottom == SHOWN_BOTTOM);
        }
    }
}
