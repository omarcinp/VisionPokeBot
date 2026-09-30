//! `Sell`: walk to the clerk of the nearest mart, talk, and sell through
//! the mart's SELL bag ([`crate::sell::Sale`]).

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;

use super::go::GoStep;
use super::lookup::object_tile;
use super::{
    progress, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome, ToolStep,
};
use crate::nav::Destination;
use crate::sell::Sale;
use crate::shop::nearest_clerk;
use crate::{Action, Decision, Expectation, Outcome};

pub struct SellTool;

enum Phase {
    Approach,
    Press,
    Selling,
}

struct SellStep {
    data: Arc<GameData>,
    phase: Phase,
    go: GoStep,
    sale: Sale,
}

impl ToolStep for SellStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        match self.phase {
            Phase::Approach => match self.go.next(ctx) {
                Decision::Done(_) => {
                    self.phase = Phase::Press;
                    Decision::Wait("facing the clerk".into())
                }
                d => d,
            },
            Phase::Press => Decision::Act(Action::new(
                "press A to talk",
                vec![ControllerCommand::Press(Button::A)],
                Expectation::DialogueOpen,
                45,
            )),
            Phase::Selling => self.sale.next(ctx.observation, &self.data, ctx.events),
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        match self.phase {
            Phase::Approach => self.go.on_outcome(action, outcome, ctx),
            Phase::Press => {
                if matches!(outcome, Outcome::Confirmed | Outcome::Interrupted) {
                    self.phase = Phase::Selling;
                } else {
                    self.phase = Phase::Approach;
                }
            }
            Phase::Selling => {}
        }
    }

    fn expects(&self) -> Expects {
        match self.phase {
            Phase::Approach => Expects::NONE,
            Phase::Press | Phase::Selling => Expects::MENUS,
        }
    }
}

/// Sells up to `count` of `item` at the nearest mart: fewer when fewer are
/// held or the money has no room for more. What was sold, and for how much.
pub fn sell(ctx: &mut ToolContext<'_>, item: &str, count: u32) -> Result<(u32, u32), ToolError> {
    let pose = ctx
        .pose()
        .ok_or_else(|| ToolError::Failed("player not located".into()))?;
    let (map, clerk) = nearest_clerk(&ctx.world, &ctx.data, &pose, &ctx.gone, |_| true)
        .ok_or_else(|| ToolError::Failed("no mart found".into()))?;
    let (x, y) = object_tile(&ctx.world, &map, clerk)
        .ok_or_else(|| ToolError::Failed(format!("{map} has no clerk {clerk}")))?;
    super::go::reach_map(ctx, &map)?;
    let mut step = SellStep {
        data: Arc::clone(&ctx.data),
        phase: Phase::Approach,
        go: GoStep::new(
            ctx,
            Destination::Facing {
                map: map.clone(),
                x,
                y,
            },
        ),
        sale: Sale::new(&ctx.data, item, count),
    };
    let summary = ctx.drive(&mut step)?;
    ctx.emit(progress("Mart", summary))?;
    Ok((step.sale.sold, step.sale.earned))
}

impl Tool for SellTool {
    fn name(&self) -> &str {
        "Sell"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Sell { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Sell { item, count } = intent else {
            return ToolOutcome::failed("not a Sell");
        };
        sell(ctx, item, *count).map(|_| ()).into()
    }
}
