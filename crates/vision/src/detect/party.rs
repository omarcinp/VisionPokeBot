//! Field party menu and the three summary pages. Regions follow the
//! game's window templates; numeric fields reject partial OCR.
use super::share;
use crate::text::Font;
use pokebot_core::RgbImage;
use pokebot_state::{
    PartyMenuObservation, PartyRowObservation, Region, ShinyReading, Status, SummaryObservation,
    SummaryPage,
};

fn read(image: &RgbImage, font: &Font, x: u32, y: u32, w: u32, h: u32) -> String {
    font.read(image, Region::new(x, y, w, h), &[])
        .join(" ")
        .trim()
        .to_owned()
}

fn pair(text: &str) -> Option<(u16, u16)> {
    let compact = text.replace(' ', "");
    let (a, b) = compact.split_once('/')?;
    let (a, b): (u16, u16) = (a.parse().ok()?, b.parse().ok()?);
    (b > 0 && a <= b).then_some((a, b))
}

pub fn summary(image: &RgbImage, font: &Font) -> Option<SummaryObservation> {
    // Do not classify a dark fade as an ailment palette.
    if share(image, Region::new(40, 18, 64, 14), [255, 251, 255], 1) < 60 {
        return None;
    }
    let title = read(image, font, 0, 0, 108, 15);
    let page = if title.starts_with("POKéMON INFO") {
        SummaryPage::Info
    } else if title.starts_with("POKéMON SKILLS") {
        SummaryPage::Skills
    } else if title.starts_with("KNOWN MOVES") {
        SummaryPage::Moves
    } else {
        return None;
    };
    let nickname = read(image, font, 40, 18, 64, 14);
    let level_text = read(image, font, 4, 18, 32, 14);
    let level = level_text
        .strip_prefix("Lv")
        .and_then(|s| s.parse().ok())
        .filter(|n| (1..=100).contains(n));
    let species = (page == SummaryPage::Info).then(|| read(image, font, 166, 35, 73, 14));
    let held_item = (page == SummaryPage::Info).then(|| read(image, font, 166, 95, 73, 14));
    let hp = (page == SummaryPage::Skills)
        .then(|| pair(&font.read_dark(image, Region::new(172, 18, 66, 14)).join("")))
        .flatten();
    let mut details = pokebot_state::SummaryDetails::default();
    if page == SummaryPage::Info {
        let ot = read(image, font, 167, 65, 72, 14);
        details.original_trainer = (!ot.is_empty() && !ot.contains('?')).then_some(ot);
        let id = font.read_dark(image, Region::new(167, 80, 36, 14)).join("");
        details.trainer_id =
            (id.len() == 5 && id.bytes().all(|c| c.is_ascii_digit()) && id.parse::<u16>().is_ok())
                .then_some(id);
        // TRAINER MEMO: "LAX nature." then where it was met.
        let memo = read(image, font, 4, 114, 232, 14);
        details.nature = memo
            .strip_suffix(" nature.")
            .filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_uppercase()))
            .map(str::to_owned);
    } else if page == SummaryPage::Skills {
        let number = |x: u32, y: u32, w: u32| -> Option<u32> {
            let text = font
                .read_dark_excluding(
                    image,
                    Region::new(x, y, w, 13),
                    &[Region::new(172, 32, 66, 7), Region::new(172, 128, 66, 7)],
                )
                .join("");
            (!text.is_empty() && text.bytes().all(|c| c.is_ascii_digit())).then_some(())?;
            text.parse().ok()
        };
        let stat = |y| {
            number(210, y, 28)
                .filter(|n| (1..=999).contains(n))
                .map(|n| n as u16)
        };
        details.attack = stat(38);
        details.defense = stat(51);
        details.sp_attack = stat(64);
        details.sp_defense = stat(77);
        details.speed = stat(90);
        details.exp_points = number(175, 103, 63).filter(|n| *n <= 1_640_000);
        details.next_level = number(175, 116, 63).filter(|n| *n <= 1_640_000);
        let ability = read(image, font, 74, 129, 158, 14);
        details.ability = (!ability.is_empty() && !ability.contains('?')).then_some(ability);
    }
    let mut moves = Vec::new();
    if page == SummaryPage::Moves {
        for i in 0..4 {
            let name = read(image, font, 160, 19 + 28 * i, 78, 14);
            if !name.is_empty() && name.chars().all(|c| c == '-') {
                continue;
            }
            // PP prints dark, then yellow/orange/red as it runs low: the
            // fixed dark mask first, any ink when it misses, then any
            // saturated ink (red 0/N on the Switch splits into clusters).
            let region = Region::new(205, 32 + 28 * i, 31, 13);
            let pp = pair(&font.read_dark(image, region).join(""))
                .or_else(|| pair(&font.read(image, region, &[]).join("")))
                .or_else(|| pair(&font.read_saturated(image, region).join("")))
                .and_then(|(a, b)| Some((a.try_into().ok()?, b.try_into().ok()?)));
            moves.push((name, pp));
        }
    }
    // The status sprite is 32x8, centred at (16,38).
    let status = ailment_icon(image, Region::new(0, 34, 32, 8), [255, 251, 255]);
    // Healthy summary background is pale lavender with horizontal lines.
    let status = status.or_else(|| {
        (share(image, Region::new(0, 34, 32, 8), [239, 227, 247], 1) > 650)
            .then_some(Status::Healthy)
    });
    let shiny = matches!(page, SummaryPage::Info | SummaryPage::Skills).then(|| {
        if shiny_star(image) {
            ShinyReading::Shiny
        } else {
            ShinyReading::Normal
        }
    });
    Some(SummaryObservation {
        details,
        page,
        nickname,
        level,
        species,
        held_item,
        hp,
        status,
        moves,
        shiny,
    })
}

