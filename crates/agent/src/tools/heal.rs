//! `Heal`: talk to the healer of a heal spot (the nurse of the given
//! Pokémon Center, Mom at home: whichever object on that map has a heal
//! effect in its script; or a costed reachable healer). Answer YES, then
//! verify HP, status and PP through the party summaries.

use pokebot_state::GameEvent;

use super::lookup::healer_on;
use super::{progress, Answer, Intent, Tool, ToolContext, ToolError, ToolOutcome};

pub struct HealTool;

/// The healer to talk to for `center` (a map), or the nearest nurse.
pub fn nurse_for(
    ctx: &mut ToolContext<'_>,
    center: Option<&str>,
) -> Result<(String, u32), ToolError> {
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
            let graph = ctx
                .scheduler
                .graph
                .get_or_insert_with(|| crate::scheduler::graph(&ctx.world));
            let mut choice = crate::scheduler::recovery(
                &ctx.world,
                graph,
                ctx.runtime.state(),
                &ctx.data,
                &pose,
                None,
                true,
            )
            .ok_or_else(|| ToolError::Failed("no known safe route to recovery".into()))?;
            // Heal specifically requests a healer. Medicines are selected
            // by the scheduler before invoking this tool.
            if choice.medicine.is_some() {
                let mut without_items = ctx.runtime.state().clone();
                without_items.bag.pockets.clear();
                choice = crate::scheduler::recovery(
                    &ctx.world,
                    graph,
                    &without_items,
                    &ctx.data,
                    &pose,
                    None,
                    true,
                )
                .ok_or_else(|| ToolError::Failed("no known safe route to a healer".into()))?;
            }
            let nurse = healer_on(&ctx.world, &choice.map)
                .ok_or_else(|| ToolError::Failed("selected healer disappeared".into()))?;
            Ok((choice.map, nurse))
        }
    }
}

pub fn heal(ctx: &mut ToolContext<'_>, center: Option<&str>) -> Result<(), ToolError> {
    let (map, nurse) = nurse_for(ctx, center)?;
    let outcome = ctx.invoke(&Intent::Talk {
        map: map.clone(),
        object: nurse,
        answers: vec![Answer::Yes],
    });
    outcome.result?;
    ctx.invoke(&Intent::Probe {
        fact: super::ProbeFact::Party,
    })
    .result?;
    let verified = ctx.state().party.value.as_ref().is_some_and(|party| {
        !party.is_empty()
            && party.iter().all(|mon| {
                mon.hp.value.is_some_and(|(hp, max)| hp == max && max > 0)
                    && mon.status.value == Some(pokebot_state::Status::Healthy)
                    && mon
                        .moves
                        .iter()
                        .flatten()
                        .all(|m| m.pp.value.is_some_and(|(pp, max)| pp == max))
            })
    });
    if !verified {
        return Err(ToolError::Failed(
            "healer conversation ended, but menu audit did not confirm full recovery".into(),
        ));
    }
    ctx.emit(GameEvent::Healed)?;
    ctx.emit(progress(
        "Heal",
        "full party recovery verified in summaries",
    ))?;
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
