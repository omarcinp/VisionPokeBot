//! Perception: turns normalized frames into observations. Produces facts
//! only; it never chooses actions.
//!
//! Detectors are deterministic rules over FireRed's fixed UI (palette colours
//! and geometry measured from the game), not learned models or stored game
//! graphics.

pub mod color;
pub mod detect;

use std::sync::Arc;

use pokebot_core::{NormalizedFrame, RgbImage};
use pokebot_state::{
    BattleMenu, DialogueKind, FrameMetrics, Observation, Observed, PlayerPose, Region, ScreenState,
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
    previous_text: Option<(Vec<u8>, u32)>,
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
        let mut dialogue = detect::dialogue::detect(image);
        match &mut dialogue {
            Some(d) => {
                let stable = match &self.previous_text {
                    Some((cells, n))
                        if detect::dialogue::changed_cells(cells, &d.text_cells) == 0 =>
                    {
                        n + 1
                    }
                    _ => 0,
                };
                d.stable_frames = stable;
                self.previous_text = Some((d.text_cells.clone(), stable));
            }
            None => self.previous_text = None,
        }
        let battle = detect::battle::detect(image);
        let menu = if battle.is_some() {
            None
        } else {
            detect::menu::detect(image)
        };
        let battle_menu = battle.as_ref().and_then(|b| b.menu);
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
}
