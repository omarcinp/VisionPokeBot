//! Perception: turns normalized frames into observations. Produces facts
//! only; it never chooses actions.
//!
//! Detectors are deterministic rules over FireRed's fixed UI (palette colours
//! and geometry measured from the game), not learned models or stored game
//! graphics.

pub mod color;
pub mod detect;
pub mod shiny;
pub mod text;

use std::sync::Arc;

use pokebot_core::{NormalizedFrame, RgbImage};
use pokebot_state::{
    BattleMenu, DialogueKind, DialogueObservation, FrameMetrics, Observation, Observed, PlayerPose,
    Region, ScreenState,
};
use pokebot_world::{localize::PLAYER_SPRITE, Localizer, World};

pub trait PerceptionSystem {
    fn observe(&mut self, frame: &NormalizedFrame) -> Observation;

    /// Where the player is believed to be (from story knowledge); narrows the
    /// localization search. Wrong hints only cost search time.
    fn set_pose_hint(&mut self, _pose: PlayerPose) {}
}

/// FireRed/LeafGreen perception.
#[derive(Default)]
pub struct FireRedPerception {
    previous: Option<RgbImage>,
    world: Option<Arc<World>>,
    /// Search every map when there is no hint (slow; off by default).
    global_search: bool,
    /// Last located pose: where the next search starts (speed only; a wrong
    /// hint just means a wider search).
    hint: Option<PlayerPose>,
    frames_since_global_search: u32,
    /// Previous frame's text cells and how long they have been unchanged.
    /// Text cells of the last dialogue and the frame they first appeared.
    previous_text: Option<(Vec<u8>, u64)>,
    font: Option<Arc<text::Font>>,
    /// Reads small-font text (battle move names).
    small_font: Option<Arc<text::Font>>,
    /// Last text read and the text cells it was read from.
    last_read: Option<(Vec<u8>, Vec<String>)>,
    /// Species sprite palettes for the opponent's shiny check.
    palettes: Option<Arc<shiny::SpritePalettes>>,
}

/// Frames between whole-world searches while the player can't be located.
const GLOBAL_SEARCH_INTERVAL: u32 = 120;

/// A frame counts as a transition when this share of pixels (per mille) is
/// near-black or near-white.
const UNIFORM_PER_MILLE: u32 = 995;
const DARK_LUMA: u8 = 24;
const BRIGHT_LUMA: u8 = 232;

