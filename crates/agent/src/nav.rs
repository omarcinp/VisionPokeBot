//! Closed-loop walking: plan with A* on the world model, press one direction
//! per tile, and confirm every step by locating the player on screen.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_state::{Direction, Observation, PlayerPose};
use pokebot_world::behavior::{arrow_warp, stair_warp, COUNTER, WARP_DOOR};
use pokebot_world::path::{find_path, Obstacles, Step};
use pokebot_world::{MapData, World};
use serde::Serialize;

use crate::{Action, Expectation, Outcome};

/// Frames to wait for a step to show up on screen.
const STEP_TIMEOUT: u64 = 30;
/// Frames to wait for a warp's fade and the new map to be located.
const WARP_TIMEOUT: u64 = 180;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Destination {
    Tile {
        map: String,
        x: i32,
        y: i32,
    },
    /// Stand next to `(x, y)` facing it (to talk or interact).
    Facing {
        map: String,
        x: i32,
        y: i32,
    },
    /// Use warp number `warp` of `map` (door, stairs, exit mat).
    Warp {
        map: String,
        warp: usize,
    },
}

impl Destination {
    pub fn map(&self) -> &str {
        match self {
            Destination::Tile { map, .. }
            | Destination::Facing { map, .. }
            | Destination::Warp { map, .. } => map,
        }
    }
}

pub enum NavStatus {
    Act(Action),
    Arrived,
    Wait(String),
    Fail(String),
}

pub fn direction_button(dir: Direction) -> Button {
    match dir {
        Direction::Up => Button::Up,
        Direction::Down => Button::Down,
        Direction::Left => Button::Left,
        Direction::Right => Button::Right,
    }
}

pub struct Navigator {
    world: Arc<World>,
    pub destination: Destination,
    /// Tiles learned to be blocked at runtime (NPCs, scripts), per map.
    learned: HashMap<String, Obstacles>,
    /// Direction the player is believed to face (after a move or turn).
    facing: Option<Direction>,
    /// Taps in a row that did not move the player.
    stalled: u32,
    pending: Option<(PlayerPose, Direction, (i32, i32))>,
    /// First hop out of the current map toward the destination (cached per map).
    hop: Option<(String, Hop)>,
}

impl Navigator {
    pub fn new(world: Arc<World>, destination: Destination) -> Self {
        Self {
            world,
            destination,
            learned: HashMap::new(),
            facing: None,
            stalled: 0,
            pending: None,
            hop: None,
        }
    }

    pub fn next(&mut self, observation: &Observation) -> NavStatus {
        let Some(pose) = observation.player.as_ref().map(|p| p.pose.clone()) else {
            return NavStatus::Wait("locating the player".into());
        };
        let world = Arc::clone(&self.world);
        let Some(map) = world.map(&pose.map) else {
            return NavStatus::Fail(format!("unknown map {}", pose.map));
        };
        // A warp is done once we stand on the map it leads to.
        if let Destination::Warp { map: from, warp } = &self.destination {
            let dest = world
                .map(from)
                .and_then(|m| m.warps.get(*warp))
                .and_then(|w| world.name_of(&w.dest_map));
            if dest == Some(pose.map.as_str()) {
                return NavStatus::Arrived;
            }
        }
        if pose.map != self.destination.map() {
            let hop = match &self.hop {
                Some((map, hop)) if *map == pose.map => Some(*hop),
                _ => {
                    let hop = route_from(&world, &pose, self.destination.map())
                        .or_else(|| route_exit(&world, &pose.map, self.destination.map()));
                    self.hop = hop.map(|h| (pose.map.clone(), h));
                    hop
                }
            };
            return match hop {
                Some(Hop::Warp(warp)) => self.use_warp(map, &pose, warp),
                Some(Hop::Edge(dir)) => self.cross_edge(&world, map, &pose, dir),
                None => NavStatus::Fail(format!(
                    "no known route from {} to {}",
                    pose.map,
                    self.destination.map()
                )),
            };
        }
        match self.destination.clone() {
            Destination::Tile { x, y, .. } => {
                if (pose.x, pose.y) == (x, y) {
                    return NavStatus::Arrived;
                }
                self.walk(
                    map,
                    &pose,
                    |p| p == (x, y),
                    (x, y),
                    &format!("to ({x}, {y})"),
                )
            }
            Destination::Facing { x, y, .. } => {
                // Talk from an adjacent tile, or across a counter (the tile
                // between is a counter, e.g. Pokémon Center nurses, clerks).
                let spots: Vec<((i32, i32), Direction)> = Direction::ALL
                    .iter()
                    .flat_map(|&dir| {
                        let (dx, dy) = dir.delta();
                        let adjacent = ((x - dx, y - dy), dir);
                        let across = map
                            .tile(x - dx, y - dy)
                            .filter(|t| t.behavior == COUNTER)
                            .map(|_| ((x - 2 * dx, y - 2 * dy), dir));
                        std::iter::once(adjacent).chain(across)
                    })
                    .collect();
                if let Some((_, dir)) = spots.iter().find(|(p, _)| *p == (pose.x, pose.y)) {
                    if self.facing == Some(*dir) {
                        return NavStatus::Arrived;
                    }
                    self.pending = None;
                    self.facing = Some(*dir);
                    // Tapping toward an occupied tile only turns the player.
                    return NavStatus::Act(Action::new(
                        format!("face {dir:?}"),
                        vec![ControllerCommand::Press(direction_button(*dir))],
                        Expectation::InputsDone,
                        6,
                    ));
                }
                let goals: HashSet<(i32, i32)> = spots.iter().map(|(p, _)| *p).collect();
                self.walk(
                    map,
                    &pose,
                    |p| goals.contains(&p),
                    (x, y),
                    &format!("next to ({x}, {y})"),
                )
            }
            Destination::Warp { warp, .. } => self.use_warp(map, &pose, warp),
        }
    }

