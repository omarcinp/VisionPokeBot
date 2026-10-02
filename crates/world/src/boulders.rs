//! Strength boulders: the pushes that take one onto a tile, the boulders
//! that can land on a floor switch, and the holes that drop one to the
//! floor below.
//!
//! A boulder pushed onto a hole (`MB_FALL_WARP`) falls: the game removes it
//! (its hide flag set) and clears its reveal flag, the template field the
//! decomp names `trainer_type` (`HandleBoulderFallThroughHole`,
//! `GetBoulderRevealFlagByLocalIdAndMap`), which shows its twin on the floor
//! below. No script does it, so the fall is added to the compiled events as
//! an object script of the boulder ([`hole_label`]).

use std::collections::{HashSet, VecDeque};

use pokebot_state::Direction;

use crate::behavior::{is_water, ledge};
use crate::events::{Effect, Events, Script, ScriptPath};
use crate::path::{reach, walkable_elevation, Obstacles, Walk};
use crate::{MapData, ObjectEvent};

pub const GRAPHICS: &str = "OBJ_EVENT_GFX_PUSHABLE_BOULDER";
/// `MB_FALL_WARP`: a hole the player (or a pushed boulder) drops through.
pub const FALL_WARP: u16 = 0x66;
/// Push searches stop after this many (boulder, player) states.
const MAX_STATES: usize = 200_000;

/// One search state: the boulder's tile, the player's, the pushes so far.
type PushState = ((i32, i32), (i32, i32), Vec<Direction>);

pub fn is_boulder(o: &ObjectEvent) -> bool {
    o.graphics.as_deref() == Some(GRAPHICS)
}

/// The flag a boulder clears when it falls through a hole (shows the
/// boulder it lands as on the floor below).
pub fn reveal_flag(o: &ObjectEvent) -> Option<&str> {
    is_boulder(o)
        .then_some(o.trainer_type.as_deref())
        .flatten()
        .filter(|f| f.starts_with("FLAG_"))
}

/// The object's own hide flag, `None` for an object always there.
pub fn hide_flag(o: &ObjectEvent) -> Option<&str> {
    o.flag.as_deref().filter(|f| f.starts_with("FLAG_"))
}

/// The label of the script pushing boulder `local_id` of `map` into a hole.
pub fn hole_label(map: &str, local_id: u32) -> String {
    format!("{map}_BoulderHole_{local_id}")
}

/// The boulder whose hole script `label` is: its map and local id.
pub fn hole_of(label: &str) -> Option<(&str, u32)> {
    let (map, id) = label.rsplit_once("_BoulderHole_")?;
    Some((map, id.parse().ok()?))
}

/// The holes of `map` a boulder can drop through.
pub fn holes(map: &MapData) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    for y in 0..map.height {
        for x in 0..map.width {
            if map.tile(x, y).is_some_and(|t| t.behavior == FALL_WARP) {
                out.push((x, y));
            }
        }
    }
    out
}

/// Adds to `events` a script for each boulder of `maps` that falls through
/// a hole of its map: it hides the boulder and clears its reveal flag.
pub fn add_hole_scripts<'a>(events: &mut Events, maps: impl Iterator<Item = &'a MapData>) {
    for m in maps {
        if holes(m).is_empty() {
            continue;
        }
        for o in &m.objects {
            let Some(reveal) = reveal_flag(o) else {
                continue;
            };
            let mut does = Vec::new();
            if let Some(own) = hide_flag(o) {
                does.push(Effect::Set {
                    set: own.to_owned(),
                });
            }
            does.push(Effect::Clear {
                clear: reveal.to_owned(),
            });
            events.scripts.insert(
                hole_label(&m.name, o.local_id),
                Script {
                    kind: "object".into(),
                    map: Some(m.name.clone()),
                    local_id: Some(o.local_id),
                    paths: vec![ScriptPath {
                        when: Vec::new(),
                        does,
                        opaque: Vec::new(),
                    }],
                    truncated: false,
                },
            );
        }
    }
}

