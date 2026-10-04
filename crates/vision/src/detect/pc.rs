//! The Pokémon Storage System (BILL's PC → WITHDRAW / DEPOSIT POKéMON).
//!
//! Measured on the emulator's fixtures (`emu-pc-*.png`, Cerulean Center):
//! - the PKMN DATA panel fills x 0..80: a lilac-grey frame (`PANEL_GRAY`)
//!   round a blue checkered picture box, and under it the nickname,
//!   `/SPECIES` and `♂ Lv10` lines of the Pokémon under the hand
//!   (`pokemon_storage_system.c` `PrintDisplayMonInfo`); blank over an
//!   empty cell;
//! - DEPOSIT shows the party panel beside it (teal, `PARTY_TEAL`), WITHDRAW
//!   a box: the green PARTY POKéMON button top left (`PARTY_BUTTON`), the
//!   box title and the 6 × 5 grid;
//! - the hand is a 32 × 32 sprite centred on `GetCursorCoordsByPos`
//!   (`pokemon_storage_system_data.c`): box cell (c, r) at (100 + 24c,
//!   32 + 24r), party slot 0 at (104, 52), slots 1–5 at (152, 4 + 24(k −
//!   1)), CANCEL at (152, 132), the box title at (162, 12). Its glove is
//!   the only near-white on the screen apart from button labels (the
//!   backgrounds' pinks and creams all have a channel below 235), so the
//!   hand is where the most glove white is, among those places;
//! - "Deposit in which BOX?" opens a yellow picker (`PICKER_YELLOW`) with
//!   the box name and `n /30`;
//! - messages print in a white window along the bottom right (x 85..235,
//!   y 133..154).

use pokebot_core::RgbImage;
use pokebot_state::{PcCursor, PcMode, PcStorageObservation, Region};

use super::{px, share, share_any};
use crate::color::{Rgb, WHITE};
use crate::text::Font;

const PANEL_GRAY: Rgb = [148, 146, 173];
/// The picture box's frame, down the panel's left edge.
const FRAME_GRAY: Rgb = [115, 113, 123];
const PICTURE_BLUES: [Rgb; 2] = [[156, 203, 239], [181, 211, 247]];
/// The party panel's two stripes.
const PARTY_TEALS: [Rgb; 2] = [[74, 170, 165], [57, 138, 140]];
const PARTY_BUTTON: Rgb = [165, 235, 148];
const PICKER_YELLOW: Rgb = [247, 219, 115];

/// The panel's top edge, its left edge by the picture box and by the
/// text, and the picture box's top rows.
const PANEL_TOP: Region = Region::new(4, 2, 72, 2);
const FRAME_LEFT: Region = Region::new(0, 16, 3, 60);
const PANEL_LEFT: Region = Region::new(0, 92, 3, 60);
const PICTURE_TOP: Region = Region::new(10, 18, 60, 3);
/// The party panel under slot 0 (never covered by a menu or the picker).
const PARTY_PANEL: Region = Region::new(100, 84, 18, 36);
const PARTY_BUTTON_AREA: Region = Region::new(86, 3, 60, 10);

/// PKMN DATA's lines (glyph cells 14 rows tall from y 88, 102, 116).
const NICKNAME: Region = Region::new(2, 86, 76, 15);
const SPECIES: Region = Region::new(2, 100, 76, 15);
const LEVEL: Region = Region::new(14, 114, 64, 15);
/// The box title between the scroll arrows.
const BOX_TITLE: Region = Region::new(128, 20, 64, 16);
/// The box title's letters, and how far a capture strays from them: less
/// than the pale band some wallpapers put behind them (220, 249, 245).
const TITLE_WHITE: [u8; 3] = [255, 255, 255];
const TITLE_SPREAD: u8 = 16;
const PICKER: Region = Region::new(124, 66, 72, 40);
const PICKER_TEXT: Region = Region::new(126, 68, 68, 36);
const MESSAGE: Region = Region::new(86, 133, 148, 21);

/// Glove white: every channel at least this (the pink backgrounds' blue
/// is 231 at most, the title bar's 231).
const GLOVE_MIN: u8 = 240;
/// The glove's white around the hand's centre (dx −8..6, dy −9..6 on the
/// fixtures), searched a few rows up and down: the hand bobs.
const GLOVE: (i32, i32, u32, u32) = (-8, -9, 14, 16);
const BOB: [i32; 5] = [0, -2, 2, -4, 4];
/// White pixels the hand shows: about 80, but 30 over party slot 1, where
/// the screen's top edge cuts it (`emu-pc-deposit-slot1.png`). Icons show
/// a few scattered ones.
const GLOVE_MIN_PIXELS: u32 = 20;

