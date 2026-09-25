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
use pokebot_world::{localize::PLAYER_SPRITE, sprites::SpriteDetector, Localizer, World};

pub trait PerceptionSystem {
    fn observe(&mut self, frame: &NormalizedFrame) -> Observation;

    /// Where the player is believed to be (from story knowledge); narrows the
    /// localization search. Wrong hints only cost search time.
    fn set_pose_hint(&mut self, _pose: PlayerPose) {}

    /// Forgets the hint so the next frame is located from scratch (the
    /// goal loop's relocalisation: `locate_anywhere`).
    fn clear_pose_hint(&mut self) {}

    /// A hint worked out rather than seen (one of several lookalike maps):
    /// poses tracked from it are reported as inferred until the player
    /// reaches a map the frame alone names.
    fn set_pose_hint_inferred(&mut self, pose: PlayerPose) {
        self.set_pose_hint(pose);
    }
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
    /// The map of an inferred hint: poses tracked on it are inferred too.
    inferred: Option<String>,
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
    /// Brightest-surface level of the last dim frame that turned out not
    /// to be a fade (see `fade`).
    fade_miss: Option<u8>,
    /// Object sprites around the located player.
    sprites: SpriteDetector,
}

/// Frames between whole-world searches while the player can't be located
/// (each takes ~10 ms of all cores; see `Localizer::locate_anywhere`).
const GLOBAL_SEARCH_INTERVAL: u32 = 30;

