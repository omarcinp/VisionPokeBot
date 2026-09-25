//! Closed-loop walking: plan with A* on the world model, hold a direction
//! along straight runs (tap single tiles), and confirm by locating the player
//! on screen. Holds are cancelled as soon as something interrupts the walk.
//! Hold lengths and timeouts come from the device's timing model
//! ([`Syncer`]); the steps themselves from the [`Walker`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pokebot_core::{Button, ControllerCommand};
use pokebot_state::{Direction, Observation, PlayerPose};
use pokebot_world::behavior::{arrow_warp, stair_warp, COUNTER, WARP_DOOR};
use pokebot_world::path::{find_path, Obstacles, Step};
use pokebot_world::{MapData, World};
use serde::Serialize;

use crate::motion::{InputKind, Syncer, SyncerHandle, WalkStep, Walker, TILE_MS};
use crate::{Action, Expectation, Outcome};
/// Times per map the learned obstacles may be forgotten to retry a path.
/// In MtMoon_B2F's bottom corridor the view is the same for x = 21..28, so
/// each unseen step right is learned as a block and forgotten again (about
/// one forget per tile, live): 3 stranded the walk to the ladder; 8 lets it
/// cross the ambiguous stretch and still bounds a real unmodelled block.
const MAX_FORGETS: u32 = 8;
/// Longest straight run walked with one hold.
const MAX_RUN: usize = 8;
/// How long a tile learnt to be blocked is remembered when the map is not
/// left in between (a wandering NPC moves on; a trainer who walked up to
/// the player stays until the map is re-entered).
const BLOCK_TTL: Duration = Duration::from_secs(15 * 60);

/// Tiles learnt to be blocked while walking (an NPC met by bumping), kept
/// for the session and shared by every leg: a replanned `Go` routes around
/// them instead of discovering them again (flash-6: Youngster Josh, who
/// had walked up to the player, was learnt twice and the leg failed both
/// times). A map's tiles are dropped when the map is re-entered (NPCs are
/// back at their data positions) and after [`BLOCK_TTL`].
#[derive(Debug, Default)]
pub struct BlockedTiles {
    tiles: HashMap<String, HashMap<(i32, i32), Instant>>,
}

/// A session's [`BlockedTiles`], shared by the legs that learn and use them.
pub type Blocked = Arc<Mutex<BlockedTiles>>;

impl BlockedTiles {
    pub fn insert(&mut self, map: &str, tile: (i32, i32)) {
        self.tiles
            .entry(map.to_owned())
            .or_default()
            .insert(tile, Instant::now());
    }

    /// The tiles still believed blocked on `map`.
    pub fn on_map(&mut self, map: &str) -> Obstacles {
        let Some(tiles) = self.tiles.get_mut(map) else {
            return Obstacles::new();
        };
        tiles.retain(|_, since| since.elapsed() < BLOCK_TTL);
        tiles.keys().copied().collect()
    }

    /// Whether anything is believed blocked on `map`.
    pub fn any_on(&mut self, map: &str) -> bool {
        !self.on_map(map).is_empty()
    }

    /// Drops what was learnt on `map` (stale, or the map was re-entered).
    pub fn forget(&mut self, map: &str) {
        self.tiles.remove(map);
    }

    /// Every learnt tile, by map (for reports).
    pub fn all(&self) -> BTreeMap<String, Vec<(i32, i32)>> {
        self.tiles
            .iter()
            .filter(|(_, t)| !t.is_empty())
            .map(|(m, t)| {
                let mut v: Vec<_> = t.keys().copied().collect();
                v.sort_unstable();
                (m.clone(), v)
            })
            .collect()
    }
}

/// Tiles to walk with one hold from the start of `path`: the run of plain
/// one-tile steps in the first direction, excluding the path's last step
/// (arriving at a warp, door or edge stays a tap). 1 means "just tap".
pub fn straight_run(from: (i32, i32), path: &[Step]) -> usize {
    let Some(first) = path.first() else {
        return 0;
    };
    let mut prev = from;
    let mut run = 0;
    for step in &path[..path.len() - 1] {
        let (dx, dy) = step.dir.delta();
        let plain = (prev.0 + dx, prev.1 + dy) == step.to;
        if step.dir != first.dir || !plain || run == MAX_RUN {
            break;
        }
        prev = step.to;
        run += 1;
    }
    run.max(1)
}

