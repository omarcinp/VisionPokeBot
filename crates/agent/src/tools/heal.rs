//! `Heal`: talk to the healer of a heal spot (the nurse of the given
//! Pokémon Center, Mom at home: whichever object on that map has a heal
//! effect in its script; or the nearest Center), answer YES, and make sure
//! the heal is on record: the healer always heals, so a missed line is
//! inferred, and the spot becomes the respawn point.

use pokebot_state::GameEvent;

use super::lookup::{healer_on, nearest_nurse};
use super::{progress, Answer, Intent, Tool, ToolContext, ToolError, ToolOutcome};

pub struct HealTool;

/// The healer to talk to for `center` (a map), or the nearest nurse.
pub fn nurse_for(ctx: &ToolContext<'_>, center: Option<&str>) -> Result<(String, u32), ToolError> {
    match center {
        Some(map) => healer_on(&ctx.world, map)
            .map(|id| (map.to_owned(), id))
            .ok_or_else(|| {
                ToolError::Failed(format!("{map} has no healer (nurse or heal script)"))
            }),
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
    if let Some(spot) = ctx.world.places().and_then(|p| {
        p.heal_spots
            .iter()
            .find(|h| h.respawn_map == map || h.map == map)
    }) {
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
