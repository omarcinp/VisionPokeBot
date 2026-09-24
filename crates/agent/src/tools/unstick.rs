//! `Unstick` (spec §7.4): the way out of a screen no tool expected. A
//! conversation is followed with recognition (its script and effects are
//! learned when the text is known); a bare menu is closed with B; the
//! Pokédex page after a catch is dismissed. Nothing on screen: done.

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};

use super::dialogue::{finish, Conversation};
use super::{Expects, Intent, StepContext, Tool, ToolContext, ToolOutcome, ToolStep};
use crate::{Action, Decision, Expectation};

pub struct UnstickTool;

/// A conversation without a known script, plus the screens around it.
struct UnstickStep {
    conversation: Conversation,
}

impl ToolStep for UnstickStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if o.pokedex_page {
            return Decision::Act(Action::new(
                "close the Pokédex page",
                vec![ControllerCommand::Press(Button::A)],
                Expectation::PokedexPageClosed,
                120,
            ));
        }
        if o.dialogue.is_none() && o.menu.is_none() {
            if o.bag.is_some() || o.shop.is_some() {
                return Decision::Act(Action::new(
                    "leave the screen (B)",
                    vec![ControllerCommand::Press(Button::B)],
                    Expectation::BagClosed,
                    90,
                ));
            }
            if !self.conversation_started() {
                return Decision::Done("nothing to unstick".into());
            }
        }
        self.conversation.next(ctx)
    }

    fn on_outcome(&mut self, action: &Action, outcome: crate::Outcome, ctx: &mut StepContext<'_>) {
        self.conversation.on_outcome(action, outcome, ctx);
    }

    fn expects(&self) -> Expects {
        Expects::DIALOGUE
    }
}

impl UnstickStep {
    fn conversation_started(&self) -> bool {
        !self.conversation.pages.is_empty()
    }
}

impl Tool for UnstickTool {
    fn name(&self) -> &str {
        "Unstick"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Unstick)
    }

    fn run(&mut self, _intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let mut step = UnstickStep {
            conversation: Conversation::new(
                Arc::clone(&ctx.world),
                Arc::clone(&ctx.data),
                None,
                None,
                Vec::new(),
            ),
        };
        let result = ctx
            .drive(&mut step)
            .and_then(|_| finish(ctx, &step.conversation));
        result.into()
    }
}
