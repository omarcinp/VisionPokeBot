//! `Talk`: stand facing a map object (or a sign), press A, and follow its
//! conversation as the compiled script (`Go(Facing)` → `Dialogue`).

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_state::Direction;

use super::dialogue::{finish, Conversation};
use super::go::{GoStep, NavParts};
use super::lookup::{object_script, object_tile, vanishes_when_taken};
use super::scene::SceneStep;
use super::{
    progress, Answer, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep,
};
use crate::motion::InputKind;
use crate::nav::{direction_button, Destination};
use crate::{Action, Decision, Expectation, Outcome};

/// Frames waiting for the conversation to open after A before the
/// approach is retried.
const TALK_TIMEOUT_FRAMES: u64 = 45;
/// Missed presses (no dialogue after A) before the talk fails.
const MAX_RETRIES: u32 = 3;

pub struct TalkTool;

enum Phase {
    Approach,
    /// Face the way the target must be read from (a sign read from below).
    Turn,
    Press,
    Talking,
}

/// Approach, press A, converse.
pub struct TalkStep {
    phase: Phase,
    map: String,
    x: i32,
    y: i32,
    /// The only direction the target answers to (signs), if any.
    facing: Option<Direction>,
    nav: NavParts,
    go: GoStep,
    /// The conversation, and the scene around it when [`TalkStep::scene`]
    /// is set (what the script plays after the last page: people walking
    /// off, a gift handed over).
    pub scene: SceneStep,
    follow_scene: bool,
    retries: u32,
}

impl TalkStep {
    pub fn new(
        ctx: &ToolContext<'_>,
        map: &str,
        object: u32,
        answers: Vec<Answer>,
    ) -> Result<Self, ToolError> {
        let (x, y) = object_tile(&ctx.world, map, object)
            .ok_or_else(|| ToolError::Failed(format!("{map} has no object {object}")))?;
        let script = object_script(&ctx.world, map, object);
        Ok(Self::toward(ctx, map, (x, y), None, script, answers))
    }

    /// Talks to whatever is at `(x, y)` of `map` (an object, a sign),
    /// facing it from `facing`'s side only when given.
    pub fn toward(
        ctx: &ToolContext<'_>,
        map: &str,
        at: (i32, i32),
        facing: Option<Direction>,
        script: Option<String>,
        answers: Vec<Answer>,
    ) -> Self {
        Self::toward_with(
            NavParts::of(ctx),
            Arc::clone(&ctx.data),
            map,
            at,
            facing,
            script,
            answers,
        )
    }

    /// [`TalkStep::toward`] from its parts.
    pub fn toward_with(
        nav: NavParts,
        data: Arc<pokebot_gamedata::GameData>,
        map: &str,
        (x, y): (i32, i32),
        facing: Option<Direction>,
        script: Option<String>,
        answers: Vec<Answer>,
    ) -> Self {
        let world = Arc::clone(&nav.world);
        let mut step = Self {
            phase: Phase::Approach,
            map: map.to_owned(),
            x,
            y,
            facing,
            go: GoStep::with(
                &nav,
                Destination::Tile {
                    map: String::new(),
                    x: 0,
                    y: 0,
                },
            ),
            nav,
            scene: SceneStep::new(Conversation::new(world, data, script, None, answers)),
            follow_scene: false,
            retries: 0,
        };
        step.go = GoStep::with(&step.nav, step.approach());
        step
    }

    /// Follows the scene after the conversation too ([`SceneStep`]).
    pub fn with_scene(mut self) -> Self {
        self.follow_scene = true;
        self
    }

    pub fn conversation(&self) -> &Conversation {
        &self.scene.conversation
    }

    /// Where to walk: next to the target, or on the one tile it is read
    /// from.
    fn approach(&self) -> Destination {
        match self.facing {
            Some(dir) => {
                let (dx, dy) = dir.delta();
                Destination::Tile {
                    map: self.map.clone(),
                    x: self.x - dx,
                    y: self.y - dy,
                }
            }
            None => Destination::Facing {
                map: self.map.clone(),
                x: self.x,
                y: self.y,
            },
        }
    }
}

