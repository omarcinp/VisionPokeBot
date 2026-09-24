//! `Talk`: stand facing a map object, press A, and follow its conversation
//! as the object's compiled script (`Go(Facing)` → `Dialogue`).

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};

use super::dialogue::{finish, Conversation};
use super::go::{GoStep, NavParts};
use super::lookup::{object_script, object_tile, vanishes_when_taken};
use super::{
    progress, Answer, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep,
};
use crate::nav::Destination;
use crate::{Action, Decision, Expectation, Outcome};

/// Frames waiting for the conversation to open after A before the
/// approach is retried.
const TALK_TIMEOUT_FRAMES: u64 = 45;
/// Missed presses (no dialogue after A) before the talk fails.
const MAX_RETRIES: u32 = 3;

pub struct TalkTool;

enum Phase {
    Approach,
    Press,
    Talking,
}

/// Approach, press A, converse.
pub struct TalkStep {
    phase: Phase,
    map: String,
    x: i32,
    y: i32,
    nav: NavParts,
    go: GoStep,
    pub conversation: Conversation,
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
        let nav = NavParts::of(ctx);
        let facing = Destination::Facing {
            map: map.to_owned(),
            x,
            y,
        };
        Ok(Self {
            phase: Phase::Approach,
            map: map.to_owned(),
            x,
            y,
            go: GoStep::with(&nav, facing),
            nav,
            conversation: Conversation::new(
                Arc::clone(&ctx.world),
                Arc::clone(&ctx.data),
                script,
                None,
                answers,
            ),
            retries: 0,
        })
    }

    fn facing(&self) -> Destination {
        Destination::Facing {
            map: self.map.clone(),
            x: self.x,
            y: self.y,
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
                        self.phase = Phase::Press;
                        Decision::Wait("facing them".into())
                    }
                    d => d,
                }
            }
            Phase::Press => Decision::Act(Action::new(
                "press A to talk",
                vec![ControllerCommand::Press(Button::A)],
                Expectation::DialogueOpen,
                TALK_TIMEOUT_FRAMES,
            )),
            Phase::Talking => self.conversation.next(ctx),
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        match self.phase {
            Phase::Approach => self.go.on_outcome(action, outcome, ctx),
            Phase::Press => match outcome {
                Outcome::Confirmed | Outcome::Interrupted => self.phase = Phase::Talking,
                _ => {
                    // Probably not facing it: approach again.
                    self.retries += 1;
                    self.phase = Phase::Approach;
                    self.go = GoStep::with(&self.nav, self.facing());
                }
            },
            Phase::Talking => self.conversation.on_outcome(action, outcome, ctx),
        }
    }

    fn expects(&self) -> Expects {
        match self.phase {
            Phase::Approach => Expects::NONE,
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
    if step.conversation.gained_item && vanishes_when_taken(&ctx.world, map, object) {
        ctx.gone.insert((map.to_owned(), object));
        ctx.emit(progress(
            "Talk",
            format!("{map}#{object} taken: its tile is free"),
        ))?;
    }
    finish(ctx, &step.conversation)?;
    Ok(step.conversation)
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