/// How long to hold a direction to walk `tiles` tiles before the timing
/// model has learned anything: release inside the last tile, which the
/// game then finishes ([`Syncer::hold_for`] with the defaults).
pub fn run_hold(tiles: usize) -> std::time::Duration {
    std::time::Duration::from_millis(TILE_MS * tiles as u64 - TILE_MS / 2)
}

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
    /// Tiles learned to be blocked at runtime (NPCs, scripts), per map;
    /// the session's store when given ([`Navigator::with_blocked`]).
    learned: Blocked,
    /// Times the learned tiles were forgotten, per map.
    forgets: HashMap<String, u32>,
    /// Direction the player is believed to face (after a move or turn);
    /// unknown again after a map change (warps set it).
    facing: Option<Direction>,
    /// The map the player was last located on.
    last_map: Option<String>,
    /// Holds and taps along the current path.
    walker: Walker,
    /// The device's timing model (shared with the executor, which feeds it).
    syncer: SyncerHandle,
    /// First hop out of the current map toward the destination, or None to
    /// walk on this map (planned on entering each map).
    hop: Option<(String, Option<Hop>)>,
    /// Map objects no longer there (items and fossils taken), by map and
    /// local id: they don't block their tiles.
    gone: Gone,
}

/// Map objects known to be gone, as (map, local id).
pub type Gone = BTreeSet<(String, u32)>;

impl Navigator {
    pub fn new(world: Arc<World>, destination: Destination) -> Self {
        Self {
            world,
            destination,
            learned: Blocked::default(),
            forgets: HashMap::new(),
            facing: None,
            last_map: None,
            walker: Walker::new(),
            syncer: Arc::new(Mutex::new(Syncer::new("emulator"))),
            hop: None,
            gone: Gone::new(),
        }
    }

    /// Hold lengths and timeouts from this timing model instead of the
    /// defaults.
    pub fn with_syncer(mut self, syncer: SyncerHandle) -> Self {
        self.syncer = syncer;
        self
    }