impl PerceptionSystem for FireRedPerception {
    fn observe(&mut self, frame: &NormalizedFrame) -> Observation {
        let image = frame.image();
        let metrics = self.metrics(image);
        let screen = |value, detector: &str| Observed {
            value,
            detector: detector.to_owned(),
        };
        if is_uniform(image) {
            return Observation::bare(
                frame.frame_id,
                screen(ScreenState::Transition, "uniform-frame"),
                metrics,
            );
        }
        if let Some(naming) = detect::naming::detect(image) {
            let mut observation = Observation::bare(
                frame.frame_id,
                screen(ScreenState::Naming, "naming-keyboard"),
                metrics,
            );
            observation.naming = Some(naming);
            return observation;
        }
        if detect::title::is_title(image) {
            return Observation::bare(
                frame.frame_id,
                screen(ScreenState::TitleScreen, "title-bands"),
                metrics,
            );
        }
        if let Some(menu) = detect::main_menu::detect(image) {
            let mut observation = Observation::bare(
                frame.frame_id,
                screen(ScreenState::MainMenu, "main-menu"),
                metrics,
            );
            observation.menu = Some(menu);
            return observation;
        }
        if detect::pokedex::is_page(image) {
            let mut observation = Observation::bare(
                frame.frame_id,
                screen(ScreenState::Unknown, "pokedex-page"),
                metrics,
            );
            observation.pokedex_page = true;
            return observation;
        }
        if let Some(list) = detect::move_list::detect(image, self.font.as_deref()) {
            let mut observation = Observation::bare(
                frame.frame_id,
                screen(ScreenState::LearnMove, "known-moves"),
                metrics,
            );
            observation.move_list = Some(list);
            return observation;
        }
        if let (Some(font), Some(small_font)) = (&self.font, &self.small_font) {
            if let Some(bag) = detect::bag::detect(image, font, small_font) {
                let state = if bag.prompt.is_some() {
                    ScreenState::BattleBag
                } else {
                    ScreenState::Bag
                };
                let mut observation =
                    Observation::bare(frame.frame_id, screen(state, "bag-screen"), metrics);
                observation.bag = Some(bag);
                return observation;
            }
            if let Some(shop) = detect::shop::detect(image, font, small_font) {
                let mut observation = Observation::bare(
                    frame.frame_id,
                    screen(ScreenState::Shop, "shop-screen"),
                    metrics,
                );
                observation.shop = Some(shop);
                return observation;
            }
        }
        let mut dialogue = detect::dialogue::detect(image);
        match &mut dialogue {
            Some(d) => {
                // Frames (not observations) since the text last changed.
                let since = match &self.previous_text {
                    Some((cells, since))
                        if detect::dialogue::changed_cells(cells, &d.text_cells) == 0 =>
                    {
                        *since
                    }
                    _ => frame.frame_id,
                };
                d.stable_frames = u32::try_from(frame.frame_id - since).unwrap_or(u32::MAX);
                self.previous_text = Some((d.text_cells.clone(), since));
                d.lines = self.read_text(image, d);
                if let (DialogueKind::InfoPage, Some(font)) = (d.kind, &self.font) {
                    let title = font.read(image, detect::dialogue::INFO_TITLE, &[]);
                    d.help = detect::dialogue::is_help_title(&title);
                }
            }
            None => self.previous_text = None,
        }
        let mut battle = detect::battle::detect(image);
        if let (Some(b), Some(font)) = (&mut battle, &self.font) {
            if matches!(b.menu, Some(BattleMenu::Moves { .. })) {
                b.move_pp = read_fraction(&font.read(image, detect::battle::MOVE_PP, &[]).join(""));
            }
        }
        if let (Some(b), Some(small)) = (&mut battle, &self.small_font) {
            if matches!(b.menu, Some(BattleMenu::Moves { .. })) {
                b.move_names = detect::battle::MOVE_NAME_CELLS
                    .iter()
                    .map(|r| small.read(image, *r, &[]).join(" "))
                    .collect();
            }
        }
        if let (Some(b), Some(palettes)) = (&mut battle, &self.palettes) {
            b.opponent_shiny = opponent_shiny(image, b, palettes);
        }
        let battle_menu = battle.as_ref().and_then(|b| b.menu);
        // In battle, list menus appear only over battle text (YES/NO
        // questions such as "Delete a move…?").
        let menu = if battle_menu.is_some() {
            None
        } else {
            detect::menu::detect(image)
        };
        let state = match (&dialogue, &menu) {
            _ if matches!(battle_menu, Some(BattleMenu::Command { .. })) => {
                screen(ScreenState::BattleCommand, "battle-cursor")
            }
            _ if matches!(battle_menu, Some(BattleMenu::Moves { .. })) => {
                screen(ScreenState::BattleMoveSelection, "battle-cursor")
            }
            (Some(d), _) if d.kind == DialogueKind::BattleText => {
                screen(ScreenState::BattleText, "battle-text-box")
            }
            (Some(d), _) if d.kind == DialogueKind::InfoPage => {
                screen(ScreenState::InfoPage, "info-page")
            }
            (Some(_), _) => screen(ScreenState::Dialogue, "message-box"),
            (None, Some(_)) => screen(ScreenState::Menu, "menu-cursor"),
            (None, None) => screen(ScreenState::Unknown, "none"),
        };
        let mut observation = Observation::bare(frame.frame_id, state, metrics);
        if let (Some(m), Some(font)) = (&menu, &self.font) {
            let cursor = detect::menu::cursor_region(image, m);
            observation.menu_lines = font.read(image, m.window, &[cursor]);
        }
        observation.dialogue = dialogue;
        observation.menu = menu;
        let in_battle = battle.is_some();
        observation.battle = battle;
        if !in_battle {
            observation.player = self.locate(image, &observation);
        }
        observation
    }

