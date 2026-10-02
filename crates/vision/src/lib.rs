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
use pokebot_world::{localize::PLAYER_SPRITE, sprites::SpriteDetector, Localizer, World, BLOCK};

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

/// A warp that fires as the player steps onto it (a ladder, a hole); an
/// arrow warp (a cave's exit mat) fires only on the next step along its
/// arrow, a stair warp on a push sideways.
fn fires_on_step(behavior: u16) -> bool {
    use pokebot_world::behavior::{arrow_warp, stair_warp, warp_fires};
    warp_fires(behavior) && arrow_warp(behavior).is_none() && stair_warp(behavior).is_none()
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
    /// The hint last given from outside (the pose a save was made at): a
    /// title screen or the main menu puts it back, as the game reloads
    /// there (Switch: frames of the elevator before the soft reset took
    /// the hint over; after CONTINUE onto Rocket Hideout B4F, the tracker
    /// kept looking at the elevator and its neighbours for 15 minutes).
    given: Option<PlayerPose>,
    frames_since_global_search: u32,
    /// The last frame of a battle (its HUD or text): the player is on the
    /// map they fought on, so no whole-world search replaces it for
    /// [`POST_BATTLE_FRAMES`] (fleet worker 3 caught a MACHOP in dark Rock
    /// Tunnel; a frame of the fade back matched Six Island's Sapphire Room
    /// and the belief moved there).
    last_battle_frame: Option<u64>,
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
    /// Consecutive frames read as a fade that didn't change.
    static_fade: u32,
    /// Object sprites around the located player.
    sprites: SpriteDetector,
    /// Dead reckoning in a dark cave (see `reckon`).
    reckoned: Option<Reckoned>,
}

/// The player's position in a dark cave, followed frame to frame from where
/// it was last known (a warp's arrival, a located tile).
#[derive(Debug, Clone)]
struct Reckoned {
    map: String,
    /// The sprite's map pixel (its tile's top-left while standing).
    px: i32,
    py: i32,
    /// The tile reported: a step counts once it has played out.
    tile: (i32, i32),
    /// Pixels moved on the last frame followed (a step under way goes on).
    velocity: (i32, i32),
    /// The last frame followed.
    frame: u64,
    /// Dark views in a row that matched nowhere near.
    misses: u32,
}

/// Pixels the view scrolls at most between two frames followed (walking
/// is 1 px a frame, running 2, the bike 4).
const DARK_REACH: i32 = 8;
/// A dark view matching this well (per mille) is where the player is. The
/// lit disc is small: an NPC next to the player, or the edge of the disc,
/// takes a good share of it (fleet worker 1, a trainer below the player in
/// Rock Tunnel: the whole disc scored about 600 on the true tile).
const DARK_ACCEPT: u32 = 700;
/// A hint (a pose from outside, maybe stale: a save's) starts the
/// reckoning only matching this well: the disc matches 700–875 at many
/// places of Rock Tunnel.
const DARK_HINT_ACCEPT: u32 = 850;
/// Frames without a dark view (a fade, a battle, a menu) after which the
/// player may have taken a warp next to them.
const DARK_GAP: u64 = 6;
/// Dark views in a row that match nowhere near before the reckoning is
/// given up (the player is somewhere else).
const DARK_MISSES: u32 = 90;
/// How much better (per mille) the view must match past a warp than where
/// the player was for the reckoning to take it.
const WARP_MARGIN: u32 = 50;

/// Frames a "fade" may stay unchanged before it counts as the screen
/// itself: a fade changes the whole screen every frame and is over in
/// half a second, but a dark map can have almost nothing bright (Viridian
/// Forest (19, 21): tall grass hides the player's highlights, 6 pixels
/// reach the fade ceiling; two fleet workers waited there for hours).
const STATIC_FADE_FRAMES: u32 = 30;
/// Changed pixels below which a frame counts as unchanged (capture noise).
const STATIC_PIXELS: u32 = 240 * 160 / 100;

/// Frames between whole-world searches while the player can't be located
/// (each takes ~10 ms of all cores; see `Localizer::locate_anywhere`).
const GLOBAL_SEARCH_INTERVAL: u32 = 30;
/// Frames after a battle during which no whole-world search runs.
const POST_BATTLE_FRAMES: u64 = 300;

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
        let observation = self.observe_frame(frame);
        // The game reloads from here: the hint given (the pose saved at)
        // holds again, whatever was tracked before the reset.
        if matches!(
            observation.screen.value,
            ScreenState::TitleScreen | ScreenState::MainMenu
        ) {
            if let Some(given) = self.given.clone() {
                if self.hint.as_ref() != Some(&given) {
                    self.forget_reckoning_unless(&given);
                    self.hint = Some(given);
                    self.inferred = None;
                }
            }
        }
        observation
    }

    fn set_pose_hint(&mut self, pose: PlayerPose) {
        self.forget_reckoning_unless(&pose);
        self.given = Some(pose.clone());
        self.hint = Some(pose);
        self.inferred = None;
    }

    fn set_pose_hint_inferred(&mut self, pose: PlayerPose) {
        self.forget_reckoning_unless(&pose);
        self.inferred = Some(pose.map.clone());
        self.hint = Some(pose);
    }

    /// Drops the hint and searches every map on the next frame (global
    /// search is switched on: without it nothing could be located again).
    fn clear_pose_hint(&mut self) {
        self.given = None;
        self.hint = None;
        self.inferred = None;
        self.reckoned = None;
        self.global_search = true;
        self.frames_since_global_search = GLOBAL_SEARCH_INTERVAL;
    }
}

