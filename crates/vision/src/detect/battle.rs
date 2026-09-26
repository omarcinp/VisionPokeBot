//! Battle screen: message box, command/move menus and HP bars.

use pokebot_core::RgbImage;
use pokebot_state::{BattleMenu, BattleObservation, Region};

use super::{px, share};
use crate::color::{near, Rgb, TOLERANCE};

pub const BOX_EDGE: Rgb = [41, 48, 49];
pub const BOX_GOLD: Rgb = [206, 170, 74];
pub const BOX_BLUE: Rgb = [41, 81, 107];
/// The battle ▶ (near-black, rows 2-3-4-5-5-5-4-3-2 wide).
const CURSOR: Rgb = [41, 48, 49];
const CURSOR_WIDTHS: [u32; 9] = [2, 3, 4, 5, 5, 5, 4, 3, 2];

/// First row of the bottom battle panel (message box and menus).
pub const PANEL_TOP: u32 = 112;

/// Where the move menu prints the selected move's "PP left/max" numbers.
pub const MOVE_PP: Region = Region {
    x: 196,
    y: 118,
    width: 40,
    height: 16,
};

/// Move-menu name cells (latin_small), menu order: top-left, top-right,
/// bottom-left, bottom-right.
pub const MOVE_NAME_CELLS: [Region; 4] = [
    Region {
        x: 16,
        y: 120,
        width: 64,
        height: 14,
    },
    Region {
        x: 88,
        y: 120,
        width: 64,
        height: 14,
    },
    Region {
        x: 16,
        y: 136,
        width: 64,
        height: 14,
    },
    Region {
        x: 88,
        y: 136,
        width: 64,
        height: 14,
    },
];

/// The game's 64×64 opponent front-sprite box (bottom on the platform).
/// Species sprites sit at different heights inside it.
pub const OPPONENT_SPRITE: Region = Region {
    x: 144,
    y: 8,
    width: 64,
    height: 64,
};

/// HP bars: left edge, row, 48 px wide.
const OPPONENT_BAR: (u32, u32) = (52, 34);
const PLAYER_BAR: (u32, u32) = (174, 92);
const BAR_WIDTH: u32 = 48;

/// The dark-blue, gold-framed message box at the bottom of battles. Only its
/// left half is checked: windows such as the level-up stats panel cover the
/// right side.
pub fn is_battle_text_box(image: &RgbImage) -> bool {
    share(image, Region::new(10, 112, 110, 1), BOX_EDGE, 2) >= 800
        && share(image, Region::new(10, 114, 110, 2), BOX_GOLD, 2) >= 800
        && share(image, Region::new(8, 120, 112, 32), BOX_BLUE, 2) >= 450
}

/// The box's light frame rows (y 117..=118) inside the gold band.
const BOX_FRAME: Rgb = [231, 219, 231];

/// How a fade has dimmed the screen. The battle's fade-out after its last
/// message subtracts one amount from every 5-bit channel (rec2-120440:
/// every colour 33 lower, blue (41,81,107) → (8,48,74); rec2-82040: 115
/// lower, the blue clamped to black); the fade into the evolution scene
/// scales instead (switch-goal-14-64460: every colour about halved).
#[derive(Debug, Clone, Copy)]
enum Dim {
    Subtract(u8),
    /// Brightness in 1/256ths.
    Scale(u32),
}

impl Dim {
    fn apply(self, c: Rgb) -> Rgb {
        match self {
            Dim::Subtract(s) => c.map(|v| v.saturating_sub(s)),
            Dim::Scale(k) => c.map(|v| (u32::from(v) * k / 256) as u8),
        }
    }
}

