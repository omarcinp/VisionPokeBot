//! Scenes: what the game plays by itself once a script has started —
//! pages of dialogue, questions, battles, and forced movement (the player
//! or NPCs walked by the script, a warp to another map) — followed until
//! the player is back in control.
//!
//! A scene is over once nothing has happened for a while: no dialogue,
//! menu, battle or transition, the player on the same tile (forced
//! movement changes it without any input), and the picture at rest: an
//! NPC walking up changes a few hundred pixels on nearly every frame,
//! while an idle overworld changes none, or a few hundred on one frame in
//! sixteen (flowers and water animating). A capture whose picture never
//! rests (noise, a wanderer in view) ends the wait after
//! [`SCENE_QUIET_MAX_FRAMES`].
//! Dialogue is followed by the [`Conversation`] (recognition, answers); a
//! battle is the interrupt handler's (`ToolContext::act`).

use pokebot_state::{Observation, PlayerPose, ScreenState};

use super::{Conversation, Expects, StepContext, ToolStep};
use crate::{Action, Decision, Outcome};

/// Frames the scene must be at rest before it counts as over.
pub const SCENE_STILL_FRAMES: u32 = 120;
/// Frames without dialogue or the player moving after which a picture
/// that never rests is taken as over too.
pub const SCENE_QUIET_MAX_FRAMES: u32 = 600;
/// Frames a scene may take to start (the step onto a trigger, a map's
/// entry scene after the fade).
pub const SCENE_START_FRAMES: u64 = 300;
/// Pixels changed from one frame to the next that count as a changed
/// frame; fewer is rest.
pub const MOTION_PIXELS: u32 = 24;
/// Frames looked back over, and how many of them must have changed, for
/// the picture to be in motion (a tile animation changes one in sixteen).
const MOTION_WINDOW: usize = 8;
const MOTION_CHANGED: usize = 4;

/// How long the screen has been at rest.
#[derive(Debug, Default)]
pub struct Stillness {
    last_pose: Option<PlayerPose>,
    last_frame: Option<u64>,
    /// Frames without dialogue, menus, battles, fades or the player moving.
    pub quiet: u32,
    /// Frames of that without motion on the picture either.
    pub still: u32,
    /// Whether each of the last observations changed.
    recent: std::collections::VecDeque<bool>,
}

impl Stillness {
    /// Books `o`; whether anything happened on it (dialogue, a battle, a
    /// fade, the player moved). Motion alone only resets `still`.
    pub fn observe(&mut self, o: &Observation) -> bool {
        let frames = self
            .last_frame
            .map_or(1, |f| o.frame_id.saturating_sub(f).max(1));
        let frames = u32::try_from(frames).unwrap_or(u32::MAX);
        self.last_frame = Some(o.frame_id);
        let pose = o.player.as_ref().map(|p| p.pose.clone());
        let moved = pose.is_some() && self.last_pose.is_some() && pose != self.last_pose;
        if pose.is_some() {
            self.last_pose = pose;
        }
        let busy = o.dialogue.is_some()
            || o.menu.is_some()
            || o.battle.is_some()
            || o.screen.value == ScreenState::Transition
            || o.player.is_none();
        if busy || moved {
            self.quiet = 0;
            self.still = 0;
            return true;
        }
        self.quiet = self.quiet.saturating_add(frames);
        self.recent
            .push_back(o.metrics.changed_pixels >= MOTION_PIXELS);
        if self.recent.len() > MOTION_WINDOW {
            self.recent.pop_front();
        }
        if self.recent.iter().filter(|c| **c).count() >= MOTION_CHANGED {
            self.still = 0;
        } else {
            self.still = self.still.saturating_add(frames);
        }
        false
    }

    /// At rest for `frames`, or quiet so long that the motion is the
    /// picture's own.
    pub fn settled(&self, frames: u32) -> bool {
        self.still >= frames || self.quiet >= SCENE_QUIET_MAX_FRAMES
    }
}

/// A scene on screen (or about to start), followed to its end.
pub struct SceneStep {
    pub conversation: Conversation,
    rest: Stillness,
    /// Something happened: a page, a battle, a fade, the player moved.
    pub happened: bool,
    /// The first frame seen, for the start timeout.
    first_frame: Option<u64>,
    start_frames: u64,
}

impl SceneStep {
    pub fn new(conversation: Conversation) -> Self {
        Self {
            conversation,
            rest: Stillness::default(),
            happened: false,
            first_frame: None,
            start_frames: SCENE_START_FRAMES,
        }
    }
}

impl ToolStep for SceneStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let first = *self.first_frame.get_or_insert(ctx.observation.frame_id);
        if self.rest.observe(ctx.observation) {
            self.happened = true;
        }
        let o = ctx.observation;
        if o.dialogue.is_some() || o.menu.is_some() {
            return self.conversation.next(ctx);
        }
        if !self.happened {
            if o.frame_id.saturating_sub(first) > self.start_frames {
                return Decision::Fail(format!(
                    "no scene started within {} frames",
                    self.start_frames
                ));
            }
            return Decision::Wait("waiting for the scene".into());
        }
        if self.rest.settled(SCENE_STILL_FRAMES) {
            return Decision::Done(format!(
                "scene over: {} pages, {} recognised",
                self.conversation.pages.len(),
                self.conversation.recognised.len()
            ));
        }
        Decision::Wait("the scene plays out".into())
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        self.conversation.on_outcome(action, outcome, ctx);
    }

    fn expects(&self) -> Expects {
        Expects::DIALOGUE
    }
}

/// Waits until the player has control again ([`Stillness`] for `frames`).
/// A scene that starts meanwhile (a map's entry scene after a walk in, a
/// trigger stepped on) is the interrupt handler's: its dialogue runs
/// `Unstick`, which recognises the script.
pub struct SettleStep {
    rest: Stillness,
    frames: u32,
}

impl SettleStep {
    pub fn new(frames: u32) -> Self {
        Self {
            rest: Stillness::default(),
            frames,
        }
    }
}

impl ToolStep for SettleStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        self.rest.observe(ctx.observation);
        if self.rest.settled(self.frames) {
            return Decision::Done("settled".into());
        }
        Decision::Wait("letting the scene settle".into())
    }
}