    pub fn on_outcome(&mut self, action: &Action, outcome: Outcome) {
        let Some((from, dir, target)) = self.pending.take() else {
            return;
        };
        match (outcome, &action.expect) {
            (Outcome::Confirmed, _) => {
                self.facing = Some(dir);
                self.stalled = 0;
            }
            (Outcome::TimedOut, Expectation::PlayerMovedFrom(_)) => {
                // First miss: the tap probably just turned the player.
                // Second miss: something is in the way; avoid that tile.
                self.facing = Some(dir);
                self.stalled += 1;
                if self.stalled >= 2 {
                    self.learned.entry(from.map).or_default().insert(target);
                    self.stalled = 0;
                }
            }
            _ => self.stalled += 1,
        }
    }

    /// How many taps in a row failed to move the player.
    pub fn stalled(&self) -> u32 {
        self.stalled
    }

    fn obstacles(&self, map: &MapData) -> Obstacles {
        let mut obstacles: Obstacles = self.learned.get(&map.name).cloned().unwrap_or_default();
        // Stationary NPCs block their tiles; wanderers are learned when met.
        for o in &map.objects {
            let still = o
                .movement
                .as_deref()
                .is_some_and(|m| m.contains("FACE") || m.contains("LOOK_AROUND"));
            if let (true, Some(x), Some(y)) = (still, o.x, o.y) {
                obstacles.insert((x, y));
            }
        }
        obstacles
    }

    fn walk(
        &mut self,
        map: &MapData,
        pose: &PlayerPose,
        goal: impl Fn((i32, i32)) -> bool,
        toward: (i32, i32),
        what: &str,
    ) -> NavStatus {
        let mut obstacles = self.obstacles(map);
        obstacles.remove(&(pose.x, pose.y));
        let heuristic = |p: (i32, i32)| (p.0 - toward.0).abs() + (p.1 - toward.1).abs();
        let Some(path) = find_path(map, (pose.x, pose.y), &obstacles, &goal, heuristic) else {
            // Learned blocks may be stale (a wandering NPC moved on).
            if self.learned.remove(&map.name).is_some() {
                return NavStatus::Wait(format!("no path {what}; forgetting learned obstacles"));
            }
            return NavStatus::Fail(format!("no path {what} on {}", map.name));
        };
        match path.first() {
            Some(step) => self.step(pose, *step, &format!("walk {what}: {:?}", step.dir)),
            None => NavStatus::Arrived,
        }
    }

    fn step(&mut self, pose: &PlayerPose, step: Step, label: &str) -> NavStatus {
        self.pending = Some((pose.clone(), step.dir, step.to));
        NavStatus::Act(Action::new(
            label.to_owned(),
            vec![ControllerCommand::Press(direction_button(step.dir))],
            Expectation::PlayerMovedFrom(pose.clone()),
            STEP_TIMEOUT,
        ))
    }

    /// Walk to a tile on the map's edge that continues into the neighbour,
    /// then step across.
    fn cross_edge(
        &mut self,
        world: &World,
        map: &MapData,
        pose: &PlayerPose,
        dir: Direction,
    ) -> NavStatus {
        let exits: HashSet<(i32, i32)> = world
            .crossings(map, dir)
            .into_iter()
            .map(|(a, _)| a)
            .collect();
        if exits.contains(&(pose.x, pose.y)) {
            self.pending = Some((pose.clone(), dir, (pose.x, pose.y)));
            return NavStatus::Act(Action::new(
                format!("cross into the next map ({dir:?})"),
                vec![ControllerCommand::Press(direction_button(dir))],
                Expectation::LeftMap(map.name.clone()),
                STEP_TIMEOUT * 2,
            ));
        }
        let (dx, dy) = dir.delta();
        let toward = (pose.x + dx * 100, pose.y + dy * 100);
        self.walk(
            map,
            pose,
            |p| exits.contains(&p),
            toward,
            &format!("to the {dir:?} edge"),
        )
    }

