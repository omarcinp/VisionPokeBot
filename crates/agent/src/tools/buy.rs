//! `Buy`: walk to the clerk of the nearest mart selling the item, talk,
//! and run the purchase through the mart menus.

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;

use super::go::GoStep;
use super::lookup::object_tile;
use super::{
    progress, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome, ToolStep,
};
use crate::nav::Destination;
use crate::shop::{nearest_mart, Purchase};
use crate::stock::{ball_count, potion_count, potions_to_buy};
use crate::{Action, Decision, Expectation, Outcome};

pub struct BuyTool;

enum Phase {
    Approach,
    Press,
    Shopping,
}

struct BuyStep {
    data: Arc<GameData>,
    phase: Phase,
    go: GoStep,
    purchase: Purchase,
}

impl ToolStep for BuyStep {
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
            Phase::Shopping => self.purchase.next(ctx.observation, &self.data, ctx.events),
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        match self.phase {
            Phase::Approach => self.go.on_outcome(action, outcome, ctx),
            Phase::Press => {
                if matches!(outcome, Outcome::Confirmed | Outcome::Interrupted) {
                    self.phase = Phase::Shopping;
                } else {
                    self.phase = Phase::Approach;
                }
            }
            Phase::Shopping => {}
        }
    }

    fn expects(&self) -> Expects {
        match self.phase {
            Phase::Approach => Expects::NONE,
            Phase::Press | Phase::Shopping => Expects::MENUS,
        }
    }
}

pub fn buy(ctx: &mut ToolContext<'_>, item: &str, count: u16) -> Result<String, ToolError> {
    let pose = ctx
        .pose()
        .ok_or_else(|| ToolError::Failed("player not located".into()))?;
    let (map, clerk) = nearest_mart(&ctx.world, &ctx.data, &pose, item, &ctx.gone)
        .ok_or_else(|| ToolError::Failed(format!("no mart selling {item} found")))?;
    let (x, y) = object_tile(&ctx.world, &map, clerk)
        .ok_or_else(|| ToolError::Failed(format!("{map} has no clerk {clerk}")))?;
    super::go::reach_map(ctx, &map)?;
    let stock = ball_count(ctx.state());
    let mut step = BuyStep {
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
        purchase: Purchase::new(item, count).with_stock(stock),
    };
    let summary = ctx.drive(&mut step)?;
    ctx.emit(progress("Mart", summary.clone()))?;
    top_up_potions(ctx, &map, clerk, (x, y))?;
    Ok(summary)
}

/// At the counter anyway: the Potions the money is kept for, when this
/// mart sells them and fewer than [`crate::stock::POTIONS_KEPT`] are held.
fn top_up_potions(
    ctx: &mut ToolContext<'_>,
    map: &str,
    clerk: u32,
    (x, y): (i32, i32),
) -> Result<(), ToolError> {
    let Some(pose) = ctx.pose() else {
        return Ok(());
    };
    let sells = nearest_mart(&ctx.world, &ctx.data, &pose, "ITEM_POTION", &ctx.gone)
        .is_some_and(|(m, c)| m == map && c == clerk);
    let money = ctx.state().money.value.unwrap_or(0);
    let count = potions_to_buy(&ctx.data, potion_count(ctx.state()), money);
    if !sells || count == 0 {
        return Ok(());
    }
    let mut step = BuyStep {
        data: Arc::clone(&ctx.data),
        phase: Phase::Approach,
        go: GoStep::new(
            ctx,
            Destination::Facing {
                map: map.to_owned(),
                x,
                y,
            },
        ),
        purchase: Purchase::new("ITEM_POTION", count),
    };
    let summary = ctx.drive(&mut step)?;
    ctx.emit(progress("Mart", summary))
}

impl Tool for BuyTool {
    fn name(&self) -> &str {
        "Buy"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Buy { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Buy { item, count } = intent else {
            return ToolOutcome::failed("not a Buy");
        };
        buy(ctx, item, *count).map(|_| ()).into()
    }
}
