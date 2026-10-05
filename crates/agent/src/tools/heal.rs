//! `Heal`: talk to the healer of a heal spot (the nurse of the given
//! Pokémon Center, Mom at home: whichever object on that map has a heal
//! effect in its script; or a costed reachable healer). Answer YES, then
//! verify HP, status and PP through the party summaries.

use pokebot_state::GameEvent;

use super::lookup::healer_on;
use super::{progress, Answer, Intent, Tool, ToolContext, ToolError, ToolOutcome};

pub struct HealTool;

/// Whether a heal failed for want of any healer in reach (the Elite
/// Four's rooms lock behind the player): a battle that wanted it is
/// fought as the party stands.
pub fn no_healer_in_reach(e: &ToolError) -> bool {
    matches!(e, ToolError::Failed(why) if why.contains("no known safe route to"))
}

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

/// HP poison takes in the field: 1 every 4 steps (`DoPoisonFieldEffect`
/// every fourth step), and in Generation III it faints.
const STEPS_PER_POISON_HP: u32 = 4;
/// HP kept above what the walk's poison takes (a step or two more than
/// the route counts: a bump, a turn).
const POISON_MARGIN_HP: u32 = 2;

/// Steps the walk to `map` takes on foot (or surfing): its walk legs'
/// tiles, a step through each warp or edge.
fn steps_to(ctx: &mut ToolContext<'_>, map: &str) -> Option<u32> {
    let pose = ctx.pose()?;
    let graph = ctx
        .scheduler
        .graph
        .get_or_insert_with(|| crate::scheduler::graph(&ctx.world));
    let belief = crate::belief_view::StateBelief(ctx.runtime.state());
    let out = pokebot_world::route::route_to_map(
        &ctx.world,
        graph,
        &belief,
        &pose,
        map,
        pokebot_world::route::UnknownPolicy::Pessimistic,
    );
    if !out.found() {
        return None;
    }
    Some(walk_steps(&out.legs))
}

fn walk_steps(legs: &[pokebot_world::route::Leg]) -> u32 {
    use pokebot_world::route::EdgeKind;
    legs.iter()
        .map(|l| match l.kind {
            EdgeKind::Walk { tiles, .. } => tiles,
            EdgeKind::Fly | EdgeKind::Dig | EdgeKind::EscapeRope => 0,
            _ => 1,
        })
        .sum()
}

/// The medicine to use on a poisoned lead at `hp` before a walk of
/// `steps`, from `items` held: a cure first, else the smallest potion
/// that sees it through; `None` when it lasts the walk or nothing helps.
fn poison_medicine(hp: u16, steps: u32, items: &[(String, u16)]) -> Option<&'static str> {
    let taken = steps / STEPS_PER_POISON_HP + POISON_MARGIN_HP;
    if u32::from(hp) > taken {
        return None;
    }
    let held = |item: &str| items.iter().any(|(i, n)| i == item && *n > 0);
    for cure in ["ITEM_ANTIDOTE", "ITEM_FULL_HEAL", "ITEM_FULL_RESTORE"] {
        if held(cure) {
            return Some(cure);
        }
    }
    [
        ("ITEM_POTION", 20u32),
        ("ITEM_SUPER_POTION", 50),
        ("ITEM_HYPER_POTION", 200),
        ("ITEM_MAX_POTION", u32::MAX),
    ]
    .into_iter()
    .filter(|(item, _)| held(item))
    .find(|(_, heal)| u32::from(hp).saturating_add(*heal) > taken)
    .map(|(item, _)| item)
}

/// A poisoned lead that the walk to `map` would faint is cured (or its
/// HP raised) first. Fleet emu3: SQUIRTLE poisoned at 8/25 in Viridian
/// Forest walked for Viridian City's Center, and the poison fainted it on
/// Route 2; the party whited out.
fn cure_for_the_walk(ctx: &mut ToolContext<'_>, map: &str) -> Result<(), ToolError> {
    let Some((hp, poisoned)) = ctx
        .state()
        .party
        .value
        .as_ref()
        .and_then(|p| p.first())
        .map(|m| {
            (
                m.hp.value.map_or(0, |h| h.0),
                matches!(
                    m.status.value,
                    Some(pokebot_state::Status::Poisoned | pokebot_state::Status::BadlyPoisoned)
                ),
            )
        })
    else {
        return Ok(());
    };
    if !poisoned || hp == 0 {
        return Ok(());
    }
    let Some(steps) = steps_to(ctx, map) else {
        return Ok(());
    };
    let items: Vec<(String, u16)> = ctx
        .state()
        .bag
        .pockets
        .get(&pokebot_state::Pocket::Items)
        .and_then(|k| k.value.clone())
        .unwrap_or_default();
    let Some(item) = poison_medicine(hp, steps, &items) else {
        return Ok(());
    };
    ctx.emit(progress(
        "Heal",
        format!("poisoned at {hp} HP, {steps} steps to {map}: {item} first"),
    ))?;
    match super::medicine::use_item(ctx, item) {
        Err(e @ (ToolError::Stopped | ToolError::Device(_))) => Err(e),
        Err(e) => ctx.emit(progress(
            "Heal",
            format!("{item} not used: {e}; walking on"),
        )),
        Ok(()) => Ok(()),
    }
}

pub fn heal(ctx: &mut ToolContext<'_>, center: Option<&str>) -> Result<(), ToolError> {
    let (map, nurse) = nurse_for(ctx, center)?;
    cure_for_the_walk(ctx, &map)?;
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

    /// Fleet emu3: SQUIRTLE poisoned at 8/25 HP in Viridian Forest walked
    /// for Viridian's Center and fainted to the poison on Route 2. A walk
    /// the poison would end is preceded by a cure, else a potion that
    /// sees it through; a short one isn't.
    #[test]
    fn a_poisoned_lead_is_cured_before_a_walk_it_would_not_survive() {
        let items = |list: &[(&str, u16)]| -> Vec<(String, u16)> {
            list.iter().map(|(i, n)| (i.to_string(), *n)).collect()
        };
        let both = items(&[("ITEM_POTION", 2), ("ITEM_ANTIDOTE", 1)]);
        // 8 HP, 60 steps: 15 HP of poison.
        assert_eq!(poison_medicine(8, 60, &both), Some("ITEM_ANTIDOTE"));
        assert_eq!(
            poison_medicine(8, 60, &items(&[("ITEM_POTION", 2)])),
            Some("ITEM_POTION")
        );
        // A potion that doesn't see it through isn't the answer.
        assert_eq!(poison_medicine(8, 200, &items(&[("ITEM_POTION", 2)])), None);
        assert_eq!(
            poison_medicine(8, 200, &items(&[("ITEM_SUPER_POTION", 1)])),
            Some("ITEM_SUPER_POTION")
        );
        // A short walk: on to the nurse.
        assert_eq!(poison_medicine(20, 40, &both), None);
        assert_eq!(poison_medicine(8, 60, &items(&[("ITEM_POTION", 0)])), None);
    }

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