/// The storage screen, read: `None` on any other screen. `covered` is
/// where a menu window is open (its white interior is no glove).
pub fn detect(
    image: &RgbImage,
    font: &Font,
    covered: Option<Region>,
) -> Option<PcStorageObservation> {
    if share(image, PANEL_TOP, PANEL_GRAY, 1) < 800
        || share(image, FRAME_LEFT, FRAME_GRAY, 1) < 800
        || share(image, PANEL_LEFT, PANEL_GRAY, 1) < 800
        // A tall picture covers much of the box's top rows (fleet
        // continue-6: FEAROW's left 372 of 1000, the screen read Unknown,
        // and the swap failed "no progress in box").
        || share_any(image, PICTURE_TOP, &PICTURE_BLUES, 1) < 250
    {
        return None;
    }
    let mode = if share_any(image, PARTY_PANEL, &PARTY_TEALS, 1) >= 900 {
        PcMode::Party
    } else if share(image, PARTY_BUTTON_AREA, PARTY_BUTTON, 1) >= 300 {
        PcMode::Box
    } else {
        return None;
    };
    let line = |region: Region| {
        let text = font.read(image, region, &[]).join(" ");
        let text = text.trim().to_owned();
        (!text.is_empty()).then_some(text)
    };
    let nickname = line(NICKNAME);
    let species = line(SPECIES).map(|s| s.trim_start_matches('/').trim().to_owned());
    let level = line(LEVEL).and_then(|l| parse_level(&l));
    let picker = (share(image, PICKER, PICKER_YELLOW, 2) >= 500).then(|| {
        let lines = font.read(image, PICKER_TEXT, &[]);
        let name = lines.first().cloned().unwrap_or_default();
        let count = lines.get(1).and_then(|l| parse_count(l));
        (name, count)
    });
    let message = if share(image, MESSAGE, WHITE, 2) >= 500 {
        font.read(image, MESSAGE, &[])
    } else {
        Vec::new()
    };
    // Windows over the screen: their white is no glove.
    let mut windows: Vec<Region> = covered.map(|w| w.inflate(4)).into_iter().collect();
    if !message.is_empty() {
        windows.push(MESSAGE.inflate(4));
    }
    if picker.is_some() {
        windows.push(PICKER);
    }
    let cursor = cursor(image, mode, &windows);
    // The hand over the title's row hides part of it: its pixels are left
    // out (the rest of the title still reads).
    let box_title = match mode {
        PcMode::Box => {
            let hand = cursor
                .and_then(|c| places(mode).into_iter().find(|(p, _)| *p == c))
                .map(|(_, (cx, cy))| {
                    Region::new((cx - 12).max(0) as u32, (cy - 16).max(0) as u32, 24, 28)
                });
            let mut text = font
                .read(image, BOX_TITLE, hand.as_slice())
                .join(" ")
                .trim()
                .to_owned();
            // The title's letters are white; on a pale wallpaper band the
            // palette merges them into the background.
            if text.is_empty() {
                text = font
                    .read_in(image, BOX_TITLE, hand.as_slice(), TITLE_WHITE, TITLE_SPREAD)
                    .join(" ")
                    .trim()
                    .to_owned();
            }
            (!text.is_empty()).then_some(text)
        }
        PcMode::Party => None,
    };
    Some(PcStorageObservation {
        mode,
        cursor,
        nickname,
        species,
        level,
        box_title,
        picker,
        message,
    })
}

/// The hand's centre for each place it can point in `mode`.
fn places(mode: PcMode) -> Vec<(PcCursor, (i32, i32))> {
    match mode {
        PcMode::Party => {
            let mut v = vec![(PcCursor::Party(0), (104, 52))];
            v.extend((1..6u8).map(|k| (PcCursor::Party(k), (152, 4 + 24 * (i32::from(k) - 1)))));
            v.push((PcCursor::PartyCancel, (152, 132)));
            v
        }
        PcMode::Box => {
            let mut v: Vec<_> = (0..30u8)
                .map(|i| {
                    let (c, r) = (i32::from(i % 6), i32::from(i / 6));
                    (PcCursor::Cell(i), (100 + 24 * c, 32 + 24 * r))
                })
                .collect();
            v.push((PcCursor::BoxTitle, (162, 12)));
            v
        }
    }
}