    fn set_pose_hint(&mut self, pose: PlayerPose) {
        self.hint = Some(pose);
    }
}

impl FireRedPerception {
    /// Perception that also locates the player on the world model.
    pub fn with_world(world: Arc<World>) -> Self {
        Self {
            world: Some(world),
            ..Self::default()
        }
    }

    /// Reads dialogue text with the game font.
    pub fn with_font(mut self, font: Arc<text::Font>) -> Self {
        self.font = Some(font);
        self
    }

    /// Reads small-font text (battle move names).
    pub fn with_small_font(mut self, font: Arc<text::Font>) -> Self {
        self.small_font = Some(font);
        self
    }

    /// Checks the opponent's sprite against these palettes (keyed by the
    /// species name the HUD prints).
    pub fn with_palettes(mut self, palettes: Arc<shiny::SpritePalettes>) -> Self {
        self.palettes = Some(palettes);
        self
    }

    /// The dialogue's text, re-read only when its text cells changed.
    fn read_text(&mut self, image: &RgbImage, d: &DialogueObservation) -> Vec<String> {
        let Some(font) = &self.font else {
            return Vec::new();
        };
        if let Some((cells, lines)) = &self.last_read {
            if *cells == d.text_cells {
                return lines.clone();
            }
        }
        let exclude: Vec<Region> = d.arrow.iter().map(|a| a.inflate(2)).collect();
        let lines = font.read(image, d.region, &exclude);
        self.last_read = Some((d.text_cells.clone(), lines.clone()));
        lines
    }

    /// Allows whole-world searches when the player's map is unknown.
    pub fn with_global_search(mut self, enabled: bool) -> Self {
        self.global_search = enabled;
        self
    }

    fn locate(
        &mut self,
        image: &RgbImage,
        observation: &Observation,
    ) -> Option<pokebot_state::PoseObservation> {
        let world = self.world.clone()?;
        let mut exclude = vec![PLAYER_SPRITE];
        if observation.dialogue.is_some() {
            exclude.push(Region::new(0, 112, 240, 48));
        }
        if let Some(menu) = &observation.menu {
            exclude.push(menu.window.inflate(8));
        }
        let localizer = Localizer::new(&world);
        let found = match &self.hint {
            Some(hint) => localizer.locate_from(image, hint, &exclude),
            None if !self.global_search => return None,
            None => {
                self.frames_since_global_search += 1;
                if self.frames_since_global_search < GLOBAL_SEARCH_INTERVAL {
                    return None;
                }
                self.frames_since_global_search = 0;
                localizer.locate_anywhere(image, &exclude)
            }
        };
        if let Some(found) = &found {
            self.hint = Some(found.pose.clone());
        }
        found
    }
    fn metrics(&mut self, image: &RgbImage) -> FrameMetrics {
        let bytes = image.as_bytes();
        let total = (bytes.len() / 3).max(1) as u64;
        let mean_luma = (bytes
            .chunks_exact(3)
            .map(|p| u64::from(color::luma([p[0], p[1], p[2]])))
            .sum::<u64>()
            / total) as u8;
        let changed_pixels = self.previous.as_ref().map_or(0, |previous| {
            previous
                .as_bytes()
                .chunks_exact(3)
                .zip(bytes.chunks_exact(3))
                .filter(|(a, b)| a != b)
                .count() as u32
        });
        self.previous = Some(image.clone());
        FrameMetrics {
            mean_luma,
            changed_pixels,
        }
    }
}