/// The battle text box under a fade: the same frame and fill, every colour
/// dimmed by one amount (measured on the gold band's red channel). The
/// whole screen fades with it, so it is a transition, not a message: its
/// text is the page already read before the fade began, and pressing A
/// through it would land on whatever comes next.
pub fn is_faded_text_box(image: &RgbImage) -> bool {
    let band = Region::new(10, 114, 110, 2);
    let (mut sum, mut n) = (0u32, 0u32);
    for y in band.y..band.y + band.height {
        for x in (band.x..band.x + band.width).step_by(2) {
            sum += u32::from(px(image, x, y)[0]);
            n += 1;
        }
    }
    let red = sum / n.max(1);
    // Brighter is the box itself (or no box); darker than this, the gold
    // is indistinguishable from the black a fade ends in.
    if !(40..=u32::from(BOX_GOLD[0]) - 16).contains(&red) {
        return false;
    }
    let lower = (u32::from(BOX_GOLD[0]) - red) as u8;
    let scale = red * 256 / u32::from(BOX_GOLD[0]);
    [Dim::Subtract(lower), Dim::Scale(scale)]
        .iter()
        .any(|&dim| {
            share(image, Region::new(10, 114, 110, 2), dim.apply(BOX_GOLD), 2) >= 800
                && share(image, Region::new(10, 117, 110, 1), dim.apply(BOX_FRAME), 2) >= 800
                && share(image, Region::new(8, 120, 112, 32), dim.apply(BOX_BLUE), 2) >= 450
        })
}

/// The stats window a level-up opens over the right of the battle text
/// box ("MAX. HP 39 / ATTACK 18 …"): white, framed in the `std` window's
/// mauve band (outline x 145, band x 147..=148, top band y 59..=60;
/// switch-goal-15 frame 87930). It covers the text box from x 145 on.
pub const LEVEL_UP_PANEL_LEFT: u32 = 145;

pub fn is_level_up_panel(image: &RgbImage) -> bool {
    use crate::color::{SCENE_BORDER, WHITE};
    share(image, Region::new(147, 70, 2, 80), SCENE_BORDER, 1) >= 800
        && share(image, Region::new(160, 59, 70, 2), SCENE_BORDER, 1) >= 800
        && share(image, Region::new(150, 62, 2, 90), WHITE, 1) >= 800
}

pub fn detect(image: &RgbImage) -> Option<BattleObservation> {
    let menu = find_cursor(image, PANEL_TOP + 4..image.height()).map(|(x, y)| {
        let row = u8::from(y >= 132);
        if x >= 120 {
            BattleMenu::Command {
                column: u8::from(x >= 160),
                row,
            }
        } else {
            BattleMenu::Moves {
                column: u8::from(x >= 60),
                row,
            }
        }
    });
    let player_hp = bar(image, PLAYER_BAR);
    let opponent_hp = bar(image, OPPONENT_BAR);
    let text_box = is_battle_text_box(image);
    if menu.is_none() && !text_box && player_hp.is_none() && opponent_hp.is_none() {
        return None;
    }
    let player = player_hp.and_then(|_| super::hud::player_line(image));
    let opponent = opponent_hp.and_then(|_| super::hud::opponent_line(image));
    // The icon is read only when the HUD name was: an unread name means the
    // HUD isn't settled (or isn't there), not "not caught".
    let opponent_caught = opponent
        .as_ref()
        .filter(|l| !l.name.is_empty())
        .map(|_| caught_icon(image));
    Some(BattleObservation {
        menu,
        player_name: player.as_ref().map(|l| l.name.clone()),
        player_level: player.and_then(|l| l.level),
        player_hp_numbers: player_hp.and_then(|_| super::hud::player_hp(image)),
        opponent_name: opponent.as_ref().map(|l| l.name.clone()),
        opponent_level: opponent.and_then(|l| l.level),
        player_hp,
        opponent_hp,
        move_pp: None,
        move_names: Vec::new(),
        opponent_caught,
        opponent_shiny: None,
    })
}