    /// A snapshot of the timing model.
    fn sync(&self) -> Syncer {
        self.syncer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Objects known to be gone (taken items, fossils) don't block the way.
    pub fn with_gone(mut self, gone: Gone) -> Self {
        self.gone = gone;
        self
    }

    /// Learns blocked tiles into (and routes around those in) the
    /// session's store instead of a private one.
    pub fn with_blocked(mut self, blocked: Blocked) -> Self {
        self.learned = blocked;
        self
    }

    /// The direction the player is believed to face.
    pub fn facing(&self) -> Option<Direction> {
        self.facing
    }

    fn learned(&self) -> std::sync::MutexGuard<'_, BlockedTiles> {
        self.learned.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn next(&mut self, observation: &Observation) -> NavStatus {
        let Some(pose) = observation.player.as_ref().map(|p| p.pose.clone()) else {
            return NavStatus::Wait("locating the player".into());
        };
        if self.last_map.as_deref() != Some(pose.map.as_str()) {
            // A warp or an edge sets the facing; don't trust the old one.
            if self.last_map.is_some() {
                self.facing = None;
            }
            self.last_map = Some(pose.map.clone());
        }
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
        let hop = match &self.hop {
            Some((map, hop)) if *map == pose.map => *hop,
            _ => {
                let hop = plan_hop(&world, &pose, &self.destination, &self.gone);
                self.hop = Some((pose.map.clone(), hop));
                hop
            }
        };
        match hop {
            Some(Hop::Warp(warp)) => return self.use_warp(observation, map, &pose, warp),
            Some(Hop::Edge(dir)) => return self.cross_edge(observation, &world, map, &pose, dir),
            None if pose.map != self.destination.map() => {
                return NavStatus::Fail(format!(
                    "no known route from {} to {}",
                    pose.map,
                    self.destination.map()
                ))
            }
            None => {}
        }
        match self.destination.clone() {
            Destination::Tile { x, y, .. } => {
                if (pose.x, pose.y) == (x, y) {
                    return NavStatus::Arrived;
                }
                self.walk(
                    observation,
                    map,
                    |p| p == (x, y),
                    (x, y),
                    &format!("to ({x}, {y})"),
                )
            }
            Destination::Facing { x, y, .. } => {
                // Talk from an adjacent tile, or across a counter (the tile
                // between is a counter, e.g. Pokémon Center nurses, clerks).
                let spots = facing_spots(map, x, y);
                if let Some((_, dir)) = spots.iter().find(|(p, _)| *p == (pose.x, pose.y)) {
                    if self.facing == Some(*dir) {
                        return NavStatus::Arrived;
                    }
                    self.walker.clear_pending();
                    self.facing = Some(*dir);
                    // Tapping toward an occupied tile only turns the player.
                    return NavStatus::Act(
                        Action::new(
                            format!("face {dir:?}"),
                            vec![ControllerCommand::Press(direction_button(*dir))],
                            Expectation::InputsDone,
                            self.sync().timeout_frames(InputKind::Turn, 1),
                        )
                        .timed(InputKind::Turn, 1),
                    );
                }
                let goals: HashSet<(i32, i32)> = spots.iter().map(|(p, _)| *p).collect();
                self.walk(
                    observation,
                    map,
                    |p| goals.contains(&p),
                    (x, y),
                    &format!("next to ({x}, {y})"),
                )
            }
            Destination::Warp { warp, .. } => self.use_warp(observation, map, &pose, warp),
        }
    }

    /// Books the executor's verdict, seen on `observation` (the frame the
    /// action ended on); the tile learnt to be blocked, if any.
    pub fn on_outcome(
        &mut self,
        action: &Action,
        outcome: Outcome,
        observation: &Observation,
    ) -> Option<(String, (i32, i32))> {
        if action
            .timing
            .is_some_and(|(kind, _)| kind == InputKind::Turn)
        {
            // A turn in place: the walker has nothing in flight.
            return None;
        }
        // A tap that timed out into a battle's fade, a dialogue or a menu
        // was interrupted, not blocked (flash-7: a wild encounter on
        // MtMoon_1F (20, 25) learnt the tile the player had just stepped
        // onto).
        let busy = observation.dialogue.is_some()
            || observation.battle.is_some()
            || observation.menu.is_some()
            || observation.screen.value == pokebot_state::ScreenState::Transition;
        let outcome = if outcome == Outcome::TimedOut && busy {
            Outcome::Interrupted
        } else {
            outcome
        };
        let done = self.walker.on_outcome(action, outcome)?;
        if done.faced {
            self.facing = Some(done.dir);
        }
        if let Some((map, tile)) = &done.blocked {
            self.learned().insert(map, *tile);
        }
        done.blocked
    }

    /// Drops the obstacles learned on `map`, at most [`MAX_FORGETS`] times
    /// per map (a real block the world model lacks would otherwise be
    /// learned and forgotten forever). Whether anything was forgotten.
    fn forget_learned(&mut self, map: &str) -> bool {
        let any = self.learned().any_on(map);
        let count = self.forgets.entry(map.to_owned()).or_insert(0);
        if *count >= MAX_FORGETS || !any {
            return false;
        }
        *count += 1;
        self.learned().forget(map);
        true
    }

    /// How many taps in a row failed to move the player.
    pub fn stalled(&self) -> u32 {
        self.walker.stalled()
    }

    fn obstacles(&self, map: &MapData) -> Obstacles {
        let mut obstacles = object_obstacles(map, &self.gone);
        obstacles.extend(self.learned().on_map(&map.name));
        obstacles
    }

    fn walk(
        &mut self,
        observation: &Observation,
        map: &MapData,
        goal: impl Fn((i32, i32)) -> bool,
        toward: (i32, i32),
        what: &str,
    ) -> NavStatus {
        let Some(pose) = observation.player.as_ref().map(|p| p.pose.clone()) else {
            return NavStatus::Wait("locating the player".into());
        };
        let mut obstacles = self.obstacles(map);
        obstacles.remove(&(pose.x, pose.y));
        let heuristic = |p: (i32, i32)| (p.0 - toward.0).abs() + (p.1 - toward.1).abs();
        let Some(path) = find_path(map, (pose.x, pose.y), &obstacles, &goal, heuristic) else {
            // Learned blocks may be stale (a wandering NPC moved on).
            if self.forget_learned(&map.name) {
                return NavStatus::Wait(format!("no path {what}; forgetting learned obstacles"));
            }
            return NavStatus::Fail(format!("no path {what} on {}", map.name));
        };
        self.step_along(observation, &path, what)
    }

    /// The next act along `path` (a hold, a tap, a turn first, or a wait
    /// while a hold runs), tracked by the walker.
    fn step_along(&mut self, observation: &Observation, path: &[Step], what: &str) -> NavStatus {
        let Some(pose) = observation.player.as_ref().map(|p| p.pose.clone()) else {
            return NavStatus::Wait("locating the player".into());
        };
        let sync = self.sync();
        match self
            .walker
            .next(observation, path, self.facing, &sync, Instant::now())
        {
            WalkStep::Hold {
                dir,
                tiles,
                target,
                duration,
                timeout_frames,
            } => NavStatus::Act(
                Action::new(
                    format!("walk {what}: {dir:?} ×{tiles}"),
                    vec![ControllerCommand::Hold {
                        buttons: [direction_button(dir)].into_iter().collect(),
                        duration,
                    }],
                    Expectation::PlayerAt(target),
                    timeout_frames,
                )
                .interruptible()
                .timed(InputKind::WalkTile, tiles),
            ),
            WalkStep::Tap {
                dir,
                timeout_frames,
                ..
            } => NavStatus::Act(
                Action::new(
                    format!("walk {what}: {dir:?}"),
                    vec![ControllerCommand::Press(direction_button(dir))],
                    Expectation::PlayerMovedFrom(pose),
                    timeout_frames,
                )
                .timed(InputKind::WalkTile, 1),
            ),
            // A short press in a new direction only turns the player
            // (flash-6: every direction change cost a timed-out tap); turn
            // first, then step.
            WalkStep::Turn {
                dir,
                timeout_frames,
            } => {
                self.facing = Some(dir);
                NavStatus::Act(
                    Action::new(
                        format!("turn {dir:?} {what}"),
                        vec![ControllerCommand::Press(direction_button(dir))],
                        Expectation::InputsDone,
                        timeout_frames,
                    )
                    .timed(InputKind::Turn, 1),
                )
            }
            WalkStep::Arrived => NavStatus::Arrived,
            // The executor watches a hold; the navigator is only asked
            // again once it is over.
            WalkStep::Cancel | WalkStep::Walking { .. } => {
                NavStatus::Wait(format!("walking {what}"))
            }
        }
    }

    /// Walk to a tile on the map's edge that continues into the neighbour,
    /// then step across.
    fn cross_edge(
        &mut self,
        observation: &Observation,
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
            self.walker
                .note_tap(pose.clone(), dir, (pose.x, pose.y), false);
            return NavStatus::Act(
                Action::new(
                    format!("cross into the next map ({dir:?})"),
                    vec![ControllerCommand::Press(direction_button(dir))],
                    Expectation::LeftMap(map.name.clone()),
                    2 * self.sync().timeout_frames(InputKind::WalkTile, 1),
                )
                .timed(InputKind::WalkTile, 1),
            );
        }
        let (dx, dy) = dir.delta();
        let toward = (pose.x + dx * 100, pose.y + dy * 100);
        self.walk(
            observation,
            map,
            |p| exits.contains(&p),
            toward,
            &format!("to the {dir:?} edge"),
        )
    }