    /// Doors: stand below and push Up. Exit mats: stand on them and push
    /// their arrow. Stairs and other warps: walk onto them.
    fn use_warp(&mut self, map: &MapData, pose: &PlayerPose, index: usize) -> NavStatus {
        let Some(warp) = map.warps.get(index) else {
            return NavStatus::Fail(format!("{} has no warp {index}", map.name));
        };
        let (wx, wy) = (warp.x, warp.y);
        let tile = map.tile(wx, wy);
        let push = |dir: Direction| {
            NavStatus::Act(Action::new(
                format!("take warp {index} of {} ({dir:?})", map.name),
                vec![ControllerCommand::Press(direction_button(dir))],
                Expectation::LeftMap(map.name.clone()),
                WARP_TIMEOUT,
            ))
        };
        if let Some(dir) =
            tile.and_then(|t| arrow_warp(t.behavior).or_else(|| stair_warp(t.behavior)))
        {
            if (pose.x, pose.y) == (wx, wy) {
                self.pending = None;
                return push(dir);
            }
            return self.walk(
                map,
                pose,
                |p| p == (wx, wy),
                (wx, wy),
                &format!("to exit ({wx}, {wy})"),
            );
        }
        if tile.is_some_and(|t| t.behavior == WARP_DOOR || t.collision != 0) {
            if (pose.x, pose.y) == (wx, wy + 1) {
                self.pending = None;
                return push(Direction::Up);
            }
            return self.walk(
                map,
                pose,
                |p| p == (wx, wy + 1),
                (wx, wy + 1),
                &format!("to door ({wx}, {wy})"),
            );
        }
        if (pose.x, pose.y) == (wx, wy) {
            // A plain warp tile that didn't fire on arrival: push toward the
            // nearest map edge (exits sit on edges).
            self.pending = None;
            let edges = [
                (wy, Direction::Up),
                (map.height - 1 - wy, Direction::Down),
                (wx, Direction::Left),
                (map.width - 1 - wx, Direction::Right),
            ];
            let dir = edges
                .iter()
                .min_by_key(|(d, _)| *d)
                .map(|(_, dir)| *dir)
                .unwrap_or(Direction::Down);
            return push(dir);
        }
        let label = format!("to warp ({wx}, {wy})");
        let mut obstacles = self.obstacles(map);
        obstacles.remove(&(pose.x, pose.y));
        let heuristic = |p: (i32, i32)| (p.0 - wx).abs() + (p.1 - wy).abs();
        match find_path(
            map,
            (pose.x, pose.y),
            &obstacles,
            |p| p == (wx, wy),
            heuristic,
        ) {
            Some(path) if !path.is_empty() => {
                let step = path[0];
                self.pending = Some((pose.clone(), step.dir, step.to));
                let expect = if step.to == (wx, wy) {
                    Expectation::LeftMap(map.name.clone())
                } else {
                    Expectation::PlayerMovedFrom(pose.clone())
                };
                let timeout = if step.to == (wx, wy) {
                    WARP_TIMEOUT
                } else {
                    STEP_TIMEOUT
                };
                NavStatus::Act(Action::new(
                    format!("walk {label}: {:?}", step.dir),
                    vec![ControllerCommand::Press(direction_button(step.dir))],
                    expect,
                    timeout,
                ))
            }
            _ => NavStatus::Fail(format!("no path {label} on {}", map.name)),
        }
    }
}

fn warp_is_marked(map: &MapData, warp: &pokebot_world::Warp) -> bool {
    map.tile(warp.x, warp.y).is_some_and(|t| {
        arrow_warp(t.behavior).is_some()
            || stair_warp(t.behavior).is_some()
            || t.behavior == WARP_DOOR
            || t.collision != 0
    })
}

/// Tiles blocked by objects that don't move: NPCs that only turn, cut trees,
/// boulders, item balls. Wanderers are learned when met.
pub fn static_obstacles(map: &MapData) -> Obstacles {
    map.objects
        .iter()
        .filter(|o| {
            o.movement
                .as_deref()
                .is_some_and(|m| m.contains("FACE") || m.contains("LOOK_AROUND"))
        })
        .filter_map(|o| Some((o.x?, o.y?)))
        .collect()
}

