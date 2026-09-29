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
    let restored = |ctx: &ToolContext<'_>| {
        ctx.state().party.value.as_ref().is_some_and(|party| {
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
        })
    };
    // The nurse's pages ran her script, whose heal restored every total
    // the belief holds. Only a total it doesn't hold needs the summaries
    // (live, Switch: every heal opened all six summaries to read what the
    // script had just set).
    if !restored(ctx) {
        ctx.invoke(&Intent::Probe {
            fact: super::ProbeFact::Party,
        })
        .result?;
    }
    if !restored(ctx) {
        return Err(ToolError::Failed(
            "healer conversation ended, but menu audit did not confirm full recovery".into(),
        ));
    }
    ctx.emit(GameEvent::Healed)?;
    ctx.emit(progress("Heal", "full party recovery verified"))?;
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
    restock_potions(ctx, &map)?;
    Ok(())
}

/// Whether two of a town's buildings: their maps share the town's name
/// (`PewterCity_Mart`, `PewterCity_PokemonCenter_1F`).
fn same_town(a: &str, b: &str) -> bool {
    let town = |m: &str| m.split('_').next().unwrap_or(m).to_owned();
    a != b && town(a) == town(b)
}

/// After a Center's heal, the Potions short of
/// [`crate::stock::POTIONS_KEPT`] are bought at the same town's mart (a
/// town's Center and mart share its name: `PewterCity_…`). Fleet worker 2
/// healed in Pewter, never bought balls (so never saw a mart), and met Mt.
/// Moon's trainers with no Potion: whited out on the way back. A failed
/// purchase doesn't fail the heal.
fn restock_potions(ctx: &mut ToolContext<'_>, center: &str) -> Result<(), ToolError> {
    use crate::stock::{potion_count, potions_to_buy};
    let money = ctx.state().money.value.unwrap_or(0);
    let count = potions_to_buy(&ctx.data, potion_count(ctx.state()), money);
    let Some(pose) = ctx.pose().filter(|_| count > 0) else {
        return Ok(());
    };
    let mart = crate::shop::nearest_mart(&ctx.world, &ctx.data, &pose, "ITEM_POTION", &ctx.gone);
    if !mart.is_some_and(|(mart, _)| same_town(&mart, center)) {
        return Ok(());
    }
    match super::buy::buy(ctx, "ITEM_POTION", count) {
        Ok(_) => Ok(()),
        Err(e @ (ToolError::Stopped | ToolError::Device(_))) => Err(e),
        Err(e) => ctx.emit(progress("Heal", format!("Potions not bought: {e}"))),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pewter's Center has Pewter's mart next door; Route 4's Center has
    /// none (the nearest, Cerulean's, is a trip).
    #[test]
    fn potions_are_bought_only_at_the_town_s_own_mart() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(world), Ok(data)) = (
            pokebot_world::World::load(root.join("data/world")),
            pokebot_gamedata::GameData::load(root.join("data/world/gamedata.json")),
        ) else {
            return;
        };
        let gone = crate::nav::Gone::new();
        for (center, pose, own) in [
            ("PewterCity_PokemonCenter_1F", (7, 4), true),
            ("Route4_PokemonCenter_1F", (7, 4), false),
        ] {
            let pose = pokebot_state::PlayerPose {
                map: center.into(),
                x: pose.0,
                y: pose.1,
            };
            let mart = crate::shop::nearest_mart(&world, &data, &pose, "ITEM_POTION", &gone);
            assert_eq!(
                mart.as_ref().is_some_and(|(m, _)| same_town(m, center)),
                own,
                "{center}: {mart:?}"
            );
        }
    }
}