/// Where INFO and SKILLS draw the shiny star (`CreateShinyStarObj`: an 8×8
/// sprite centred at (106, 40)).
pub const SHINY_STAR: Region = Region::new(102, 36, 8, 8);
/// The monster picture on INFO (`CreateMonPicSprite` at (60, 65), 64×64).
pub const SUMMARY_PICTURE: Region = Region::new(28, 33, 64, 64);

/// Without the star, [`SHINY_STAR`] shows only the panel's pale grey and
/// white rows; the star's coloured pixels stand out from both.
fn shiny_star(image: &RgbImage) -> bool {
    let r = SHINY_STAR;
    let mut ink = 0;
    for y in r.y..r.y + r.height {
        for x in r.x..r.x + r.width {
            let [red, green, blue] = image.pixel(x, y);
            let (lo, hi) = (red.min(green).min(blue), red.max(green).max(blue));
            if lo < 200 || hi - lo > 40 {
                ink += 1;
            }
        }
    }
    ink >= 8
}

/// The ailment icon (`status_icons`, 32×8) drawn in `region`: its
/// background colour tells the ailment without reading its tiny letters.
/// Colours that match `background` (the surface the icon sits on) are
/// not evidence of an icon.
fn ailment_icon(image: &RgbImage, region: Region, background: crate::color::Rgb) -> Option<Status> {
    [
        ([197, 98, 197], Status::Poisoned),
        ([189, 189, 24], Status::Paralyzed),
        ([164, 164, 139], Status::Asleep),
        ([139, 180, 230], Status::Frozen),
        ([230, 115, 82], Status::Burned),
        ([180, 32, 32], Status::Fainted),
    ]
    .into_iter()
    .filter(|(color, _)| !crate::color::near(*color, background, crate::color::TOLERANCE))
    .find_map(|(color, status)| (share(image, region, color, 1) > 300).then_some(status))
}

/// Where the party menu puts its panels (`PARTY_LAYOUT_*`): a double
/// battle shows the two members in battle on the left, one above the
/// other, and the other four on the right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Single,
    Double,
}

