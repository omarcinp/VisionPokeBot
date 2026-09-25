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

/// How the grid is walked: tiles to avoid, and whether water is walkable
/// (Surf, once the route planner knows the party can use it).
#[derive(Debug, Clone, Copy)]
pub struct Walk<'a> {
    pub obstacles: &'a Obstacles,
    pub surf: bool,
    /// Tiles walkable despite their collision (a door a script opened).
    pub opened: Option<&'a Obstacles>,
}

/// The tile reached by pressing `dir` from `from`, if the move is legal.
pub fn step(
    map: &MapData,
    from: (i32, i32),
    dir: Direction,
    obstacles: &Obstacles,
) -> Option<Step> {
    step_with(
        map,
        from,
        dir,
        &Walk {
            obstacles,
            surf: false,
            opened: None,
        },
    )
}

/// [`step`] with the walking rules in `walk`.
pub fn step_with(map: &MapData, from: (i32, i32), dir: Direction, walk: &Walk) -> Option<Step> {
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
        return (landing.collision == 0 && !walk.obstacles.contains(&land))
            .then_some(Step { dir, to: land });
    }
    // Water sits one elevation below the shore; surfing on and off it is
    // the one elevation change the game allows.
    let shore = walk.surf && (is_water(here.behavior) || is_water(target.behavior));
    let opened = walk.opened.is_some_and(|o| o.contains(&to));
    let blocked = (target.collision != 0 && !opened)
        || target.behavior == COUNTER
        || (is_water(target.behavior) && !walk.surf)
        || walk.obstacles.contains(&to)
        || blocks_edge(here.behavior, dir)
        || blocks_edge(target.behavior, dir.opposite())
        || !(shore || walkable_elevation(here.elevation, target.elevation));
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
    find_path_with(
        map,
        start,
        &Walk {
            obstacles,
            surf: false,
            opened: None,
        },
        |_| 0,
        goal,
        heuristic,
    )
}

/// [`find_path`] with the walking rules in `walk` and an extra cost per tile
/// entered (`extra`, e.g. for NPC wander areas) on top of the grass penalty.
pub fn find_path_with(
    map: &MapData,
    start: (i32, i32),
    walk: &Walk,
    extra: impl Fn((i32, i32)) -> i32,
    goal: impl Fn((i32, i32)) -> bool,
    heuristic: impl Fn((i32, i32)) -> i32,
) -> Option<Vec<Step>> {
    let (reach, found) = search(map, start, walk, &extra, &goal, &heuristic);
    reach.path(found?)
}

/// Every tile reachable from a start tile, with the cheapest way there
/// (the same costs as [`find_path`]).
#[derive(Debug, Clone)]
pub struct Reach {
    start: (i32, i32),
    came: HashMap<(i32, i32), ((i32, i32), Step)>,
    cost: HashMap<(i32, i32), i32>,
}

impl Reach {
    /// Every tile reached.
    pub fn tiles(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        self.cost.keys().copied()
    }

    /// Weighted cost (tiles plus penalties) to `to`; `None` if unreachable.
    pub fn cost(&self, to: (i32, i32)) -> Option<i32> {
        self.cost.get(&to).copied()
    }

    /// The steps to `to`; empty for the start tile, `None` if unreachable.
    pub fn path(&self, to: (i32, i32)) -> Option<Vec<Step>> {
        if to != self.start && !self.came.contains_key(&to) {
            return None;
        }
        let mut steps = Vec::new();
        let mut at = to;
        while let Some((prev, s)) = self.came.get(&at) {
            steps.push(*s);
            at = *prev;
        }
        steps.reverse();
        Some(steps)
    }
}

/// Cheapest ways from `start` to every reachable tile (a flood with the
/// [`find_path`] costs, so one run prices a whole map's places).
pub fn reach(
    map: &MapData,
    start: (i32, i32),
    walk: &Walk,
    extra: impl Fn((i32, i32)) -> i32,
) -> Reach {
    search(map, start, walk, &extra, &|_| false, &|_| 0).0
}

fn search(
    map: &MapData,
    start: (i32, i32),
    walk: &Walk,
    extra: &dyn Fn((i32, i32)) -> i32,
    goal: &dyn Fn((i32, i32)) -> bool,
    heuristic: &dyn Fn((i32, i32)) -> i32,
) -> (Reach, Option<(i32, i32)>) {
    let mut open = BinaryHeap::new();
    let mut reach = Reach {
        start,
        came: HashMap::new(),
        cost: HashMap::new(),
    };
    let mut counter = 0u64;
    reach.cost.insert(start, 0);
    open.push(Reverse((heuristic(start), counter, start)));
    while let Some(Reverse((_, _, pos))) = open.pop() {
        if goal(pos) {
            return (reach, Some(pos));
        }
        let g = reach.cost[&pos];
        for dir in Direction::ALL {
            let Some(s) = step_with(map, pos, dir, walk) else {
                continue;
            };
            let grass = map
                .tile(s.to.0, s.to.1)
                .is_some_and(|t| t.behavior == crate::behavior::TALL_GRASS);
            let ng = g
                + (s.to.0 - pos.0).abs()
                + (s.to.1 - pos.1).abs()
                + if grass { GRASS_PENALTY } else { 0 }
                + extra(s.to);
            if reach.cost.get(&s.to).is_none_or(|&old| ng < old) {
                reach.cost.insert(s.to, ng);
                reach.came.insert(s.to, (pos, s));
                counter += 1;
                open.push(Reverse((ng + heuristic(s.to), counter, s.to)));
            }
        }
    }
    (reach, None)
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
