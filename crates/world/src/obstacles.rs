//! Tiles the map's own collision doesn't cover: objects that never move,
//! and the areas wandering NPCs may occupy. Pure map data, shared by the
//! navigator and the route planner.

use std::collections::BTreeSet;

use crate::path::Obstacles;
use crate::{MapData, ObjectEvent};

/// Objects known to be gone (picked-up item balls, defeated blockers...),
/// as `(map name, local id)`.
pub type Gone = BTreeSet<(String, u32)>;

/// Tiles blocked by objects that don't move: NPCs that only turn, cut trees,
/// boulders, item balls. Wanderers are learned when met.
pub fn static_obstacles(map: &MapData) -> Obstacles {
    object_obstacles(map, &Gone::new())
}

/// [`static_obstacles`] minus the objects in `gone`.
pub fn object_obstacles(map: &MapData, gone: &Gone) -> Obstacles {
    map.objects
        .iter()
        .filter(|o| {
            o.movement
                .as_deref()
                .is_some_and(|m| m.contains("FACE") || m.contains("LOOK_AROUND"))
        })
        .filter(|o| !gone.contains(&(map.name.clone(), o.local_id)))
        .filter_map(|o| Some((o.x?, o.y?)))
        .collect()
}

/// An NPC that walks around (`MOVEMENT_TYPE_WANDER_*`, `WALK_*`), so any
/// tile in its area may be blocked when the player gets there.
pub fn is_wanderer(o: &ObjectEvent) -> bool {
    o.movement
        .as_deref()
        .is_some_and(|m| m.contains("WANDER") || m.contains("WALK"))
}

/// Every in-bounds tile inside a wandering NPC's area.
pub fn wander_tiles(map: &MapData) -> BTreeSet<(i32, i32)> {
    let mut tiles = BTreeSet::new();
    for o in map.objects.iter().filter(|o| is_wanderer(o)) {
        let (x0, y0, x1, y1) = o.area();
        for y in y0.max(0)..=y1.min(map.height - 1) {
            for x in x0.max(0)..=x1.min(map.width - 1) {
                tiles.insert((x, y));
            }
        }
    }
    tiles
}