/// The pushes taking one of `boulders` onto one of `targets` with the
/// player starting at `player`: the boulder pushed and the directions. Each
/// push is from the tile behind the boulder, which the player must reach
/// round the boulders and `blocked`; the player's step into the boulder
/// and the boulder's onward keep each to one elevation (the game stops a
/// step at an elevation mismatch before it tries the boulder).
pub fn pushes(
    map: &MapData,
    boulders: &[(i32, i32)],
    blocked: &Obstacles,
    player: (i32, i32),
    targets: &[(i32, i32)],
) -> Option<((i32, i32), Vec<Direction>)> {
    let level = |from: (i32, i32), to: (i32, i32)| {
        map.tile(from.0, from.1)
            .zip(map.tile(to.0, to.1))
            .is_some_and(|(a, b)| walkable_elevation(a.elevation, b.elevation))
    };
    for &start in boulders {
        let others: HashSet<(i32, i32)> =
            boulders.iter().copied().filter(|b| *b != start).collect();
        let floor = |t: (i32, i32)| {
            map.tile(t.0, t.1).is_some_and(|tile| {
                tile.collision == 0 && !is_water(tile.behavior) && ledge(tile.behavior).is_none()
            }) && !blocked.contains(&t)
                && !others.contains(&t)
        };
        let mut seen: HashSet<((i32, i32), (i32, i32))> = HashSet::from([(start, player)]);
        let mut queue: VecDeque<PushState> = VecDeque::from([(start, player, Vec::new())]);
        while let Some((b, p, done)) = queue.pop_front() {
            if targets.contains(&b) {
                return Some((start, done));
            }
            if seen.len() > MAX_STATES {
                break;
            }
            let mut obstacles: Obstacles = blocked.clone();
            obstacles.extend(others.iter().copied());
            obstacles.insert(b);
            let walk = Walk {
                obstacles: &obstacles,
                surf: false,
                opened: None,
            };
            let reached = reach(map, p, &walk, |_| 0);
            for dir in Direction::ALL {
                let (dx, dy) = dir.delta();
                let behind = (b.0 - dx, b.1 - dy);
                let next = (b.0 + dx, b.1 + dy);
                if (behind != p && reached.cost(behind).is_none())
                    || !floor(next)
                    || !level(behind, b)
                    || !level(b, next)
                {
                    continue;
                }
                if seen.insert((next, b)) {
                    let mut more = done.clone();
                    more.push(dir);
                    queue.push_back((next, b, more));
                }
            }
        }
    }
    None
}

/// The boulders of `map` that can be pushed onto `target` by a player
/// starting beside them (the map's other boulders where they spawn).
pub fn solvers(map: &MapData, target: (i32, i32)) -> Vec<&ObjectEvent> {
    let spawn = |o: &ObjectEvent| Some((o.x?, o.y?));
    let all: Vec<&ObjectEvent> = map.objects.iter().filter(|o| is_boulder(o)).collect();
    all.iter()
        .copied()
        .filter(|o| {
            let Some(at) = spawn(o) else {
                return false;
            };
            let blocked: Obstacles = all
                .iter()
                .filter(|b| b.local_id != o.local_id)
                .filter_map(|b| spawn(b))
                .collect();
            Direction::ALL.iter().any(|d| {
                let (dx, dy) = d.delta();
                let beside = (at.0 + dx, at.1 + dy);
                map.tile(beside.0, beside.1)
                    .is_some_and(|t| t.collision == 0 && !is_water(t.behavior))
                    && pushes(map, &[at], &blocked, beside, &[target]).is_some()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> Option<crate::World> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        crate::World::load(dir).ok()
    }

    /// Victory Road 2F: only the boulder hidden until its twin on 3F falls
    /// through the hole lands on the switch at (14, 19); the others sit
    /// above the platform's rim or past it.
    #[test]
    fn the_switch_below_the_platform_takes_the_boulder_from_above() {
        let Some(world) = world() else { return };
        let map = world.map("VictoryRoad_2F").unwrap();
        let ids: Vec<u32> = solvers(map, (14, 19)).iter().map(|o| o.local_id).collect();
        assert_eq!(ids, vec![12]);
        let hidden = map.objects.iter().find(|o| o.local_id == 12).unwrap();
        assert_eq!(hide_flag(hidden), Some("FLAG_HIDE_VICTORY_ROAD_2F_BOULDER"));
    }

    /// Victory Road 3F: the boulder at (33, 18) falling through the hole
    /// at (34, 18) shows the one on 2F; its script says so.
    #[test]
    fn a_boulder_falling_through_a_hole_reveals_its_twin() {
        let Some(world) = world() else { return };
        let map = world.map("VictoryRoad_3F").unwrap();
        assert!(holes(map).contains(&(34, 18)));
        let label = hole_label("VictoryRoad_3F", 8);
        assert_eq!(hole_of(&label), Some(("VictoryRoad_3F", 8)));
        let script = world.events().unwrap().script(&label).expect("hole script");
        assert_eq!(script.kind, "object");
        assert_eq!(
            script.paths[0].does,
            vec![
                Effect::Set {
                    set: "FLAG_HIDE_VICTORY_ROAD_3F_BOULDER".into()
                },
                Effect::Clear {
                    clear: "FLAG_HIDE_VICTORY_ROAD_2F_BOULDER".into()
                },
            ]
        );
        let (_, done) =
            pushes(map, &[(33, 18)], &Default::default(), (32, 18), &holes(map)).expect("pushes");
        assert_eq!(done, vec![Direction::Right]);
    }
}
