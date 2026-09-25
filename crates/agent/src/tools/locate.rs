//! `ConfirmLocation`: the frame matches several maps that share a layout
//! (every Pokémon Center looks the same) and nothing in the state tells
//! them apart. One of them is taken as a working hypothesis (the layout is
//! shared, so walking on it is right whichever it is), and the player walks
//! out through the nearest exit that leads to a route or town, a different
//! one for every candidate: the map outside names itself, and the location
//! is confirmed there.

use std::collections::BTreeSet;

use pokebot_state::{GameEvent, PlayerPose};
use pokebot_world::World;

use super::go::GoStep;
use super::{
    progress, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome, ToolStep,
};
use crate::nav::Destination;
use crate::{Action, Decision, Outcome};

pub struct ConfirmLocationTool;

/// Map types a frame alone names (every route and town is unique).
const UNIQUE_TYPES: [&str; 2] = ["MAP_TYPE_ROUTE", "MAP_TYPE_TOWN"];

/// The warp to leave by: the one nearest the player whose destination is a
/// route or town, a different one for every candidate (a Center's door;
/// not its stairs, which lead to a 2F like every other).
pub fn exit_warp(world: &World, candidates: &[PlayerPose]) -> Option<usize> {
    let here = candidates.first()?;
    let first = world.map(&here.map)?;
    let dest = |map: &str, i: usize| -> Option<&str> {
        world.name_of(&world.map(map)?.warps.get(i)?.dest_map)
    };
    (0..first.warps.len())
        .filter(|&i| {
            let Some(dests) = candidates
                .iter()
                .map(|c| dest(&c.map, i))
                .collect::<Option<Vec<&str>>>()
            else {
                return false;
            };
            let unique = dests.iter().all(|d| {
                world
                    .map(d)
                    .and_then(|m| m.map_type.as_deref())
                    .is_some_and(|t| UNIQUE_TYPES.contains(&t))
            });
            unique && dests.iter().collect::<BTreeSet<_>>().len() == dests.len()
        })
        .min_by_key(|&i| {
            let w = &first.warps[i];
            ((w.x - here.x).abs() + (w.y - here.y).abs(), i)
        })
}

/// Walks the hypothesis's route to the exit, done as soon as a frame names
/// the map (the candidates are cleared).
struct LeaveStep {
    go: GoStep,
}

impl ToolStep for LeaveStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        if ctx.state.player.candidates.is_empty() {
            let at = ctx
                .state
                .player
                .pose
                .value
                .as_ref()
                .map_or("?".into(), |p| p.to_string());
            return Decision::Done(format!("location confirmed: {at}"));
        }
        self.go.next(ctx)
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        self.go.on_outcome(action, outcome, ctx);
    }

    fn expects(&self) -> Expects {
        self.go.expects()
    }
}

pub fn confirm(ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
    let candidates = ctx.state().player.candidates.clone();
    if candidates.is_empty() {
        return Ok(());
    }
    let warp = exit_warp(&ctx.world, &candidates).ok_or_else(|| {
        ToolError::Failed(format!(
            "no exit tells the {} lookalike maps apart",
            candidates.len()
        ))
    })?;
    // The state's own pick if it is one of them, else the first.
    let committed = ctx.state().player.pose.value.clone();
    let hypothesis = committed
        .and_then(|p| candidates.iter().find(|c| c.map == p.map).cloned())
        .unwrap_or_else(|| candidates[0].clone());
    ctx.runtime.set_pose_hint_inferred(hypothesis.clone());
    ctx.emit(GameEvent::PlayerInferred {
        pose: hypothesis.clone(),
        candidates: candidates.clone(),
    })?;
    ctx.emit(progress(
        "ConfirmLocation",
        format!(
            "one of {} lookalike maps ({}); leaving by warp {warp} to see which",
            candidates.len(),
            hypothesis.map
        ),
    ))?;
    let mut step = LeaveStep {
        go: GoStep::new(
            ctx,
            Destination::Warp {
                map: hypothesis.map,
                warp,
            },
        ),
    };
    let walked = ctx.drive(&mut step);
    if ctx.state().player.candidates.is_empty() {
        return Ok(());
    }
    walked?;
    Err(ToolError::Failed(
        "walked out, but no frame named the map yet".into(),
    ))
}

impl Tool for ConfirmLocationTool {
    fn name(&self) -> &str {
        "ConfirmLocation"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::ConfirmLocation)
    }

    fn run(&mut self, _intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        confirm(ctx).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> Option<World> {
        World::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world")).ok()
    }

    /// Every Center has the same door mats and stairs: the door leads to a
    /// different town from each, the stairs to a 2F as alike as the 1F.
    #[test]
    fn the_way_out_of_lookalike_centers_is_the_door() {
        let Some(world) = world() else { return };
        let at = |map: &str| PlayerPose {
            map: map.into(),
            x: 7,
            y: 4,
        };
        let centers = [
            at("PewterCity_PokemonCenter_1F"),
            at("ViridianCity_PokemonCenter_1F"),
            at("CeruleanCity_PokemonCenter_1F"),
        ];
        let warp = exit_warp(&world, &centers).expect("an exit");
        let map = world.map("PewterCity_PokemonCenter_1F").unwrap();
        let door = &map.warps[warp];
        assert_eq!(world.name_of(&door.dest_map), Some("PewterCity"));
        assert_eq!((door.x, door.y), (7, 8), "the mat under the player");
        // One candidate alone is no question.
        assert!(exit_warp(&world, &centers[..1]).is_some());
    }
}