/// How to leave a map toward another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hop {
    Warp(usize),
    Edge(Direction),
}

/// First hop out of the player's map toward `to`, searching tiles across maps
/// (so a map split by trees or ledges only offers exits reachable from where
/// the player stands). Warps land on their destination warp tile; doors are
/// entered from the tile below; edges continue into the neighbour.
pub fn route_from(world: &World, pose: &PlayerPose, to: &str) -> Option<Hop> {
    type Node = (String, i32, i32);
    let mut blocked: std::collections::HashMap<String, Obstacles> =
        std::collections::HashMap::new();
    let start: Node = (pose.map.clone(), pose.x, pose.y);
    let mut first: std::collections::HashMap<Node, Option<Hop>> =
        std::collections::HashMap::from([(start.clone(), None)]);
    let mut queue = VecDeque::from([start]);
    while let Some(node) = queue.pop_front() {
        let (name, x, y) = node.clone();
        let hop_here = first[&node];
        if name == to {
            return hop_here;
        }
        let Some(map) = world.map(&name) else {
            continue;
        };
        let mut next: Vec<(Node, Option<Hop>)> = Vec::new();
        // Walking within the map.
        let obstacles = blocked
            .entry(name.clone())
            .or_insert_with(|| static_obstacles(map));
        for dir in Direction::ALL {
            if let Some(s) = pokebot_world::path::step(map, (x, y), dir, obstacles) {
                next.push(((name.clone(), s.to.0, s.to.1), None));
            }
        }
        // Warps: standing on one (mats, stairs, plain) or below a door.
        for (i, w) in map.warps.iter().enumerate() {
            if w.dest_warp < 0 {
                continue;
            }
            let door = map
                .tile(w.x, w.y)
                .is_some_and(|t| t.behavior == WARP_DOOR || t.collision != 0);
            let usable = if door {
                (x, y) == (w.x, w.y + 1)
            } else {
                (x, y) == (w.x, w.y)
            };
            if usable {
                if let Some((dest, dx, dy)) = world.warp_destination(w) {
                    next.push(((dest.name.clone(), dx, dy), Some(Hop::Warp(i))));
                }
            }
        }
        // Map edges.
        for dir in Direction::ALL {
            for (a, b) in world.crossings(map, dir) {
                if a == (x, y) {
                    if let Some(other) = map
                        .connections
                        .iter()
                        .find(|c| c.direction() == Some(dir))
                        .and_then(|c| world.name_of(&c.map))
                    {
                        next.push(((other.to_owned(), b.0, b.1), Some(Hop::Edge(dir))));
                    }
                }
            }
        }
        for (n, hop) in next {
            if first.contains_key(&n) {
                continue;
            }
            // The first hop is fixed once the path leaves the start map.
            let inherited = if name == pose.map && n.0 == pose.map {
                hop_here
            } else {
                hop_here.or(hop)
            };
            first.insert(n.clone(), inherited);
            queue.push_back(n);
        }
    }
    None
}

/// First hop from `from` toward `to`: breadth-first over fixed warps and map
/// connections (deterministic: warps in order, then Up, Down, Left, Right).
pub fn route_exit(world: &World, from: &str, to: &str) -> Option<Hop> {
    let mut queue = VecDeque::from([(from.to_owned(), None::<Hop>)]);
    let mut seen = HashSet::from([from.to_owned()]);
    while let Some((name, first)) = queue.pop_front() {
        let map = world.map(&name)?;
        // Among warps to the same place, try ones whose tile says how to use
        // them (arrow mats, doors, stairs) first.
        let mut ordered: Vec<(usize, &pokebot_world::Warp)> = map
            .warps
            .iter()
            .enumerate()
            .filter(|(_, w)| w.dest_warp >= 0)
            .collect();
        ordered.sort_by_key(|(i, w)| (u8::from(!warp_is_marked(map, w)), *i));
        let warps = ordered
            .into_iter()
            .filter_map(|(i, w)| Some((Hop::Warp(i), world.name_of(&w.dest_map)?)));
        let edges = Direction::ALL.into_iter().filter_map(|d| {
            let conn = map.connections.iter().find(|c| c.direction() == Some(d))?;
            (!world.crossings(map, d).is_empty())
                .then_some((Hop::Edge(d), world.name_of(&conn.map)?))
        });
        for (hop, dest) in warps.chain(edges) {
            if !seen.insert(dest.to_owned()) {
                continue;
            }
            let first = first.or(Some(hop));
            if dest == to {
                return first;
            }
            queue.push_back((dest.to_owned(), first));
        }
    }
    None
}
