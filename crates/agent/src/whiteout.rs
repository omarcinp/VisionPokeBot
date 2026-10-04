//! After a white-out the game shows its pages ("RED scurried to a
//! POKéMON CENTER…") and the nurse heals the party; the step presses
//! through them and ends once the Center's overworld is quiet. A run
//! that goes on from a white-out instead of reloading waits for this
//! first (fleet continue-6: its next walk began on the white-out page,
//! failed "whited out", and the run ended for a reload after all).

use pokebot_core::{Button, ControllerCommand};
use pokebot_state::ScreenState;

use crate::tools::{Expects, StepContext, ToolStep, SETTLE_FRAMES};
use crate::{Action, Decision, Expectation};

/// Frames between presses of A on the white-out pages.
const PRESS_GAP: u64 = 40;

#[derive(Debug, Default)]
pub struct AfterWhiteOut {
    last_press: Option<u64>,
}

impl ToolStep for AfterWhiteOut {
    fn expects(&self) -> Expects {
        Expects::LOSING
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if o.screen.value == ScreenState::Whiteout {
            if self.last_press.is_some_and(|f| o.frame_id < f + PRESS_GAP) {
                return Decision::Wait("white-out: waiting before A".into());
            }
            self.last_press = Some(o.frame_id);
            return Decision::Act(Action::new(
                "white-out: next page",
                vec![ControllerCommand::Press(Button::A)],
                Expectation::InputsDone,
                10,
            ));
        }
        if o.dialogue.is_some() {
            return crate::new_game::advance_or_wait(o.dialogue.as_ref(), "the nurse");
        }
        let quiet = o.menu.is_none()
            && o.battle.is_none()
            && o.screen.value != ScreenState::Transition
            && ctx.quiet_frames >= SETTLE_FRAMES;
        if quiet {
            Decision::Done("back at the Center".into())
        } else {
            Decision::Wait("back to the Center".into())
        }
    }
}
