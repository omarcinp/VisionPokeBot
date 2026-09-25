//! One field medicine use, verified by fresh inventory and party reads.
use super::menu::{open_start_menu, pick_row, Closer, MenuRow, Retries, SCREEN_FRAMES};
use super::{Expects, Intent, ProbeFact, StepContext, ToolContext, ToolError, ToolStep};
use crate::{Action, Decision, Expectation, Outcome};
use pokebot_core::Button;
use pokebot_gamedata::GameData;
use pokebot_state::Pocket;
use std::sync::Arc;

pub fn use_item(ctx: &mut ToolContext<'_>, item: &str) -> Result<(), ToolError> {
    let count = |ctx: &ToolContext<'_>| {
        ctx.state()
            .bag
            .pockets
            .get(&Pocket::Items)
            .and_then(|k| k.value.as_ref())
            .and_then(|items| items.iter().find(|(name, _)| name == item))
            .map_or(0, |(_, n)| *n)
    };
    let before_count = count(ctx);
    let before_hp = ctx
        .state()
        .party
        .value
        .as_ref()
        .and_then(|p| p.first())
        .and_then(|m| m.hp.value)
        .map(|hp| hp.0)
        .ok_or_else(|| ToolError::Failed("medicine needs observed HP".into()))?;
    if before_count == 0 {
        return Err(ToolError::Failed(
            "medicine is not in audited inventory".into(),
        ));
    }
    let mut step = UseMedicine {
        item: item.to_owned(),
        data: Arc::clone(&ctx.data),
        retries: Retries::default(),
        closer: Closer::default(),
        applied: None,
    };
    ctx.drive(&mut step)?;
    ctx.invoke(&Intent::Probe {
        fact: ProbeFact::Pocket {
            pocket: Pocket::Items,
        },
    })
    .result?;
    ctx.invoke(&Intent::Probe {
        fact: ProbeFact::Party,
    })
    .result?;
    let improved = ctx
        .state()
        .party
        .value
        .as_ref()
        .and_then(|p| p.first())
        .and_then(|m| m.hp.value)
        .is_some_and(|hp| hp.0 > before_hp);
    if count(ctx) + 1 != before_count || !improved {
        return Err(ToolError::Failed(
            "medicine use was not verified by inventory and HP".into(),
        ));
    }
    Ok(())
}
struct UseMedicine {
    item: String,
    data: Arc<GameData>,
    retries: Retries,
    closer: Closer,
    applied: Option<u64>,
}
impl ToolStep for UseMedicine {
    fn expects(&self) -> Expects {
        Expects::MENUS
    }
    fn on_outcome(&mut self, _: &Action, outcome: Outcome, _: &mut StepContext<'_>) {
        if outcome != Outcome::Confirmed {
            self.retries.failed();
        }
    }
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if let Some(frame) = self.applied {
            if o.frame_id.saturating_sub(frame) < 120 {
                return Decision::Wait("waiting for medicine result".into());
            }
            return self.closer.next(o, "medicine input applied; verify menus");
        }
        if self.retries.exhausted() {
            return Decision::Fail("medicine menu failed".into());
        }
        if let Some(party) = &o.party_menu {
            if party.selected != Some(0) {
                return self.retries.act(
                    "select lead for medicine",
                    Button::Up,
                    Expectation::PartySelected(0),
                    SCREEN_FRAMES,
                );
            }
            self.applied = Some(o.frame_id);
            return self.retries.act(
                "use medicine on lead",
                Button::A,
                Expectation::InputsDone,
                SCREEN_FRAMES,
            );
        }
        if let Some(bag) = &o.bag {
            if crate::bag::pocket_from_title(&bag.pocket) != Some(Pocket::Items) {
                return self.retries.act(
                    "ITEMS pocket",
                    Button::Left,
                    Expectation::BagPocket("ITEMS".into()),
                    SCREEN_FRAMES,
                );
            }
            return crate::bag::select_item(o, &self.data, &self.item, Expectation::PartyList);
        }
        if let Some(menu) = &o.menu {
            return pick_row(
                &mut self.retries,
                o,
                menu,
                &MenuRow::Text("BAG"),
                ctx.state,
                Expectation::BagPocket(String::new()),
                SCREEN_FRAMES,
            )
            .0;
        }
        open_start_menu(&mut self.retries, o, ctx.quiet_frames)
    }
}