    /// Doors: stand below and push Up. Exit mats: stand on them and push
    /// their arrow. Stairs and other warps: walk onto them.
    fn use_warp(
        &mut self,
        observation: &Observation,
        map: &MapData,
        pose: &PlayerPose,
        index: usize,
    ) -> NavStatus {
        let Some(warp) = map.warps.get(index) else {
            return NavStatus::Fail(format!("{} has no warp {index}", map.name));
        };
        // A plain tile beside a marked warp to the same place: use that one.
        let usable = usable_warp(map, index);
        if usable != index {
            return self.use_warp(observation, map, pose, usable);
        }
        let (wx, wy) = (warp.x, warp.y);
        let tile = map.tile(wx, wy);
        let sync = self.sync();
        let push = |dir: Direction| {
            NavStatus::Act(
                Action::new(
                    format!("take warp {index} of {} ({dir:?})", map.name),
                    vec![ControllerCommand::Press(direction_button(dir))],
                    Expectation::LeftMap(map.name.clone()),
                    sync.timeout_frames(InputKind::WarpFade, 1),
                )
                .timed(InputKind::WarpFade, 1),
            )
        };
        if let Some(dir) =
            tile.and_then(|t| arrow_warp(t.behavior).or_else(|| stair_warp(t.behavior)))
        {
            if (pose.x, pose.y) == (wx, wy) {
                self.walker.clear_pending();
                return push(dir);
            }
            return self.walk(
                observation,
                map,
                |p| p == (wx, wy),
                (wx, wy),
                &format!("to exit ({wx}, {wy})"),
            );
        }
        if tile.is_some_and(|t| t.behavior == WARP_DOOR || t.collision != 0) {
            if (pose.x, pose.y) == (wx, wy + 1) {
                self.walker.clear_pending();
                return push(Direction::Up);
            }
            return self.walk(
                observation,
                map,
                |p| p == (wx, wy + 1),
                (wx, wy + 1),
                &format!("to door ({wx}, {wy})"),
            );
        }
        if (pose.x, pose.y) == (wx, wy) {
            // A plain warp tile that didn't fire on arrival: push toward the
            // nearest map edge (exits sit on edges).
            self.walker.clear_pending();
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
            // Up to the tile before the warp: the walker (holds, taps,
            // turns first). Flash-6: this branch tapped every tile, and
            // each direction change timed out once.
            Some(path) if path.len() >= 2 => {
                self.step_along(observation, &path[..path.len() - 1], &label)
            }
            // The step onto the warp tile itself: the map changes.
            Some(path) if !path.is_empty() => {
                let step = path[0];
                if self.facing != Some(step.dir) {
                    self.walker.clear_pending();
                    self.facing = Some(step.dir);
                    return NavStatus::Act(
                        Action::new(
                            format!("turn {:?} {label}", step.dir),
                            vec![ControllerCommand::Press(direction_button(step.dir))],
                            Expectation::InputsDone,
                            sync.timeout_frames(InputKind::Turn, 1),
                        )
                        .timed(InputKind::Turn, 1),
                    );
                }
                self.walker.note_tap(pose.clone(), step.dir, step.to, true);
                NavStatus::Act(
                    Action::new(
                        format!("walk {label}: {:?}", step.dir),
                        vec![ControllerCommand::Press(direction_button(step.dir))],
                        Expectation::LeftMap(map.name.clone()),
                        sync.timeout_frames(InputKind::WarpFade, 1),
                    )
                    .timed(InputKind::WarpFade, 1),
                )
            }
            // Learned blocks may be stale (a wandering NPC moved on, or a
            // step in a featureless corridor that did happen but couldn't
            // be seen).
            _ if self.forget_learned(&map.name) => {
                NavStatus::Wait(format!("no path {label}; forgetting learned obstacles"))
            }
            _ => NavStatus::Fail(format!("no path {label} on {}", map.name)),
        }
    }
}

