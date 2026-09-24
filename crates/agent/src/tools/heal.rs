//! `Heal`: talk to the nurse of a Pokémon Center (the given one or the
//! nearest), answer YES, and make sure the heal is on record: the nurse
//! always heals, so a missed line is inferred, and the Center becomes the
//! respawn point.

use pokebot_state::GameEvent;

use super::lookup::{nearest_nurse, object_with_graphics, NURSE_GFX};
use super::{progress, Answer, Intent, Tool, ToolContext, ToolError, ToolOutcome};

pub struct HealTool;

/// The nurse to talk to for `center` (a map), or the nearest one.
pub fn nurse_for(ctx: &ToolContext<'_>, center: Option<&str>) -> Result<(String, u32), ToolError> {
    match center {
        Some(map) => object_with_graphics(&ctx.world, map, NURSE_GFX)
            .map(|id| (map.to_owned(), id))
            .ok_or_else(|| ToolError::Failed(format!("{map} has no nurse"))),
        None => {
            let pose = ctx
                .pose()
                .ok_or_else(|| ToolError::Failed("player not located".into()))?;
            nearest_nurse(&ctx.world, &pose.map)
                .ok_or_else(|| ToolError::Failed("no Pokémon Center found".into()))
        }
    }
}

pub fn heal(ctx: &mut ToolContext<'_>, center: Option<&str>) -> Result<(), ToolError> {
    let (map, nurse) = nurse_for(ctx, center)?;
    let before = ctx
        .state()
        .party
        .value
        .as_ref()
        .map(|p| p.iter().map(|m| m.hp.value).collect::<Vec<_>>());
    let outcome = ctx.invoke(&Intent::Talk {
        map: map.clone(),
        object: nurse,
        answers: vec![Answer::Yes],
    });
    outcome.result?;
    if !outcome
        .learned
        .iter()
        .any(|e| matches!(e, GameEvent::Healed))
    {
        ctx.emit(GameEvent::Healed)?;
        ctx.emit(progress(
            "Heal",
            "heal inferred: the nurse's text was not read",
        ))?;
    }
    let _ = before;
    if let Some(spot) = ctx
        .world
        .places()
        .and_then(|p| p.heal_spots.iter().find(|h| h.map == map))
    {
        ctx.emit(GameEvent::RespawnSet {
            map: spot.map.clone(),
            x: spot.x,
            y: spot.y,
        })?;
    }
    Ok(())
}

impl Tool for HealTool {
    fn name(&self) -> &str {
        "Heal"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Heal { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Heal { center } = intent else {
            return ToolOutcome::failed("not a Heal");
        };
        heal(ctx, center.as_deref()).into()
    }
}
