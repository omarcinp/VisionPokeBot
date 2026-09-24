//! Walking rules and A* over a map's tile grid.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use pokebot_state::Direction;

use crate::behavior::{blocks_edge, is_water, ledge, COUNTER};
use crate::MapData;

/// One move: press `dir`; the player ends on `to` (2 tiles away for ledges).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub dir: Direction,
    pub to: (i32, i32),
}

/// Tiles that must be avoided in addition to the map's own collision (NPCs,
/// tiles found blocked at runtime).
pub type Obstacles = HashSet<(i32, i32)>;

fn walkable_elevation(a: u8, b: u8) -> bool {
    let any = |e: u8| e == 0 || e == 15;
    any(a) || any(b) || a == b
}

/// The tile reached by pressing `dir` from `from`, if the move is legal.
pub fn step(
    map: &MapData,
    from: (i32, i32),
    dir: Direction,
    obstacles: &Obstacles,
) -> Option<Step> {
    let here = map.tile(from.0, from.1)?;
    let (dx, dy) = dir.delta();
    let to = (from.0 + dx, from.1 + dy);
    let target = map.tile(to.0, to.1)?;
    if let Some(jump) = ledge(target.behavior) {
        // Ledges are crossed only in their direction, landing one tile past.
        if jump != dir {
            return None;
        }
        let land = (to.0 + dx, to.1 + dy);
        let landing = map.tile(land.0, land.1)?;
        return (landing.collision == 0 && !obstacles.contains(&land))
            .then_some(Step { dir, to: land });
    }
    let blocked = target.collision != 0
        || target.behavior == COUNTER
        || is_water(target.behavior)
        || obstacles.contains(&to)
        || blocks_edge(here.behavior, dir)
        || blocks_edge(target.behavior, dir.opposite())
        || !walkable_elevation(here.elevation, target.elevation);
    (!blocked).then_some(Step { dir, to })
}

/// Extra cost of walking through tall grass (wild encounters).
pub const GRASS_PENALTY: i32 = 4;

/// Shortest path from `start` to any tile satisfying `goal`. Tall grass costs
/// extra, so detours that avoid it are preferred when they are short. Deterministic:
/// ties break by insertion order, and directions are tried Up, Down, Left,
/// Right.
pub fn find_path(
    map: &MapData,
    start: (i32, i32),
    obstacles: &Obstacles,
    goal: impl Fn((i32, i32)) -> bool,
    heuristic: impl Fn((i32, i32)) -> i32,
) -> Option<Vec<Step>> {
    let mut open = BinaryHeap::new();
    let mut came: HashMap<(i32, i32), ((i32, i32), Step)> = HashMap::new();
    let mut cost: HashMap<(i32, i32), i32> = HashMap::new();
    let mut counter = 0u64;
    cost.insert(start, 0);
    open.push(Reverse((heuristic(start), counter, start)));
    while let Some(Reverse((_, _, pos))) = open.pop() {
        if goal(pos) {
            let mut steps = Vec::new();
            let mut at = pos;
            while let Some((prev, s)) = came.get(&at) {
                steps.push(*s);
                at = *prev;
            }
            steps.reverse();
            return Some(steps);
        }
        let g = cost[&pos];
        for dir in Direction::ALL {
            let Some(s) = step(map, pos, dir, obstacles) else {
                continue;
            };
            let grass = map
                .tile(s.to.0, s.to.1)
                .is_some_and(|t| t.behavior == crate::behavior::TALL_GRASS);
            let ng = g
                + (s.to.0 - pos.0).abs()
                + (s.to.1 - pos.1).abs()
                + if grass { GRASS_PENALTY } else { 0 };
            if cost.get(&s.to).is_none_or(|&old| ng < old) {
                cost.insert(s.to, ng);
                came.insert(s.to, (pos, s));
                counter += 1;
                open.push(Reverse((ng + heuristic(s.to), counter, s.to)));
            }
        }
    }
    None
}

/// Path to a specific tile.
pub fn path_to(
    map: &MapData,
    start: (i32, i32),
    target: (i32, i32),
    obstacles: &Obstacles,
) -> Option<Vec<Step>> {
    find_path(
        map,
        start,
        obstacles,
        |p| p == target,
        |p| (p.0 - target.0).abs() + (p.1 - target.1).abs(),
    )
}