/// Where to talk to `(x, y)` from: an adjacent tile, or across a counter
/// (the tile between is a counter, e.g. Pokémon Center nurses, clerks), with
/// the direction to face.
fn facing_spots(map: &MapData, x: i32, y: i32) -> Vec<((i32, i32), Direction)> {
    Direction::ALL
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
        .collect()
}

/// Warp `index`, or the marked warp to the same place beside it when warp
/// `index` sits on a plain tile that never fires.
fn usable_warp(map: &MapData, index: usize) -> usize {
    let Some(warp) = map.warps.get(index) else {
        return index;
    };
    if warp_usable(map, index) {
        return index;
    }
    map.warps
        .iter()
        .position(|w| {
            w.dest_map == warp.dest_map && w.dest_warp == warp.dest_warp && warp_is_marked(map, w)
        })
        .unwrap_or(index)
}

/// The tile to stand on to take warp `index`: below a door, else the warp
/// tile itself.
fn warp_approach(map: &MapData, index: usize) -> Option<(i32, i32)> {
    let w = map.warps.get(index)?;
    let tile = map.tile(w.x, w.y);
    let marked = tile.is_some_and(|t| {
        arrow_warp(t.behavior)
            .or_else(|| stair_warp(t.behavior))
            .is_some()
    });
    let door = tile.is_some_and(|t| t.behavior == WARP_DOOR || t.collision != 0);
    Some(if door && !marked {
        (w.x, w.y + 1)
    } else {
        (w.x, w.y)
    })
}

/// Tiles of the destination's map from which the destination is reached
/// (the tile itself, the spots to talk from, or where a warp is taken).
pub fn goal_tiles(world: &World, dest: &Destination) -> HashSet<(i32, i32)> {
    let Some(map) = world.map(dest.map()) else {
        return HashSet::new();
    };
    match *dest {
        Destination::Tile { x, y, .. } => HashSet::from([(x, y)]),
        Destination::Facing { x, y, .. } => facing_spots(map, x, y)
            .into_iter()
            .map(|(p, _)| p)
            .collect(),
        Destination::Warp { warp, .. } => warp_approach(map, usable_warp(map, warp))
            .into_iter()
            .collect(),
    }
}

/// First hop out of the player's map toward `dest`, None to walk on this map.
/// Maps split into parts (Mt. Moon's floors) are entered through the warp
/// that leads to the part holding the destination; when the destination
/// isn't reachable from here at tile level, fall back to reaching its map.
pub fn plan_hop(world: &World, pose: &PlayerPose, dest: &Destination, gone: &Gone) -> Option<Hop> {
    let goals = goal_tiles(world, dest);
    if let Some(hop) = route_search(world, pose, dest.map(), |p| goals.contains(&p), gone) {
        return hop;
    }
    if pose.map == dest.map() {
        return None;
    }
    route_from(world, pose, dest.map()).or_else(|| route_exit(world, &pose.map, dest.map()))
}

fn warp_is_marked(map: &MapData, warp: &pokebot_world::Warp) -> bool {
    map.tile(warp.x, warp.y).is_some_and(|t| {
        arrow_warp(t.behavior).is_some()
            || stair_warp(t.behavior).is_some()
            || t.behavior == WARP_DOOR
            || t.collision != 0
    })
}

