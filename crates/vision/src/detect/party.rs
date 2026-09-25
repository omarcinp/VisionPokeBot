//! Field party menu and the three summary pages. Regions follow the
//! game's window templates; numeric fields reject partial OCR.
use super::share;
use crate::text::Font;
use pokebot_core::RgbImage;
use pokebot_state::{PartyMenuObservation, Region, Status, SummaryObservation, SummaryPage};

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
    let mut moves = Vec::new();
    if page == SummaryPage::Moves {
        for i in 0..4 {
            let name = read(image, font, 160, 19 + 28 * i, 78, 14);
            if !name.is_empty() && name.chars().all(|c| c == '-') {
                continue;
            }
            // PP prints dark, then yellow/orange/red as it runs low: the
            // fixed dark mask first, any ink when it misses.
            let region = Region::new(205, 32 + 28 * i, 31, 13);
            let pp = pair(&font.read_dark(image, region).join(""))
                .or_else(|| pair(&font.read(image, region, &[]).join("")))
                .and_then(|(a, b)| Some((a.try_into().ok()?, b.try_into().ok()?)));
            moves.push((name, pp));
        }
    }
    // The status sprite is 32x8, centred at (16,38). Its background
    // palette distinguishes ailments without trying to read tiny letters.
    let status = [
        ([197, 98, 197], Status::Poisoned),
        ([189, 189, 24], Status::Paralyzed),
        ([164, 164, 139], Status::Asleep),
        ([139, 180, 230], Status::Frozen),
        ([230, 115, 82], Status::Burned),
        ([180, 32, 32], Status::Fainted),
    ]
    .into_iter()
    .find_map(|(color, status)| {
        (share(image, Region::new(0, 34, 32, 8), color, 1) > 300).then_some(status)
    });
    // Healthy summary background is pale lavender with horizontal lines.
    let status = status.or_else(|| {
        (share(image, Region::new(0, 34, 32, 8), [239, 227, 247], 1) > 650)
            .then_some(Status::Healthy)
    });
    Some(SummaryObservation {
        page,
        nickname,
        level,
        species,
        held_item,
        hp,
        status,
        moves,
    })
}

pub fn menu(image: &RgbImage, font: &Font) -> Option<PartyMenuObservation> {
    // Dialogue can also say "POKéMON". Require the party menu's striped
    // teal background, outside all six panels, the bottom message and a
    // field move's description box (over the prompt, from y 112).
    if super::share_any(
        image,
        Region::new(16, 80, 70, 28),
        &[[74, 170, 165], [57, 138, 140]],
        2,
    ) < 850
    {
        return None;
    }
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
    for i in 0..5 {
        if occupied(Region::new(probe_x, 12 + 24 * i, 16, 12)) {
            count += 1;
        } else {
            break;
        }
    }
    let selected = (0..count).find(|&i| {
        let r = if i == 0 {
            Region::new(8, 26, 77, 2)
        } else {
            // The panel's orange top border: rows 10..=11 (measured); left
            // of the action window when it is open.
            let width = if options.is_empty() { 142 } else { 48 };
            Region::new(96, 9 + 24 * (i - 1) as u32, width, 3)
        };
        share(image, r, [255, 131, 49], 1) > 200
    });
    // While a TM or HM is taught every panel says ABLE! or NOT ABLE!
    // where the HP bar is (the lead's under its name, the others right
    // of theirs).
    let able: Vec<Option<bool>> = (0..count)
        .map(|i| {
            let text = if i == 0 {
                read(image, font, 8, 56, 80, 14)
            } else {
                read(image, font, 168, 10 + 24 * (u32::from(i) - 1), 70, 14)
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
    })
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
        assert_eq!(s.status, Some(Status::Healthy));
        let s = summary(&load("emu-summary-skills.png").unwrap(), &font).unwrap();
        assert_eq!(s.hp, Some((54, 54)));
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
        if let Ok(image) = load("switch-summary-skills-27.png") {
            assert_eq!(summary(&image, &font).unwrap().hp, Some((27, 27)));
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
}