impl Layout {
    /// Dialogue can also say "POKéMON". Require the party menu's striped
    /// teal background, outside all six panels, the bottom message and a
    /// field move's description box (over the prompt, from y 112): under
    /// the lead's panel in the single layout; between the right column's
    /// panels in the double one (whose slot 1 panel sits under the lead's;
    /// emulator worker 2, a double battle on Route 16 stuck on its
    /// "Choose a POKéMON." after a faint).
    fn of(image: &RgbImage) -> Option<Layout> {
        let teal = |r: Region| super::share_any(image, r, &[[74, 170, 165], [57, 138, 140]], 2);
        if teal(Region::new(16, 80, 70, 28)) >= 850 {
            return Some(Layout::Single);
        }
        // The gaps between slots 2 and 3 and slots 3 and 4 (y 32..40 and
        // 64..72 across the right column) are panels in the single layout.
        let gaps = [Region::new(96, 33, 140, 6), Region::new(96, 65, 140, 6)];
        gaps.iter()
            .all(|&r| teal(r) * 10 >= 140 * 6 * 9)
            .then_some(Layout::Double)
    }

    /// Members on the left in big panels.
    fn big(self) -> u8 {
        match self {
            Layout::Single => 1,
            Layout::Double => 2,
        }
    }

    /// The top-left of `slot`'s panel window (`s*PartyMenuWindowTemplate`,
    /// tiles × 8).
    fn window(self, slot: u8) -> (u32, u32) {
        let slot = u32::from(slot);
        match self {
            Layout::Single if slot == 0 => (8, 24),
            Layout::Single => (96, 8 + 24 * (slot - 1)),
            Layout::Double if slot < 2 => (8, 8 + 56 * slot),
            Layout::Double => (96, 8 + 32 * (slot - 2)),
        }
    }
}

pub fn menu(image: &RgbImage, font: &Font) -> Option<PartyMenuObservation> {
    let layout = Layout::of(image)?;
    // A message over the party screen ("IVYSAUR wants to learn …") is the
    // full-width message box (its mauve band runs along the whole bottom,
    // unlike the prompt box's or the action window's): dialogue, not the
    // menu's prompt.
    // (Tight tolerance: the prompt box's grey border is within the usual
    // one of the mauve.)
    let mauve = (12..228)
        .step_by(2)
        .filter(|&x| crate::color::near(image.pixel(x, 156), crate::color::SCENE_BORDER, 6))
        .count();
    if mauve * 10 >= 108 * 8 {
        return None;
    }
    let prompt = read(image, font, 6, 136, 173, 19);
    let (options, option_cursor) = action_window(image, font);
    let actions = options.iter().any(|o| o == "SUMMARY")
        || read(image, font, 156, 104, 79, 18).contains("SUMMARY");
    // The prompt box also answers a field move the game refuses ("Can't
    // use that here.").
    if prompt.is_empty() && !actions {
        return None;
    }
    // Empty slots are striped teal; occupied panels have a pale cyan fill
    // (red for fainted members). Do not derive size from cached roster.
    // The action window covers the lower panels' right halves: probe
    // left of it then.
    let occupied = |r| share(image, r, [74, 170, 165], 1) < 500;
    let probe_x = if options.is_empty() { 164 } else { 116 };
    let mut count = 1;
    for slot in 1..6 {
        let (x, y) = layout.window(slot);
        let probe = if slot < layout.big() {
            Region::new(x + 8, y + 16, 64, 24)
        } else {
            Region::new(probe_x, y + 4, 16, 12)
        };
        if occupied(probe) {
            count += 1;
        } else {
            break;
        }
    }
    let selected = (0..count).find(|&i| {
        let (x, y) = layout.window(i);
        let r = if i < layout.big() {
            Region::new(x, y + 2, 77, 2)
        } else {
            // The panel's orange top border: rows 10..=11 (measured); left
            // of the action window when it is open.
            let width = if options.is_empty() { 142 } else { 48 };
            Region::new(x, y + 1, width, 3)
        };
        share(image, r, [255, 131, 49], 1) > 200
    });
    // While a TM or HM is taught every panel says ABLE! or NOT ABLE!
    // where the HP bar is (the lead's under its name, the others right
    // of theirs).
    let able: Vec<Option<bool>> = (0..count)
        .map(|i| {
            let (x, y) = layout.window(i);
            let text = if i < layout.big() {
                read(image, font, x, y + 32, 80, 14)
            } else {
                read(image, font, x + 72, y + 2, 70, 14)
            };
            match text.as_str() {
                "ABLE!" => Some(true),
                "NOT ABLE!" => Some(false),
                _ => None,
            }
        })
        .collect();
    let able = if able.iter().any(Option::is_some) {
        able
    } else {
        Vec::new()
    };
    Some(PartyMenuObservation {
        count,
        selected,
        actions,
        prompt,
        options,
        option_cursor,
        able,
        members: Vec::new(),
        double: layout == Layout::Double,
    })
}