/// The opponent's shiny reading: only on the command menu (FIGHT/BAG/…),
/// where the front sprite is fully drawn (text frames such as "Gotcha!"
/// can show the HUD over an empty platform), and only when the HUD name
/// resolves to one palette entry.
fn opponent_shiny(
    image: &RgbImage,
    battle: &pokebot_state::BattleObservation,
    palettes: &shiny::SpritePalettes,
) -> Option<pokebot_state::ShinyReading> {
    if !matches!(battle.menu, Some(BattleMenu::Command { .. })) {
        return None;
    }
    battle.opponent_hp?;
    let name = battle.opponent_name.as_deref()?;
    let key = detect::hud::resolve(name, palettes.keys().map(String::as_str))?;
    let (normal, shiny_palette) = &palettes[key];
    Some(shiny::classify(
        image,
        detect::battle::OPPONENT_SPRITE,
        normal,
        shiny_palette,
    ))
}

/// "a/b" → (a, b).
fn read_fraction(text: &str) -> Option<(u8, u8)> {
    let (a, b) = text.trim().split_once('/')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn is_uniform(image: &RgbImage) -> bool {
    let lumas: Vec<u8> = image
        .as_bytes()
        .chunks_exact(3)
        .map(|p| color::luma([p[0], p[1], p[2]]))
        .collect();
    let total = lumas.len() as u32;
    let dark = lumas.iter().filter(|l| **l <= DARK_LUMA).count() as u32;
    let bright = lumas.iter().filter(|l| **l >= BRIGHT_LUMA).count() as u32;
    let uniform = |count: u32| count * 1000 >= total * UNIFORM_PER_MILLE;
    uniform(dark) || uniform(bright)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn frame(id: u64, image: RgbImage) -> NormalizedFrame {
        NormalizedFrame::new(id, Instant::now(), image).unwrap()
    }

    #[test]
    fn stable_text_is_counted_in_frames_not_observations() {
        // A capture card delivers every other frame to a busy bot: the
        // count must follow frame ids so settle thresholds mean the same.
        let mut image = RgbImage::filled(240, 160, [255, 255, 255]);
        detect::testing::draw_message_box(&mut image);
        let mut p = FireRedPerception::default();
        assert_eq!(
            p.observe(&frame(10, image.clone()))
                .dialogue
                .unwrap()
                .stable_frames,
            0
        );
        assert_eq!(
            p.observe(&frame(12, image.clone()))
                .dialogue
                .unwrap()
                .stable_frames,
            2
        );
        assert_eq!(
            p.observe(&frame(20, image)).dialogue.unwrap().stable_frames,
            10
        );
    }

    #[test]
    fn black_and_white_frames_are_transitions() {
        let mut perception = FireRedPerception::default();
        let black = perception.observe(&frame(0, RgbImage::filled(240, 160, [0, 0, 0])));
        assert_eq!(black.screen.value, ScreenState::Transition);
        assert_eq!(black.metrics.mean_luma, 0);
        let white = perception.observe(&frame(1, RgbImage::filled(240, 160, [255, 255, 255])));
        assert_eq!(white.screen.value, ScreenState::Transition);
        assert_eq!(white.metrics.changed_pixels, 240 * 160);
    }

    #[test]
    fn synthetic_message_box_with_arrow_is_dialogue_waiting() {
        let mut image = RgbImage::filled(240, 160, [66, 138, 132]);
        detect::testing::draw_message_box(&mut image);
        detect::testing::draw_arrow(&mut image, 120, 139);
        let observation = FireRedPerception::default().observe(&frame(0, image));
        assert_eq!(observation.screen.value, ScreenState::Dialogue);
        let dialogue = observation.dialogue.unwrap();
        assert!(dialogue.waiting_for_input);
        assert_eq!(dialogue.arrow.unwrap().x, 120);
    }

    #[test]
    fn help_system_pages_are_flagged() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = text::Font::load(root.join("data/world/font_normal.json")) else {
            return;
        };
        let font = std::sync::Arc::new(font);
        // Switch captures (colour-corrected); the HELP System pops up in
        // Oak's introduction there.
        for (fixture, help) in [
            ("switch-help-greeting", true),
            ("switch-help-menu", true),
            ("switch-help-page", true),
            ("switch-controls", false),
        ] {
            let Ok(image) =
                pokebot_video::png::load(root.join(format!("captures/fixtures/{fixture}.png")))
            else {
                return;
            };
            let mut p = FireRedPerception::default().with_font(std::sync::Arc::clone(&font));
            let d = p.observe(&frame(0, image)).dialogue.unwrap();
            assert_eq!(d.help, help, "{fixture}");
        }
    }

    #[test]
    fn start_menu_rows_are_read() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = text::Font::load(root.join("data/world/font_normal.json")) else {
            return;
        };
        let font = std::sync::Arc::new(font);
        // Emulator, Pewter mart: the ▶ on POKéDEX, then on BAG (15 px pitch).
        for (fixture, cursor_y) in [("start-menu", 10), ("start-menu-bag", 40)] {
            let Ok(image) =
                pokebot_video::png::load(root.join(format!("captures/fixtures/{fixture}.png")))
            else {
                return;
            };
            let mut p = FireRedPerception::default().with_font(std::sync::Arc::clone(&font));
            let o = p.observe(&frame(0, image));
            assert_eq!(o.screen.value, ScreenState::Menu, "{fixture}");
            assert!(
                o.dialogue.is_none(),
                "{fixture}: the help line is not dialogue"
            );
            let menu = o.menu.unwrap();
            assert_eq!((menu.window.y, menu.cursor_y), (6, cursor_y), "{fixture}");
            assert_eq!(
                o.menu_lines,
                ["POKéDEX", "POKéMON", "BAG", "RED", "SAVE", "OPTION", "EXIT"],
                "{fixture}"
            );
        }
        // Without a menu there is nothing to read.
        let Ok(image) = pokebot_video::png::load(root.join("captures/fixtures/bag-items.png"))
        else {
            return;
        };
        let mut p = FireRedPerception::default().with_font(font);
        assert!(p.observe(&frame(0, image)).menu_lines.is_empty());
    }

    #[test]
    fn jpeg_softened_arrow_is_found() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        // Switch capture: the arrow's edge pixels blend towards the shadow.
        let Ok(image) =
            pokebot_video::png::load(root.join("captures/fixtures/switch-oak-arrow.png"))
        else {
            return;
        };
        let d = FireRedPerception::default()
            .observe(&frame(0, image))
            .dialogue
            .unwrap();
        assert!(d.waiting_for_input);
    }

    #[test]
    fn switch_move_menu_cursor_is_found() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        // Two known moves; one cursor pixel is off by JPEG noise.
        let Ok(image) =
            pokebot_video::png::load(root.join("captures/fixtures/switch-move-select.png"))
        else {
            return;
        };
        let o = FireRedPerception::default().observe(&frame(0, image));
        assert_eq!(o.screen.value, ScreenState::BattleMoveSelection);
        assert_eq!(
            o.battle.unwrap().menu,
            Some(BattleMenu::Moves { column: 0, row: 0 })
        );
    }

    #[test]
    fn sign_box_is_a_message_box() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        // Walking into a sign: grey-framed box ("OAK POKéMON RESEARCH LAB").
        let Ok(image) = pokebot_video::png::load(root.join("captures/fixtures/switch-sign.png"))
        else {
            return;
        };
        let o = FireRedPerception::default().observe(&frame(0, image));
        assert_eq!(o.screen.value, ScreenState::Dialogue);
        assert_eq!(o.dialogue.unwrap().kind, DialogueKind::MessageBox);
    }

    #[test]
    fn switch_battle_hud_is_read_despite_jpeg_noise() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(small), Ok(normal)) = (
            text::Font::load(root.join("data/world/font_small.json")),
            text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let Ok(image) =
            pokebot_video::png::load(root.join("captures/fixtures/switch-battle-hud.png"))
        else {
            return;
        };
        let mut p = FireRedPerception::default()
            .with_font(std::sync::Arc::new(normal))
            .with_small_font(std::sync::Arc::new(small));
        let b = p.observe(&frame(0, image)).battle.unwrap();
        assert_eq!(b.player_name.as_deref(), Some("BULBASAUR"));
        assert_eq!(b.player_level, Some(7));
        assert_eq!(b.player_hp_numbers, Some((8, 23)));
        assert_eq!(b.opponent_name.as_deref(), Some("MANKEY"));
        assert_eq!(b.opponent_level, Some(4));
        // O (read as the digit 0) and W.
        let Ok(image) =
            pokebot_video::png::load(root.join("captures/fixtures/switch-battle-spearow.png"))
        else {
            return;
        };
        let b = p.observe(&frame(1, image)).battle.unwrap();
        assert_eq!(b.opponent_name.as_deref(), Some("SPEAROW"));
    }

    #[test]
    fn switch_message_text_is_read_despite_jpeg_noise() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = text::Font::load(root.join("data/world/font_normal.json")) else {
            return;
        };
        let Ok(image) =
            pokebot_video::png::load(root.join("captures/fixtures/switch-nurse-text.png"))
        else {
            return;
        };
        let mut p = FireRedPerception::default().with_font(std::sync::Arc::new(font));
        let d = p.observe(&frame(0, image)).dialogue.unwrap();
        assert_eq!(
            d.lines,
            vec!["Okay, I’ll take your POKéMON for a", "few seconds."]
        );
    }

    /// Normal/small fonts and the palettes of the fixtures' wild species
    /// (RATTATA in the emulator's, PIDGEY in the Switch's) from the game data.
    fn catch_perception() -> Option<FireRedPerception> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let normal = text::Font::load(root.join("data/world/font_normal.json")).ok()?;
        let small = text::Font::load(root.join("data/world/font_small.json")).ok()?;
        let data = pokebot_gamedata::GameData::load(root.join("data/world/gamedata.json")).ok()?;
        let mut palettes = shiny::SpritePalettes::new();
        for name in ["RATTATA", "PIDGEY"] {
            let p = data
                .species
                .get(&format!("SPECIES_{name}"))?
                .palettes
                .clone()?;
            palettes.insert(
                name.into(),
                (p.normal.try_into().ok()?, p.shiny.try_into().ok()?),
            );
        }
        Some(
            FireRedPerception::default()
                .with_font(std::sync::Arc::new(normal))
                .with_small_font(std::sync::Arc::new(small))
                .with_palettes(std::sync::Arc::new(palettes)),
        )
    }

    fn fixture(name: &str) -> Option<RgbImage> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        pokebot_video::png::load(root.join("captures/fixtures").join(name)).ok()
    }

    /// Wild battles on the command menu: the opponent, whether the caught
    /// icon shows, and a normal-palette reading. `dir` is the fixture
    /// source ("" = emulator, "switch/" = the physical Switch).
    fn wild_battles(dir: &str, species: &str) {
        let Some(mut p) = catch_perception() else {
            return;
        };
        for (name, caught) in [
            ("battle-wild-uncaught.png", false),
            ("battle-wild-caught.png", true),
        ] {
            let Some(image) = fixture(&format!("{dir}{name}")) else {
                continue;
            };
            let b = p.observe(&frame(0, image)).battle.expect(name);
            assert_eq!(b.opponent_name.as_deref(), Some(species), "{dir}{name}");
            assert_eq!(b.opponent_caught, Some(caught), "{dir}{name}");
            assert_eq!(
                b.opponent_shiny,
                Some(pokebot_state::ShinyReading::Normal),
                "{dir}{name}"
            );
        }
    }

    #[test]
    fn caught_icon_and_shiny_reading_on_wild_battles() {
        wild_battles("", "RATTATA");
    }

    #[test]
    fn switch_caught_icon_and_shiny_reading_on_wild_battles() {
        wild_battles("switch/", "PIDGEY");
    }

    /// "Gotcha!": the HUD and HP bar still show, the platform is empty.
    fn shiny_only_on_command_menu(dir: &str, species: &str) {
        let Some(mut p) = catch_perception() else {
            return;
        };
        let Some(image) = fixture(&format!("{dir}battle-gotcha.png")) else {
            return;
        };
        let b = p.observe(&frame(0, image)).battle.unwrap();
        assert_eq!(b.opponent_name.as_deref(), Some(species), "{dir}");
        assert_eq!(b.opponent_shiny, None, "{dir}");
    }

    #[test]
    fn shiny_is_read_only_on_the_command_menu() {
        shiny_only_on_command_menu("", "RATTATA");
    }

    #[test]
    fn switch_shiny_is_read_only_on_the_command_menu() {
        shiny_only_on_command_menu("switch/", "PIDGEY");
    }

    /// The catch flow's battle texts, as the battle text box reads them.
    fn catch_texts(dir: &str, species: &str) {
        let Some(mut p) = catch_perception() else {
            return;
        };
        let gotcha = format!("{species} was caught!");
        for (name, lines) in [
            ("battle-throw.png", ["RED used", "POKé BALL!"]),
            (
                "battle-broke-free.png",
                ["Oh, no!", "The POKéMON broke free!"],
            ),
            (
                "battle-broke-free-aww.png",
                ["Aww!", "It appeared to be caught!"],
            ),
            (
                "battle-broke-free-shoot.png",
                ["Shoot!", "It was so close, too!"],
            ),
            ("battle-broke-free-aargh.png", ["Aargh!", "Almost had it!"]),
            ("battle-gotcha.png", ["Gotcha!", gotcha.as_str()]),
        ] {
            let Some(image) = fixture(&format!("{dir}{name}")) else {
                continue;
            };
            let d = p.observe(&frame(0, image)).dialogue.expect(name);
            assert_eq!(d.lines, lines, "{dir}{name}");
        }
    }

    #[test]
    fn catch_texts_are_read() {
        catch_texts("", "RATTATA");
    }

    #[test]
    fn switch_catch_texts_are_read() {
        catch_texts("switch/", "PIDGEY");
    }

    #[test]
    fn no_shiny_reading_without_palettes() {
        let Some(image) = fixture("battle-wild-uncaught.png") else {
            return;
        };
        let b = FireRedPerception::default()
            .observe(&frame(0, image))
            .battle
            .unwrap();
        assert_eq!(b.opponent_shiny, None);
    }

    #[test]
    fn pokedex_page_is_flagged() {
        let Some(mut p) = catch_perception() else {
            return;
        };
        for dir in ["", "switch/"] {
            for (name, page) in [
                ("pokedex-page.png", true),
                ("battle-gotcha.png", false),
                ("mart-list.png", false),
            ] {
                let Some(image) = fixture(&format!("{dir}{name}")) else {
                    continue;
                };
                assert_eq!(
                    p.observe(&frame(0, image)).pokedex_page,
                    page,
                    "{dir}{name}"
                );
            }
        }
    }

    #[test]
    fn move_menu_names_are_read_with_the_small_font() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(small), Ok(normal)) = (
            text::Font::load(root.join("data/world/font_small.json")),
            text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let Ok(image) = pokebot_video::png::load(root.join("captures/fixtures/move-select.png"))
        else {
            return;
        };
        let mut p = FireRedPerception::default()
            .with_font(std::sync::Arc::new(normal))
            .with_small_font(std::sync::Arc::new(small));
        let b = p.observe(&frame(0, image)).battle.unwrap();
        assert_eq!(
            b.move_names,
            vec!["TACKLE", "GROWL", "LEECH SEED", "VINE WHIP"]
        );
        assert_eq!(b.move_pp, Some((35, 35)));
    }
}
