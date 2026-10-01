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
use pokebot_world::gates::GateTiles;
use pokebot_world::path::{find_path_with, Obstacles, Step, Walk};
use pokebot_world::route::warp_takeable;
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

    /// Drops one tile (learnt by mistake).
    pub fn remove(&mut self, map: &str, tile: (i32, i32)) {
        if let Some(tiles) = self.tiles.get_mut(map) {
            tiles.remove(&tile);
        }
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

/// Tiles a tap `dir` from `from` covers to end on `to`: 1 for a plain
/// step (or a ledge's hop), the slide's Manhattan length when a spin tile
/// carries the player on (`pokebot_world::path::step_with`).
pub fn slide_tiles(from: (i32, i32), dir: Direction, to: (i32, i32)) -> usize {
    let (dx, dy) = dir.delta();
    let hop = (from.0 + 2 * dx, from.1 + 2 * dy);
    if (from.0 + dx, from.1 + dy) == to || hop == to {
        return 1;
    }
    ((to.0 - from.0).unsigned_abs() + (to.1 - from.1).unsigned_abs()) as usize
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
    /// Where the last turn was pressed from, and the frame: a tap meant to
    /// turn can take a step instead (the Switch: a turn Down right after
    /// leaving Mt. Moon walked, the next step overshot to the sign, and
    /// the walk back stepped onto the cave's mouth), and the pose shows it
    /// only frames later. Nothing moves on until that time has passed.
    turned: Option<(PlayerPose, u64)>,
    /// The map the player was last located on.
    last_map: Option<String>,
    /// Holds and taps along the current path.
    pub(crate) walker: Walker,
    /// The device's timing model (shared with the executor, which feeds it).
    syncer: SyncerHandle,
    /// First hop out of the current map toward the destination, or None to
    /// walk on this map (planned on entering each map), and for an edge the
    /// tile the route crosses from.
    hop: Option<(String, Route)>,
    /// Map objects no longer there (items and fossils taken), by map and
    /// local id: they don't block their tiles.
    gone: Gone,
    /// Water is walkable: the player is surfing (or about to).
    surf: bool,
    /// Story passages as the belief stands (closed triggers, opened doors),
    /// the destination's own tile never closed.
    gates: Arc<GateTiles>,
    /// Where this walk entered maps.
    entries: MapEntries,
    /// Tiles walked only when no way round them exists, per map: unbeaten
    /// trainers' sight while the lead is too worn to fight ([`sight_tiles`]).
    avoid: HashMap<String, Obstacles>,
}

/// Times one walk may enter a map at the same tile: a route crosses each
/// place once, so a third entry is a walk going round in circles (Switch
/// goal run, 2026-09-27: 800 crossings between Cerulean City and Route 5
/// in two hours, nothing noticing).
pub const MAX_SAME_ENTRY: u32 = 3;
/// Frames within which those entries count as one circle: going round
/// takes seconds (Cerulean ↔ Route 5: 130 frames a lap), a trip to heal
/// and back through the same way tens of seconds (fleet worker 2: Route 1
/// to Mom's and back, 2700 frames, counted as a circle).
pub const SAME_ENTRY_FRAMES: u64 = 1800;

/// Where a walk has entered maps (warps and edges), to notice it going
/// round in circles.
#[derive(Debug, Clone, Default)]
pub struct MapEntries {
    last_map: Option<String>,
    /// Frames each (map, tile) was entered at, recent ones.
    entries: HashMap<(String, i32, i32), Vec<u64>>,
}

impl MapEntries {
    /// Notes where the player is at `frame`; on entering a map for the
    /// [`MAX_SAME_ENTRY`]th time at the same tile within
    /// [`SAME_ENTRY_FRAMES`], why the walk has failed.
    pub fn note(&mut self, pose: &PlayerPose, frame: u64) -> Option<String> {
        if self.last_map.as_deref() == Some(pose.map.as_str()) {
            return None;
        }
        let from = self.last_map.replace(pose.map.clone())?;
        let times = self
            .entries
            .entry((pose.map.clone(), pose.x, pose.y))
            .or_default();
        times.push(frame);
        times.retain(|f| frame.saturating_sub(*f) <= SAME_ENTRY_FRAMES);
        let n = times.len() as u32;
        (n >= MAX_SAME_ENTRY)
            .then(|| format!("going in circles: entered {pose} from {from} {n} times in a row"))
    }
}

/// Map objects known to be gone, as (map, local id).
pub type Gone = BTreeSet<(String, u32)>;

/// The objects the belief knows are no longer on their map: their hide
/// flag is set (`FLAG_HIDE_*`; `removeobject` sets it for good), or a
/// script path run to completion removed them. This is in `state.json`,
/// so a restart remembers what a session's [`Gone`] set forgot (Switch,
/// Mt. Moon B2F: after a restart both taken fossils blocked the only
/// corridor between the ladders, and every replan walked into a part of
/// B1F without the exit).
pub fn belief_gone(world: &World, state: &pokebot_state::GameState) -> Gone {
    let mut gone = Gone::new();
    for map in world.maps() {
        for o in &map.objects {
            let hidden = o
                .flag
                .as_deref()
                .is_some_and(|f| state.world.flag(f).value == Some(true));
            if hidden {
                gone.insert((map.name.clone(), o.local_id));
            }
        }
    }
    let Some(events) = world.events() else {
        return gone;
    };
    for (script, path) in &state.world.paths_run {
        let Some(s) = events.script(script) else {
            continue;
        };
        for effect in s.paths.get(*path).map_or(&[][..], |p| &p.does[..]) {
            if let pokebot_world::events::Effect::RemoveObject { remove_object, map } = effect {
                let map = map.clone().or_else(|| s.map.clone());
                let id = remove_object.as_int().and_then(|i| u32::try_from(i).ok());
                if let (Some(map), Some(id)) = (map, id) {
                    gone.insert((map, id));
                }
            }
        }
    }
    gone
}

impl Navigator {
    pub fn new(world: Arc<World>, destination: Destination) -> Self {
        Self {
            world,
            destination,
            learned: Blocked::default(),
            forgets: HashMap::new(),
            facing: None,
            turned: None,
            last_map: None,
            walker: Walker::new(),
            syncer: Arc::new(Mutex::new(Syncer::new("emulator"))),
            hop: None,
            gone: Gone::new(),
            surf: false,
            gates: Arc::new(GateTiles::default()),
            entries: MapEntries::default(),
            avoid: HashMap::new(),
        }
    }

    /// Walks through the doors the belief knows a script opened and around
    /// the passages it knows closed ([`GateTiles::believed`]); the
    /// destination tile itself stays reachable (a trigger is stepped onto).
    pub fn with_gates(mut self, gates: &GateTiles) -> Self {
        let gates = match &self.destination {
            Destination::Tile { map, x, y } => gates.clone().without(map, (*x, *y)),
            _ => gates.clone(),
        };
        self.gates = Arc::new(gates);
        self
    }

    /// Walks over water too (the player surfs on it; stepping back onto
    /// land dismounts).
    pub fn with_surf(mut self, surf: bool) -> Self {
        self.surf = surf;
        self
    }

    /// The tiles of `map` to go round when a way round exists (replacing
    /// what was set for it).
    pub fn set_avoid(&mut self, map: &str, tiles: Obstacles) {
        if tiles.is_empty() {
            self.avoid.remove(map);
        } else {
            self.avoid.insert(map.to_owned(), tiles);
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
        if let Some(why) = self.entries.note(&pose, observation.frame_id) {
            return NavStatus::Fail(why);
        }
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
        // A warp is done once we stand on the map it leads to; an
        // elevator's door (to `MAP_DYNAMIC`, set by its panel) once we
        // stand off the car. A warp onto its own map (Saffron Gym's pads)
        // is done on the tile it lands on: the map alone read as arrived
        // before the first step, and every RunScript to SABRINA failed
        // "no path to (14, 12)" from the door.
        if let Destination::Warp { map: from, warp } = &self.destination {
            let target = world.map(from).and_then(|m| m.warps.get(*warp));
            let dest = target.and_then(|w| world.name_of(&w.dest_map));
            let dynamic = target.is_some_and(|w| w.dest_map == "MAP_DYNAMIC");
            let landed = if dest == Some(from.as_str()) {
                target
                    .and_then(|w| world.warp_destination(w))
                    .is_some_and(|(m, x, y)| m.name == pose.map && (x, y) == (pose.x, pose.y))
            } else {
                dest == Some(pose.map.as_str())
            };
            if landed || (dynamic && pose.map != *from) {
                return NavStatus::Arrived;
            }
        }
        let (hop, via) = match &self.hop {
            Some((map, route)) if *map == pose.map => *route,
            _ => {
                let route =
                    plan_route_with(&world, &pose, &self.destination, &self.gone, &self.gates);
                self.hop = Some((pose.map.clone(), route));
                route
            }
        };
        match hop {
            Some(Hop::Warp(warp)) => return self.use_warp(observation, map, &pose, warp),
            Some(Hop::Edge(dir)) => {
                return self.cross_edge(observation, &world, map, &pose, dir, via)
            }
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

    /// Takes back a tile learnt as blocked (the miss had another cause).
    pub fn unlearn(&mut self, map: &str, tile: (i32, i32)) {
        self.learned().remove(map, tile);
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
        obstacles.extend(self.gates.closed_on(&map.name));
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
        let opened = self.gates.opened_on(&map.name);
        let walk = Walk {
            obstacles: &obstacles,
            surf: self.surf,
            opened: Some(&opened),
        };
        let avoid = self.avoid.get(&map.name);
        let extra = |t: (i32, i32)| {
            if avoid.is_some_and(|a| a.contains(&t)) {
                AVOID_COST
            } else {
                0
            }
        };
        let Some(path) = find_path_with(map, (pose.x, pose.y), &walk, extra, &goal, heuristic)
        else {
            // Learned blocks may be stale (a wandering NPC moved on).
            if self.forget_learned(&map.name) {
                return NavStatus::Wait(format!("no path {what}; forgetting learned obstacles"));
            }
            return NavStatus::Fail(format!("no path {what} on {}", map.name));
        };
        self.step_along(observation, &path, what)
    }

    /// Whether the walk can get from `pose` to a tile satisfying `goal` on
    /// `map`, around what it knows blocked.
    fn reachable(
        &self,
        map: &MapData,
        pose: &PlayerPose,
        goal: impl Fn((i32, i32)) -> bool,
    ) -> bool {
        let mut obstacles = self.obstacles(map);
        obstacles.remove(&(pose.x, pose.y));
        let opened = self.gates.opened_on(&map.name);
        let walk = Walk {
            obstacles: &obstacles,
            surf: self.surf,
            opened: Some(&opened),
        };
        find_path_with(map, (pose.x, pose.y), &walk, |_| 0, &goal, |_| 0).is_some()
    }

    /// The next act along `path` (a hold, a tap, a turn first, or a wait
    /// while a hold runs), tracked by the walker.
    fn step_along(&mut self, observation: &Observation, path: &[Step], what: &str) -> NavStatus {
        let Some(pose) = observation.player.as_ref().map(|p| p.pose.clone()) else {
            return NavStatus::Wait("locating the player".into());
        };
        let sync = self.sync();
        if let Some((from, at)) = self.turned.take() {
            let settle = sync.timeout_frames(InputKind::WalkTile, 1);
            if from == pose && observation.frame_id < at + settle {
                self.turned = Some((from, at));
                return NavStatus::Wait(format!("checking the turn {what} didn't step"));
            }
            // It stepped: the walk goes on from where the player is.
        }
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
            // A step onto a spin tile slides on to where the arrows end:
            // the tap is done there, not when the player leaves the tile
            // (Switch, Rocket Hideout B2F: the walk replanned from the
            // tiles slid through, pressed on, and the press landing after
            // the slide stepped onto an arrow back to the start).
            WalkStep::Tap {
                dir,
                to,
                timeout_frames,
            } if slide_tiles((pose.x, pose.y), dir, to) > 1 => {
                let tiles = slide_tiles((pose.x, pose.y), dir, to);
                NavStatus::Act(
                    Action::new(
                        format!("walk {what}: {dir:?}, sliding to {to:?}"),
                        vec![ControllerCommand::Press(direction_button(dir))],
                        Expectation::PlayerAt(PlayerPose {
                            map: pose.map.clone(),
                            x: to.0,
                            y: to.1,
                        }),
                        timeout_frames.max(sync.timeout_frames(InputKind::WalkTile, 2 * tiles)),
                    )
                    .timed(InputKind::WalkTile, 1),
                )
            }
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
                self.turned = Some((pose, observation.frame_id));
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
    /// then step across: the route's crossing `via` when known (another
    /// may lead into a part of the neighbour the destination isn't in),
    /// else any.
    fn cross_edge(
        &mut self,
        observation: &Observation,
        world: &World,
        map: &MapData,
        pose: &PlayerPose,
        dir: Direction,
        via: Option<(i32, i32)>,
    ) -> NavStatus {
        let all: HashSet<(i32, i32)> = world
            .crossings(map, dir)
            .into_iter()
            .map(|(a, _)| a)
            .collect();
        // The route's crossing while the walk can reach it; the route
        // search doesn't know what the walk has learnt blocked (an NPC on
        // the tile: fleet workers failed "no path to the Right edge on
        // PewterCity" at the one tile they were sent to).
        let exits = match via.filter(|v| all.contains(v)) {
            Some(v) if self.reachable(map, pose, |p| p == v) => HashSet::from([v]),
            _ => all,
        };
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
        if (pose.x, pose.y) == (wx, wy)
            && tile.is_some_and(|t| pokebot_world::behavior::warp_fires(t.behavior))
        {
            // Standing on a warp walked onto (a ladder, a cave mouth: where
            // the way in landed): it fires on entering it, so step off and
            // come back (fleet worker 3 pushed Up into Mt. Moon B1F's wall
            // from the ladder it had climbed down, 40 times a replan).
            let mut obstacles = self.obstacles(map);
            obstacles.remove(&(wx, wy));
            let opened = self.gates.opened_on(&map.name);
            let walk = Walk {
                obstacles: &obstacles,
                surf: self.surf,
                opened: Some(&opened),
            };
            if let Some(dir) = Direction::ALL
                .into_iter()
                .find(|d| pokebot_world::path::step_with(map, (wx, wy), *d, &walk).is_some())
            {
                self.walker.clear_pending();
                self.facing = Some(dir);
                return NavStatus::Act(
                    Action::new(
                        format!("step off warp {index} of {} ({dir:?}) to take it", map.name),
                        vec![ControllerCommand::Press(direction_button(dir))],
                        Expectation::PlayerMovedFrom(pose.clone()),
                        sync.timeout_frames(InputKind::WalkTile, 1),
                    )
                    .timed(InputKind::WalkTile, 1),
                );
            }
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
        let opened = self.gates.opened_on(&map.name);
        let walk = Walk {
            obstacles: &obstacles,
            surf: self.surf,
            opened: Some(&opened),
        };
        match find_path_with(
            map,
            (pose.x, pose.y),
            &walk,
            |_| 0,
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
pub(crate) fn facing_spots(map: &MapData, x: i32, y: i32) -> Vec<((i32, i32), Direction)> {
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
    plan_hop_with(world, pose, dest, gone, &GateTiles::default())
}

/// [`plan_hop`] around the passages `gates` knows closed and through the
/// doors it knows opened.
pub fn plan_hop_with(
    world: &World,
    pose: &PlayerPose,
    dest: &Destination,
    gone: &Gone,
    gates: &GateTiles,
) -> Option<Hop> {
    plan_route_with(world, pose, dest, gone, gates).0
}

/// [`plan_hop_with`], and for an edge hop on a walkable route the tile to
/// cross from (see [`route_search_via`]).
pub fn plan_route_with(
    world: &World,
    pose: &PlayerPose,
    dest: &Destination,
    gone: &Gone,
    gates: &GateTiles,
) -> Route {
    let goals = goal_tiles(world, dest);
    if let Some(route) =
        route_search_via(world, pose, dest.map(), |p| goals.contains(&p), gone, gates)
    {
        return route;
    }
    if pose.map == dest.map() {
        return (None, None);
    }
    // No walkable route (a gate, or an object the belief doesn't know is
    // gone): head for the destination's map anyway, but only through an
    // exit the player can walk to from here. A map-level guess through an
    // exit in a walled-off part of the map fails every time.
    let hop = route_from(world, pose, dest.map()).or_else(|| {
        route_exit(world, &pose.map, dest.map())
            .filter(|hop| reachable_hop(world, pose, *hop, gone, gates))
    });
    (hop, None)
}

/// Whether the player can walk from `pose` to where `hop` leaves the map.
fn reachable_hop(
    world: &World,
    pose: &PlayerPose,
    hop: Hop,
    gone: &Gone,
    gates: &GateTiles,
) -> bool {
    match hop {
        Hop::Warp(warp) => {
            let dest = Destination::Warp {
                map: pose.map.clone(),
                warp,
            };
            let goals = goal_tiles(world, &dest);
            route_search_with(world, pose, &pose.map, |p| goals.contains(&p), gone, gates).is_some()
        }
        Hop::Edge(dir) => {
            let Some(map) = world.map(&pose.map) else {
                return false;
            };
            let sides: HashSet<(i32, i32)> = world
                .crossings(map, dir)
                .into_iter()
                .map(|(here, _)| here)
                .collect();
            route_search_with(world, pose, &pose.map, |p| sides.contains(&p), gone, gates).is_some()
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

/// The extra cost of a tile to go round (in tiles walked): a detour of
/// up to this many steps is preferred to crossing it.
const AVOID_COST: i32 = 60;

/// The tiles from which an unbeaten trainer on `map` sees the player and
/// walks up to battle: along each way it faces, up to its sight range,
/// until a tile that can't be walked (Viridian Forest: a worn BULBASAUR
/// walking to Pewter's Center crossed a Bug Catcher's line at 10/29 HP and
/// fainted; a trainer's battle can't be fled).
pub fn sight_tiles(
    map: &MapData,
    trainers: &[pokebot_gamedata::MapTrainer],
    beaten: impl Fn(&str) -> bool,
) -> Obstacles {
    trainers
        .iter()
        .filter(|t| !beaten(&t.trainer))
        .flat_map(|t| pokebot_world::obstacles::sight_line(map, t.local_id, t.sight))
        .collect()
}

/// A walk on one map: the map, from, to.
pub type MapWalk = (String, (i32, i32), (i32, i32));

/// The walked legs of a route (from, to on one map) whose best path, going
/// round unbeaten trainers' sight where it can, still crosses it: a
/// trainer's battle is met on each, and can't be fled.
pub fn unavoidable_sightings(
    world: &World,
    data: &pokebot_gamedata::GameData,
    walks: &[MapWalk],
    beaten: impl Fn(&str) -> bool,
) -> u32 {
    let mut met = 0;
    for (name, from, to) in walks {
        let (Some(map), Some(trainers)) = (world.map(name), data.map_trainers.get(name)) else {
            continue;
        };
        let sight = sight_tiles(map, trainers, &beaten);
        if sight.is_empty() {
            continue;
        }
        let mut obstacles = object_obstacles(map, &Gone::new());
        obstacles.remove(from);
        obstacles.remove(to);
        let walk = Walk {
            obstacles: &obstacles,
            surf: false,
            opened: None,
        };
        let extra = |t: (i32, i32)| if sight.contains(&t) { AVOID_COST } else { 0 };
        let heuristic = |p: (i32, i32)| (p.0 - to.0).abs() + (p.1 - to.1).abs();
        if let Some(path) = find_path_with(map, *from, &walk, extra, |t| t == *to, heuristic) {
            if path.iter().any(|s| sight.contains(&s.to)) {
                met += 1;
            }
        }
    }
    met
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

/// The first hop out of the start map (None: walk on it), and for an edge
/// the tile the route crosses from.
pub type Route = (Option<Hop>, Option<(i32, i32)>);

/// Tiles one move from `(x, y)` on `map`: walking within the map, a warp
/// (standing on one, or below a door) or a map edge, each with the hop that
/// leaves `map` (None when walking within it).
fn neighbours(
    world: &World,
    map: &MapData,
    (x, y): (i32, i32),
    walk: &Walk,
) -> Vec<(Node, Option<Hop>)> {
    let name = &map.name;
    let mut next: Vec<(Node, Option<Hop>)> = Vec::new();
    // Walking within the map.
    for dir in Direction::ALL {
        if let Some(s) = pokebot_world::path::step_with(map, (x, y), dir, walk) {
            next.push(((name.clone(), s.to.0, s.to.1), None));
        }
    }
    // Warps: standing on one (mats, stairs, plain) or below a door.
    for (i, w) in map.warps.iter().enumerate() {
        if w.dest_warp < 0 || !warp_usable(map, i) || !warp_takeable(world, map, i) {
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
        for (other, a, b) in world.edge_crossings(map, dir) {
            if a == (x, y) {
                next.push(((other, b.0, b.1), Some(Hop::Edge(dir))));
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
    route_search_with(world, pose, to, goal, gone, &GateTiles::default())
}

/// [`route_search`] around the passages `gates` knows closed and through
/// the doors it knows opened.
pub fn route_search_with(
    world: &World,
    pose: &PlayerPose,
    to: &str,
    goal: impl Fn((i32, i32)) -> bool,
    gone: &Gone,
    gates: &GateTiles,
) -> Option<Option<Hop>> {
    route_search_via(world, pose, to, goal, gone, gates).map(|(hop, _)| hop)
}

/// [`route_search_with`], with the tile of the start map the path leaves
/// it from when the first hop is an edge: which crossing matters when the
/// neighbour is split (Route 5's grass is fenced off from the corridor
/// Cerulean's crossing at x 32 leads into; any other column of the edge
/// reaches it).
pub fn route_search_via(
    world: &World,
    pose: &PlayerPose,
    to: &str,
    goal: impl Fn((i32, i32)) -> bool,
    gone: &Gone,
    gates: &GateTiles,
) -> Option<Route> {
    let mut blocked: HashMap<String, (Obstacles, Obstacles)> = HashMap::new();
    let start: Node = (pose.map.clone(), pose.x, pose.y);
    let mut first: HashMap<Node, Route> = HashMap::from([(start.clone(), (None, None))]);
    let mut queue = VecDeque::from([start]);
    while let Some(node) = queue.pop_front() {
        let (name, x, y) = node.clone();
        let here = first[&node];
        if name == to && goal((x, y)) {
            return Some(here);
        }
        let Some(map) = world.map(&name) else {
            continue;
        };
        let (obstacles, opened) = blocked.entry(name.clone()).or_insert_with(|| {
            let mut o = object_obstacles(map, gone);
            o.extend(gates.closed_on(&name));
            (o, gates.opened_on(&name))
        });
        let walk = Walk {
            obstacles,
            surf: false,
            opened: Some(opened),
        };
        for (n, hop) in neighbours(world, map, (x, y), &walk) {
            if first.contains_key(&n) {
                continue;
            }
            // The first hop is fixed once the path leaves the start map.
            let inherited = if name == pose.map && n.0 == pose.map || here.0.is_some() {
                here
            } else {
                let via = matches!(hop, Some(Hop::Edge(_))).then_some((x, y));
                (hop, via)
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
        let walk = Walk {
            obstacles,
            surf: false,
            opened: None,
        };
        for (n, _) in neighbours(world, map, (x, y), &walk) {
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
            .filter(|(i, w)| {
                w.dest_warp >= 0 && warp_usable(map, *i) && warp_takeable(world, map, *i)
            })
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

    fn located(map: &str, x: i32, y: i32) -> Observation {
        use pokebot_state::{Observed, PoseObservation, ScreenState};
        let mut o = Observation::bare(
            1,
            Observed {
                value: ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.player = Some(PoseObservation {
            pose: PlayerPose {
                map: map.into(),
                x,
                y,
            },
            score: 1000,
        });
        o
    }

    fn world_with_events() -> Option<Arc<World>> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let world = World::load(root.join("data/world")).ok()?;
        world.events()?;
        Some(Arc::new(world))
    }

    /// Switch goal run (2026-09-27, 800 crossings in 2 h): Route 5's
    /// grass is fenced off from the corridor Cerulean's crossing at x 32
    /// leads into, and reached from Cerulean's other south-edge columns.
    /// From Route 5 the route went Up; in Cerulean any crossing tile would
    /// do, so it stepped straight back Down at x 32. The route's crossing
    /// is kept: in Cerulean the walk goes to it first.
    #[test]
    fn an_edge_is_crossed_where_the_route_crosses_it() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let world = Arc::new(world);
        let grass = Destination::Tile {
            map: "Route5".into(),
            x: 26,
            y: 5,
        };
        let corridor = PlayerPose {
            map: "Route5".into(),
            x: 32,
            y: 0,
        };
        let city = PlayerPose {
            map: "CeruleanCity".into(),
            x: 32,
            y: 39,
        };
        let none = (Gone::new(), GateTiles::default());
        // Up out of the corridor (x 29..=32): the only way.
        let (hop, via) = plan_route_with(&world, &corridor, &grass, &none.0, &none.1);
        assert_eq!(hop, Some(Hop::Edge(Direction::Up)));
        assert!(
            via.is_some_and(|(x, y)| (29..=32).contains(&x) && y == 0),
            "{via:?}"
        );
        let (hop, via) = plan_route_with(&world, &city, &grass, &none.0, &none.1);
        assert_eq!(hop, Some(Hop::Edge(Direction::Down)));
        let (x, y) = via.expect("the crossing tile");
        assert!((20..=27).contains(&x) && y == 39, "crosses at ({x}, {y})");
        let mut nav = Navigator::new(Arc::clone(&world), grass);
        match nav.next(&located("CeruleanCity", 32, 39)) {
            NavStatus::Act(a) => assert!(!a.label.contains("cross"), "{}", a.label),
            _ => panic!("expected a walk along the edge"),
        }
    }

    /// Switch, Rocket Hideout B2F: from (3, 9) the way to the elevator
    /// steps Right onto a spin tile that slides the player to (8, 11). The
    /// tap waits for the slide's end, not for the player leaving (3, 9):
    /// replanned from the tiles slid through, the walk pressed on, and the
    /// press after the slide stepped onto an arrow back to (1, 4).
    #[test]
    fn a_step_onto_a_spin_tile_waits_for_the_slide_to_end() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let dest = Destination::Tile {
            map: "RocketHideout_B2F".into(),
            x: 28,
            y: 17,
        };
        let mut nav = Navigator::new(Arc::new(world), dest);
        nav.facing = Some(Direction::Right);
        match nav.next(&located("RocketHideout_B2F", 3, 9)) {
            NavStatus::Act(a) => {
                assert_eq!(
                    a.expect,
                    Expectation::PlayerAt(PlayerPose {
                        map: "RocketHideout_B2F".into(),
                        x: 8,
                        y: 11,
                    }),
                    "{}",
                    a.label
                );
                // A slide runs about 8 frames a tile (6 in 48 on the Switch).
                assert!(a.timeout_frames >= 7 * 8, "{}", a.timeout_frames);
            }
            NavStatus::Wait(why) => panic!("expected the tap, waiting: {why}"),
            _ => panic!("expected the tap"),
        }
        assert_eq!(slide_tiles((3, 9), Direction::Right, (4, 9)), 1);
        assert_eq!(slide_tiles((3, 9), Direction::Right, (5, 9)), 1);
        assert_eq!(slide_tiles((3, 9), Direction::Right, (8, 11)), 7);
    }

    /// Fleet workers: the route's crossing out of Pewter City was a tile
    /// the walk had learnt blocked (an NPC stood on it), and "no path to
    /// the Right edge" failed Go(MtMoon_B2F) every replan. An unreachable
    /// crossing gives way to the others.
    #[test]
    fn a_blocked_crossing_gives_way_to_the_others() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let world = Arc::new(world);
        let m = world.map("Route3").unwrap();
        let dest = Destination::Tile {
            map: "Route3".into(),
            x: 5,
            y: m.height / 2,
        };
        let pewter = world.map("PewterCity").unwrap();
        // A walkable tile a few steps west of a crossing.
        let pose = world
            .crossings(pewter, Direction::Right)
            .into_iter()
            .flat_map(|((x, y), _)| (2..6).map(move |d| (x - d, y)))
            .find(|&(x, y)| pewter.tile(x, y).is_some_and(|t| t.collision == 0))
            .map(|(x, y)| PlayerPose {
                map: "PewterCity".into(),
                x,
                y,
            })
            .unwrap();
        let (hop, via) = plan_route_with(&world, &pose, &dest, &Gone::new(), &GateTiles::default());
        assert_eq!(hop, Some(Hop::Edge(Direction::Right)));
        let via = via.expect("a crossing");
        let blocked = Blocked::default();
        blocked.lock().unwrap().insert("PewterCity", via);
        let mut nav = Navigator::new(Arc::clone(&world), dest).with_blocked(blocked);
        // At once toward another crossing, not "no path" (nor a wait while
        // what was learnt is forgotten, to be learnt again).
        match nav.next(&located("PewterCity", pose.x, pose.y)) {
            NavStatus::Act(_) => {}
            NavStatus::Fail(r) | NavStatus::Wait(r) => panic!("{r}"),
            _ => panic!("expected a walk"),
        }
    }

    /// Fleet worker 3: climbed down to Mt. Moon B1F, it stood on the
    /// ladder (3, 3) and pushed Up into the wall to climb it again. A
    /// ladder fires when stepped onto: step off first.
    #[test]
    fn a_ladder_stood_on_is_stepped_off_and_back_onto() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let mut nav = Navigator::new(
            Arc::new(world),
            Destination::Warp {
                map: "MtMoon_B1F".into(),
                warp: 0,
            },
        );
        match nav.next(&located("MtMoon_B1F", 3, 3)) {
            NavStatus::Act(a) => assert!(a.label.starts_with("step off warp 0"), "{}", a.label),
            _ => panic!("expected a step off the ladder"),
        }
        // One tile off, the step back onto it takes the ladder.
        let off = nav.next(&located("MtMoon_B1F", 3, 4));
        assert!(matches!(off, NavStatus::Act(_)));
    }

    /// The same run's pattern, whatever causes it: a walk that enters a
    /// map at the same tile a third time fails instead of going on.
    #[test]
    fn a_walk_going_round_in_circles_fails() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let mut nav = Navigator::new(
            Arc::new(world),
            Destination::Tile {
                map: "Route5".into(),
                x: 26,
                y: 5,
            },
        );
        let mut failed = None;
        for _ in 0..4 {
            for o in [located("CeruleanCity", 32, 39), located("Route5", 32, 0)] {
                if let NavStatus::Fail(why) = nav.next(&o) {
                    failed.get_or_insert(why);
                }
            }
        }
        let why = failed.expect("fails");
        assert!(why.contains("going in circles"), "{why}");
        // Moving about one map is not a circle.
        let mut entries = MapEntries::default();
        let at = |x| PlayerPose {
            map: "Route5".into(),
            x,
            y: 0,
        };
        assert!((0..10).all(|x| entries.note(&at(x), 0).is_none()));
        // A trip to heal and back through the same way is not a circle.
        let mut entries = MapEntries::default();
        let (route1, pallet) = (
            PlayerPose {
                map: "Route1".into(),
                x: 13,
                y: 39,
            },
            PlayerPose {
                map: "PalletTown".into(),
                x: 13,
                y: 0,
            },
        );
        for lap in 0..4u64 {
            entries.note(&pallet, lap * 2700);
            assert_eq!(entries.note(&route1, lap * 2700 + 100), None, "lap {lap}");
        }
    }

    /// Cinnabar Gym's quiz doors are walls in the map data that a script
    /// opens once its question is answered: the navigator walks through
    /// one the belief knows open, and finds no way while it doesn't.
    #[test]
    fn a_door_the_belief_knows_open_is_walked_through() {
        use pokebot_world::predicate::MapBelief;
        let Some(world) = world_with_events() else {
            return;
        };
        let gates = pokebot_world::gates::derive(&world);
        let map = "CinnabarIsland_Gym";
        let dest = Destination::Tile {
            map: map.into(),
            x: 26,
            y: 7,
        };
        let o = located(map, 26, 10);
        let open = GateTiles::believed(
            &world,
            &gates,
            &MapBelief::default().flag("FLAG_CINNABAR_GYM_QUIZ_1", true),
        );
        let mut nav = Navigator::new(Arc::clone(&world), dest.clone()).with_gates(&open);
        match nav.next(&o) {
            NavStatus::Act(a) => assert!(a.label.contains("Up"), "{}", a.label),
            _ => panic!("expected a step through the door"),
        }
        let shut = GateTiles::believed(
            &world,
            &gates,
            &MapBelief::default().flag("FLAG_CINNABAR_GYM_QUIZ_1", false),
        );
        let mut nav = Navigator::new(Arc::clone(&world), dest).with_gates(&shut);
        assert!(
            !matches!(nav.next(&o), NavStatus::Act(a) if a.label.contains("Up")),
            "the closed door is a wall"
        );
    }

    /// Oak's trigger at the edge of Pallet Town while its scene is armed:
    /// no walk goes past it to Route 1, but a walk to the trigger itself
    /// (to start the scene) ends on it.
    #[test]
    fn a_passage_the_belief_knows_closed_is_not_walked_past_but_may_be_stepped_onto() {
        use pokebot_world::predicate::MapBelief;
        let Some(world) = world_with_events() else {
            return;
        };
        let gates = pokebot_world::gates::derive(&world);
        let armed = GateTiles::believed(
            &world,
            &gates,
            &MapBelief::default().var("VAR_MAP_SCENE_PALLET_TOWN_OAK", 0),
        );
        let o = located("PalletTown", 12, 3);
        let north = Destination::Tile {
            map: "Route1".into(),
            x: 12,
            y: 30,
        };
        let mut nav = Navigator::new(Arc::clone(&world), north.clone()).with_gates(&armed);
        match nav.next(&o) {
            NavStatus::Fail(r) => assert!(r.contains("no path"), "{r}"),
            _ => panic!("expected no way past the trigger"),
        }
        // As the map draws it (nothing known): the walk goes north.
        let mut nav = Navigator::new(Arc::clone(&world), north);
        assert!(matches!(nav.next(&o), NavStatus::Act(_)));
        let onto = Destination::Tile {
            map: "PalletTown".into(),
            x: 12,
            y: 1,
        };
        let mut nav = Navigator::new(world, onto).with_gates(&armed);
        match nav.next(&o) {
            NavStatus::Act(a) => assert!(a.label.contains("(12, 1)"), "{}", a.label),
            _ => panic!("expected a step toward the trigger"),
        }
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

    /// Switch goal run, after a restart: on MtMoon_B1F (22, 18) (the part
    /// between the B2F ladder and the 1F one) every plan's first hop was
    /// the exit to Route 4, in another part of the floor, and failed with
    /// "no path to warp (45, 4)" eight replans in a row. The two fossils
    /// were taken (the Dome Fossil's script removes both), but only the
    /// lost session remembered it, so they closed B2F's corridor to the
    /// ladder up to the exit. The belief keeps the script's run.
    #[test]
    fn taken_fossils_known_to_the_belief_open_the_way_out_of_mt_moon() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let pose = PlayerPose {
            map: "MtMoon_B1F".into(),
            x: 22,
            y: 18,
        };
        let gym = Destination::Tile {
            map: "CeruleanCity".into(),
            x: 22,
            y: 20,
        };
        // Without the belief: no walkable route, and the fallback no longer
        // guesses the exit this part of the floor can't reach.
        let blind = plan_hop(&world, &pose, &gym, &Gone::new());
        assert_ne!(blind, Some(Hop::Warp(7)), "{blind:?}");
        let mut state = pokebot_state::GameState::default();
        state
            .world
            .record_path("MtMoon_B2F_EventScript_DomeFossil", 1);
        let gone = belief_gone(&world, &state);
        assert!(gone.contains(&("MtMoon_B2F".to_owned(), 1)));
        assert!(gone.contains(&("MtMoon_B2F".to_owned(), 2)));
        // Back down to B2F, along its corridor, up to the exit's part.
        assert_eq!(plan_hop(&world, &pose, &gym, &gone), Some(Hop::Warp(3)));
        // Emulator goal run: Route 4's west part, in front of its Center,
        // took the right edge (the east part's) for the way to Cerulean.
        let west = PlayerPose {
            map: "Route4".into(),
            x: 12,
            y: 6,
        };
        let blind = plan_hop(&world, &west, &gym, &Gone::new());
        assert_ne!(blind, Some(Hop::Edge(Direction::Right)), "{blind:?}");
        assert!(
            matches!(plan_hop(&world, &west, &gym, &gone), Some(Hop::Warp(_))),
            "into Mt. Moon"
        );
        // A hide flag set in the belief does the same.
        let mut state = pokebot_state::GameState::default();
        for flag in ["FLAG_HIDE_DOME_FOSSIL", "FLAG_HIDE_HELIX_FOSSIL"] {
            state
                .world
                .flags
                .insert(flag.into(), pokebot_state::Knowledge::tracked(true, None));
        }
        assert_eq!(
            plan_hop(&world, &pose, &gym, &belief_gone(&world, &state)),
            Some(Hop::Warp(3))
        );
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

    /// The Switch, leaving Mt. Moon: a tap meant to turn walked, the pose
    /// showed it frames later, and the next step overshot (then, walking
    /// back, onto the cave's mouth). After a turn nothing moves on until a
    /// step would have shown; if it did, the walk goes on from there.
    #[test]
    fn a_turn_is_checked_for_a_step_before_the_next_move() {
        use pokebot_state::{Observed, PoseObservation, ScreenState};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let at = |x, y| PlayerPose {
            map: "Route4".into(),
            x,
            y,
        };
        let observation = |frame: u64, pose: PlayerPose| {
            let mut o = Observation::bare(
                frame,
                Observed {
                    value: ScreenState::Unknown,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.player = Some(PoseObservation { pose, score: 1000 });
            o
        };
        // The Pokémon Center's door, from the cave's mouth, facing Up.
        let door = Destination::Warp {
            map: "Route4".into(),
            warp: 2,
        };
        let mut nav = Navigator::new(Arc::new(world), door);
        nav.facing = Some(Direction::Up);
        let o = observation(100, at(19, 5));
        let NavStatus::Act(turn) = nav.next(&o) else {
            panic!("expected a turn");
        };
        assert!(turn.label.starts_with("turn Down"), "{}", turn.label);
        assert!(nav.on_outcome(&turn, Outcome::Confirmed, &o).is_none());
        // Right after: the pose hasn't had time to show a step.
        assert!(matches!(
            nav.next(&observation(105, at(19, 5))),
            NavStatus::Wait(w) if w.contains("didn't step")
        ));
        // It had stepped: the walk goes on from (19, 6), never Down again.
        let NavStatus::Act(next) = nav.next(&observation(110, at(19, 6))) else {
            panic!("expected a move");
        };
        assert!(!next.label.contains("Down"), "{}", next.label);
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