/// Where one panel prints its facts (`sPartyBoxInfoRects` in the windows
/// of `sSinglePartyMenuWindowTemplate`, in FONT_SMALL glyph cells): the
/// lead's window is at (8, 24), the others' at (96, 8 + 24(i − 1));
/// [`Layout::window`] for a double battle's.
/// Measured on `emu-party-cant-use.png`: the white ink sits in cell rows
/// 4..=10 with its shadow on row 11.
struct Panel {
    name: Region,
    level: Region,
    hp: Region,
    /// Rows at the top of the level cell that belong to the name above.
    level_overlap: Region,
    /// Rows at the top of the HP cell that belong to the HP bar.
    hp_overlap: Region,
    /// The ailment icon sprite (32×8, centred on `sPartyMenuSpriteCoords`'
    /// status coordinates), drawn instead of the level.
    icon: Region,
}

fn panel(layout: Layout, slot: u8) -> Panel {
    let (x, y) = layout.window(slot);
    if slot < layout.big() {
        return Panel {
            name: Region::new(x + 24, y + 11, 56, 12),
            level: Region::new(x + 32, y + 20, 32, 12),
            hp: Region::new(x + 30, y + 36, 50, 12),
            level_overlap: Region::new(x + 32, y + 20, 32, 3),
            hp_overlap: Region::new(x + 30, y + 36, 50, 4),
            icon: Region::new(x + 32, y + 24, 32, 8),
        };
    }
    Panel {
        name: Region::new(x + 22, y + 3, 50, 12),
        level: Region::new(x + 32, y + 12, 32, 12),
        hp: Region::new(x + 100, y + 12, 42, 12),
        level_overlap: Region::new(x + 32, y + 12, 32, 3),
        hp_overlap: Region::new(x + 100, y + 12, 42, 3),
        icon: Region::new(x + 32, y + 15, 32, 8),
    }
}

/// Every occupied panel's nickname, level, HP and ailment, index = slot.
/// Fields under the open action window read as `None`; so do fields that
/// don't read cleanly (the sensor validates what's left).
pub fn members(
    image: &RgbImage,
    small: &Font,
    menu: &PartyMenuObservation,
) -> Vec<PartyRowObservation> {
    let covered = (!menu.options.is_empty())
        .then(|| super::menu::detect(image))
        .flatten()
        .filter(|m| m.window.x >= 140)
        // The window's frame reaches a few pixels beyond its white interior.
        .map(|m| m.window.inflate(6));
    let visible = |r: Region| covered.is_none_or(|c| !c.intersects(&r));
    let text = |r: Region, exclude: &[Region]| {
        visible(r).then(|| small.read(image, r, exclude).join(" ").trim().to_owned())
    };
    // The small font's 0 and O are the same bitmap (ties read as the
    // letter): numbers read with O for 0.
    let number = |r: Region, exclude: &[Region]| text(r, exclude).map(|t| t.replace('O', "0"));
    let layout = Layout::of(image).unwrap_or(Layout::Single);
    (0..menu.count)
        .map(|slot| {
            let p = panel(layout, slot);
            let nickname = text(p.name, &[]).filter(|n| !n.is_empty());
            let level = number(p.level, &[p.level_overlap])
                .and_then(|t| t.strip_prefix("Lv").and_then(|n| n.trim().parse().ok()))
                .filter(|n| (1..=100).contains(n));
            let hp = number(p.hp, &[p.hp_overlap]).and_then(|t| pair(&t));
            // The level is printed only without an ailment; without it,
            // the icon's colour, unless that is the panel's own (the FRZ
            // icon's blue is within tolerance of the panels' blue).
            let background = image.pixel(p.icon.x - 4, p.icon.y + 3);
            let icon = (level.is_none() && visible(p.icon))
                .then(|| ailment_icon(image, p.icon, background))
                .flatten();
            let status = match (hp, icon) {
                (Some((0, _)), _) => Some(Status::Fainted),
                (_, Some(status)) => Some(status),
                _ if level.is_some() => Some(Status::Healthy),
                _ => None,
            };
            PartyRowObservation {
                nickname,
                level,
                hp,
                status,
            }
        })
        .collect()
}