impl ToolStep for TalkStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        match self.phase {
            Phase::Approach => {
                if self.retries > MAX_RETRIES {
                    return Decision::Fail(format!(
                        "{}#({}, {}) does not answer to A",
                        self.map, self.x, self.y
                    ));
                }
                match self.go.next(ctx) {
                    Decision::Done(_) => {
                        self.phase = if self.facing.is_some() {
                            Phase::Turn
                        } else {
                            Phase::Press
                        };
                        Decision::Wait("facing them".into())
                    }
                    d => d,
                }
            }
            Phase::Turn => {
                let dir = self.facing.unwrap_or(Direction::Up);
                // Toward a wall or a sign a press only turns the player.
                Decision::Act(
                    Action::new(
                        format!("face {dir:?}"),
                        vec![ControllerCommand::Press(direction_button(dir))],
                        Expectation::InputsDone,
                        30,
                    )
                    .timed(InputKind::Turn, 1),
                )
            }
            Phase::Press => Decision::Act(Action::new(
                "press A to talk",
                vec![ControllerCommand::Press(Button::A)],
                Expectation::DialogueOpen,
                TALK_TIMEOUT_FRAMES,
            )),
            Phase::Talking if self.follow_scene => self.scene.next(ctx),
            Phase::Talking => self.scene.conversation.next(ctx),
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        match self.phase {
            Phase::Approach => self.go.on_outcome(action, outcome, ctx),
            Phase::Turn => self.phase = Phase::Press,
            Phase::Press => match outcome {
                Outcome::Confirmed | Outcome::Interrupted => {
                    self.phase = Phase::Talking;
                    self.scene.happened = true;
                }
                _ => {
                    // Probably not facing it: approach again.
                    self.retries += 1;
                    self.phase = Phase::Approach;
                    self.go = GoStep::with(&self.nav, self.approach());
                }
            },
            Phase::Talking => self.scene.on_outcome(action, outcome, ctx),
        }
    }

    fn expects(&self) -> Expects {
        match self.phase {
            Phase::Approach | Phase::Turn => Expects::NONE,
            Phase::Press | Phase::Talking => Expects::DIALOGUE,
        }
    }
}

/// Talks to `object` of `map` and follows the conversation; the
/// conversation, for what it recognised.
pub fn talk(
    ctx: &mut ToolContext<'_>,
    map: &str,
    object: u32,
    answers: Vec<Answer>,
) -> Result<Conversation, ToolError> {
    let mut step = TalkStep::new(ctx, map, object, answers)?;
    ctx.drive(&mut step)?;
    let conversation = step.scene.conversation;
    if conversation.gained_item && vanishes_when_taken(&ctx.world, map, object) {
        ctx.gone.insert((map.to_owned(), object));
        ctx.emit(progress(
            "Talk",
            format!("{map}#{object} taken: its tile is free"),
        ))?;
    }
    finish(ctx, &conversation)?;
    Ok(conversation)
}

impl Tool for TalkTool {
    fn name(&self) -> &str {
        "Talk"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Talk { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Talk {
            map,
            object,
            answers,
        } = intent
        else {
            return ToolOutcome::failed("not a Talk");
        };
        talk(ctx, map, *object, answers.clone()).map(|_| ()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_gamedata::GameData;
    use pokebot_state::{
        GameState, Observation, Observed, PlayerPose, PoseObservation, ScreenState,
    };
    use pokebot_world::World;

    /// A sign read facing north only: walked to the tile below it, the
    /// player turns up, then presses A.
    #[test]
    fn a_sign_is_faced_from_its_side_then_read() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let (Ok(world), Ok(data)) = (World::load(&dir), GameData::load(dir.join("gamedata.json")))
        else {
            return;
        };
        let world = Arc::new(world);
        let nav = NavParts {
            world: Arc::clone(&world),
            gone: Default::default(),
            syncer: None,
            blocked: Default::default(),
            gates: Default::default(),
        };
        // The phases only: a sign at (11, 12), read from (11, 13).
        let (map, sign) = ("PalletTown", (11, 12));
        let mut step = TalkStep::toward_with(
            nav,
            Arc::new(data),
            map,
            sign,
            Some(Direction::Up),
            None,
            Vec::new(),
        );
        assert_eq!(
            step.approach(),
            Destination::Tile {
                map: map.into(),
                x: 11,
                y: 13
            }
        );
        let state = GameState::default();
        let mut o = Observation::bare(
            1,
            Observed {
                value: ScreenState::Overworld,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.player = Some(PoseObservation {
            pose: PlayerPose {
                map: map.into(),
                x: 11,
                y: 13,
            },
            score: 1000,
        });
        let mut events = Vec::new();
        let mut ctx = StepContext {
            observation: &o,
            state: &state,
            events: &mut events,
            quiet_frames: 500,
            frame: None,
            learned: &[],
        };
        // Already there: the walk is done.
        assert!(matches!(step.next(&mut ctx), Decision::Wait(_)));
        let turn = match step.next(&mut ctx) {
            Decision::Act(a) => a,
            _ => panic!("expected a turn"),
        };
        assert_eq!(turn.label, "face Up");
        step.on_outcome(&turn, Outcome::Confirmed, &mut ctx);
        match step.next(&mut ctx) {
            Decision::Act(a) => assert_eq!(a.label, "press A to talk"),
            _ => panic!("expected A"),
        }
    }
}