fn glove_pixels(image: &RgbImage, (cx, cy): (i32, i32), windows: &[Region]) -> u32 {
    let (dx, dy, w, h) = GLOVE;
    let mut n = 0;
    for y in cy + dy..cy + dy + h as i32 {
        for x in cx + dx..cx + dx + w as i32 {
            if x < 0 || y < 0 || x >= image.width() as i32 || y >= image.height() as i32 {
                continue;
            }
            let (x, y) = (x as u32, y as u32);
            if !windows.iter().any(|w| w.contains(x, y))
                && px(image, x, y).iter().all(|&c| c >= GLOVE_MIN)
            {
                n += 1;
            }
        }
    }
    n
}

/// Where the hand is: the place with the most glove white (first place
/// on ties), when it shows enough.
fn cursor(image: &RgbImage, mode: PcMode, windows: &[Region]) -> Option<PcCursor> {
    let mut best: Option<(u32, PcCursor)> = None;
    for (place, (cx, cy)) in places(mode) {
        let n = BOB
            .iter()
            .map(|b| glove_pixels(image, (cx, cy + b), windows))
            .max()
            .unwrap_or(0);
        if best.is_none_or(|(m, _)| n > m) {
            best = Some((n, place));
        }
    }
    best.filter(|(n, _)| *n >= GLOVE_MIN_PIXELS).map(|(_, p)| p)
}