/// The action window opened on a member (right half, above the prompt):
/// its rows read one at a time, since field moves print in blue and the
/// rest in grey (one read picks one ink), and the ▶'s row.
fn action_window(image: &RgbImage, font: &Font) -> (Vec<String>, Option<u8>) {
    let Some(menu) = super::menu::detect(image).filter(|m| m.window.x >= 140) else {
        return (Vec::new(), None);
    };
    let cursor = super::menu::cursor_region(image, &menu);
    let options: Vec<String> = (0..u32::from(menu.rows))
        .map(|row| {
            let r = Region::new(
                menu.window.x,
                menu.window.y + 16 * row,
                menu.window.width,
                16,
            );
            font.read(image, r, &[cursor]).join(" ").trim().to_owned()
        })
        .collect();
    if options.iter().all(String::is_empty) {
        return (Vec::new(), None);
    }
    (options, Some(menu.cursor_row))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn id_leading_zeroes_survive_and_missing_stats_stay_unknown() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = Font::load(root.join("data/world/font_normal.json")) else {
            return;
        };
        let mut info =
            pokebot_video::png::load(root.join("captures/fixtures/emu-summary-info.png")).unwrap();
        super::super::testing::fill(&mut info, 167, 80, 36, 14, [255, 251, 255]);
        font.render(&mut info, 167, 80, "00042", [82, 82, 82], [181, 181, 181]);
        assert_eq!(
            summary(&info, &font).unwrap().details.trainer_id.as_deref(),
            Some("00042")
        );
        super::super::testing::fill(&mut info, 167, 80, 36, 14, [255, 251, 255]);
        font.render(&mut info, 167, 80, "42", [82, 82, 82], [181, 181, 181]);
        assert!(summary(&info, &font).unwrap().details.trainer_id.is_none());
        let mut skills =
            pokebot_video::png::load(root.join("captures/fixtures/emu-summary-skills.png"))
                .unwrap();
        super::super::testing::fill(&mut skills, 210, 51, 28, 13, [255, 251, 255]);
        let s = summary(&skills, &font).unwrap();
        assert!(s.details.defense.is_none());
        assert_eq!(s.details.attack, Some(33));
    }

    #[test]
    fn the_shiny_star_is_read_on_info_and_skills() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = Font::load(root.join("data/world/font_normal.json")) else {
            return;
        };
        for name in [
            "emu-summary-info.png",
            "emu-summary-skills.png",
            "switch-summary-info-hasty.png",
            "switch-summary-skills-27.png",
        ] {
            let Ok(mut image) = pokebot_video::png::load(root.join("captures/fixtures").join(name))
            else {
                continue;
            };
            let s = summary(&image, &font).unwrap();
            assert_eq!(s.shiny, Some(ShinyReading::Normal), "{name}");
            // A yellow star drawn where the game puts it.
            super::super::testing::fill(&mut image, 104, 37, 4, 6, [255, 213, 0]);
            let s = summary(&image, &font).unwrap();
            assert_eq!(s.shiny, Some(ShinyReading::Shiny), "{name}");
        }
        // The emulator's shiny-starter hunt: a real star, on both pages.
        for name in ["emu-summary-info-shiny.png", "emu-summary-skills-shiny.png"] {
            if let Ok(image) = pokebot_video::png::load(root.join("captures/fixtures").join(name)) {
                let s = summary(&image, &font).unwrap();
                assert_eq!(s.shiny, Some(ShinyReading::Shiny), "{name}");
            }
        }
        let Ok(moves) =
            pokebot_video::png::load(root.join("captures/fixtures/emu-summary-moves.png"))
        else {
            return;
        };
        assert_eq!(summary(&moves, &font).unwrap().shiny, None);
    }

    #[test]
    fn reads_real_summary_pages_and_rejects_bad_numbers() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = Font::load(root.join("data/world/font_normal.json")) else {
            return;
        };
        let load = |name: &str| pokebot_video::png::load(root.join("captures/fixtures").join(name));
        let Ok(info) = load("emu-summary-info.png") else {
            return;
        };
        let s = summary(&info, &font).unwrap();
        assert_eq!(s.species.as_deref(), Some("IVYSAUR"));
        assert_eq!(s.level, Some(18));
        assert_eq!(s.details.trainer_id.as_deref(), Some("62540"));
        assert_eq!(s.details.original_trainer.as_deref(), Some("RED"));
        assert_eq!(s.details.nature.as_deref(), Some("LAX"));
        // Switch goal run, frame 2594860: the memo on the capture card.
        if let Ok(info) = load("switch-summary-info-hasty.png") {
            let s = summary(&info, &font).unwrap();
            assert_eq!(s.level, Some(10));
            assert_eq!(s.details.nature.as_deref(), Some("HASTY"));
        }
        assert_eq!(s.status, Some(Status::Healthy));
        let s = summary(&load("emu-summary-skills.png").unwrap(), &font).unwrap();
        assert_eq!(s.hp, Some((54, 54)));
        assert_eq!(
            s.details,
            pokebot_state::SummaryDetails {
                attack: Some(33),
                defense: Some(30),
                sp_attack: Some(37),
                sp_defense: Some(32),
                speed: Some(34),
                exp_points: Some(3878),
                next_level: Some(697),
                ability: Some("OVERGROW".into()),
                ..Default::default()
            }
        );
        let s = summary(&load("emu-summary-moves.png").unwrap(), &font).unwrap();
        assert_eq!(
            s.moves,
            vec![
                ("TACKLE".into(), Some((35, 35))),
                ("SLEEP POWDER".into(), Some((15, 15))),
                ("LEECH SEED".into(), Some((10, 10))),
                ("VINE WHIP".into(), Some((10, 10)))
            ]
        );
        assert_eq!(
            menu(&load("emu-party-menu.png").unwrap(), &font)
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            menu(&load("emu-party-menu.png").unwrap(), &font)
                .unwrap()
                .selected,
            Some(0)
        );
        assert!(
            menu(&load("emu-party-actions.png").unwrap(), &font)
                .unwrap()
                .actions
        );
        for name in [
            "emu-summary-moves-low-pp.png",
            "switch-summary-moves-low-pp.png",
        ] {
            if let Ok(image) = load(name) {
                let s = summary(&image, &font).unwrap();
                assert_eq!(s.moves[3], ("VINE WHIP".into(), Some((5, 10))), "{name}");
            }
        }
        // A move out of PP prints 0/10 in red (live: the audit failed on it).
        if let Ok(image) = load("switch-summary-moves-pp0.png") {
            let s = summary(&image, &font).unwrap();
            assert_eq!(
                s.moves[3],
                ("VINE WHIP".into(), Some((0, 10))),
                "{:?}",
                s.moves
            );
        }
        if let Ok(image) = load("switch-summary-skills-27.png") {
            let s = summary(&image, &font).unwrap();
            assert_eq!(s.hp, Some((27, 27)));
            assert_eq!(
                s.details,
                pokebot_state::SummaryDetails {
                    attack: Some(12),
                    defense: Some(14),
                    sp_attack: Some(17),
                    sp_defense: Some(18),
                    speed: Some(13),
                    exp_points: Some(489),
                    next_level: Some(71),
                    ability: Some("OVERGROW".into()),
                    ..Default::default()
                }
            );
        }
        if let Ok(image) = load("emu-summary-fade.png") {
            assert!(summary(&image, &font).is_none());
        }
        if let Ok(image) = load("emu-nurse-question.png") {
            assert!(menu(&image, &font).is_none());
        }
        assert_eq!(pair("9/2?"), None);
        assert_eq!(pair("9/2"), None);
    }

    /// The action window with a field move (CUT printed in blue), the
    /// description box that covers the prompt while CUT is under the ▶,
    /// a member without field moves, and ABLE!/NOT ABLE! while teaching.
    #[test]
    fn reads_the_action_window_and_teach_panels() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = Font::load(root.join("data/world/font_normal.json")) else {
            return;
        };
        let load = |name: &str| pokebot_video::png::load(root.join("captures/fixtures").join(name));
        let rows = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        if let Ok(image) = load("emu-party-field-moves.png") {
            let m = menu(&image, &font).expect("party menu");
            assert!(m.actions);
            assert_eq!(
                m.options,
                rows(&["SUMMARY", "CUT", "SWITCH", "ITEM", "CANCEL"])
            );
            assert_eq!(m.option_cursor, Some(0));
            assert_eq!(m.selected, Some(0));
            assert_eq!(m.count, 5);
            assert!(m.able.is_empty());
        }
        if let Ok(image) = load("emu-party-field-move-hover.png") {
            let m = menu(&image, &font).expect("party menu under the description");
            assert_eq!(m.option_cursor, Some(1));
            assert_eq!(m.options.get(1).map(String::as_str), Some("CUT"));
        }
        if let Ok(image) = load("emu-party-actions-clefairy.png") {
            let m = menu(&image, &font).expect("party menu");
            assert_eq!(m.selected, Some(4), "under the action window");
            assert_eq!(
                m.options,
                rows(&["SUMMARY", "FLASH", "SWITCH", "ITEM", "CANCEL"])
            );
        }
        if let Ok(image) = load("emu-party-actions-geodude.png") {
            let m = menu(&image, &font).expect("party menu");
            assert_eq!(m.options, rows(&["SUMMARY", "SWITCH", "ITEM", "CANCEL"]));
            assert_eq!(m.selected, Some(1));
        }
        if let Ok(image) = load("emu-teach-which.png") {
            let m = menu(&image, &font).expect("party menu");
            assert_eq!(m.prompt, "Teach which POKéMON?");
            assert_eq!(
                m.able,
                vec![
                    Some(true),
                    Some(false),
                    Some(false),
                    Some(true),
                    Some(false)
                ]
            );
            assert!(m.options.is_empty());
        }
        if let Ok(image) = load("emu-teach-which-clefairy.png") {
            let m = menu(&image, &font).expect("party menu");
            assert_eq!(m.selected, Some(4));
            assert_eq!(
                m.able,
                vec![
                    Some(false),
                    Some(false),
                    Some(false),
                    Some(false),
                    Some(true)
                ]
            );
        }
        if let Ok(image) = load("emu-party-cant-use.png") {
            let m = menu(&image, &font).expect("party menu");
            assert_eq!(m.prompt, "Can’t use that here.");
        }
        // Messages over the party screen are dialogue, not the menu.
        for name in ["emu-teach-four-moves.png", "emu-teach-replace-yes-no.png"] {
            if let Ok(image) = load(name) {
                assert!(menu(&image, &font).is_none(), "{name}");
            }
        }
    }

    /// The panels' printed facts (emulator, Route 3 save): five members
    /// with the prompt showing, the action window hiding the lower
    /// panels, and teaching (ABLE!/NOT ABLE! where the HP was).
    #[test]
    fn reads_every_members_panel() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(font), Ok(small)) = (
            Font::load(root.join("data/world/font_normal.json")),
            Font::load(root.join("data/world/font_small.json")),
        ) else {
            return;
        };
        let load = |name: &str| pokebot_video::png::load(root.join("captures/fixtures").join(name));
        let rows = |name: &str| {
            let image = load(name).ok()?;
            let m = menu(&image, &font).expect(name);
            Some(members(&image, &small, &m))
        };
        let row = |name: &str, level, hp| PartyRowObservation {
            nickname: Some(String::from(name)),
            level: Some(level),
            hp,
            status: Some(Status::Healthy),
        };
        // Mid-swap (Switch) the lead's and slot 1's panels are off
        // screen: the count reads 1, but the lead's name doesn't, so the
        // sensor takes no size from it.
        if let Some(r) = rows("switch-party-swap-slide.png") {
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].nickname, None);
        }
        if let Some(r) = rows("emu-party-cant-use.png") {
            assert_eq!(
                r,
                vec![
                    row("IVYSAUR", 26, Some((64, 75))),
                    row("GEODUDE", 8, Some((26, 26))),
                    row("ZUBAT", 8, Some((26, 26))),
                    row("PARAS", 8, Some((25, 25))),
                    row("CLEFAIRY", 8, Some((30, 30))),
                ]
            );
        }
        if let Some(r) = rows("emu-party-menu.png") {
            assert_eq!(r, vec![row("IVYSAUR", 18, Some((54, 54)))]);
        }
        if let Some(r) = rows("emu-party-field-moves.png") {
            assert_eq!(r.len(), 5);
            assert_eq!(r[0], row("IVYSAUR", 26, Some((64, 75))));
            assert_eq!(r[1], row("GEODUDE", 8, Some((26, 26))));
            // Under the action window: nothing is read there.
            assert_eq!(r[3].hp, None);
            assert_eq!(r[4].hp, None);
        }
        // Switch (JPEG-softened), a one-member party: the prompt, and the
        // action window (which covers no part of the lead's panel).
        for (name, level, hp) in [
            ("switch-party-choose-lv14.png", 14, (35, 37)),
            ("switch-party-actions-lv14.png", 14, (35, 37)),
            ("switch-party-choose-lv15.png", 15, (39, 39)),
        ] {
            if let Some(r) = rows(name) {
                assert_eq!(r, vec![row("BULBASAUR", level, Some(hp))], "{name}");
            }
        }
        // A double battle's layout (emulator worker 2, Route 16): the two
        // in battle on the left, RATTATA just fainted.
        if let Some(r) = rows("emu-battle-party-double.png") {
            let fainted = PartyRowObservation {
                nickname: Some(String::from("RATTATA")),
                level: None,
                hp: Some((0, 24)),
                status: Some(Status::Fainted),
            };
            assert_eq!(
                r,
                vec![
                    row("PRIMEAPE", 48, Some((126, 135))),
                    fainted,
                    row("CHARMELEON", 24, Some((69, 69))),
                    row("WEEDLE", 5, Some((19, 19))),
                    row("CATERPIE", 3, Some((16, 16))),
                    row("DUGTRIO", 29, Some((60, 60))),
                ]
            );
            let image = load("emu-battle-party-double.png").unwrap();
            let m = menu(&image, &font).unwrap();
            assert_eq!(m.selected, Some(0));
            assert!(m.double);
            assert_eq!(m.prompt, "Choose a POKéMON.");
        }
        // Switch, Vermilion: CUT with the tree already gone answers in the
        // prompt box.
        if let Ok(image) = load("switch-party-nothing-to-cut.png") {
            let m = menu(&image, &font).expect("party menu");
            assert_eq!(m.prompt, "There’s nothing to CUT.");
            assert_eq!(m.count, 5);
        }
        // 60/ 60: the zeros read as the letter O in the small font.
        if let Some(r) = rows("switch-party-choose-ivysaur-60.png") {
            assert_eq!(r, vec![row("IVYSAUR", 22, Some((60, 60)))]);
        }
        if let Some(r) = rows("emu-teach-which.png") {
            assert_eq!(r.len(), 5);
            assert!(r.iter().all(|m| m.hp.is_none()), "{r:?}");
            assert_eq!(r[3], row("PARAS", 8, None));
        }
    }
}
