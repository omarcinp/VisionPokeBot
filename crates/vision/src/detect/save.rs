//! SAVE's stats panel: a count, not an ordered list of gym badges.
//! `start_menu.c::PrintSaveStats`: window (8,8), small-font rows 14/28.
use crate::text::Font;
use pokebot_core::RgbImage;
use pokebot_state::Region;

pub fn badge_count(image: &RgbImage, small: &Font) -> Option<u8> {
    // Two complete labels and the panel's pale background prevent a number
    // elsewhere on the field (or a fade) from becoming a badge count.
    if super::share(image, Region::new(10, 20, 108, 57), [255, 251, 255], 2) < 500 {
        return None;
    }
    let label = |y| small.read(image, Region::new(10, y, 53, 14), &[]).join("");
    if label(22) != "PLAYER" || label(36) != "BADGES" {
        return None;
    }
    let text = small
        .read(image, Region::new(68, 36, 45, 14), &[])
        .join("")
        .replace('O', "0");
    if text.len() != 1 || !text.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|n| *n <= 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_totals_and_unreadable_or_faded_counts() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = Font::load(root.join("data/world/font_small.json")) else {
            return;
        };
        let original =
            pokebot_video::png::load(root.join("captures/fixtures/emu-save-badges.png")).unwrap();
        for count in ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "1?", ""] {
            let mut image = original.clone();
            super::super::testing::fill(&mut image, 68, 36, 45, 14, [255, 251, 255]);
            // The small font shares 0's bitmap with O; the loaded font
            // retains the letter alias, including in its test renderer.
            font.render(
                &mut image,
                68,
                36,
                &count.replace('0', "O"),
                [82, 82, 82],
                [181, 181, 181],
            );
            assert_eq!(
                badge_count(&image, &font),
                count.parse::<u8>().ok().filter(|n| *n <= 8),
                "{count}"
            );
        }
        let mut fade = original;
        for y in 0..160 {
            for x in 0..240 {
                let p = fade.pixel(x, y).map(|c| c / 2);
                fade.put_pixel(x, y, p);
            }
        }
        assert_eq!(badge_count(&fade, &font), None);
    }

    #[test]
    fn save_totals_on_emulator_and_switch_keep_the_confirmation_dialogue() {
        use crate::PerceptionSystem;
        use pokebot_core::NormalizedFrame;
        use std::{path::Path, sync::Arc, time::Instant};
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(small) = Font::load(root.join("data/world/font_small.json")) else {
            return;
        };
        let small = Arc::new(small);
        let normal = Arc::new(Font::load(root.join("data/world/font_normal.json")).unwrap());
        let mut perception = crate::FireRedPerception::default()
            .with_font(normal)
            .with_small_font(small.clone());
        for name in ["emu-save-badges.png", "switch-save-badges.png"] {
            let image =
                pokebot_video::png::load(root.join("captures/fixtures").join(name)).unwrap();
            assert_eq!(badge_count(&image, &small), Some(1), "{name}");
            let frame = NormalizedFrame::new(100, Instant::now(), image).unwrap();
            let o = perception.observe(&frame);
            assert_eq!(o.save_badge_count, Some(1), "{name}");
            assert!(
                o.dialogue.is_some(),
                "save dialogue must still be readable: {name}"
            );
        }
        for name in [
            "emu-trainer-card.png",
            "emu-summary-info.png",
            "emu-summary-skills.png",
        ] {
            let image =
                pokebot_video::png::load(root.join("captures/fixtures").join(name)).unwrap();
            assert_eq!(badge_count(&image, &small), None, "{name}");
        }
    }
}