/// Whether warp `index` can be used. A warp event on a plain tile never
/// fires when the map has a marked warp (arrow mat, door, stairs) to the same
/// place beside it (e.g. the tiles flanking Oak's lab exit mat).
pub fn warp_usable(map: &MapData, index: usize) -> bool {
    let Some(warp) = map.warps.get(index) else {
        return false;
    };
    warp_is_marked(map, warp)
        || !map.warps.iter().any(|w| {
            w.dest_map == warp.dest_map && w.dest_warp == warp.dest_warp && warp_is_marked(map, w)
        })
}

/// Tiles blocked by objects that don't move: NPCs that only turn, cut trees,
/// boulders, item balls. Wanderers are learned when met.
pub fn static_obstacles(map: &MapData) -> Obstacles {
    object_obstacles(map, &Gone::new())
}

/// [`static_obstacles`] without the objects in `gone`.
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
    route_search(world, pose, to, |_| true, &Gone::new()).flatten()
}

type Node = (String, i32, i32);

/// Tiles one move from `(x, y)` on `map`: walking within the map, a warp
/// (standing on one, or below a door) or a map edge, each with the hop that
/// leaves `map` (None when walking within it).
fn neighbours(
    world: &World,
    map: &MapData,
    (x, y): (i32, i32),
    obstacles: &Obstacles,
) -> Vec<(Node, Option<Hop>)> {
    let name = &map.name;
    let mut next: Vec<(Node, Option<Hop>)> = Vec::new();
    // Walking within the map.
    for dir in Direction::ALL {
        if let Some(s) = pokebot_world::path::step(map, (x, y), dir, obstacles) {
            next.push(((name.clone(), s.to.0, s.to.1), None));
        }
    }
    // Warps: standing on one (mats, stairs, plain) or below a door.
    for (i, w) in map.warps.iter().enumerate() {
        if w.dest_warp < 0 || !warp_usable(map, i) {
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
    next
}

/// Breadth-first search over tiles across maps from `pose` to a tile of map
/// `to` satisfying `goal`: None when no such tile is reachable, else the
/// first hop out of the start map (None: walk there on the start map).
pub fn route_search(
    world: &World,
    pose: &PlayerPose,
    to: &str,
    goal: impl Fn((i32, i32)) -> bool,
    gone: &Gone,
) -> Option<Option<Hop>> {
    let mut blocked: HashMap<String, Obstacles> = HashMap::new();
    let start: Node = (pose.map.clone(), pose.x, pose.y);
    let mut first: HashMap<Node, Option<Hop>> = HashMap::from([(start.clone(), None)]);
    let mut queue = VecDeque::from([start]);
    while let Some(node) = queue.pop_front() {
        let (name, x, y) = node.clone();
        let hop_here = first[&node];
        if name == to && goal((x, y)) {
            return Some(hop_here);
        }
        let Some(map) = world.map(&name) else {
            continue;
        };
        let obstacles = blocked
            .entry(name.clone())
            .or_insert_with(|| object_obstacles(map, gone));
        for (n, hop) in neighbours(world, map, (x, y), obstacles) {
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

/// The maps among `goals` (map → tiles to reach) that can be walked to
/// from `pose` in the fewest moves (a warp or an edge counts as one), in
/// name order; empty when none is reachable.
pub fn nearest_reachable(
    world: &World,
    pose: &PlayerPose,
    goals: &BTreeMap<String, HashSet<(i32, i32)>>,
    gone: &Gone,
) -> Vec<String> {
    let mut blocked: HashMap<String, Obstacles> = HashMap::new();
    let start: Node = (pose.map.clone(), pose.x, pose.y);
    let mut dist: HashMap<Node, u32> = HashMap::from([(start.clone(), 0)]);
    let mut queue = VecDeque::from([start]);
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut found_at: Option<u32> = None;
    while let Some(node) = queue.pop_front() {
        let d = dist[&node];
        if found_at.is_some_and(|f| d > f) {
            break;
        }
        let (name, x, y) = node;
        if goals
            .get(&name)
            .is_some_and(|tiles| tiles.contains(&(x, y)))
        {
            found.insert(name.clone());
            found_at = Some(d);
            continue;
        }
        let Some(map) = world.map(&name) else {
            continue;
        };
        let obstacles = blocked
            .entry(name.clone())
            .or_insert_with(|| object_obstacles(map, gone));
        for (n, _) in neighbours(world, map, (x, y), obstacles) {
            if !dist.contains_key(&n) {
                dist.insert(n.clone(), d + 1);
                queue.push_back(n);
            }
        }
    }
    found.into_iter().collect()
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
            .filter(|(i, w)| w.dest_warp >= 0 && warp_usable(map, *i))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn path(dirs: &[Direction], from: (i32, i32)) -> Vec<Step> {
        let mut at = from;
        dirs.iter()
            .map(|&dir| {
                let (dx, dy) = dir.delta();
                at = (at.0 + dx, at.1 + dy);
                Step { dir, to: at }
            })
            .collect()
    }

    #[test]
    fn straight_run_merges_same_direction_steps_but_keeps_the_last_step() {
        use Direction::*;
        // Right×4 then Up: hold Right for 4 tiles.
        assert_eq!(
            straight_run((0, 0), &path(&[Right, Right, Right, Right, Up], (0, 0))),
            4
        );
        // The final step stays a tap (warps, doors and edges need it).
        assert_eq!(
            straight_run((0, 0), &path(&[Right, Right, Right], (0, 0))),
            2
        );
        // Too short to be worth a hold.
        assert_eq!(straight_run((0, 0), &path(&[Right, Up, Up], (0, 0))), 1);
        assert_eq!(straight_run((0, 0), &path(&[Up], (0, 0))), 1);
        // Capped, so a long hold can't overshoot far if the timing drifts.
        assert_eq!(straight_run((0, 0), &path(&[Down; 20], (0, 0))), MAX_RUN);
    }

    #[test]
    fn a_ledge_jump_ends_the_run() {
        // A ledge moves two tiles in one step: not a plain walk.
        let mut steps = path(&[Direction::Down, Direction::Down], (0, 0));
        steps.push(Step {
            dir: Direction::Down,
            to: (0, 4),
        });
        steps.push(Step {
            dir: Direction::Down,
            to: (0, 5),
        });
        assert_eq!(straight_run((0, 0), &steps), 2);
        // Starting with the jump: no hold.
        let jump_first = vec![
            Step {
                dir: Direction::Down,
                to: (0, 2),
            },
            Step {
                dir: Direction::Down,
                to: (0, 3),
            },
            Step {
                dir: Direction::Down,
                to: (0, 4),
            },
        ];
        assert_eq!(straight_run((0, 0), &jump_first), 1);
    }

    /// Live: in MtMoon_B2F's featureless bottom corridor two Right taps
    /// that did move the player weren't seen (the view is the same at
    /// every x), the tile ahead was learned as blocked, and the walk to
    /// the ladder failed with "no path to warp (25, 21)".
    #[test]
    fn a_warp_walk_forgets_learned_obstacles_before_giving_up() {
        use pokebot_state::{Observed, PoseObservation, ScreenState};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let pose = PlayerPose {
            map: "MtMoon_B2F".into(),
            x: 27,
            y: 38,
        };
        let mut o = Observation::bare(
            1,
            Observed {
                value: ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.player = Some(PoseObservation {
            pose: pose.clone(),
            score: 1000,
        });
        let mut nav = Navigator::new(
            Arc::new(world),
            Destination::Warp {
                map: "MtMoon_B2F".into(),
                warp: 0,
            },
        );
        // Both corridor rows ahead learned as blocked.
        let block = |nav: &Navigator| {
            let mut learned = nav.learned();
            for tile in [(28, 38), (28, 37), (27, 37)] {
                learned.insert("MtMoon_B2F", tile);
            }
        };
        block(&nav);
        match nav.next(&o) {
            NavStatus::Wait(r) => assert!(r.contains("forgetting"), "{r}"),
            _ => panic!("expected a wait"),
        }
        match nav.next(&o) {
            NavStatus::Act(a) => {
                assert!(a.label.ends_with("to warp (25, 21)"), "{}", a.label)
            }
            _ => panic!("expected a step"),
        }
        // A block that keeps coming back is forgotten at most MAX_FORGETS
        // times on a map; then the walk fails instead of cycling.
        for _ in 1..MAX_FORGETS {
            block(&nav);
            assert!(matches!(nav.next(&o), NavStatus::Wait(_)));
        }
        block(&nav);
        match nav.next(&o) {
            NavStatus::Fail(r) => assert!(r.contains("no path to warp"), "{r}"),
            _ => panic!("expected the walk to fail"),
        }
    }

    /// Flash-6: on MtMoon_1F (16, 17), facing Down after the walk there,
    /// with Youngster Josh standing on (15, 17) (he walked up to the
    /// player; his data position is (13, 17)). The walk to the ladder at
    /// (5, 6) pressed Left three times (a turn, an interrupted tap, a
    /// blocked tap) and the loop rule failed the leg before the detour;
    /// the replanned leg knew nothing and did it again.
    #[test]
    fn a_blocked_npc_is_learnt_in_one_tap_after_a_turn_and_shared_with_the_next_leg() {
        use pokebot_state::{Observed, PoseObservation, ScreenState};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let world = Arc::new(world);
        let at = |x, y| PlayerPose {
            map: "MtMoon_1F".into(),
            x,
            y,
        };
        let observation = |pose: PlayerPose| {
            let mut o = Observation::bare(
                1,
                Observed {
                    value: ScreenState::Unknown,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.player = Some(PoseObservation { pose, score: 1000 });
            o
        };
        let ladder = Destination::Warp {
            map: "MtMoon_1F".into(),
            warp: 0,
        };
        let blocked = Blocked::default();
        let mut nav =
            Navigator::new(Arc::clone(&world), ladder.clone()).with_blocked(Arc::clone(&blocked));
        // Facing Down after the last hold Down. The path's first steps
        // are Left: a hold (it turns and walks by itself), which Josh
        // stalls; the walker falls back to taps, facing Left now.
        nav.facing = Some(Direction::Down);
        let o = observation(at(16, 17));
        let NavStatus::Act(hold) = nav.next(&o) else {
            panic!("expected a hold");
        };
        assert!(hold.label.ends_with("Left ×2"), "{}", hold.label);
        assert!(nav.on_outcome(&hold, Outcome::Stalled, &o).is_none());
        assert_eq!(nav.facing(), Some(Direction::Left));
        // The tap Left he blocks is learnt at once (no second miss).
        let NavStatus::Act(tap) = nav.next(&o) else {
            panic!("expected a tap");
        };
        assert!(tap.label.ends_with("Left"), "{}", tap.label);
        assert!(matches!(tap.expect, Expectation::PlayerMovedFrom(_)));
        assert_eq!(
            nav.on_outcome(&tap, Outcome::TimedOut, &o),
            Some(("MtMoon_1F".to_owned(), (15, 17)))
        );
        // The detour turns Down and steps, without Left again.
        let NavStatus::Act(detour) = nav.next(&o) else {
            panic!("expected the detour");
        };
        assert!(detour.label.starts_with("turn Down"), "{}", detour.label);
        // A new leg (the replanned Go) shares the store: it never tries
        // (15, 17) again.
        let mut again = Navigator::new(world, ladder).with_blocked(blocked);
        again.facing = Some(Direction::Left);
        let NavStatus::Act(first) = again.next(&o) else {
            panic!("expected a step");
        };
        assert!(first.label.starts_with("turn Down"), "{}", first.label);
    }

    /// Flash-7: on MtMoon_1F a hold stalled because a wild encounter
    /// froze the player, the follow-up tap timed out during the battle's
    /// fade, and (20, 25), the tile the player had just stepped onto, was
    /// learnt as blocked.
    #[test]
    fn a_tap_timing_out_into_a_transition_learns_nothing() {
        use pokebot_state::{Observed, PoseObservation, ScreenState};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let pose = PlayerPose {
            map: "MtMoon_1F".into(),
            x: 19,
            y: 25,
        };
        let observation = |screen: ScreenState, located: bool| {
            let mut o = Observation::bare(
                1,
                Observed {
                    value: screen,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.player = located.then(|| PoseObservation {
                pose: pose.clone(),
                score: 1000,
            });
            o
        };
        let mut nav = Navigator::new(
            Arc::new(world),
            Destination::Warp {
                map: "MtMoon_1F".into(),
                warp: 0,
            },
        );
        nav.facing = Some(Direction::Right);
        let quiet = observation(ScreenState::Unknown, true);
        // Two taps for a bit after a stalled hold.
        nav.walker
            .note_tap(pose.clone(), Direction::Right, (20, 25), true);
        let tap = Action::new(
            "walk to warp (5, 6): Right",
            vec![],
            Expectation::PlayerMovedFrom(pose.clone()),
            30,
        )
        .timed(InputKind::WalkTile, 1);
        let fade = observation(ScreenState::Transition, false);
        assert_eq!(nav.on_outcome(&tap, Outcome::TimedOut, &fade), None);
        assert!(nav.learned().on_map("MtMoon_1F").is_empty());
        // The same miss on a quiet frame is a block.
        nav.walker
            .note_tap(pose.clone(), Direction::Right, (20, 25), true);
        assert_eq!(
            nav.on_outcome(&tap, Outcome::TimedOut, &quiet),
            Some(("MtMoon_1F".to_owned(), (20, 25)))
        );
    }

    #[test]
    fn hold_ends_inside_the_last_tile() {
        // Released half a tile early: the game finishes the step it's in.
        assert_eq!(run_hold(1).as_millis(), TILE_MS as u128 / 2);
        assert_eq!(run_hold(4).as_millis(), (TILE_MS * 4 - TILE_MS / 2) as u128);
    }
}