/// The caught-ball icon: a 7×7 Poké Ball at x 20..=26, y 31..=37, under
/// the opponent's name. Checked one pixel wider on every side so a capture
/// that lands a pixel off still counts.
const CAUGHT_ICON: Region = Region {
    x: 19,
    y: 30,
    width: 9,
    height: 9,
};
const ICON_OUTLINE: Rgb = [74, 65, 90];
const ICON_ORANGE: Rgb = [255, 178, 66];
const ICON_RED: Rgb = [222, 105, 90];
/// Shares (per mille of the 9×9 probe): the icon has 16 outline and 7
/// orange/red pixels (198‰ and 86‰); the empty HUD has none.
const ICON_OUTLINE_SHARE: u32 = 120;
const ICON_TOP_SHARE: u32 = 50;

/// The caught-ball icon under the opponent's name (the species is caught).
/// Needs the outline and the orange/red top half: HUD ink alone (the name
/// line) is close to the outline colour but never orange.
pub fn caught_icon(image: &RgbImage) -> bool {
    share(image, CAUGHT_ICON, ICON_OUTLINE, 1) >= ICON_OUTLINE_SHARE
        && share(image, CAUGHT_ICON, ICON_ORANGE, 1) + share(image, CAUGHT_ICON, ICON_RED, 1)
            >= ICON_TOP_SHARE
}

/// Top-left of the first battle ▶ whose top row is in `rows`.
pub fn find_cursor(image: &RgbImage, rows: std::ops::Range<u32>) -> Option<(u32, u32)> {
    // Compression only darkens the cursor's edges (Switch, the tip at BAG
    // read (19, 27, 24), 25 off): a neutral grey darker than it counts too.
    // The shape keeps letters (grey 63..85) and black screens out.
    let dark = |x: u32, y: u32| {
        let p = px(image, x, y);
        near(p, CURSOR, TOLERANCE)
            || (p.iter().zip(CURSOR).all(|(a, b)| *a <= b)
                && p.iter().max().unwrap() - p.iter().min().unwrap() <= 16)
    };
    let last = rows.end.min(image.height() - CURSOR_WIDTHS.len() as u32);
    for y in rows.start..last {
        for x in 1..image.width() - 8 {
            let matches = CURSOR_WIDTHS.iter().enumerate().all(|(i, &w)| {
                let yy = y + i as u32;
                (0..w).all(|c| dark(x + c, yy)) && !dark(x + w, yy) && !dark(x - 1, yy)
            });
            if matches {
                return Some((x, y));
            }
        }
    }
    None
}

/// HP bar frame and empty-segment colour.
const BAR_FRAME: Rgb = [82, 105, 90];
const BAR_LEFT_EDGE: Rgb = [255, 251, 255];
const BAR_RIGHT_EDGE: Rgb = [255, 251, 222];

/// Fill of a 48-px HP bar (per mille), or `None` if no bar is there. The bar
/// must have its frame (gray-green outside, white/cream inner edges) and
/// every pixel must be either fill (green/yellow/red) or empty (frame
/// colour). Checks neighbouring rows because the player's box bobs.
fn bar(image: &RgbImage, (x0, y0): (u32, u32)) -> Option<u16> {
    let filled = |p: Rgb| {
        let (max, min) = (*p.iter().max().unwrap(), *p.iter().min().unwrap());
        max >= 180 && max - min >= 90
    };
    (y0.saturating_sub(2)..=y0 + 2).find_map(|y| {
        let framed = near(px(image, x0 - 2, y), BAR_FRAME, 20)
            && near(px(image, x0 - 1, y), BAR_LEFT_EDGE, 20)
            && near(px(image, x0 + BAR_WIDTH, y), BAR_RIGHT_EDGE, 20);
        if !framed {
            return None;
        }
        let run = (0..BAR_WIDTH)
            .take_while(|dx| filled(px(image, x0 + dx, y)))
            .count() as u32;
        let rest_empty = (run..BAR_WIDTH).all(|dx| near(px(image, x0 + dx, y), BAR_FRAME, 20));
        rest_empty.then(|| (run * 1000 / BAR_WIDTH) as u16)
    })
}
