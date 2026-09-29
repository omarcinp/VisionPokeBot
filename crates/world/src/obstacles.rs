//! Tiles the map's own collision doesn't cover: objects that never move,
//! and the areas wandering NPCs may occupy. Pure map data, shared by the
//! navigator and the route planner.

use std::collections::BTreeSet;

use crate::events::{Effect, Events};
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
        .filter(|o| is_stationary(o))
        .filter(|o| !gone.contains(&(map.name.clone(), o.local_id)))
        .filter_map(|o| Some((o.x?, o.y?)))
        .collect()
}

/// An object that stays on its tile (`MOVEMENT_TYPE_FACE_*`,
/// `LOOK_AROUND`): a wall until something removes it.
pub fn is_stationary(o: &ObjectEvent) -> bool {
    o.movement
        .as_deref()
        .is_some_and(|m| m.contains("FACE") || m.contains("LOOK_AROUND"))
}

/// A way past a stationary object's tile.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Passage {
    /// The object is hidden once `flag` is set (item balls picked up, story
    /// blockers that leave).
    Hidden { flag: String },
    /// A trainer: beaten, it steps aside. `trainer` is the id its script
    /// battles (`TRAINER_*`).
    Trainer { trainer: String },
}

/// A stationary object and the ways past its tile; none means a wall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub local_id: u32,
    pub x: i32,
    pub y: i32,
    pub passages: Vec<Passage>,
}

/// The map's stationary objects with how each can be passed. `FLAG_TEMP_*`
/// flags don't count (cut trees and boulders are reset on every map load;
/// the route planner models those as gates).
pub fn blockers(map: &MapData, events: Option<&Events>) -> Vec<Blocker> {
    map.objects
        .iter()
        .filter(|o| is_stationary(o))
        .filter_map(|o| {
            let (x, y) = (o.x?, o.y?);
            let mut passages = Vec::new();
            if let Some(flag) = o.flag.as_deref() {
                if flag != "0" && !flag.starts_with("FLAG_TEMP_") {
                    passages.push(Passage::Hidden {
                        flag: flag.to_string(),
                    });
                }
            }
            if let Some(trainer) = o.script.as_deref().and_then(|s| battled_by(events?, s)) {
                passages.push(Passage::Trainer { trainer });
            }
            Some(Blocker {
                local_id: o.local_id,
                x,
                y,
                passages,
            })
        })
        .collect()
}

/// The trainer id a script battles, if any.
fn battled_by(events: &Events, script: &str) -> Option<String> {
    events
        .script(script)?
        .paths
        .iter()
        .flat_map(|p| &p.does)
        .find_map(|e| match e {
            Effect::Battle { battle, .. } => Some(battle.clone()),
            _ => None,
        })
}

/// The directions an object faces by its movement type: the fixed ones its
/// name gives (`FACE_LEFT`, `FACE_DOWN_AND_UP`), every direction for one
/// that looks around, wanders or turns.
pub fn facings(movement: &str) -> Vec<(i32, i32)> {
    const ALL: [(&str, (i32, i32)); 4] = [
        ("UP", (0, -1)),
        ("DOWN", (0, 1)),
        ("LEFT", (-1, 0)),
        ("RIGHT", (1, 0)),
    ];
    let named: Vec<(i32, i32)> = movement
        .strip_prefix("MOVEMENT_TYPE_FACE_")
        .map(|rest| {
            ALL.iter()
                .filter(|(w, _)| rest.split('_').any(|p| p == *w))
                .map(|(_, d)| *d)
                .collect()
        })
        .unwrap_or_default();
    if named.is_empty() {
        ALL.iter().map(|(_, d)| *d).collect()
    } else {
        named
    }
}

/// The tiles from which trainer object `local_id` sees the player and
/// walks up to battle: along each way it faces, up to `sight` tiles, until
/// a tile that can't be walked. A trainer's battle can't be fled.
pub fn sight_line(map: &MapData, local_id: u32, sight: u8) -> Obstacles {
    let mut out = Obstacles::new();
    let Some(o) = map.objects.iter().find(|o| o.local_id == local_id) else {
        return out;
    };
    let (Some(x0), Some(y0)) = (o.x, o.y) else {
        return out;
    };
    for (dx, dy) in facings(o.movement.as_deref().unwrap_or("")) {
        for k in 1..=i32::from(sight) {
            let tile = (x0 + dx * k, y0 + dy * k);
            if map.tile(tile.0, tile.1).is_none_or(|t| t.collision != 0) {
                break;
            }
            out.insert(tile);
        }
    }
    out
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