/// A frame counts as a transition when this share of pixels (per mille) is
/// near-black or near-white.
const UNIFORM_PER_MILLE: u32 = 995;
/// Near-black: also the last steps of a fade, which are dark grey rather
/// than black (the zoom-in out of Mt. Moon's first-entry intro ends in
/// concentric greys of luma 0–40). Such a frame matched the black void of
/// MtMoon_B1F at score 1000; it must never be localized.
const DARK_LUMA: u8 = 48;
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
        if let Some((state, detector)) = scene(image) {
            // A fade: name the screen under it when brightening shows one.
            if detector == "fade" {
                if let Some(fade) = self.fade(frame.frame_id, image, metrics) {
                    return fade;
                }
            }
            return Observation::bare(frame.frame_id, screen(state, detector), metrics);
        }
        if detect::whiteout::is_whiteout(image) {
            return Observation::bare(
                frame.frame_id,
                screen(ScreenState::Whiteout, "whiteout-text"),
                metrics,
            );
        }
        if detect::cut_in::detect(image) {
            return Observation::bare(
                frame.frame_id,
                screen(ScreenState::Transition, "field-move-cut-in"),
                metrics,
            );
        }
        if let Some(observation) = self.full_screen(frame.frame_id, image, metrics) {
            return observation;
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
            (Some(d), _) if d.kind == DialogueKind::BattleText && is_evolution(d, &battle) => {
                screen(ScreenState::Evolution, "evolution-text")
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
            let popup = detect::map_popup::detect(image);
            if let (Some(popup), Some(font)) = (&popup, &self.font) {
                observation.map_popup = detect::map_popup::read(image, popup, font);
            }
            let covered = popup.map(|p| p.covers);
            self.locate(image, &mut observation, covered);
            self.see_sprites(frame.frame_id, image, &mut observation, covered);
        }
        if observation.screen.value == ScreenState::Unknown
            && observation.player.is_none()
            && !in_battle
        {
            if let Some(fade) = self.fade(frame.frame_id, image, metrics) {
                return fade;
            }
        }
        observation
    }

    fn set_pose_hint(&mut self, pose: PlayerPose) {
        self.hint = Some(pose);
        self.inferred = None;
    }

    fn set_pose_hint_inferred(&mut self, pose: PlayerPose) {
        self.inferred = Some(pose.map.clone());
        self.hint = Some(pose);
    }

    /// Drops the hint and searches every map on the next frame (global
    /// search is switched on: without it nothing could be located again).
    fn clear_pose_hint(&mut self) {
        self.hint = None;
        self.inferred = None;
        self.global_search = true;
        self.frames_since_global_search = GLOBAL_SEARCH_INTERVAL;
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

    /// Whole-screen UIs (naming keyboard, title, menus, party screens,
    /// bag, mart, …) recognised by their own layout.
    fn full_screen(
        &self,
        frame_id: u64,
        image: &RgbImage,
        metrics: FrameMetrics,
    ) -> Option<Observation> {
        let screen = |value, detector: &str| Observed {
            value,
            detector: detector.to_owned(),
        };
        if let Some(naming) = detect::naming::detect(image) {
            let mut observation = Observation::bare(
                frame_id,
                screen(ScreenState::Naming, "naming-keyboard"),
                metrics,
            );
            observation.naming = Some(naming);
            return Some(observation);
        }
        if detect::title::is_title(image) {
            return Some(Observation::bare(
                frame_id,
                screen(ScreenState::TitleScreen, "title-bands"),
                metrics,
            ));
        }
        if let Some(menu) = detect::main_menu::detect(image) {
            let mut observation = Observation::bare(
                frame_id,
                screen(ScreenState::MainMenu, "main-menu"),
                metrics,
            );
            observation.menu = Some(menu);
            return Some(observation);
        }
        // The KNOWN MOVES list a move is forgotten on is the summary's
        // moves page with a red selection frame: it must win over the
        // summary reading of the same page (its title reads the same).
        if let Some(list) = detect::move_list::detect(image, self.font.as_deref()) {
            let mut observation = Observation::bare(
                frame_id,
                screen(ScreenState::LearnMove, "known-moves"),
                metrics,
            );
            observation.move_list = Some(list);
            return Some(observation);
        }
        if let Some(font) = self.font.as_deref() {
            let summary = detect::party::summary(image, font);
            let mut party_menu = if summary.is_none() {
                detect::party::menu(image, font)
            } else {
                None
            };
            if let (Some(menu), Some(small)) = (&mut party_menu, &self.small_font) {
                menu.members = detect::party::members(image, small, menu);
            }
            if summary.is_some() || party_menu.is_some() {
                let mut observation = Observation::bare(
                    frame_id,
                    screen(ScreenState::PartyMenu, "party-summary"),
                    metrics,
                );
                observation.summary = summary;
                observation.party_menu = party_menu;
                return Some(observation);
            }
        }
        if detect::pokedex::is_page(image) {
            let mut observation = Observation::bare(
                frame_id,
                screen(ScreenState::Unknown, "pokedex-page"),
                metrics,
            );
            observation.pokedex_page = true;
            return Some(observation);
        }
        if let Some(card) = detect::trainer_card::detect(image, self.font.as_deref()) {
            let mut observation = Observation::bare(
                frame_id,
                screen(ScreenState::Unknown, "trainer-card"),
                metrics,
            );
            observation.trainer_card = Some(card);
            return Some(observation);
        }
        if let Some(map) = detect::fly_map::detect(image) {
            let mut observation = Observation::bare(
                frame_id,
                screen(ScreenState::Unknown, "region-map"),
                metrics,
            );
            observation.fly_map = Some(map);
            return Some(observation);
        }
        if let Some(list) = self
            .font
            .as_deref()
            .and_then(|font| detect::pokedex::list(image, font))
        {
            let mut observation = Observation::bare(
                frame_id,
                screen(ScreenState::Unknown, "pokedex-list"),
                metrics,
            );
            observation.pokedex_list = Some(list);
            return Some(observation);
        }
        if let (Some(font), Some(small_font)) = (&self.font, &self.small_font) {
            if let Some(bag) = detect::bag::detect(image, font, small_font) {
                let state = if bag.prompt.is_some() {
                    ScreenState::BattleBag
                } else {
                    ScreenState::Bag
                };
                let mut observation =
                    Observation::bare(frame_id, screen(state, "bag-screen"), metrics);
                observation.bag = Some(bag);
                return Some(observation);
            }
            if let Some(shop) = detect::shop::detect(image, font, small_font) {
                let mut observation =
                    Observation::bare(frame_id, screen(ScreenState::Shop, "shop-screen"), metrics);
                observation.shop = Some(shop);
                return Some(observation);
            }
        }
        None
    }

    /// A frame darkened by a palette fade whose screen, brightened back,
    /// is recognised: a `Transition` named after the screen under it
    /// (`fade:menu-cursor`, `fade:party-summary`, …). The game takes no
    /// input until the fade ends, and the sensor ignores transitions, so
    /// nothing is read from it: text and numbers on a frame scaled up by
    /// up to 16/5 carry that much more capture noise.
    ///
    /// Each candidate factor costs a pass of the screen detectors, so a dim
    /// scene that is no fade (a cave the player can't be located in) is
    /// searched once: its brightest level holds from frame to frame, while
    /// a fade changes it at every step.
    fn fade(
        &mut self,
        frame_id: u64,
        image: &RgbImage,
        metrics: FrameMetrics,
    ) -> Option<Observation> {
        let level = detect::fade::surface_level(image);
        if self.fade_miss.is_some_and(|miss| miss.abs_diff(level) <= 1) {
            return None;
        }
        let under = detect::fade::candidates(level)
            .into_iter()
            .find_map(|sixteenths| {
                self.screen_detector(
                    frame_id,
                    &detect::fade::brighten(image, sixteenths),
                    metrics,
                )
            });
        self.fade_miss = under.is_none().then_some(level);
        let under = under?;
        Some(Observation::bare(
            frame_id,
            Observed {
                value: ScreenState::Transition,
                detector: format!("fade:{under}"),
            },
            metrics,
        ))
    }

    /// The detector that recognises `image` as a screen or a window
    /// (without the stateful parts of `observe`: text settling, locating).
    fn screen_detector(
        &self,
        frame_id: u64,
        image: &RgbImage,
        metrics: FrameMetrics,
    ) -> Option<String> {
        if let Some(o) = self.full_screen(frame_id, image, metrics) {
            return Some(o.screen.detector);
        }
        if let Some(d) = detect::dialogue::detect(image) {
            return Some(
                match d.kind {
                    DialogueKind::BattleText => "battle-text-box",
                    DialogueKind::InfoPage => "info-page",
                    DialogueKind::MessageBox => "message-box",
                }
                .into(),
            );
        }
        // A window, not a stray ▶-shaped speck of a dark scene.
        detect::menu::detect(image)
            .filter(|m| m.window.width >= 32 && m.window.height >= 16)
            .map(|_| "menu-cursor".into())
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

    /// Allows whole-world searches when the player's map is unknown. The
    /// first frame that could show the map is searched at once (not 30
    /// frames in: a run continued in the Pewter Gym stood unlocated).
    pub fn with_global_search(mut self, enabled: bool) -> Self {
        self.global_search = enabled;
        self.frames_since_global_search = GLOBAL_SEARCH_INTERVAL - 1;
        self
    }

    /// The player's pose, matching the frame outside the player sprite and
    /// any window drawn over the map (`popup`: the map-name popup's area).
    /// Sets `observation.player`, or `pose_candidates` when the frame
    /// matches several maps equally (maps sharing a layout, every Pokémon
    /// Center): perception never picks one of those; the sensor does, from
    /// the state, or the scheduler confirms the location.
    fn locate(&mut self, image: &RgbImage, observation: &mut Observation, popup: Option<Region>) {
        let Some(world) = self.world.clone() else {
            return;
        };
        let mut exclude = vec![PLAYER_SPRITE];
        if observation.dialogue.is_some() {
            exclude.push(Region::new(0, 112, 240, 48));
        }
        if let Some(menu) = &observation.menu {
            exclude.push(menu.window.inflate(8));
        }
        if let Some(popup) = popup {
            exclude.push(popup);
        }
        let localizer = Localizer::new(&world);
        let tracked = self
            .hint
            .as_ref()
            .and_then(|hint| localizer.locate_from(image, hint, &exclude));
        // A hint that stopped matching is searched past: the tracker only
        // looks at the hint's map and its neighbours, so a wrong hint (a
        // warp to somewhere unconnected, a reset) otherwise kept the player
        // unlocated for good (Switch audit: the Pewter Gym read score 994
        // but was never searched).
        let found = match tracked {
            Some(found) => {
                self.frames_since_global_search = 0;
                match &self.inferred {
                    // Still on the inferred pose's map: as unconfirmed as it.
                    Some(map) if *map == found.pose.map => {
                        observation.pose_inferred = true;
                        Some(found)
                    }
                    // Tracked off it through a warp: the new map counts once
                    // no lookalike matches as well (the stairs of one Center
                    // lead to a 2F like every other).
                    Some(_) => {
                        let mut all = localizer.locate_anywhere_candidates(image, &exclude);
                        if all.len() == 1 {
                            self.inferred = None;
                            Some(all.remove(0))
                        } else {
                            self.inferred = Some(found.pose.map.clone());
                            observation.pose_inferred = true;
                            Some(found)
                        }
                    }
                    None => Some(found),
                }
            }
            None if !self.global_search => None,
            None => {
                self.frames_since_global_search += 1;
                if self.frames_since_global_search < GLOBAL_SEARCH_INTERVAL {
                    return;
                }
                self.frames_since_global_search = 0;
                let mut all = localizer.locate_anywhere_candidates(image, &exclude);
                if all.len() == 1 {
                    self.inferred = None;
                    Some(all.remove(0))
                } else {
                    observation.pose_candidates = all;
                    None
                }
            }
        };
        if let Some(found) = &found {
            self.hint = Some(found.pose.clone());
        }
        observation.player = found;
    }

    /// The sprites around the located player, and the objects seen not to
    /// be there. The UI windows `locate` leaves out hide the field.
    fn see_sprites(
        &mut self,
        frame_id: u64,
        image: &RgbImage,
        observation: &mut Observation,
        popup: Option<Region>,
    ) {
        let (Some(world), Some(player)) = (&self.world, &observation.player) else {
            return;
        };
        let Some(map) = world.map(&player.pose.map) else {
            return;
        };
        let mut occluded: Vec<Region> = popup.into_iter().collect();
        if observation.dialogue.is_some() {
            occluded.push(Region::new(0, 112, 240, 48));
        }
        if let Some(menu) = &observation.menu {
            occluded.push(menu.window.inflate(8));
        }
        let scan = self
            .sprites
            .scan(frame_id, image, world, map, &player.pose, &occluded);
        observation.sprites = scan.sprites;
        observation.objects_absent = scan.absent;
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

/// The evolution scene: the battle text box without either HUD, saying
/// "What? BULBASAUR is evolving!" (the text stays up through the whole
/// animation: switch-goal-14 frames 64600–65670), "Congratulations! Your
/// BULBASAUR evolved into IVYSAUR!" or "Huh? … stopped evolving!". No
/// battle message says these, and battle text without a HUD is only a
/// trainer's challenge or the last page before the fade.
fn is_evolution(
    d: &DialogueObservation,
    battle: &Option<pokebot_state::BattleObservation>,
) -> bool {
    let hud = battle
        .as_ref()
        .is_some_and(|b| b.player_hp.is_some() || b.opponent_hp.is_some());
    let text = d.lines.join(" ");
    !hud && [
        "is evolving",
        "evolved into",
        "stopped evolving",
        "Congratulations",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// Brightness (1/256ths) below which Oak's stage is still fading in or
/// out (live-story-1544, ×0.86, is fading; frames as drawn measure ≥ 250).
const OAK_STAGE_LIT: u32 = 240;

/// Full-screen scenes that show no state: the boot intro, the Quest Log,
/// fades, battle wipes and the map preview. Checked before every UI
/// detector: none of them may be read or localized.
fn scene(image: &RgbImage) -> Option<(ScreenState, &'static str)> {
    use detect::{intro, transition};
    if intro::is_copyright(image) {
        return Some((ScreenState::Intro, "intro-copyright"));
    }
    if intro::is_game_freak(image) {
        return Some((ScreenState::Intro, "intro-game-freak"));
    }
    if detect::quest_log::detect(image) {
        return Some((ScreenState::QuestLog, "quest-log"));
    }
    if transition::is_faded(image) {
        return Some((ScreenState::Transition, "fade"));
    }
    if detect::battle::is_faded_text_box(image) {
        return Some((ScreenState::Transition, "battle-fade"));
    }
    if transition::is_battle_intro(image) {
        return Some((ScreenState::Transition, "battle-intro"));
    }
    if transition::is_clockwise_wipe(image) {
        return Some((ScreenState::Transition, "battle-wipe"));
    }
    if transition::is_pokeballs_trail(image) {
        return Some((ScreenState::Transition, "battle-wipe"));
    }
    if transition::is_map_preview(image) {
        return Some((ScreenState::Transition, "map-preview"));
    }
    match intro::oak_stage_brightness(image) {
        Some(k) if k < OAK_STAGE_LIT => Some((ScreenState::Transition, "fade")),
        Some(_) => Some((ScreenState::Intro, "intro-oak")),
        None => None,
    }
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

    /// Switch goal run: after the white-out on Route 1 these screens read
    /// `Unknown` for ~1200 frames until the stuck rule pressed B.
    #[test]
    fn the_white_out_screen_is_recognised() {
        let Some(image) = fixture("switch-whiteout-scurried-home.png") else {
            return;
        };
        let o = FireRedPerception::default().observe(&frame(0, image));
        assert_eq!(o.screen.value, ScreenState::Whiteout, "{:?}", o.screen);
        // A black frame with a lone bright spot is not one.
        let mut image = RgbImage::filled(240, 160, [0, 0, 0]);
        image.put_pixel(120, 80, [255, 255, 255]);
        let o = FireRedPerception::default().observe(&frame(1, image));
        assert_ne!(o.screen.value, ScreenState::Whiteout);
        // Flash-8: the Poké Ball wipe into a trainer battle (black with a
        // piece of the ball) ended the run as a white-out on Route 3.
        let Some(wipe) = fixture("emu-trainer-battle-wipe-black.png") else {
            return;
        };
        let o = FireRedPerception::default().observe(&frame(2, wipe));
        assert_ne!(o.screen.value, ScreenState::Whiteout, "{:?}", o.screen);
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

    /// Live: the last frame of the fade after Mt. Moon's first-entry
    /// intro (concentric greys, luma ≤ 40) was localized in MtMoon_B1F's
    /// black void, sending CrossMtMoon to a ladder it couldn't reach.
    #[test]
    fn a_dark_grey_fade_frame_is_a_transition() {
        let mut image = RgbImage::filled(240, 160, [8, 8, 8]);
        let greys = [
            [0, 0, 0],
            [16, 16, 16],
            [24, 24, 24],
            [33, 32, 33],
            [40, 40, 40],
        ];
        for (i, grey) in greys.iter().enumerate() {
            let inset = 12 * (i as u32 + 1);
            for y in inset..160 - inset {
                for x in inset..240 - inset {
                    image.put_pixel(x, y, *grey);
                }
            }
        }
        let observation = FireRedPerception::default().observe(&frame(0, image));
        // Transitions are never localized (observe returns before locate).
        assert_eq!(observation.screen.value, ScreenState::Transition);
    }

    /// Emulator, Oak's lab: located frames carry the sprites around the
    /// player, named.
    #[test]
    fn located_frames_carry_the_sprites() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let Ok(image) =
            pokebot_video::png::load(root.join("captures/fixtures/emu-sprites-oaks-lab.png"))
        else {
            return;
        };
        let mut p = FireRedPerception::with_world(Arc::new(world));
        p.set_pose_hint(PlayerPose {
            map: "PalletTown_ProfessorOaksLab".into(),
            x: 6,
            y: 4,
        });
        let o = p.observe(&frame(0, image.clone()));
        let named: Vec<u32> = o.sprites.iter().filter_map(|s| s.local_id).collect();
        assert_eq!(named, [9, 10, 4, 8, 5, 6, 7]);
        assert!(o.objects_absent.is_empty());
        // Unlocated frames (a battle, a fade) carry none.
        let o = p.observe(&frame(1, RgbImage::filled(240, 160, [0, 0, 0])));
        assert!(o.sprites.is_empty());
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

    /// Switch goal run: the frames perception missed around the menus were
    /// palette fades (the white window at 224/190/124/90 = 14, 12, 8 and 6
    /// sixteenths): the Start menu fading out after a row was chosen, the
    /// party menu, the bag's empty ITEMS pocket, the Trainer Card and the
    /// summary INFO page fading in. The emulator's card (rec2, 36 frames)
    /// sat at half brightness. They are transitions, named after the
    /// screen under the fade, and carry no readings.
    #[test]
    fn fading_menu_screens_are_transitions() {
        let Some(mut p) = catch_perception() else {
            return;
        };
        for (name, under) in [
            ("switch-fade-start-menu-pokemon.png", "menu-cursor"),
            ("switch-fade-start-menu-pokemon-14.png", "menu-cursor"),
            ("switch-fade-start-menu-red.png", "menu-cursor"),
            ("switch-fade-start-menu-bag.png", "menu-cursor"),
            ("switch-fade-party-menu.png", "party-summary"),
            ("switch-fade-bag-empty-items.png", "bag-screen"),
            ("switch-fade-trainer-card.png", "trainer-card"),
            ("switch-fade-summary-info.png", "party-summary"),
            ("emu-fade-trainer-card.png", "trainer-card"),
        ] {
            let Some(image) = fixture(name) else {
                continue;
            };
            let o = p.observe(&frame(0, image));
            assert_eq!(o.screen.value, ScreenState::Transition, "{name}");
            assert_eq!(o.screen.detector, format!("fade:{under}"), "{name}");
            assert!(o.menu.is_none() && o.party_menu.is_none() && o.bag.is_none());
        }
    }

    /// Every step of a fade of the emulator's Start menu down to 5/16 is
    /// the menu (the first steps are still within the colour tolerance)
    /// or the menu fading; the menu itself and a dark cave are not fades.
    #[test]
    fn a_start_menu_fade_is_recognised_at_every_step() {
        let Some(mut p) = catch_perception() else {
            return;
        };
        let Some(menu) = fixture("start-menu.png") else {
            return;
        };
        assert_eq!(
            p.observe(&frame(0, menu.clone())).screen.value,
            ScreenState::Menu
        );
        for sixteenths in 5..16 {
            let o = p.observe(&frame(1, detect::fade::darken(&menu, sixteenths)));
            let expected = if sixteenths >= 13 {
                "menu-cursor"
            } else {
                "fade:menu-cursor"
            };
            assert_eq!(o.screen.detector, expected, "{sixteenths}/16");
        }
        for name in [
            "mtmoon-1f.png",
            "mtmoon-1f-b.png",
            "emu-tools-overworld.png",
        ] {
            if let Some(image) = fixture(name) {
                let o = p.observe(&frame(2, image));
                assert_ne!(o.screen.value, ScreenState::Transition, "{name}");
            }
        }
        // A scene that was no fade doesn't hide the next real one.
        let o = p.observe(&frame(3, detect::fade::darken(&menu, 8)));
        assert_eq!(o.screen.detector, "fade:menu-cursor");
    }

    /// Teaching a TM/HM: the mauve-framed messages over the party menu and
    /// the TM scene are dialogue (YES/NO included), and the KNOWN MOVES
    /// list with its red frame is the move list, not the summary page.
    #[test]
    fn teach_screens_are_recognised() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = text::Font::load(root.join("data/world/font_normal.json")) else {
            return;
        };
        let font = std::sync::Arc::new(font);
        let observe = |name: &str| {
            let image =
                pokebot_video::png::load(root.join(format!("captures/fixtures/{name}.png")))
                    .ok()?;
            let mut p = FireRedPerception::default().with_font(std::sync::Arc::clone(&font));
            Some(p.observe(&frame(0, image)))
        };
        if let Some(o) = observe("emu-teach-four-moves") {
            let d = o.dialogue.expect("dialogue over the party menu");
            assert_eq!(d.lines, ["IVYSAUR wants to learn the", "move CUT."]);
            assert!(d.waiting_for_input);
            assert!(o.party_menu.is_none());
        }
        if let Some(o) = observe("emu-teach-replace-yes-no") {
            assert!(o.dialogue.is_some());
            assert_eq!(o.menu_lines, ["YES", "NO"]);
        }
        if let Some(o) = observe("emu-teach-learned") {
            assert_eq!(o.dialogue.unwrap().lines, ["IVYSAUR learned", "CUT!"]);
        }
        if let Some(o) = observe("emu-teach-known-moves") {
            let list = o.move_list.expect("move list");
            assert_eq!(
                list.moves,
                ["TACKLE", "SLEEP POWDER", "RAZOR LEAF", "VINE WHIP", "CUT"]
            );
            assert_eq!(list.selected, Some(0));
            assert!(o.summary.is_none());
        }
        if let Some(o) = observe("emu-summary-moves") {
            assert!(o.move_list.is_none());
            assert!(o.summary.is_some());
        }
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
        // V and F (captures/stuck on Mt. Moon read I?YSAUR and CLE?AIRY).
        for (i, (fixture, player, opponent)) in [
            ("switch-battle-ivysaur", "IVYSAUR", "GRIMER"),
            ("switch-battle-clefairy", "IVYSAUR", "CLEFAIRY"),
        ]
        .into_iter()
        .enumerate()
        {
            let Ok(image) =
                pokebot_video::png::load(root.join(format!("captures/fixtures/{fixture}.png")))
            else {
                return;
            };
            let b = p.observe(&frame(2 + i as u64, image)).battle.unwrap();
            assert_eq!(b.player_name.as_deref(), Some(player));
            assert_eq!(b.opponent_name.as_deref(), Some(opponent));
        }
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

    /// Live (Switch): the clerk's blue "Please come again!" read as nothing:
    /// the window's top-left corner, smeared into the text's blue, joined
    /// the line and pushed its glyph cells out of the searched range.
    #[test]
    fn switch_blue_text_under_a_smeared_corner_is_read() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(font) = text::Font::load(root.join("data/world/font_normal.json")) else {
            return;
        };
        let Ok(image) =
            pokebot_video::png::load(root.join("captures/fixtures/switch-mart-come-again.png"))
        else {
            return;
        };
        let mut p = FireRedPerception::default().with_font(std::sync::Arc::new(font));
        let d = p.observe(&frame(0, image)).dialogue.unwrap();
        assert_eq!(d.lines, vec!["Please come again!"]);
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

    /// The probe screens (Stream B2): trainer card, region map, Pokédex list.
    #[test]
    fn probe_screens_are_observed() {
        let Some(mut p) = catch_perception() else {
            return;
        };
        if let Some(image) = fixture("emu-trainer-card.png") {
            let o = p.observe(&frame(0, image));
            assert_eq!(o.screen.detector, "trainer-card");
            assert_eq!(o.trainer_card.unwrap().badges, vec![1]);
            assert!(o.pokedex_list.is_none() && o.fly_map.is_none());
        }
        if let Some(image) = fixture("synthetic-fly-map.png") {
            let o = p.observe(&frame(1, image));
            assert_eq!(o.screen.detector, "region-map");
            let map = o.fly_map.unwrap();
            assert_eq!(map.lit, vec!["PewterCity", "ViridianCity", "PalletTown"]);
            assert_eq!(map.lit.len() + map.dark.len(), 13);
        }
        if let Some(image) = fixture("emu-pokedex.png") {
            let o = p.observe(&frame(2, image));
            assert_eq!(o.screen.detector, "pokedex-list");
            assert!(!o.pokedex_page);
            let list = o.pokedex_list.unwrap();
            assert_eq!(list.rows[0], ("BULBASAUR".to_owned(), true));
            assert_eq!(list.cursor, Some(0));
        }
        if let Some(image) = fixture("emu-pokedex-contents.png") {
            let o = p.observe(&frame(3, image));
            assert!(o.pokedex_list.is_none() && o.trainer_card.is_none());
        }
    }

    fn world() -> Option<Arc<World>> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        World::load(root.join("data/world")).ok().map(Arc::new)
    }

    /// Switch audit: after the hint went stale (the player was elsewhere
    /// than the tracker's map and its neighbours) the Pewter Gym matched at
    /// 994 but was never searched; with global search on, a failing hint
    /// now falls back to it every `GLOBAL_SEARCH_INTERVAL` frames.
    #[test]
    fn a_stale_hint_falls_back_to_the_global_search() {
        let (Some(world), Some(image)) = (world(), fixture("switch-pewter-gym.png")) else {
            return;
        };
        let stale = PlayerPose {
            map: "Route3".into(),
            x: 5,
            y: 9,
        };
        let gym = |o: Option<pokebot_state::PoseObservation>| {
            o.is_some_and(|f| (f.pose.map.as_str(), f.pose.x, f.pose.y) == ("PewterCity_Gym", 6, 6))
        };
        let mut p = FireRedPerception::with_world(Arc::clone(&world)).with_global_search(true);
        // The first frame is searched at once.
        p.set_pose_hint(stale.clone());
        assert!(gym(p.observe(&frame(0, image.clone())).player));
        // From there on it is tracked frame by frame.
        assert!(gym(p.observe(&frame(1, image.clone())).player));
        // A stale hint again: the world is searched every interval.
        p.set_pose_hint(stale.clone());
        let located: Vec<_> = (0..u64::from(GLOBAL_SEARCH_INTERVAL))
            .map(|i| p.observe(&frame(2 + i, image.clone())).player)
            .collect();
        assert!(located[..located.len() - 1].iter().all(Option::is_none));
        assert!(gym(located.last().unwrap().clone()));
        // Without global search a stale hint stays unlocated.
        let mut p = FireRedPerception::with_world(world);
        p.set_pose_hint(stale);
        assert!((0..40).all(|i| p.observe(&frame(i, image.clone())).player.is_none()));
    }

    /// Entering Route 3 from Pewter (Switch): the popup is read, and its
    /// area is left out of the match. Tracked from Pewter's east edge the
    /// view is matched in Pewter's render, whose padding draws Route 3.
    #[test]
    fn the_map_popup_is_read_and_left_out_of_localization() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Some(world), Some(image), Ok(font)) = (
            world(),
            fixture("switch-map-popup-route3.png"),
            text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let mut p = FireRedPerception::with_world(world).with_font(Arc::new(font));
        p.set_pose_hint(PlayerPose {
            map: "PewterCity".into(),
            x: 47,
            y: 19,
        });
        let o = p.observe(&frame(0, image));
        assert_eq!(o.map_popup.as_deref(), Some("ROUTE 3"));
        let found = o.player.expect("located");
        assert_eq!(
            (found.pose.map.as_str(), found.pose.x, found.pose.y),
            ("Route3", 0, 9)
        );
        assert!(found.score >= 950, "{}", found.score);
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

    /// Perception with the normal font, or `None` without the built world.
    fn reading_perception() -> Option<FireRedPerception> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let font = text::Font::load(root.join("data/world/font_normal.json")).ok()?;
        Some(FireRedPerception::default().with_font(std::sync::Arc::new(font)))
    }

    /// Each fixture reads as `state` from `detector` (missing fixtures are
    /// skipped).
    fn assert_screens(cases: &[(&str, ScreenState, &str)]) {
        for (i, (name, state, detector)) in cases.iter().enumerate() {
            let Some(image) = fixture(name) else {
                continue;
            };
            let o = FireRedPerception::default().observe(&frame(i as u64, image));
            assert_eq!(
                (o.screen.value, o.screen.detector.as_str()),
                (*state, *detector),
                "{name}"
            );
            if matches!(state, ScreenState::Transition | ScreenState::Intro) {
                assert!(o.player.is_none() && o.dialogue.is_none(), "{name}");
            }
        }
    }

    /// Emulator (rec2), Mt. Moon: the battle's fade-out after its last
    /// page ("IVYSAUR gained 98 EXP. Points!", "IVYSAUR grew to LV. 18!")
    /// dims every colour by one amount, the box included, and read
    /// `Unknown`; the page was read before the fade began.
    #[test]
    fn a_fading_battle_text_box_is_a_transition() {
        assert_screens(&[
            (
                "emu-battle-fade-start.png",
                ScreenState::Transition,
                "battle-fade",
            ),
            (
                "emu-battle-fade-level-up.png",
                ScreenState::Transition,
                "battle-fade",
            ),
            // Dim enough that nothing is bright: the general fade rule.
            ("emu-battle-fade-exp.png", ScreenState::Transition, "fade"),
            (
                "switch-evolution-fade-in.png",
                ScreenState::Transition,
                "fade:battle-text-box",
            ),
        ]);
        // The box as drawn is still battle text.
        assert_screens(&[(
            "switch-level-up-text.png",
            ScreenState::BattleText,
            "battle-text-box",
        )]);
    }

    /// Switch goal run: with the level-up stats window over the text box's
    /// right side the page read as nothing ("MAX. HP 39 / ATTACK 18 …"
    /// outvoted the message's ink).
    #[test]
    fn the_level_up_page_reads_beside_the_stats_window() {
        let (Some(mut p), Some(image)) = (
            reading_perception(),
            fixture("switch-level-up-stats-panel.png"),
        ) else {
            return;
        };
        let o = p.observe(&frame(0, image));
        assert_eq!(o.screen.value, ScreenState::BattleText);
        let d = o.dialogue.unwrap();
        assert_eq!(d.lines, vec!["BULBASAUR grew to", "LV. 15!"]);
        assert!(d.waiting_for_input);
    }

    /// Battle intro windows (emulator, Mt. Moon; Switch, Route 2 grass)
    /// and the cave wipe read `Unknown` and could be localized in a
    /// cave's void; Mt. Moon's own rooms framed by void must still not
    /// be transitions.
    #[test]
    fn battle_wipes_are_transitions_and_cave_maps_are_not() {
        assert_screens(&[
            (
                "emu-battle-intro-band.png",
                ScreenState::Transition,
                "battle-intro",
            ),
            (
                "emu-battle-intro-band-box.png",
                ScreenState::Transition,
                "battle-intro",
            ),
            (
                "switch-battle-intro-band.png",
                ScreenState::Transition,
                "battle-intro",
            ),
            (
                "emu-clockwise-wipe-quarter.png",
                ScreenState::Transition,
                "battle-wipe",
            ),
            (
                "emu-clockwise-wipe-three-quarters.png",
                ScreenState::Transition,
                "battle-wipe",
            ),
            (
                "emu-clockwise-wipe-late.png",
                ScreenState::Transition,
                "battle-wipe",
            ),
            (
                "mtmoon-battle-wipe.png",
                ScreenState::Transition,
                "battle-wipe",
            ),
            (
                "mtmoon-battle-wipe-b.png",
                ScreenState::Transition,
                "battle-wipe",
            ),
            (
                "switch-pokeballs-trail.png",
                ScreenState::Transition,
                "battle-wipe",
            ),
            (
                "switch-pokeballs-trail-late.png",
                ScreenState::Transition,
                "battle-wipe",
            ),
            (
                "emu-map-preview-mtmoon.png",
                ScreenState::Transition,
                "map-preview",
            ),
            (
                "emu-map-preview-mtmoon-fade-in.png",
                ScreenState::Transition,
                "map-preview",
            ),
            (
                "mtmoon-entry-intro.png",
                ScreenState::Transition,
                "map-preview",
            ),
        ]);
        // The title screen's flash across Charizard's silhouette is not a
        // battle opening (whatever else it is).
        if let Some(image) = fixture("emu-title-flash.png") {
            let o = FireRedPerception::default().observe(&frame(0, image));
            assert_ne!(o.screen.detector, "battle-intro");
        }
        for name in [
            "mtmoon-1f.png",
            "mtmoon-1f-b.png",
            "emu-mtmoon-void-below.png",
            "emu-mtmoon-popup-void.png",
            "emu-mtmoon-popup-void-b.png",
            "emu-tools-overworld.png",
        ] {
            let Some(image) = fixture(name) else {
                continue;
            };
            let o = FireRedPerception::default().observe(&frame(0, image));
            assert_eq!(
                o.screen.value,
                ScreenState::Unknown,
                "{name}: {:?}",
                o.screen
            );
        }
    }

    /// Fades: the Pokémon Center dimming on the Switch, Viridian City
    /// under a fade with its message box, Oak's stage fading in (dark
    /// and nearly lit). They read `Unknown` and dim maps were localized.
    #[test]
    fn dimmed_frames_are_fades() {
        assert_screens(&[
            (
                "switch-fade-pokemon-center.png",
                ScreenState::Transition,
                "fade",
            ),
            (
                "switch-fade-viridian-message.png",
                ScreenState::Transition,
                "fade",
            ),
            ("emu-oak-stage-dim.png", ScreenState::Transition, "fade"),
            ("emu-oak-stage-fading.png", ScreenState::Transition, "fade"),
        ]);
    }

    /// The boot sequence and Oak's stage (new game from boot, emulator;
    /// copyright on the Switch) read `Unknown`.
    #[test]
    fn intro_scenes_are_recognised() {
        assert_screens(&[
            (
                "switch-copyright.png",
                ScreenState::Intro,
                "intro-copyright",
            ),
            ("emu-copyright.png", ScreenState::Intro, "intro-copyright"),
            (
                "emu-copyright-fade-white.png",
                ScreenState::Intro,
                "intro-copyright",
            ),
            (
                "emu-copyright-fade-grey.png",
                ScreenState::Intro,
                "intro-copyright",
            ),
            (
                "emu-game-freak-star.png",
                ScreenState::Intro,
                "intro-game-freak",
            ),
            ("emu-oak-stage.png", ScreenState::Intro, "intro-oak"),
            ("emu-oak-stage-nidoran.png", ScreenState::Intro, "intro-oak"),
            ("emu-oak-stage-player.png", ScreenState::Intro, "intro-oak"),
        ]);
    }

    /// CONTINUE plays the Quest Log recap (Switch goal run, emulator): it
    /// read `Unknown` and its grey replay could be localized.
    #[test]
    fn the_quest_log_is_recognised() {
        assert_screens(&[
            ("switch-quest-log.png", ScreenState::QuestLog, "quest-log"),
            ("emu-quest-log.png", ScreenState::QuestLog, "quest-log"),
            (
                "emu-quest-log-ending.png",
                ScreenState::QuestLog,
                "quest-log",
            ),
        ]);
        // The main menu's slate background and the emulator's plain grey
        // frames (90,89,90) are not the recap.
        for name in ["switch-main-menu-continue.png", "emu-plain-grey.png"] {
            if let Some(image) = fixture(name) {
                let o = FireRedPerception::default().observe(&frame(0, image));
                assert_ne!(o.screen.value, ScreenState::QuestLog, "{name}");
            }
        }
    }

    /// Switch goal run: BULBASAUR evolving after a trainer battle read as
    /// battle text; the text stays readable through the animation.
    #[test]
    fn the_evolution_scene_is_recognised_and_read() {
        let Some(mut p) = reading_perception() else {
            return;
        };
        for (i, name) in [
            "switch-evolution-start.png",
            "switch-evolution-dark.png",
            "switch-evolution-flash.png",
        ]
        .iter()
        .enumerate()
        {
            let Some(image) = fixture(name) else {
                continue;
            };
            let o = p.observe(&frame(i as u64, image));
            assert_eq!(o.screen.value, ScreenState::Evolution, "{name}");
            let d = o.dialogue.expect(name);
            assert_eq!(d.lines, vec!["What?", "BULBASAUR is evolving!"], "{name}");
        }
    }
}
