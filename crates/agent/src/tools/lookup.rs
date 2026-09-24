//! Map lookups the tools share: where NPCs stand, which script they run,
//! the nearest Pokémon Center, and encounter tiles. Copied from
//! `StoryTask`'s private helpers (`nearest_nurse`, `grass_spot`,
//! `encounter_tile`, `vanishes_when_taken`); to be removed there once the
//! story runs on tools.

use std::collections::{HashMap, VecDeque};

use pokebot_world::behavior::TALL_GRASS;
use pokebot_world::World;

use crate::story::spin_tile;

pub const NURSE_GFX: &str = "OBJ_EVENT_GFX_NURSE";
pub const CLERK_GFX: &str = "OBJ_EVENT_GFX_CLERK";

/// The tile map object `object` of `map` stands on in the map data.
pub fn object_tile(world: &World, map: &str, object: u32) -> Option<(i32, i32)> {
    world
        .map(map)?
        .objects
        .iter()
        .find(|ob| ob.local_id == object)
        .and_then(|ob| Some((ob.x?, ob.y?)))
}

/// The compiled script label of map object `object` of `map`.
pub fn object_script(world: &World, map: &str, object: u32) -> Option<String> {
    world
        .events()?
        .objects
        .iter()
        .find(|o| o.map == map && o.local_id == object)
        .and_then(|o| o.script.clone())
}

/// The local id of the first object of `map` drawn with `graphics`.
pub fn object_with_graphics(world: &World, map: &str, graphics: &str) -> Option<u32> {
    world
        .map(map)?
        .objects
        .iter()
        .filter(|ob| ob.graphics.as_deref() == Some(graphics))
        .map(|ob| ob.local_id)
        .min()
}

/// Nearest Pokémon Center nurse from `from` (fewest maps crossed), as
/// (map, nurse local id).
pub fn nearest_nurse(world: &World, from: &str) -> Option<(String, u32)> {
    let mut dist = HashMap::from([(from.to_owned(), 0u32)]);
    let mut queue = VecDeque::from([from.to_owned()]);
    let mut best: Option<(u32, String)> = None;
    while let Some(name) = queue.pop_front() {
        let d = dist[&name];
        if name.ends_with("PokemonCenter_1F")
            && best.as_ref().is_none_or(|(bd, bn)| (d, &name) < (*bd, bn))
        {
            best = Some((d, name.clone()));
        }
        let Some(map) = world.map(&name) else {
            continue;
        };
        let next: Vec<String> = map
            .warps
            .iter()
            .filter(|w| w.dest_warp >= 0)
            .filter_map(|w| world.name_of(&w.dest_map))
            .chain(map.connections.iter().filter_map(|c| world.name_of(&c.map)))
            .map(str::to_owned)
            .collect();
        for n in next {
            if !dist.contains_key(&n) {
                dist.insert(n.clone(), d + 1);
                queue.push_back(n);
            }
        }
    }
    let (_, map) = best?;
    let nurse = object_with_graphics(world, &map, NURSE_GFX)?;
    Some((map, nurse))
}

/// A tile wild Pokémon appear on: tall grass, or cave floor (`MB_CAVE`/
/// `MB_SAND_CAVE`) with land encounters. Never ladders, warps or water.
pub fn encounter_tile(t: &pokebot_world::Tile) -> bool {
    const CAVE: u16 = 0x08;
    const SAND_CAVE: u16 = 0x2B;
    t.collision == 0
        && (t.behavior == TALL_GRASS
            || (t.encounter == 1 && matches!(t.behavior, 0x00 | CAVE | SAND_CAVE)))
}

/// Where to spin on `map` for encounters, near `near`.
pub fn grass_spot(world: &World, map: &str, near: (i32, i32)) -> Option<(i32, i32)> {
    let m = world.map(map)?;
    let grass = |x: i32, y: i32| m.tile(x, y).is_some_and(|t| encounter_tile(&t));
    spin_tile(&grass, m.width, m.height, near)
}

/// An item ball or fossil: gone from the map once its item is taken.
pub fn vanishes_when_taken(world: &World, map: &str, object: u32) -> bool {
    world
        .map(map)
        .and_then(|m| m.objects.iter().find(|o| o.local_id == object))
        .and_then(|o| o.graphics.as_deref())
        .is_some_and(|g| matches!(g, "OBJ_EVENT_GFX_ITEM_BALL" | "OBJ_EVENT_GFX_FOSSIL"))
}