/// `Lv10` (after the gender sign) → 10.
fn parse_level(line: &str) -> Option<u8> {
    let at = line.find("Lv")?;
    let digits: String = line[at + 2..]
        .chars()
        .skip_while(|c| *c == ' ')
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// `1 /30` → 1 (the picker's `O` reads as `0` in the small font).
fn parse_count(line: &str) -> Option<u8> {
    let (n, _) = line.split_once('/')?;
    n.trim().replace('O', "0").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn read(name: &str) -> Option<Option<PcStorageObservation>> {
        let image = pokebot_video::png::load(root().join("captures/fixtures").join(name)).ok()?;
        let font = Font::load(root().join("data/world/font_normal.json")).ok()?;
        let menu = crate::detect::menu::detect(&image).map(|m| m.window);
        Some(detect(&image, &font, menu))
    }

    fn cursor_and_panel(o: &PcStorageObservation) -> (Option<PcCursor>, Option<&str>, Option<u8>) {
        (o.cursor, o.species.as_deref(), o.level)
    }

    /// DEPOSIT POKéMON in the Cerulean Center (emulator): the party panel,
    /// the hand on slot 0 (PARAS) and slot 1 (CLEFAIRY, where the screen's
    /// top cuts the hand), the STORE window, then the box picker.
    #[test]
    fn the_deposit_screens_read_the_member_under_the_hand() {
        let Some(o) = read("emu-pc-deposit-party.png") else {
            return;
        };
        let o = o.expect("party panel");
        assert_eq!(o.mode, PcMode::Party);
        assert_eq!(
            cursor_and_panel(&o),
            (Some(PcCursor::Party(0)), Some("PARAS"), Some(10))
        );
        assert_eq!(o.nickname.as_deref(), Some("PARAS"));
        assert!(o.message.is_empty() && o.picker.is_none() && o.box_title.is_none());
        let o = read("emu-pc-deposit-slot1.png").unwrap().expect("slot 1");
        assert_eq!(
            cursor_and_panel(&o),
            (Some(PcCursor::Party(1)), Some("CLEFAIRY"), Some(8))
        );
        let o = read("emu-pc-deposit-store.png").unwrap().expect("STORE");
        assert_eq!(o.cursor, Some(PcCursor::Party(1)));
        assert_eq!(o.message, ["CLEFAIRY is selected."]);
        let o = read("emu-pc-deposit-box.png").unwrap().expect("picker");
        assert_eq!(o.picker, Some(("BOX1".to_owned(), Some(0))));
        assert_eq!(o.message, ["Deposit in which BOX?"]);
        assert_eq!(o.cursor, Some(PcCursor::Party(1)));
    }

    /// Switch, Cinnabar: BOX2 scrolled in for WEEZING, its wallpaper's
    /// pale band behind the title's white letters, the hand on cell 8. The
    /// title read as nothing, and the withdraw failed "no progress in box".
    #[test]
    fn a_box_title_on_a_pale_wallpaper_band_is_read() {
        let Some(o) = read("switch-pc-box2-title.png") else {
            return;
        };
        let o = o.expect("box");
        assert_eq!(o.mode, PcMode::Box);
        assert_eq!(o.box_title.as_deref(), Some("BOX2"));
    }

    /// WITHDRAW POKéMON: box 1 with CLEFAIRY in cell 0, the hand on it and
    /// on the empty cell 1 (a blank panel), its WITHDRAW window, and
    /// "Continue BOX operations?" after it was taken.
    #[test]
    fn the_withdraw_screens_read_the_cell_under_the_hand() {
        let Some(o) = read("emu-pc-withdraw-box.png") else {
            return;
        };
        let o = o.expect("box");
        assert_eq!(o.mode, PcMode::Box);
        assert_eq!(
            cursor_and_panel(&o),
            (Some(PcCursor::Cell(0)), Some("CLEFAIRY"), Some(8))
        );
        assert_eq!(o.box_title.as_deref(), Some("BOX1"));
        let o = read("emu-pc-withdraw-empty.png")
            .unwrap()
            .expect("empty cell");
        assert_eq!(cursor_and_panel(&o), (Some(PcCursor::Cell(1)), None, None));
        assert_eq!(o.nickname, None);
        // The hand over the title's left end: the title still names box 1.
        assert!(o.box_title.as_deref().is_some_and(|t| t.ends_with("BOX1")));
        let o = read("emu-pc-withdraw-actions.png")
            .unwrap()
            .expect("actions");
        assert_eq!(
            o.cursor,
            Some(PcCursor::Cell(0)),
            "the menu's white is no glove"
        );
        assert_eq!(o.message, ["CLEFAIRY is selected."]);
        let o = read("emu-pc-continue.png").unwrap().expect("question");
        assert_eq!(o.message, ["Continue BOX operations?"]);
        assert_eq!(o.species, None);
    }

    /// From the swap on the emulator (`tests/pc_emulator.rs`): the hand on
    /// cell 2 hides the title's `BO` (the rest reads), an empty mid-grid
    /// cell, and the party's last slot after a withdrawal.
    #[test]
    fn the_hand_anywhere_on_the_grid_and_the_party() {
        let Some(o) = read("emu-pc-withdraw-title-hidden.png") else {
            return;
        };
        let o = o.expect("box");
        assert_eq!(
            cursor_and_panel(&o),
            (Some(PcCursor::Cell(2)), Some("PARAS"), Some(13))
        );
        assert_eq!(o.box_title.as_deref(), Some("X1"));
        let o = read("emu-pc-withdraw-cell17.png").unwrap().expect("box");
        assert_eq!(cursor_and_panel(&o), (Some(PcCursor::Cell(17)), None, None));
        assert_eq!(o.box_title.as_deref(), Some("BOX1"));
        let o = read("emu-pc-deposit-slot5.png").unwrap().expect("party");
        assert_eq!(o.mode, PcMode::Party);
        assert_eq!(
            cursor_and_panel(&o),
            (Some(PcCursor::Party(5)), Some("EKANS"), Some(8))
        );
    }

    /// The PC's own menus are field menus over the Center: not the storage
    /// screen. Neither is any other fixture.
    #[test]
    fn no_other_screen_is_the_storage_system() {
        let Ok(font) = Font::load(root().join("data/world/font_normal.json")) else {
            return;
        };
        let Ok(dir) = std::fs::read_dir(root().join("captures/fixtures")) else {
            return;
        };
        let mut seen = 0;
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".png") {
                continue;
            }
            let Ok(image) = pokebot_video::png::load(entry.path()) else {
                continue;
            };
            if image.width() != 240 || image.height() != 160 {
                continue;
            }
            let storage = [
                "emu-pc-deposit",
                "emu-pc-withdraw",
                "emu-pc-continue",
                "emu-pc-box",
                "switch-pc-box",
            ]
            .iter()
            .any(|p| name.starts_with(p));
            let menu = crate::detect::menu::detect(&image).map(|m| m.window);
            assert_eq!(detect(&image, &font, menu).is_some(), storage, "{name}");
            seen += 1;
        }
        assert!(seen > 0);
    }

    #[test]
    fn levels_and_counts_parse() {
        assert_eq!(parse_level("♀ Lv10"), Some(10));
        assert_eq!(parse_level("Lv 8"), Some(8));
        assert_eq!(parse_level("Lv"), None);
        assert_eq!(parse_count("0 /30"), Some(0));
        assert_eq!(parse_count("3O /30"), Some(30));
    }
}