impl FireRedPerception {
    /// What `frame` shows ([`PerceptionSystem::observe`] without the hint
    /// put back on a reload).
    fn observe_frame(&mut self, frame: &NormalizedFrame) -> Observation {
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
        let scene = scene(image);
        // A fade or a battle's wipe changes every frame; unchanged, the
        // "transition" is the screen itself (Switch, Route 16's gatehouse:
        // the player on its west mat, the map's black edge filling the
        // left of the view, read as a battle wipe, and the walk waited for
        // it to end).
        let fading = scene.is_some_and(|(_, d)| d == "fade" || d == "battle-wipe");
        self.static_fade = match fading && metrics.changed_pixels < STATIC_PIXELS {
            true => self.static_fade + 1,
            false => 0,
        };
        if let Some((state, detector)) = scene.filter(|_| self.static_fade <= STATIC_FADE_FRAMES) {
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
            if detect::battle::is_level_up_panel(image) {
                b.level_up_stats = detect::battle::level_up_stats(image, font);
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
        observation.save_badge_count = self
            .small_font
            .as_deref()
            .and_then(|font| detect::save::badge_count(image, font));
        observation.dialogue = dialogue;
        observation.menu = menu;
        let in_battle = battle.is_some();
        observation.battle = battle;
        if in_battle || observation.screen.value == ScreenState::BattleText {
            self.last_battle_frame = Some(frame.frame_id);
        }

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
            let mut summary = detect::party::summary(image, font);
            if let (Some(s), Some(palettes)) = (&mut summary, &self.palettes) {
                summary_shiny_cross_check(image, s, palettes);
            }
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
        // The storage system's action window (STORE / WITHDRAW …) and its
        // YES/NO are ordinary menus over it: read with the screen.
        if let Some(font) = self.font.as_deref() {
            let menu = detect::menu::detect(image);
            if let Some(pc) = detect::pc::detect(image, font, menu.map(|m| m.window)) {
                let mut observation = Observation::bare(
                    frame_id,
                    screen(ScreenState::PcStorage, "pc-storage"),
                    metrics,
                );
                if let Some(m) = &menu {
                    let cursor = detect::menu::cursor_region(image, m);
                    observation.menu_lines = font.read(image, m.window, &[cursor]);
                }
                observation.menu = menu;
                observation.pc_storage = Some(pc);
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
        let mut windows = Vec::new();
        if observation.dialogue.is_some() {
            windows.push(Region::new(0, 112, 240, 48));
        }
        if let Some(menu) = &observation.menu {
            windows.push(menu.window.inflate(8));
        }
        if let Some(popup) = popup {
            windows.push(popup);
        }
        let mut exclude = vec![PLAYER_SPRITE];
        exclude.extend(windows.iter().copied());
        let localizer = Localizer::new(&world);
        // A dark cave without Flash shows only a disc around the player:
        // the black around it is no part of the map (fleet worker 1 stood
        // unlocated in Rock Tunnel, "stuck waiting: locating the player").
        // The disc is too small a view to search the cave by: the player
        // is followed from where they came in (dead reckoning).
        let dark = dark_view(image, &windows);
        let mut disc = vec![DARK_PLAYER_SPRITE];
        if let Some(lit) = dark {
            exclude.extend(outside_disc(lit));
            disc.extend(windows.iter().copied());
            disc.extend(outside_disc(lit));
            if self.reckoned.is_none() {
                if let Some(hint) = self.hint.clone() {
                    self.seed_reckoning(
                        observation.frame_id,
                        image,
                        &localizer,
                        &disc,
                        &hint,
                        DARK_HINT_ACCEPT,
                    );
                }
            }
            if self.reckoned.is_some() {
                let found = self.reckon(observation.frame_id, image, &localizer, &disc);
                if self.reckoned.is_some() {
                    if let Some(found) = &found {
                        self.hint = Some(found.pose.clone());
                    }
                    observation.player = found;
                    return;
                }
                // Given up: searched for as usual (near the hint only).
            }
        }
        let tracked = self.hint.as_ref().and_then(|hint| match dark {
            Some(_) => localizer.locate_near(image, hint, &exclude),
            None => localizer.locate_from(image, hint, &exclude),
        });
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
            // Nor the world by it.
            None if !self.global_search || dark.is_some() => None,
            // Just out of a battle: still on the map it was fought on.
            None if self
                .last_battle_frame
                .is_some_and(|f| observation.frame_id < f + POST_BATTLE_FRAMES) =>
            {
                None
            }
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
            match dark {
                Some(_) => {
                    let pose = found.pose.clone();
                    if let Some(followed) = self.seed_reckoning(
                        observation.frame_id,
                        image,
                        &localizer,
                        &disc,
                        &pose,
                        DARK_ACCEPT,
                    ) {
                        self.hint = Some(followed.pose.clone());
                        observation.player = Some(followed);
                        return;
                    }
                }
                None => self.reckoned = None,
            }
        }
        observation.player = found;
    }

    /// Dead reckoning in a dark cave: the lit disc is too small a view to
    /// search the cave by (its floor repeats), but from where the player
    /// was last known each frame's view has scrolled a few pixels at most,
    /// and past a fade or a battle only a warp next to them has moved them
    /// elsewhere. `None` when the view matched nowhere near; after
    /// [`DARK_MISSES`] such frames the reckoning is given up.
    fn reckon(
        &mut self,
        frame_id: u64,
        image: &RgbImage,
        localizer: &Localizer<'_>,
        exclude: &[Region],
    ) -> Option<pokebot_state::PoseObservation> {
        let world = self.world.clone()?;
        let r = self.reckoned.clone()?;
        let map = world.map(&r.map)?;
        let gap = frame_id.saturating_sub(r.frame) > DARK_GAP;
        let (expect, reach) = match gap {
            true => ((r.px, r.py), BLOCK - 1),
            false => ((r.px + r.velocity.0, r.py + r.velocity.1), DARK_REACH),
        };
        let here = localizer.follow(image, map, (r.px, r.py), expect, reach, exclude);
        let mut best = here.map(|f| (map, f));
        if gap {
            let floor = here.map_or(0, |f| f.score) + WARP_MARGIN;
            let near = map
                .warps
                .iter()
                .filter(|w| (w.x - r.tile.0).abs() + (w.y - r.tile.1).abs() <= 1);
            for warp in near {
                // A lit map is located as usual.
                let Some((dest, x, y)) = world.warp_destination(warp) else {
                    continue;
                };
                if !dest.requires_flash {
                    continue;
                }
                let start = (x * BLOCK, y * BLOCK);
                let Some(f) = localizer.follow(image, dest, start, start, BLOCK - 1, exclude)
                else {
                    continue;
                };
                if f.score >= floor && best.is_none_or(|(_, b)| f.score > b.score) {
                    best = Some((dest, f));
                }
            }
        }
        let r = self.reckoned.as_mut()?;
        r.frame = frame_id;
        let Some((at, f)) = best.filter(|(_, f)| f.score >= DARK_ACCEPT) else {
            r.misses += 1;
            r.velocity = (0, 0);
            if r.misses > DARK_MISSES {
                self.reckoned = None;
            }
            return None;
        };
        if at.name == r.map {
            r.velocity = (f.px - r.px, f.py - r.py);
            let before = r.tile;
            r.tile = f.tile(r.tile);
            // A step onto a warp that fires is that warp, whatever the
            // disc shows: the fade from dark to dark reads as no fade, and
            // a ladder's disc looks alike on both floors (Switch, Rock
            // Tunnel B1F (33, 3): the belief stayed below, the walk
            // stepped off and on the ladder, "looping").
            if r.tile != before {
                if let Some((dest, x, y)) = at
                    .tile(r.tile.0, r.tile.1)
                    .filter(|t| fires_on_step(t.behavior))
                    .and_then(|_| at.warps.iter().find(|w| (w.x, w.y) == r.tile))
                    .and_then(|w| world.warp_destination(w))
                {
                    r.map = dest.name.clone();
                    r.tile = (x, y);
                    r.velocity = (0, 0);
                    (r.px, r.py, r.misses) = (x * BLOCK, y * BLOCK, 0);
                    let pose = PlayerPose {
                        map: dest.name.clone(),
                        x,
                        y,
                    };
                    if !dest.requires_flash {
                        // Lit: located as usual from there.
                        self.reckoned = None;
                    }
                    self.hint = Some(pose.clone());
                    return Some(pokebot_state::PoseObservation {
                        pose,
                        score: f.score as u16,
                    });
                }
            }
        } else {
            r.map = at.name.clone();
            r.velocity = (0, 0);
            r.tile = f.tile((f.px.div_euclid(BLOCK), f.py.div_euclid(BLOCK)));
        }
        (r.px, r.py, r.misses) = (f.px, f.py, 0);
        Some(pokebot_state::PoseObservation {
            pose: PlayerPose {
                map: r.map.clone(),
                x: r.tile.0,
                y: r.tile.1,
            },
            score: f.score as u16,
        })
    }

    /// Starts the reckoning at `pose` (a warp's arrival, a located tile, the
    /// hint) when it is on a dark map and the view matches there (the view
    /// may be mid-step): the pose followed.
    fn seed_reckoning(
        &mut self,
        frame_id: u64,
        image: &RgbImage,
        localizer: &Localizer<'_>,
        exclude: &[Region],
        pose: &PlayerPose,
        accept: u32,
    ) -> Option<pokebot_state::PoseObservation> {
        let world = self.world.clone()?;
        let map = world.map(&pose.map).filter(|m| m.requires_flash)?;
        let start = (pose.x * BLOCK, pose.y * BLOCK);
        let f = localizer
            .follow(image, map, start, start, BLOCK - 1, exclude)
            .filter(|f| f.score >= accept)?;
        let tile = f.tile((pose.x, pose.y));
        self.reckoned = Some(Reckoned {
            map: pose.map.clone(),
            px: f.px,
            py: f.py,
            tile,
            velocity: (0, 0),
            frame: frame_id,
            misses: 0,
        });
        Some(pokebot_state::PoseObservation {
            pose: PlayerPose {
                map: pose.map.clone(),
                x: tile.0,
                y: tile.1,
            },
            score: f.score as u16,
        })
    }

    /// A pose from outside (a reset, a warp worked out) other than the one
    /// reckoned ends the reckoning.
    fn forget_reckoning_unless(&mut self, pose: &PlayerPose) {
        let same = self
            .reckoned
            .as_ref()
            .is_some_and(|r| r.map == pose.map && r.tile == (pose.x, pose.y));
        if !same {
            self.reckoned = None;
        }
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

/// On INFO, the picture's palette must agree with the star; a picture that
/// reads the other way (or can't decide) makes the reading `Unclear`.
fn summary_shiny_cross_check(
    image: &RgbImage,
    summary: &mut pokebot_state::SummaryObservation,
    palettes: &shiny::SpritePalettes,
) {
    if summary.page != pokebot_state::SummaryPage::Info {
        return;
    }
    let Some(star) = summary.shiny else { return };
    let Some(key) = summary
        .species
        .as_deref()
        .and_then(|name| detect::hud::resolve(name, palettes.keys().map(String::as_str)))
    else {
        return;
    };
    let (normal, shiny_palette) = &palettes[key];
    let picture = shiny::classify(image, detect::party::SUMMARY_PICTURE, normal, shiny_palette);
    if picture != star {
        summary.shiny = Some(pokebot_state::ShinyReading::Unclear);
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

/// The player's sprite alone (16×32 over its tile and the one above), for
/// the lit disc of a dark cave: [`PLAYER_SPRITE`]'s margin would take a
/// third of it.
const DARK_PLAYER_SPRITE: Region = Region {
    x: 110,
    y: 54,
    width: 20,
    height: 36,
};

/// Luma at or below which a pixel is the darkness of an unlit cave.
const UNLIT_LUMA: u8 = 12;

/// The lit disc of a dark cave without Flash (the bounding box of what
/// isn't black, around the player at the screen's centre), when the rest
/// of the frame is black: `(x0, y0, x1, y1)`, inclusive.
fn dark_view(image: &RgbImage, windows: &[Region]) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    let mut lit = 0u32;
    for y in 0..image.height() {
        for x in 0..image.width() {
            if windows.iter().any(|w| w.contains(x, y)) {
                continue;
            }
            if color::luma(image.pixel(x, y)) > UNLIT_LUMA {
                lit += 1;
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
    }
    let (w, h) = (x1.checked_sub(x0)? + 1, y1.checked_sub(y0)? + 1);
    let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
    // A disc (the box square, the lit area about π/4 of it) on the player.
    let disc = w.abs_diff(h) <= 4
        && (20..=120).contains(&w)
        && cx.abs_diff(120) <= 4
        && cy.abs_diff(80) <= 4
        && lit * 100 >= w * h * 65
        && lit * 100 <= w * h * 95;
    disc.then_some((x0, y0, x1, y1))
}

/// Everything but the disc inscribed in `lit`, a margin inside its edge
/// (the rim blends into black): the frame around its box, and row by row
/// the box outside the circle.
fn outside_disc((x0, y0, x1, y1): (u32, u32, u32, u32)) -> Vec<Region> {
    const MARGIN: f64 = 3.0;
    let (cx, cy) = (f64::from(x0 + x1) / 2.0, f64::from(y0 + y1) / 2.0);
    let r = f64::from((x1 - x0).min(y1 - y0)) / 2.0 - MARGIN;
    let mut out = vec![
        Region::new(0, 0, 240, y0),
        Region::new(0, y1 + 1, 240, 160 - (y1 + 1)),
        Region::new(0, y0, x0, y1 - y0 + 1),
        Region::new(x1 + 1, y0, 240 - (x1 + 1), y1 - y0 + 1),
    ];
    for y in y0..=y1 {
        let dy = f64::from(y) + 0.5 - cy;
        let half = (r * r - dy * dy).max(0.0).sqrt();
        let (left, right) = ((cx - half).ceil() as u32, (cx + half).floor() as u32);
        if half <= 0.0 || left > right {
            out.push(Region::new(x0, y, x1 - x0 + 1, 1));
            continue;
        }
        out.push(Region::new(x0, y, left.saturating_sub(x0), 1));
        out.push(Region::new(right + 1, y, x1.saturating_sub(right), 1));
    }
    out
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

    /// The PC: "Which PC should be accessed?" and the storage menu are
    /// dialogue with a field menu; the storage system is its own screen,
    /// its STORE / WITHDRAW window and YES/NO read as menus over it.
    #[test]
    fn pc_screens_are_recognised() {
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
        if let Some(o) = observe("emu-pc-which-pc-someone") {
            assert_eq!(o.dialogue.unwrap().lines, ["Which PC should be accessed?"]);
            assert_eq!(o.menu_lines.len(), 4);
            assert!(o.menu_lines[0].starts_with("SOMEONE"));
            assert_eq!(o.menu_lines[3], "LOG OFF");
            assert!(o.pc_storage.is_none());
        }
        if let Some(o) = observe("emu-pc-storage-menu") {
            assert!(o.dialogue.is_some());
            assert_eq!(
                o.menu_lines,
                [
                    "WITHDRAW POKéMON",
                    "DEPOSIT POKéMON",
                    "MOVE POKéMON",
                    "MOVE ITEMS",
                    "SEE YA!"
                ]
            );
        }
        if let Some(o) = observe("emu-pc-deposit-store") {
            assert_eq!(o.screen.value, ScreenState::PcStorage);
            assert!(o.dialogue.is_none());
            assert_eq!(
                o.menu_lines,
                ["STORE", "SUMMARY", "MARK", "RELEASE", "CANCEL"]
            );
            assert!(o.pc_storage.is_some());
        }
        if let Some(o) = observe("emu-pc-continue") {
            assert_eq!(o.menu_lines, ["YES", "NO"]);
            assert_eq!(o.menu.unwrap().cursor_row, 1);
            assert!(o.pc_storage.is_some());
        }
        if let Some(o) = observe("emu-pc-withdraw-box") {
            assert_eq!(o.screen.value, ScreenState::PcStorage);
            assert!(o.menu.is_none() && o.dialogue.is_none());
        }
        // Fleet continue-6: a box with FEAROW under the hand, its tall
        // picture over most of the picture box's top rows; read Unknown,
        // the swap failed "no progress in box".
        if let Some(o) = observe("emu-pc-box-grass-fearow") {
            assert_eq!(o.screen.value, ScreenState::PcStorage);
            let pc = o.pc_storage.expect("the storage screen");
            assert_eq!(pc.species.as_deref(), Some("FEAROW"));
        }
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

    /// Switch, Route 1: the command menu with the cursor on BAG read as
    /// battle text (one cursor pixel 25 off the colour), so the catch's A
    /// opened the bag unarmed and closed it again, over and over.
    #[test]
    fn switch_command_menu_cursor_on_bag_is_seen() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(image) =
            pokebot_video::png::load(root.join("captures/fixtures/switch-battle-command-bag.png"))
        else {
            return;
        };
        let b = detect::battle::detect(&image).unwrap();
        assert_eq!(b.menu, Some(BattleMenu::Command { column: 1, row: 0 }));
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

    /// The summary's shiny star, checked against the picture's palette: a
    /// shiny CHARMANDER from the emulator's shiny-starter hunt, a normal
    /// IVYSAUR, and the shiny one with its star painted over.
    #[test]
    fn the_summary_star_and_picture_agree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(font), Ok(data)) = (
            text::Font::load(root.join("data/world/font_normal.json")),
            pokebot_gamedata::GameData::load(root.join("data/world/gamedata.json")),
        ) else {
            return;
        };
        let mut palettes = shiny::SpritePalettes::new();
        for name in ["CHARMANDER", "IVYSAUR"] {
            let p = data.species[&format!("SPECIES_{name}")]
                .palettes
                .clone()
                .unwrap();
            palettes.insert(
                name.into(),
                (p.normal.try_into().unwrap(), p.shiny.try_into().unwrap()),
            );
        }
        let mut p = FireRedPerception::default()
            .with_font(std::sync::Arc::new(font))
            .with_palettes(std::sync::Arc::new(palettes));
        use pokebot_state::ShinyReading::{Normal, Shiny, Unclear};
        let mut read = |image: RgbImage| p.observe(&frame(0, image)).summary.and_then(|s| s.shiny);
        if let Some(image) = fixture("emu-summary-info.png") {
            assert_eq!(read(image), Some(Normal));
        }
        let Some(mut image) = fixture("emu-summary-info-shiny.png") else {
            return;
        };
        assert_eq!(read(image.clone()), Some(Shiny));
        let r = detect::party::SHINY_STAR;
        for y in r.y..r.y + r.height {
            for x in r.x..r.x + r.width {
                image.put_pixel(x, y, [239, 235, 239]);
            }
        }
        assert_eq!(read(image), Some(Unclear));
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

    /// Switch: saved on Rocket Hideout B4F by the lift, the run began in
    /// the elevator; tracking its frames before the soft reset took over
    /// the saved pose's hint, and after CONTINUE the tracker looked at the
    /// elevator and its neighbours (its door leads nowhere fixed) for 15
    /// minutes. The title screen puts the given hint back.
    #[test]
    fn a_reload_tracks_from_the_pose_given_again() {
        let (Some(world), Some(lift), Some(title), Some(b4f)) = (
            world(),
            fixture("switch-rocket-elevator.png"),
            fixture("switch-title-screen.png"),
            fixture("switch-rocket-b4f-lift-doors.png"),
        ) else {
            return;
        };
        let saved = PlayerPose {
            map: "RocketHideout_B4F".into(),
            x: 20,
            y: 24,
        };
        let on_b4f = |o: Option<pokebot_state::PoseObservation>| o.is_some_and(|f| f.pose == saved);
        let mut p = FireRedPerception::with_world(Arc::clone(&world));
        p.set_pose_hint(saved.clone());
        // The car, a neighbour of B4F: tracked there, the hint follows.
        let car = p.observe(&frame(0, lift.clone())).player;
        assert!(car.is_some_and(|f| f.pose.map.ends_with("_Elevator")));
        // Without the title screen, B4F isn't searched from the car.
        let mut stuck = FireRedPerception::with_world(Arc::clone(&world));
        stuck.set_pose_hint(saved.clone());
        stuck.observe(&frame(0, lift));
        assert!((1..5).all(|i| stuck.observe(&frame(i, b4f.clone())).player.is_none()));
        // The reload: the title screen, then the map the save was made on.
        assert_eq!(
            p.observe(&frame(1, title)).screen.value,
            ScreenState::TitleScreen
        );
        assert!(on_b4f(p.observe(&frame(2, b4f)).player));
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

    /// Fleet worker 3 caught a MACHOP in dark Rock Tunnel; a frame of the
    /// fade back matched Six Island's Sapphire Room and the belief moved
    /// there. Right after a battle the player is on the map it was fought
    /// on: no whole-world search for [`POST_BATTLE_FRAMES`], then as usual.
    #[test]
    fn no_world_search_right_after_a_battle() {
        let (Some(world), Some(image), Some(battle)) = (
            world(),
            fixture("switch-pewter-gym.png"),
            fixture("emu-tools-battle-command.png"),
        ) else {
            return;
        };
        let stale = PlayerPose {
            map: "Route3".into(),
            x: 5,
            y: 9,
        };
        let mut p = FireRedPerception::with_world(world).with_global_search(true);
        assert!(p.observe(&frame(0, battle)).battle.is_some());
        p.set_pose_hint(stale);
        let located: Vec<bool> = (1..POST_BATTLE_FRAMES + u64::from(GLOBAL_SEARCH_INTERVAL) + 1)
            .map(|i| p.observe(&frame(i, image.clone())).player.is_some())
            .collect();
        let window = usize::try_from(POST_BATTLE_FRAMES).unwrap() - 1;
        assert!(
            located[..window].iter().all(|l| !l),
            "searched right after the battle"
        );
        assert!(located[window..].iter().any(|l| *l), "never searched after");
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

    /// Fleet worker 1 in Rock Tunnel without Flash: only a disc around the
    /// player is drawn. It is located from the disc alone.
    #[test]
    fn a_dark_cave_is_located_from_the_lit_disc() {
        let Some(image) = fixture("emu-rock-tunnel-dark.png") else {
            return;
        };
        assert!(dark_view(&image, &[]).is_some());
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = pokebot_world::World::load(root.join("data/world")) else {
            return;
        };
        // Tracked from the tunnel's mouth on Route 10, where the player was
        // last located: the disc alone is too plain for a whole-world
        // search, not for the maps the mouth leads to.
        let mut p = FireRedPerception::with_world(std::sync::Arc::new(world));
        p.set_pose_hint(pokebot_state::PlayerPose {
            map: "Route10".into(),
            x: 8,
            y: 19,
        });
        let pose = (0..3)
            .find_map(|f| p.observe(&frame(f, image.clone())).player)
            .map(|p| p.pose);
        let pose = pose.expect("located");
        assert!(pose.map.starts_with("RockTunnel"), "{pose:?}");
        // A lit field is no dark view.
        if let Some(lit) = fixture("emu-forest-tall-grass-static.png") {
            assert!(dark_view(&lit, &[]).is_none());
        }
    }

    /// Fleet continue-1, Rock Tunnel B1F without Flash: the reckoning had
    /// drifted (it held (15, 35), the player stood at (17, 34)), lost the
    /// player, and the search near the hint "located" them on 1F (6, 2),
    /// by the ladder from B1F (33, 3): the disc of rock floor matched there
    /// too. Every route from there failed "stuck waiting: locating the
    /// player". Off a dark map only a warp next to the player leads; a
    /// hint too far off for the search near it finds nothing instead.
    #[test]
    fn a_dark_cave_is_not_left_but_by_a_warp_next_to_the_player() {
        let Some(image) = fixture("emu-rock-tunnel-b1f-lost.png") else {
            return;
        };
        let Some(world) = world() else { return };
        let mut p = FireRedPerception::with_world(world);
        p.set_pose_hint(PlayerPose {
            map: "RockTunnel_B1F".into(),
            x: 21,
            y: 35,
        });
        for id in 0..u64::from(DARK_MISSES) * 2 {
            let seen = p.observe(&frame(id, image.clone())).player;
            assert!(
                seen.as_ref().is_none_or(|o| o.pose.map == "RockTunnel_B1F"),
                "frame {id}: {seen:?}"
            );
        }
    }

    /// Fleet continue-1/2/3, the Underground Path's east-west tunnel: a
    /// uniform corridor whose only marks are ceiling lights a few pixels
    /// wide. Walking east, the sparse search near the last tile put the
    /// player three tiles back (983 against the true tile's 1000 when
    /// compared densely), and every walk through failed "looping". Mid-step
    /// east of (9, 3) and of (15, 3), the pose stays on its tile.
    #[test]
    fn a_uniform_corridor_is_tracked_tile_by_tile() {
        let Some(world) = world() else { return };
        for (name, x) in [
            ("emu-underground-tunnel-east-9.png", 9),
            ("emu-underground-tunnel-east-15.png", 15),
        ] {
            let Some(image) = fixture(name) else { return };
            let mut p = FireRedPerception::with_world(world.clone());
            let pose = |x| PlayerPose {
                map: "UndergroundPath_EastWestTunnel".into(),
                x,
                y: 3,
            };
            p.set_pose_hint(pose(x));
            let seen = p.observe(&frame(1, image)).player.map(|o| o.pose);
            assert_eq!(seen, Some(pose(x)), "{name}");
        }
    }

    /// Switch, Seafoam Islands B3F at (27, 11): the currents (behaviours
    /// 0x50–0x53) animate like water, but only Surf's water was left out
    /// of the match; on a third of the frames the player was not located,
    /// the walk waited for the scene to settle, and every plan failed
    /// there. The currents are left out too.
    #[test]
    fn seafoams_currents_dont_hide_the_player() {
        let Some(world) = world() else { return };
        let pose = PlayerPose {
            map: "SeafoamIslands_B3F".into(),
            x: 27,
            y: 11,
        };
        for id in ["00031260", "00031380", "00031570"] {
            let name = format!("switch-seafoam-b3f-current-{id}.png");
            let Some(image) = fixture(&name) else { return };
            let mut p = FireRedPerception::with_world(world.clone());
            p.set_pose_hint(pose.clone());
            let seen = p.observe(&frame(1, image)).player.map(|o| o.pose);
            assert_eq!(seen, Some(pose.clone()), "{name}");
        }
    }

    /// Fleet continue-2, surfing Route 21 north past its rocks: each rock's
    /// block is drawn on animated sea, but only water blocks forgave the
    /// waves. The true tile, (5, 36), scored 900, and a step from it the
    /// search took (7, 40) at 903; the pose wandered and every walk
    /// "looped". Water pixels inside other blocks are forgiven too.
    #[test]
    fn the_sea_round_rocks_doesnt_move_the_player() {
        let Some(world) = world() else { return };
        let Some(image) = fixture("emu-route21-north-rocks-surf.png") else {
            return;
        };
        let pose = |x, y| PlayerPose {
            map: "Route21_North".into(),
            x,
            y,
        };
        let mut p = FireRedPerception::with_world(world);
        p.set_pose_hint(pose(5, 37));
        let seen = p.observe(&frame(1, image)).player.map(|o| o.pose);
        assert_eq!(seen, Some(pose(5, 36)));
    }

    /// A dark cave's view at sprite map pixel `(px, py)`: the map's render
    /// inside the lit disc (as fleet worker 1 saw it in Rock Tunnel: its
    /// box 96–143 × 56–104), black around it, the player's sprite on top.
    fn dark_frame(map: &pokebot_world::MapData, px: i32, py: i32) -> RgbImage {
        let render = map.render().unwrap();
        let (ox, oy) = (px + map.pad * BLOCK - 112, py + map.pad * BLOCK - 72);
        let mut image = RgbImage::filled(240, 160, [0, 0, 0]);
        for y in 0..160u32 {
            for x in 0..240u32 {
                let (dx, dy) = (f64::from(x) + 0.5 - 120.0, f64::from(y) + 0.5 - 80.5);
                if dx * dx + dy * dy > 24.0 * 24.0 {
                    continue;
                }
                let (rx, ry) = (ox + x as i32, oy + y as i32);
                let inside =
                    rx >= 0 && ry >= 0 && rx < render.width() as i32 && ry < render.height() as i32;
                if inside {
                    image.put_pixel(x, y, render.pixel(rx as u32, ry as u32));
                }
            }
        }
        for y in 58..88 {
            for x in 113..127 {
                image.put_pixel(
                    x,
                    y,
                    if (x + y) % 3 == 0 {
                        [248, 248, 248]
                    } else {
                        [200, 48, 40]
                    },
                );
            }
        }
        image
    }

    /// The user's idea for dark caves: the player's first tile past the
    /// warp is known, and every step from there can be followed. Rock
    /// Tunnel's lit disc matches many places of the cave (the whole-cave
    /// search "located" fleet worker 1 eight tiles off, then in Diglett's
    /// Cave), but between two frames the view scrolls a few pixels: a walk
    /// and a run across 1F from the ladder, then down the ladder to B1F,
    /// are followed tile by tile.
    #[test]
    fn a_dark_cave_walk_is_followed_from_the_warp() {
        use pokebot_world::path::{find_path_with, Obstacles, Walk};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = pokebot_world::World::load(root.join("data/world")) else {
            return;
        };
        let world = std::sync::Arc::new(world);
        let map = world.map("RockTunnel_1F").unwrap();
        let none = Obstacles::new();
        let walk = Walk {
            obstacles: &none,
            surf: false,
            opened: None,
        };
        let mut p = FireRedPerception::with_world(world.clone());
        let mut id = 0;
        let mut see = |p: &mut FireRedPerception, image: RgbImage| {
            id += 1;
            p.observe(&frame(id, image)).player.map(|o| o.pose)
        };
        let pose = |map: &str, x: i32, y: i32| PlayerPose {
            map: map.into(),
            x,
            y,
        };
        // Arrived from B1F at the ladder (45, 21).
        p.set_pose_hint(pose("RockTunnel_1F", 45, 21));
        let mut at = (45, 21);
        for (goal, speed) in [((18, 37), 1), ((45, 21), 2)] {
            let path = find_path_with(map, at, &walk, |_| 0, |t| t == goal, |_| 0).unwrap();
            assert!(path.len() > 30, "{}", path.len());
            for step in path {
                let (dx, dy) = step.dir.delta();
                for k in (0..BLOCK).step_by(speed) {
                    let (px, py) = (at.0 * BLOCK + dx * k, at.1 * BLOCK + dy * k);
                    let seen = see(&mut p, dark_frame(map, px, py));
                    assert_eq!(
                        seen,
                        Some(pose("RockTunnel_1F", at.0, at.1)),
                        "{step:?} +{k}"
                    );
                }
                at = step.to;
                let seen = see(&mut p, dark_frame(map, at.0 * BLOCK, at.1 * BLOCK));
                // A step onto the ladder is its warp (the Switch on B1F
                // (33, 3): the fade from dark to dark reads as none).
                let fires = map
                    .tile(at.0, at.1)
                    .is_some_and(|t| fires_on_step(t.behavior));
                let expected = match map.warps.iter().find(|w| fires && (w.x, w.y) == at) {
                    Some(w) => {
                        let (to, x, y) = world.warp_destination(w).unwrap();
                        pose(&to.name, x, y)
                    }
                    None => pose("RockTunnel_1F", at.0, at.1),
                };
                assert_eq!(seen, Some(expected));
            }
        }
        // Down the ladder: a fade, then B1F's ladder the warp leads to.
        let warp = map.warps.iter().find(|w| (w.x, w.y) == at).unwrap();
        let (b1f, x, y) = world.warp_destination(warp).unwrap();
        for _ in 0..20 {
            see(&mut p, RgbImage::filled(240, 160, [0, 0, 0]));
        }
        let seen = see(&mut p, dark_frame(b1f, x * BLOCK, y * BLOCK));
        assert_eq!(seen, Some(pose(&b1f.name, x, y)));
        // Fleet worker 1 after a battle on 1F (27, 9): the trainer beaten
        // stands below the player, taking a third of the disc. The
        // reckoning holds.
        if let Some(image) = fixture("emu-rock-tunnel-dark-trainer.png") {
            p.set_pose_hint(pose("RockTunnel_1F", 27, 9));
            for _ in 0..3 {
                let seen = see(&mut p, image.clone());
                assert_eq!(seen, Some(pose("RockTunnel_1F", 27, 9)));
            }
        }
    }

    /// Switch: the Safari Zone's "time's up" put the player in the
    /// entrance gate while the belief stayed in SafariZone_North, whose
    /// neighbours the tracker searched for twenty minutes. Once the hint is
    /// dropped (a long "locating the player"), every map is searched and
    /// the gate is found.
    #[test]
    fn a_player_taken_off_the_hints_maps_is_found_once_the_hint_is_dropped() {
        let (Some(world), Some(image)) =
            (world(), fixture("switch-safari-entrance-after-time-up.png"))
        else {
            return;
        };
        let mut p = FireRedPerception::with_world(world);
        p.set_pose_hint(PlayerPose {
            map: "SafariZone_North".into(),
            x: 30,
            y: 24,
        });
        for id in 0..5 {
            assert!(p.observe(&frame(id, image.clone())).player.is_none());
        }
        p.clear_pose_hint();
        let found = (5..5 + u64::from(GLOBAL_SEARCH_INTERVAL) + 3)
            .find_map(|id| p.observe(&frame(id, image.clone())).player)
            .map(|o| o.pose.map);
        assert_eq!(found.as_deref(), Some("FuchsiaCity_SafariZone_Entrance"));
    }

    /// Switch, Route 16's north gatehouse: standing on the west mat, the
    /// map's black edge fills the left of the view and read as a battle
    /// wipe; the walk waited for it to end. A wipe changes every frame;
    /// unchanged, it is the screen, and the player is located.
    #[test]
    fn an_unchanging_wipe_is_the_screen() {
        let (Some(world), Some(image)) = (world(), fixture("switch-gatehouse-west-mat.png")) else {
            return;
        };
        let mut p = FireRedPerception::with_world(world);
        p.set_pose_hint(PlayerPose {
            map: "Route16_NorthEntrance_1F".into(),
            x: 1,
            y: 3,
        });
        let first = p.observe(&frame(0, image.clone()));
        assert_eq!(first.screen.value, ScreenState::Transition);
        let later: Vec<_> = (1..=u64::from(STATIC_FADE_FRAMES) + 2)
            .map(|i| p.observe(&frame(i, image.clone())))
            .collect();
        let last = later.last().unwrap();
        assert_ne!(last.screen.value, ScreenState::Transition);
        assert_eq!(
            last.player
                .as_ref()
                .map(|o| (o.pose.map.as_str(), o.pose.x, o.pose.y)),
            Some(("Route16_NorthEntrance_1F", 1, 3))
        );
    }

    /// Fleet workers 4 and 6, Viridian Forest (19, 21): in the tall grass
    /// the dark forest has only 6 pixels at fade brightness, so the field
    /// read as a fade and the walk waited for it to end, for hours. A fade
    /// changes every frame; unchanged for half a second it is the screen.
    #[test]
    fn an_unchanging_dark_field_is_not_a_fade_for_long() {
        let Some(image) = fixture("emu-forest-tall-grass-static.png") else {
            return;
        };
        let mut p = FireRedPerception::default();
        let first = p.observe(&frame(0, image.clone()));
        assert_eq!(first.screen.value, ScreenState::Transition);
        let later = (1..=40)
            .map(|f| p.observe(&frame(f, image.clone())))
            .last()
            .unwrap();
        assert_ne!(
            later.screen.value,
            ScreenState::Transition,
            "{}",
            later.screen.detector
        );
    }

    /// Switch goal run (frames 2621200 / 2621240): BULBASAUR grew to Lv 11.
    /// The window's first page shows the gains ("+ 2"), its second the new
    /// stats; only the second is read (HP, Atk, Def, Spe, SpA, SpD).
    #[test]
    fn the_level_up_window_reads_the_new_stats_not_the_gains() {
        let (Some(mut p), Some(gains), Some(totals)) = (
            reading_perception(),
            fixture("switch-level-up-stats-gains.png"),
            fixture("switch-level-up-stats-totals.png"),
        ) else {
            return;
        };
        let o = p.observe(&frame(0, gains));
        assert_eq!(o.battle.unwrap().level_up_stats, None);
        let o = p.observe(&frame(1, totals));
        assert_eq!(
            o.dialogue.unwrap().lines,
            vec!["BULBASAUR grew to", "LV. 11!"]
        );
        assert_eq!(
            o.battle.unwrap().level_up_stats,
            Some([32, 17, 17, 18, 22, 22])
        );
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
