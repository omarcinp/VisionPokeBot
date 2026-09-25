//! Route planner (spec §5): a graph of places (warps, connection crossings,
//! Fly landings, heal spots, field-move gates, script warps) joined by edges
//! with a cost in seconds and a requirement, searched with Dijkstra against
//! the belief. Edges the belief rules out are kept as blocked alternatives
//! so the goal planner can price satisfying their requirement.
//!
//! A warp's place is the tile the player stands on to take it (below a
//! door, on an arrow mat or stairs), the same rule as the navigator's, so a
//! route's legs are exactly what the tools do. Stationary NPCs are walls
//! unless a flag hides them or they are trainers: their tiles then carry a
//! requirement (and a battle's cost) on the walk that crosses them.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use pokebot_state::{Direction, PlayerPose};
use serde::{Deserialize, Serialize};

use crate::behavior::{arrow_warp, is_water, stair_warp, WARP_DOOR};
use crate::events::{Condition, Effect, Val};
use crate::gates::{self, is_local_flag, is_local_var, Gates};
use crate::obstacles::{blockers, wander_tiles, Passage};
use crate::path::{reach, Obstacles, Reach, Walk};
use crate::predicate::{check, BeliefView, Predicate, Requirement, Truth};
use crate::{MapData, World};

/// Timing model of the edges, in seconds. The tile time is the syncer's
/// estimate (§6.1) and is injected by the caller once it is measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteParams {
    pub tile_s: f64,
    /// Warp fade (doors, stairs, connections don't fade).
    pub warp_s: f64,
    /// Fly: menu, animation and landing.
    pub fly_s: f64,
    /// Extra tiles charged per tile inside a wandering NPC's area.
    pub wander_area_penalty_tiles: u32,
    /// Using Cut / Rock Smash on a gate: menu and animation.
    pub gate_s: f64,
    /// Starting to Surf: menu and the mount animation, once per leg.
    pub surf_s: f64,
    /// Talking to an NPC (or standing on a trigger) that warps the player.
    pub talk_s: f64,
    /// Beating a trainer that stands in the way.
    pub battle_s: f64,
}

impl Default for RouteParams {
    fn default() -> Self {
        RouteParams {
            tile_s: 0.268,
            warp_s: 1.5,
            fly_s: 12.0,
            wander_area_penalty_tiles: 2,
            gate_s: 6.0,
            surf_s: 5.0,
            talk_s: 4.0,
            battle_s: 60.0,
        }
    }
}

/// Whether warp `index` of `map` is a door: a tile the player can't stand
/// on (outdoor building doors, cave mouths with collision).
fn warp_is_door(map: &MapData, index: usize) -> bool {
    let Some(w) = map.warps.get(index) else {
        return false;
    };
    map.tile(w.x, w.y)
        .is_some_and(|t| t.behavior == WARP_DOOR || t.collision != 0)
}

/// Whether warp `index` is marked on the ground (arrow mat, stairs, door):
/// the ones the player takes by pushing into them.
fn warp_is_marked(map: &MapData, index: usize) -> bool {
    let Some(w) = map.warps.get(index) else {
        return false;
    };
    map.tile(w.x, w.y)
        .is_some_and(|t| arrow_warp(t.behavior).is_some() || stair_warp(t.behavior).is_some())
        || warp_is_door(map, index)
}

/// Whether warp `index` can be taken. A warp on a plain tile never fires
/// when the map has a marked warp to the same place (the tiles flanking an
/// exit mat), the navigator's rule.
pub fn warp_usable(map: &MapData, index: usize) -> bool {
    let Some(warp) = map.warps.get(index) else {
        return false;
    };
    warp_is_marked(map, index)
        || !map.warps.iter().enumerate().any(|(i, w)| {
            w.dest_map == warp.dest_map && w.dest_warp == warp.dest_warp && warp_is_marked(map, i)
        })
}

/// The tile the player stands on to take warp `index`: below a door, on
/// the mat, stairs or plain warp tile otherwise (the navigator's rule).
pub fn warp_approach(map: &MapData, index: usize) -> Option<(i32, i32)> {
    let w = map.warps.get(index)?;
    let on_mat = map
        .tile(w.x, w.y)
        .is_some_and(|t| arrow_warp(t.behavior).is_some() || stair_warp(t.behavior).is_some());
    Some(if warp_is_door(map, index) && !on_mat {
        (w.x, w.y + 1)
    } else {
        (w.x, w.y)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceKind {
    Warp,
    Crossing,
    Fly,
    Heal,
    Gate,
    /// Where the player stands to run a script that warps.
    Script,
    /// A tile the caller asked for.
    Tile,
}

/// A node of the graph. Identity is the tile; `kind` says why it is one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Place {
    pub map: String,
    pub x: i32,
    pub y: i32,
    pub kind: PlaceKind,
}

impl Place {
    pub fn tile(map: &str, x: i32, y: i32) -> Place {
        Place {
            map: map.to_string(),
            x,
            y,
            kind: PlaceKind::Tile,
        }
    }

    fn key(&self) -> PlaceKey {
        (self.map.clone(), self.x, self.y)
    }
}

impl From<&PlayerPose> for Place {
    fn from(p: &PlayerPose) -> Place {
        Place::tile(&p.map, p.x, p.y)
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}, {})", self.map, self.x, self.y)
    }
}

type PlaceKey = (String, i32, i32);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Within a map; `surf` when the path crosses water.
    Walk {
        tiles: u32,
        surf: bool,
    },
    Warp,
    Connection,
    /// Cut tree or Rock Smash rock (`kind` as in `places.json`).
    Gate {
        kind: String,
    },
    /// A script (talk to the NPC, step on the trigger) that warps.
    ScriptWarp {
        script: String,
    },
    Fly,
}

impl fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EdgeKind::Walk { tiles, surf: false } => write!(f, "walk {tiles}"),
            EdgeKind::Walk { tiles, surf: true } => write!(f, "surf {tiles}"),
            EdgeKind::Warp => write!(f, "warp"),
            EdgeKind::Connection => write!(f, "connection"),
            EdgeKind::Gate { kind } => write!(f, "gate:{kind}"),
            EdgeKind::ScriptWarp { script } => write!(f, "script:{script}"),
            EdgeKind::Fly => write!(f, "fly"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub to: Place,
    pub kind: EdgeKind,
    pub cost_s: f64,
    pub requires: Requirement,
}

/// One tool invocation of a route (§5.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Leg {
    pub from: Place,
    pub to: Place,
    pub kind: EdgeKind,
    pub cost_s: f64,
    pub requires: Requirement,
}

impl fmt::Display for Leg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} -> {} [{}] {:.1}s",
            self.from, self.to, self.kind, self.cost_s
        )?;
        if !self.requires.is_empty() {
            let req: Vec<String> = self.requires.iter().map(|p| p.to_string()).collect();
            write!(f, " needs {}", req.join(" & "))?;
        }
        Ok(())
    }
}

/// How edges with an `Unknown` requirement are treated (spec §4.3).
#[derive(Clone, Copy)]
pub enum UnknownPolicy {
    /// Unknown counts as false: the edge is blocked.
    Pessimistic,
    /// Unknown is assumed true at a price: the route lists the assumptions.
    /// An infinite price means the fact is too unlikely to assume: it
    /// counts as false (the route is a blocked alternative needing it).
    Optimistic { penalty_of: fn(&Predicate) -> f64 },
}

#[derive(Debug, Clone, Default)]
pub struct RouteResult {
    /// Empty (with infinite cost) when nothing is open.
    pub legs: Vec<Leg>,
    pub cost_s: f64,
    /// Unknown predicates the route relies on (Optimistic only).
    pub assumes: Vec<Predicate>,
    /// Cheaper routes that need these unmet predicates, with their cost.
    pub blocked: Vec<(Vec<Predicate>, f64)>,
}

impl RouteResult {
    pub fn found(&self) -> bool {
        self.cost_s.is_finite()
    }
}

/// Blocked alternatives priced per route (one search each).
pub const MAX_BLOCKED_SETS: usize = 8;

pub const MOVE_SURF: &str = "MOVE_SURF";
pub const MOVE_FLY: &str = "MOVE_FLY";
pub const MOVE_CUT_NAME: &str = "MOVE_CUT";

/// What Surf takes: the move in the party and the Soul Badge.
pub fn surf_requirement() -> Requirement {
    vec![
        Predicate::PartyHasMove {
            mv: MOVE_SURF.to_string(),
        },
        Predicate::Badge { n: 5 },
    ]
}

/// What Fly to `dest` takes: the move, the Thunder Badge and having been there.
pub fn fly_requirement(dest: &str) -> Requirement {
    vec![
        Predicate::PartyHasMove {
            mv: MOVE_FLY.to_string(),
        },
        Predicate::Badge { n: 3 },
        Predicate::Visited {
            map: dest.to_string(),
        },
    ]
}

/// (map, how each predicate its ways need stands): a cached terrain.
type TerrainKey = (String, Vec<u8>);
/// Tiles reached, per map.
pub type TilesByMap = BTreeMap<String, BTreeSet<(i32, i32)>>;

/// `(map, from, surf, terrain hash)`: what a cached flood depends on.
type FloodKey = (String, (i32, i32), bool, u64);

/// One way past a blocker's tile: what it needs and what it costs on top
/// of the walk (a battle for trainers, nothing for hidden objects).
#[derive(Debug, Clone)]
struct Way {
    requires: Requirement,
    cost_s: f64,
}

struct MapInfo {
    /// Tiles of stationary objects nothing removes.
    walls: Obstacles,
    /// Tiles of stationary objects with a way past, and of gates the story
    /// opens (triggers, objects placed by scripts), by tile.
    blockers: BTreeMap<(i32, i32), Vec<Way>>,
    /// Tiles whose own collision a script lifts (an opened door), with the
    /// ways that open them.
    openable: BTreeMap<(i32, i32), Vec<Way>>,
    /// Every predicate a way past a blocker or an openable tile requires:
    /// what the map's terrain depends on.
    relevant: Vec<Predicate>,
    /// Digest of the walls.
    walls_hash: u64,
    wander: BTreeSet<(i32, i32)>,
    outdoor: bool,
}

/// A map's walkability for one search: the walls plus the blockers whose
/// ways the pass rules out, and the price of the tiles it may cross.
struct Terrain {
    obstacles: Obstacles,
    /// Collision tiles a way opens for this search.
    opened: Obstacles,
    /// Blocker tiles the pass may cross, with the cheapest allowed way.
    passable: BTreeMap<(i32, i32), Way>,
    hash: u64,
}

/// The place graph of a world (§5.1). Intra-map distances are searched on
/// demand and cached; everything else is built once.
pub struct PlaceGraph {
    params: RouteParams,
    places: BTreeMap<PlaceKey, Place>,
    by_map: BTreeMap<String, Vec<Place>>,
    edges: BTreeMap<PlaceKey, Vec<Edge>>,
    fly_spots: Vec<Place>,
    maps: BTreeMap<String, MapInfo>,
    floods: RefCell<HashMap<FloodKey, Rc<Reach>>>,
    /// Walk edges from a place to the other places of its map, per flood.
    walks: RefCell<HashMap<FloodKey, Rc<Vec<Edge>>>>,
    /// Terrains per (map, how each predicate it depends on stands).
    terrains: RefCell<HashMap<TerrainKey, Rc<Terrain>>>,
    /// Passages the story opens and closes ([`crate::gates`]).
    gates: Gates,
    /// Every predicate an edge or a passage requires, and the vars they
    /// compare: what a route can depend on.
    relevant: BTreeSet<Predicate>,
    relevant_vars: BTreeSet<String>,
    /// Maps whose entry script takes over on arrival (every path of an
    /// `on_frame` script that fires on each load fights or warps: the
    /// Champion's room): nobody walks there.
    intercepted: BTreeSet<String>,
}

impl PlaceGraph {
    pub fn build(world: &World, params: RouteParams) -> PlaceGraph {
        let mut g = PlaceGraph {
            params,
            places: BTreeMap::new(),
            by_map: BTreeMap::new(),
            edges: BTreeMap::new(),
            fly_spots: Vec::new(),
            maps: BTreeMap::new(),
            floods: RefCell::new(HashMap::new()),
            walks: RefCell::new(HashMap::new()),
            terrains: RefCell::new(HashMap::new()),
            gates: gates::derive(world),
            relevant: BTreeSet::new(),
            relevant_vars: BTreeSet::new(),
            intercepted: BTreeSet::new(),
        };
        let mut maps: Vec<&MapData> = world.maps().collect();
        maps.sort_by(|a, b| a.name.cmp(&b.name));
        for map in &maps {
            let mut walls = Obstacles::new();
            let mut blockers_by_tile: BTreeMap<(i32, i32), Vec<Way>> = BTreeMap::new();
            for b in blockers(map, world.events()) {
                if b.passages.is_empty() {
                    walls.insert((b.x, b.y));
                    continue;
                }
                let ways = b.passages.iter().map(|p| match p {
                    Passage::Hidden { flag } => Way {
                        requires: vec![Predicate::from_flag(flag, true)],
                        cost_s: 0.0,
                    },
                    // Trainer ids are flags in the game (`TRAINER_FLAGS_START
                    // + id`); the planner names defeated trainers this way.
                    Passage::Trainer { trainer } => Way {
                        requires: vec![Predicate::Flag {
                            name: trainer.clone(),
                            is: true,
                        }],
                        cost_s: params.battle_s,
                    },
                });
                blockers_by_tile.entry((b.x, b.y)).or_default().extend(ways);
            }
            // Two objects on one tile: a wall wins.
            blockers_by_tile.retain(|t, _| !walls.contains(t));
            let mut openable = BTreeMap::new();
            for (&tile, gate) in g.gates.get(&map.name).into_iter().flatten() {
                if walls.contains(&tile) {
                    continue;
                }
                let base_open = map.tile(tile.0, tile.1).is_some_and(|t| t.collision == 0);
                let ways = |reqs: &[Requirement]| -> Vec<Way> {
                    reqs.iter()
                        .map(|r| Way {
                            requires: r.clone(),
                            cost_s: 0.0,
                        })
                        .collect()
                };
                if !base_open {
                    if !gate.ways.is_empty() {
                        openable.insert(tile, ways(&gate.ways));
                    }
                    continue;
                }
                if gate.ways.iter().any(Vec::is_empty) {
                    continue;
                }
                if gate.ways.is_empty() {
                    walls.insert(tile);
                    blockers_by_tile.remove(&tile);
                    continue;
                }
                let gate_ways = ways(&gate.ways);
                match blockers_by_tile.get_mut(&tile) {
                    // An object on a gated tile: both must let the player by.
                    Some(existing) => {
                        let mut joint = Vec::new();
                        for a in existing.iter() {
                            for b in &gate_ways {
                                let mut requires = a.requires.clone();
                                requires.extend(b.requires.iter().cloned());
                                requires.sort();
                                requires.dedup();
                                joint.push(Way {
                                    requires,
                                    cost_s: a.cost_s + b.cost_s,
                                });
                            }
                        }
                        *existing = joint;
                    }
                    None => {
                        blockers_by_tile.insert(tile, gate_ways);
                    }
                }
            }
            let mut relevant: Vec<Predicate> = blockers_by_tile
                .values()
                .chain(openable.values())
                .flatten()
                .flat_map(|w| w.requires.iter().cloned())
                .collect();
            relevant.sort();
            relevant.dedup();
            let walls_hash = {
                let sorted: BTreeSet<_> = walls.iter().copied().collect();
                let mut h = std::hash::DefaultHasher::new();
                sorted.hash(&mut h);
                h.finish()
            };
            g.maps.insert(
                map.name.clone(),
                MapInfo {
                    walls,
                    blockers: blockers_by_tile,
                    openable,
                    relevant,
                    walls_hash,
                    wander: wander_tiles(map),
                    outdoor: map.is_outdoor(),
                },
            );
        }
        for map in &maps {
            g.add_warps(world, map);
            g.add_connections(world, map);
        }
        if let Some(places) = world.places() {
            for f in &places.fly_spots {
                if world.map(&f.map).is_some() {
                    let p = g.add_place(&f.map, f.x, f.y, PlaceKind::Fly);
                    g.fly_spots.push(p);
                }
            }
            g.fly_spots.sort();
            for h in &places.heal_spots {
                if world.map(&h.map).is_some() {
                    g.add_place(&h.map, h.x, h.y, PlaceKind::Heal);
                }
            }
            for gate in &places.gates {
                if let Some(map) = world.map(&gate.map) {
                    g.add_gate(map, gate);
                }
            }
        }
        if let Some(events) = world.events() {
            g.add_script_warps(world, events);
            for (map, ms) in &events.map_scripts {
                let takes_over = ms.on_frame.iter().any(|f| {
                    // Local vars are 0 on every load: such a frame always runs.
                    is_local_var(&f.var)
                        && f.value.as_int() == Some(0)
                        && f.script
                            .as_deref()
                            .and_then(|l| events.script(l))
                            .is_some_and(|s| {
                                !s.paths.is_empty()
                                    && s.paths.iter().all(|p| {
                                        p.does.iter().any(|e| {
                                            matches!(e, Effect::Warp { .. } | Effect::Battle { .. })
                                        })
                                    })
                            })
                });
                if takes_over {
                    g.intercepted.insert(map.clone());
                }
            }
        }
        for edges in g.edges.values_mut() {
            edges.sort_by(|a, b| (&a.kind, &a.to, &a.requires).cmp(&(&b.kind, &b.to, &b.requires)));
            edges.dedup_by(|a, b| a.kind == b.kind && a.to == b.to && a.requires == b.requires);
        }
        let mut relevant: BTreeSet<Predicate> = BTreeSet::new();
        relevant.extend(surf_requirement());
        for spot in &g.fly_spots {
            relevant.extend(fly_requirement(&spot.map));
        }
        for edges in g.edges.values() {
            for e in edges {
                relevant.extend(e.requires.iter().cloned());
            }
        }
        for info in g.maps.values() {
            for ways in info.blockers.values().chain(info.openable.values()) {
                for w in ways {
                    relevant.extend(w.requires.iter().cloned());
                }
            }
        }
        g.relevant_vars = relevant
            .iter()
            .filter_map(|p| match p {
                Predicate::Var { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        g.relevant = relevant;
        g
    }

    /// Whether the story opens or closes passages on `map` ([`crate::gates`]).
    pub fn has_gates(&self, map: &str) -> bool {
        self.gates.contains_key(map)
    }

    /// Whether a route can depend on `p` (an edge or passage requires it,
    /// or it sets a var one compares): the rest of a belief leaves routes
    /// unchanged, so caches may ignore it.
    pub fn route_relevant(&self, p: &Predicate) -> bool {
        match p {
            Predicate::Var { name, .. } => self.relevant_vars.contains(name),
            // Item counts compare against any threshold.
            Predicate::HasItem { item, .. } => self
                .relevant
                .iter()
                .any(|q| matches!(q, Predicate::HasItem { item: i, .. } if i == item)),
            Predicate::Flag { name, is } => {
                self.relevant.contains(p)
                    || self.relevant.contains(&Predicate::Flag {
                        name: name.clone(),
                        is: !is,
                    })
                    || Predicate::from_flag(name, true) != *p
                        && self.relevant.contains(&Predicate::from_flag(name, true))
            }
            _ => self.relevant.contains(p),
        }
    }

    pub fn params(&self) -> &RouteParams {
        &self.params
    }

    pub fn places(&self) -> impl Iterator<Item = &Place> {
        self.places.values()
    }

    /// Places on `map`, in (x, y) order.
    pub fn places_on(&self, map: &str) -> &[Place] {
        self.by_map.get(map).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Outgoing non-walk edges of the place at `map (x, y)`.
    pub fn edges_from(&self, map: &str, x: i32, y: i32) -> &[Edge] {
        self.edges
            .get(&(map.to_string(), x, y))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn add_place(&mut self, map: &str, x: i32, y: i32, kind: PlaceKind) -> Place {
        let key = (map.to_string(), x, y);
        if let Some(p) = self.places.get(&key) {
            return p.clone();
        }
        let p = Place {
            map: map.to_string(),
            x,
            y,
            kind,
        };
        self.places.insert(key, p.clone());
        let on = self.by_map.entry(map.to_string()).or_default();
        on.push(p.clone());
        on.sort();
        p
    }

    fn add_edge(&mut self, from: &Place, edge: Edge) {
        self.edges.entry(from.key()).or_default().push(edge);
    }

    /// A warp's place is its approach tile ([`warp_approach`]); the landing
    /// is the destination warp's tile itself (a door tile when leaving a
    /// building: the flood starts there and steps off it).
    fn add_warps(&mut self, world: &World, map: &MapData) {
        for (i, w) in map.warps.iter().enumerate() {
            let Some((dest, dx, dy)) = world.warp_destination(w) else {
                continue;
            };
            if !warp_usable(map, i) {
                continue;
            }
            let Some((ax, ay)) = warp_approach(map, i) else {
                continue;
            };
            if !map.in_bounds(ax, ay) {
                continue;
            }
            // A warp a script covers up or opens works in those states.
            let gate = self.gates.get(&map.name).and_then(|g| g.get(&(w.x, w.y)));
            let ways: Vec<Requirement> = match gate {
                Some(g) => match &g.warp_ways {
                    Some(ways) => ways.clone(),
                    None if (ax, ay) != (w.x, w.y) => g.ways.clone(),
                    None => vec![Vec::new()],
                },
                None => vec![Vec::new()],
            };
            if ways.is_empty() {
                continue;
            }
            let from = self.add_place(&map.name, ax, ay, PlaceKind::Warp);
            let to = self.add_place(&dest.name, dx, dy, PlaceKind::Warp);
            for requires in ways {
                self.add_edge(
                    &from,
                    Edge {
                        to: to.clone(),
                        kind: EdgeKind::Warp,
                        cost_s: self.params.warp_s,
                        requires,
                    },
                );
            }
        }
    }

    fn add_connections(&mut self, world: &World, map: &MapData) {
        for dir in Direction::ALL {
            let Some(conn) = map.connections.iter().find(|c| c.direction() == Some(dir)) else {
                continue;
            };
            let Some(other) = world.name_of(&conn.map).map(str::to_string) else {
                continue;
            };
            for ((ax, ay), (bx, by)) in world.crossings(map, dir) {
                let from = self.add_place(&map.name, ax, ay, PlaceKind::Crossing);
                let to = self.add_place(&other, bx, by, PlaceKind::Crossing);
                self.add_edge(
                    &from,
                    Edge {
                        to,
                        kind: EdgeKind::Connection,
                        cost_s: self.params.tile_s,
                        requires: Vec::new(),
                    },
                );
            }
        }
    }

    /// A Cut tree or Rock Smash rock joins the tiles on its opposite sides.
    /// Boulders (Strength) are puzzles, not crossings: left to the Sokoban
    /// solver (§7.2).
    fn add_gate(&mut self, map: &MapData, gate: &crate::places::Gate) {
        if gate.kind != "cut_tree" && gate.kind != "rock_smash" {
            return;
        }
        let requires = vec![
            Predicate::PartyHasMove {
                mv: gate.requires.r#move.clone(),
            },
            Predicate::from_flag(&gate.requires.badge, true),
        ];
        let (x, y) = (gate.x, gate.y);
        for (a, b) in [((x, y - 1), (x, y + 1)), ((x - 1, y), (x + 1, y))] {
            let obstacles = &self.maps[&map.name].walls;
            let open = |(x, y): (i32, i32)| {
                map.tile(x, y).is_some_and(|t| t.collision == 0) && !obstacles.contains(&(x, y))
            };
            if !open(a) || !open(b) {
                continue;
            }
            let pa = self.add_place(&map.name, a.0, a.1, PlaceKind::Gate);
            let pb = self.add_place(&map.name, b.0, b.1, PlaceKind::Gate);
            let cost_s = self.params.gate_s + 2.0 * self.params.tile_s;
            for (from, to) in [(&pa, &pb), (&pb, &pa)] {
                self.add_edge(
                    from,
                    Edge {
                        to: to.clone(),
                        kind: EdgeKind::Gate {
                            kind: gate.kind.clone(),
                        },
                        cost_s,
                        requires: requires.clone(),
                    },
                );
            }
        }
    }

    /// The values each var can hold: 0 at the start and whatever a script
    /// sets it to. A path comparing a var outside them never runs (the
    /// fall-through of a switch on the starter).
    fn var_domains(events: &crate::events::Events) -> BTreeMap<String, BTreeSet<i64>> {
        let mut out: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        for script in events.scripts.values() {
            for path in &script.paths {
                for e in &path.does {
                    if let Effect::Var { var, change } = e {
                        let values = out
                            .entry(var.clone())
                            .or_insert_with(|| BTreeSet::from([0]));
                        match change.eq.as_ref().and_then(Val::as_int) {
                            Some(v) => {
                                values.insert(v);
                            }
                            // Added to, or set from a symbol: anything.
                            None => {
                                values.insert(i64::MIN);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// Object, trigger and map-entry scripts whose paths warp: an edge per
    /// path whose conditions the belief can answer. A trigger's own var
    /// condition is part of the requirement; a map's `on_frame` script
    /// warps from wherever the player lands on the map. Paths that fight
    /// on the way are left to the goal planner (a battle is a step, not a
    /// walk).
    fn add_script_warps(&mut self, world: &World, events: &crate::events::Events) {
        let domains = Self::var_domains(events);
        for (label, script) in &events.scripts {
            let Some(map) = script.map.as_deref().and_then(|m| world.map(m)) else {
                continue;
            };
            let mut extra: Requirement = Vec::new();
            let origins: Vec<(i32, i32)> = match script.kind.as_str() {
                "object" => script
                    .local_id
                    .and_then(|id| map.objects.iter().find(|o| o.local_id == id))
                    .and_then(|o| Some((o.x?, o.y?)))
                    .and_then(|(x, y)| self.tile_beside(map, x, y))
                    .into_iter()
                    .collect(),
                // A panel (an elevator's floor menu): stood in front of.
                "sign" => map
                    .signs
                    .iter()
                    .filter(|s| s.script.as_deref() == Some(label.as_str()))
                    .filter_map(|s| self.tile_beside(map, s.x, s.y))
                    .take(1)
                    .collect(),
                "trigger" => {
                    let t = events
                        .triggers
                        .iter()
                        .find(|t| t.map == map.name && t.script.as_deref() == Some(label));
                    if let Some(t) = t {
                        match requirement_of(&t.when) {
                            Some(r) => extra = r,
                            None => continue,
                        }
                    }
                    t.map(|t| (t.x, t.y)).into_iter().collect()
                }
                "map" => {
                    let frame = events.map_scripts.get(&map.name).and_then(|ms| {
                        ms.on_frame
                            .iter()
                            .find(|f| f.script.as_deref() == Some(label.as_str()))
                    });
                    let Some(frame) = frame else { continue };
                    self.add_entry_walks(map, label, script);
                    if !is_local_var(&frame.var) {
                        let Some(value) = frame.value.as_int() else {
                            continue;
                        };
                        extra.push(Predicate::Var {
                            name: frame.var.clone(),
                            op: crate::predicate::CmpOp::Eq,
                            value,
                        });
                    }
                    self.places_on(&map.name)
                        .iter()
                        .filter(|p| p.kind == PlaceKind::Warp)
                        .map(|p| (p.x, p.y))
                        .collect()
                }
                _ => Vec::new(),
            };
            for (ox, oy) in origins {
                self.add_script_warps_from(world, map, label, script, (ox, oy), &extra, &domains);
            }
        }
    }

    /// A map's `on_frame` script that walks the player in from where the
    /// warps land (the Elite Four rooms, whose doors are walls to walk
    /// through): an edge from each landing to where the walk ends. The
    /// frame's own var condition is left out: the story always arrives in
    /// the state the walk is written for.
    fn add_entry_walks(&mut self, map: &MapData, label: &str, script: &crate::events::Script) {
        let landings: Vec<(i32, i32)> = self
            .places_on(&map.name)
            .iter()
            .filter(|p| p.kind == PlaceKind::Warp)
            .map(|p| (p.x, p.y))
            .collect();
        for path in &script.paths {
            if path
                .does
                .iter()
                .any(|e| matches!(e, Effect::Battle { .. } | Effect::Warp { .. }))
            {
                continue;
            }
            let (dx, dy) = path.does.iter().fold((0, 0), |(x, y), e| match e {
                Effect::MovePlayer { move_player } => (x + move_player.0, y + move_player.1),
                _ => (x, y),
            });
            if (dx, dy) == (0, 0) {
                continue;
            }
            let Some(requires) = requirement_of(&path.when) else {
                continue;
            };
            for &(ox, oy) in &landings {
                let (tx, ty) = (ox + dx, oy + dy);
                if !map.tile(tx, ty).is_some_and(|t| t.collision == 0) {
                    continue;
                }
                let from = self.add_place(&map.name, ox, oy, PlaceKind::Script);
                let to = self.add_place(&map.name, tx, ty, PlaceKind::Script);
                let tiles = (dx.abs() + dy.abs()) as f64;
                self.add_edge(
                    &from,
                    Edge {
                        to,
                        kind: EdgeKind::ScriptWarp {
                            script: label.to_string(),
                        },
                        cost_s: self.params.talk_s + tiles * self.params.tile_s,
                        requires: requires.clone(),
                    },
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_script_warps_from(
        &mut self,
        world: &World,
        map: &MapData,
        label: &str,
        script: &crate::events::Script,
        (ox, oy): (i32, i32),
        extra: &[Predicate],
        domains: &BTreeMap<String, BTreeSet<i64>>,
    ) {
        {
            for path in &script.paths {
                if path.does.iter().any(|e| matches!(e, Effect::Battle { .. })) {
                    continue;
                }
                let Some(mut requires) = requirement_of(&path.when) else {
                    continue;
                };
                // A var compared outside the values it can hold: never.
                let possible = |name: &str| -> Option<&BTreeSet<i64>> {
                    domains.get(name).filter(|d| !d.contains(&i64::MIN))
                };
                let mut vars: BTreeMap<&str, Vec<(crate::predicate::CmpOp, i64)>> = BTreeMap::new();
                for q in &requires {
                    if let Predicate::Var { name, op, value } = q {
                        vars.entry(name).or_default().push((*op, *value));
                    }
                }
                if vars.iter().any(|(name, cmps)| {
                    let domain = possible(name)
                        .cloned()
                        .unwrap_or_else(|| BTreeSet::from([0]));
                    !domain
                        .iter()
                        .any(|v| cmps.iter().all(|(op, x)| op.holds(*v, *x)))
                }) {
                    continue;
                }
                requires.extend(extra.iter().cloned());
                requires.sort();
                requires.dedup();
                // An elevator sets where its door leads, then the player
                // walks out: a warp all the same.
                let dynamic_exit = map.warps.iter().any(|w| w.dest_map == "MAP_DYNAMIC");
                for effect in &path.does {
                    let (warp, warp_id, x, y) = match effect {
                        Effect::Warp {
                            warp,
                            warp_id,
                            x,
                            y,
                        } => (warp, warp_id, x, y),
                        Effect::SetWarp {
                            set_warp,
                            warp_id,
                            x,
                            y,
                        } if dynamic_exit => (set_warp, warp_id, x, y),
                        _ => continue,
                    };
                    let Some(dest) = world.name_of(warp).and_then(|n| world.map(n)) else {
                        continue;
                    };
                    let tile = match (
                        x.as_ref().and_then(Val::as_int),
                        y.as_ref().and_then(Val::as_int),
                    ) {
                        (Some(x), Some(y)) => Some((x as i32, y as i32)),
                        _ => warp_id
                            .as_ref()
                            .and_then(Val::as_int)
                            .and_then(|id| dest.warps.get(usize::try_from(id).ok()?))
                            .map(|w| (w.x, w.y)),
                    };
                    let Some((dx, dy)) = tile else {
                        continue;
                    };
                    if !dest.in_bounds(dx, dy) {
                        continue;
                    }
                    let from = self.add_place(&map.name, ox, oy, PlaceKind::Script);
                    let to = self.add_place(&dest.name, dx, dy, PlaceKind::Warp);
                    self.add_edge(
                        &from,
                        Edge {
                            to,
                            kind: EdgeKind::ScriptWarp {
                                script: label.to_string(),
                            },
                            cost_s: self.params.talk_s + self.params.warp_s,
                            requires: requires.clone(),
                        },
                    );
                    // One warp per path is all a script does before it ends.
                    break;
                }
            }
        }
    }

    /// The tile the player talks to an object from: below, above, left,
    /// right, the first that is open.
    fn tile_beside(&self, map: &MapData, x: i32, y: i32) -> Option<(i32, i32)> {
        let info = &self.maps[&map.name];
        [(x, y + 1), (x, y - 1), (x - 1, y), (x + 1, y)]
            .into_iter()
            .find(|&(nx, ny)| {
                map.tile(nx, ny)
                    .is_some_and(|t| t.collision == 0 && !is_water(t.behavior))
                    && !info.walls.contains(&(nx, ny))
            })
    }

    /// The map's terrain for one search: each blocker's cheapest way the
    /// pass allows (an ordinary tile once its requirement holds), the rest
    /// walls.
    fn terrain(
        &self,
        map: &str,
        belief: &dyn BeliefView,
        policy: UnknownPolicy,
        pass: Pass,
    ) -> Terrain {
        let info = &self.maps[map];
        let mut obstacles = info.walls.clone();
        let mut passable = BTreeMap::new();
        for (&tile, ways) in &info.blockers {
            let mut best: Option<(usize, Way)> = None;
            for way in ways {
                let c = check(belief, &way.requires);
                let unmet = unmet_of(&c, policy);
                if !pass.allows(&unmet) {
                    continue;
                }
                let held = c.truth() == Truth::True;
                let cost_s = if held { 0.0 } else { way.cost_s };
                // The way that lacks the least, then the cheapest.
                let better = best
                    .as_ref()
                    .is_none_or(|(n, b)| (unmet.len(), cost_s) < (*n, b.cost_s));
                if better {
                    best = Some((
                        unmet.len(),
                        Way {
                            requires: if held {
                                Vec::new()
                            } else {
                                way.requires.clone()
                            },
                            cost_s,
                        },
                    ));
                }
            }
            match best.map(|(_, w)| w) {
                Some(way) => {
                    passable.insert(tile, way);
                }
                None => {
                    obstacles.insert(tile);
                }
            }
        }
        let mut opened = Obstacles::new();
        for (&tile, ways) in &info.openable {
            let mut best: Option<(usize, Way)> = None;
            for way in ways {
                let c = check(belief, &way.requires);
                let unmet = unmet_of(&c, policy);
                if !pass.allows(&unmet) {
                    continue;
                }
                let held = c.truth() == Truth::True;
                let requires = if held {
                    Vec::new()
                } else {
                    way.requires.clone()
                };
                let key = (unmet.len(), way.cost_s);
                if best.as_ref().is_none_or(|(n, b)| key < (*n, b.cost_s)) {
                    best = Some((
                        unmet.len(),
                        Way {
                            requires,
                            cost_s: way.cost_s,
                        },
                    ));
                }
            }
            if let Some((_, way)) = best {
                opened.insert(tile);
                passable.insert(tile, way);
            }
        }
        let hash = {
            let mut h = std::hash::DefaultHasher::new();
            info.walls_hash.hash(&mut h);
            let blocked: BTreeSet<_> = obstacles
                .iter()
                .filter(|t| !info.walls.contains(t))
                .copied()
                .collect();
            blocked.hash(&mut h);
            for (tile, way) in &passable {
                tile.hash(&mut h);
                way.requires.hash(&mut h);
                way.cost_s.to_bits().hash(&mut h);
            }
            h.finish()
        };
        Terrain {
            obstacles,
            opened,
            passable,
            hash,
        }
    }

    /// [`PlaceGraph::terrain`], cached by how the predicates the map's ways
    /// require stand for this search (held, assumed, lacking and allowed,
    /// lacking and not): the rest of the belief doesn't change it.
    fn cached_terrain(
        &self,
        map: &str,
        belief: &dyn BeliefView,
        policy: UnknownPolicy,
        pass: Pass,
    ) -> Rc<Terrain> {
        let info = &self.maps[map];
        let codes: Vec<u8> = info
            .relevant
            .iter()
            .map(|p| {
                let c = check(belief, std::slice::from_ref(p));
                let unmet = unmet_of(&c, policy);
                if c.truth() == Truth::True {
                    0
                } else if unmet.is_empty() {
                    1
                } else if pass.allows(&unmet) {
                    3
                } else {
                    2
                }
            })
            .collect();
        let key = (map.to_string(), codes);
        if let Some(t) = self.terrains.borrow().get(&key) {
            return Rc::clone(t);
        }
        let t = Rc::new(self.terrain(map, belief, policy, pass));
        self.terrains.borrow_mut().insert(key, Rc::clone(&t));
        t
    }

    /// The flood from `from` on `map` over `terrain`, from the cache when
    /// it has it. The flood starts on `from` even when it is a tile the
    /// player can't walk onto (a door just left).
    fn flood(&self, map: &MapData, from: (i32, i32), surf: bool, terrain: &Terrain) -> Rc<Reach> {
        let info = &self.maps[&map.name];
        let key = (map.name.clone(), from, surf, terrain.hash);
        if let Some(r) = self.floods.borrow().get(&key) {
            return Rc::clone(r);
        }
        let penalty = self.params.wander_area_penalty_tiles as i32;
        let tile_s = self.params.tile_s;
        let extra = |t: (i32, i32)| {
            let wander = if info.wander.contains(&t) { penalty } else { 0 };
            // Land is preferred over water of the same length: no mount.
            let water = map
                .tile(t.0, t.1)
                .is_some_and(|tile| is_water(tile.behavior)) as i32;
            // A blocker's way, in tiles, so the flood detours when it can;
            // at least one so a free path wins ties against a requirement.
            let way = terrain.passable.get(&t).map_or(0, |w| {
                if w.requires.is_empty() {
                    0
                } else {
                    ((w.cost_s / tile_s).round() as i32).max(1)
                }
            });
            wander + water + way
        };
        let walk = Walk {
            obstacles: &terrain.obstacles,
            surf,
            opened: Some(&terrain.opened),
        };
        let r = Rc::new(reach(map, from, &walk, extra));
        self.floods.borrow_mut().insert(key, Rc::clone(&r));
        r
    }

    /// The walk edge from `from` to `to` on `map` along the flood, priced;
    /// blocker tiles crossed add their requirement and cost.
    fn walk_edge(
        &self,
        map: &MapData,
        from: (i32, i32),
        flood: &Reach,
        terrain: &Terrain,
        to: &Place,
    ) -> Option<Edge> {
        let steps = flood.path((to.x, to.y))?;
        if steps.is_empty() {
            return None;
        }
        let info = &self.maps[&map.name];
        let mut tiles = 0u32;
        let mut wander = 0u32;
        let mut surf = false;
        let mut requires = Requirement::new();
        let mut ways_s = 0.0;
        let mut prev = from;
        for s in &steps {
            tiles += step_len(prev, s.to);
            wander += info.wander.contains(&s.to) as u32;
            surf |= map
                .tile(s.to.0, s.to.1)
                .is_some_and(|t| is_water(t.behavior));
            if let Some(way) = terrain.passable.get(&s.to) {
                requires.extend(way.requires.iter().cloned());
                ways_s += way.cost_s;
            }
            prev = s.to;
        }
        if surf {
            requires.extend(surf_requirement());
        }
        requires.sort();
        requires.dedup();
        let charged = tiles + wander * self.params.wander_area_penalty_tiles;
        let cost_s = charged as f64 * self.params.tile_s
            + if surf { self.params.surf_s } else { 0.0 }
            + ways_s;
        Some(Edge {
            to: to.clone(),
            kind: EdgeKind::Walk { tiles, surf },
            cost_s,
            requires,
        })
    }
}

fn step_len(from: (i32, i32), to: (i32, i32)) -> u32 {
    ((to.0 - from.0).abs() + (to.1 - from.1).abs()) as u32
}

/// The belief-checkable requirement of a script path, or `None` when a
/// condition can't be expressed (the path is then not an edge). The
/// player's own answers and facing are free choices, not conditions.
pub fn requirement_of(when: &[Condition]) -> Option<Requirement> {
    let mut req = Vec::new();
    for c in when {
        match c {
            // Script-local state is the script's own business.
            Condition::Flag { flag, .. } if is_local_flag(flag) => {}
            Condition::Var { var, .. } if is_local_var(var) => {}
            Condition::Flag { flag, is } => req.push(Predicate::from_flag(flag, *is)),
            // Trainer ids are flags in the game (`TRAINER_FLAGS_START + id`).
            Condition::Trainer { trainer, defeated } => req.push(Predicate::Flag {
                name: trainer.clone(),
                is: *defeated,
            }),
            Condition::Item {
                item,
                count,
                has: true,
            } => req.push(Predicate::HasItem {
                item: item.clone(),
                n: (*count).max(1) as u32,
            }),
            Condition::Move {
                r#move,
                known: true,
            } => req.push(Predicate::PartyHasMove { mv: r#move.clone() }),
            Condition::Answer { .. } | Condition::Choice { .. } => {}
            // What a menu or a special returns (an elevator's floor list
            // position), bag space, money: the player's side of the
            // script, not a fact to route on.
            Condition::Special { .. }
            | Condition::ResultOf { .. }
            | Condition::BagSpace { .. }
            | Condition::Money { .. }
            | Condition::Coins { .. }
            | Condition::PartySize { .. } => {}
            Condition::Var { var, .. } if var == "VAR_FACING" => {}
            Condition::Var { var, cmp } => {
                let ops = [
                    (&cmp.eq, crate::predicate::CmpOp::Eq),
                    (&cmp.ne, crate::predicate::CmpOp::Ne),
                    (&cmp.lt, crate::predicate::CmpOp::Lt),
                    (&cmp.gt, crate::predicate::CmpOp::Gt),
                    (&cmp.le, crate::predicate::CmpOp::Le),
                    (&cmp.ge, crate::predicate::CmpOp::Ge),
                ];
                let (val, op) = ops
                    .into_iter()
                    .find_map(|(v, op)| v.as_ref().map(|v| (v, op)))?;
                req.push(Predicate::Var {
                    name: var.clone(),
                    op,
                    value: val.as_int()?,
                });
            }
            _ => return None,
        }
    }
    req.sort();
    req.dedup();
    Some(req)
}

/// f64 with a total order, for the open set.
#[derive(Clone, Copy, PartialEq)]
struct Cost(f64);

impl Eq for Cost {}

impl PartialOrd for Cost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cost {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Which edges a pass may take.
#[derive(Clone, Copy)]
enum Pass<'a> {
    /// Only edges whose requirement is met.
    Open,
    /// Any edge, except ones needing a predicate already priced.
    Forbid(&'a [Predicate]),
}

impl Pass<'_> {
    fn allows(self, unmet: &[Predicate]) -> bool {
        match self {
            Pass::Open => unmet.is_empty(),
            Pass::Forbid(f) => !unmet.iter().any(|p| f.contains(p)),
        }
    }
}

struct Found {
    legs: Vec<Leg>,
    cost_s: f64,
    assumes: Vec<Predicate>,
    /// Unmet predicates of the edges taken (empty for an open route).
    unmet: Vec<Predicate>,
}

/// What a search runs to: one place, or any place on a map.
#[derive(Clone, Copy)]
enum Goal<'a> {
    Place(&'a Place),
    Map(&'a str),
    /// Explore everything reachable.
    Nowhere,
    /// Any of these tiles of a map (the spots around an NPC).
    Tiles(&'a [Place]),
}

impl Goal<'_> {
    fn reached(self, key: &PlaceKey) -> bool {
        match self {
            Goal::Place(p) => *key == p.key(),
            Goal::Map(m) => key.0 == m,
            Goal::Nowhere => false,
            Goal::Tiles(ts) => ts.iter().any(|p| *key == p.key()),
        }
    }

    /// The goal's tiles that are no place of the graph (walked to
    /// explicitly when the search is on their map).
    fn tiles(&self) -> &[Place] {
        match self {
            Goal::Place(p) => std::slice::from_ref(*p),
            Goal::Tiles(ts) => ts,
            _ => &[],
        }
    }
}

/// Every map the player can reach from `from` over edges whose
/// requirements hold (under `policy` for unknown ones).
pub fn reachable_maps(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    policy: UnknownPolicy,
) -> BTreeSet<String> {
    reachable(world, graph, belief, from, policy).0
}

/// [`reachable_maps`], plus for each reached map whose passages the story
/// opens ([`PlaceGraph::has_gates`]) the tiles reached on it.
pub fn reachable(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    policy: UnknownPolicy,
) -> (BTreeSet<String>, TilesByMap) {
    let mut places = BTreeSet::new();
    let start = Place::from(from);
    search(
        world,
        graph,
        belief,
        &start,
        Goal::Nowhere,
        policy,
        Pass::Open,
        Some(&mut places),
    );
    places.insert((from.map.clone(), from.x, from.y));
    let maps: BTreeSet<String> = places.iter().map(|k| k.0.clone()).collect();
    let mut tiles: BTreeMap<String, BTreeSet<(i32, i32)>> = BTreeMap::new();
    for (name, x, y) in &places {
        if !graph.has_gates(name) || graph.intercepted.contains(name) {
            continue;
        }
        let (Some(map), true) = (world.map(name), graph.maps.contains_key(name)) else {
            continue;
        };
        let terrain = graph.cached_terrain(name, belief, policy, Pass::Open);
        let surf_ok = Pass::Open.allows(&unmet_of(&check(belief, &surf_requirement()), policy));
        let flood = graph.flood(map, (*x, *y), surf_ok, &terrain);
        tiles.entry(name.clone()).or_default().extend(flood.tiles());
    }
    (maps, tiles)
}

/// Cheapest route from `from` to `to` (§5.2). Dijkstra over places (no
/// admissible cross-map heuristic beats Fly), deterministic: ties break by
/// (cost, map, x, y, edge kind).
///
/// `blocked` lists routes that beat the open one but need predicates the
/// belief denies (or, under `Pessimistic`, can't answer): the best route
/// over every edge with the unmet predicates it uses, then the best that
/// avoids those, and so on (at most [`MAX_BLOCKED_SETS`] entries), so the
/// entries come in cost order and each names exactly what it needs.
pub fn route(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    to: &Place,
    policy: UnknownPolicy,
) -> RouteResult {
    plan(world, graph, belief, from, Goal::Place(to), policy)
}

/// Cheapest route from `from` to any place on `map` (the landing of a warp
/// or connection into it, a heal spot...): a map's landings may lie in
/// parts of it that don't connect, so `At{map}` is whichever is cheapest.
/// Empty at no cost when the player is already there.
pub fn route_to_map(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    map: &str,
    policy: UnknownPolicy,
) -> RouteResult {
    plan(world, graph, belief, from, Goal::Map(map), policy)
}

/// [`route_to_map`] without the blocked alternatives: only the open route
/// (one search instead of several).
pub fn open_route_to_map(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    map: &str,
    policy: UnknownPolicy,
) -> RouteResult {
    plan_with(world, graph, belief, from, Goal::Map(map), policy, false)
}

/// The open route to the nearest (cheapest) of `tiles` of `map`, one
/// search for all of them; no blocked alternatives.
pub fn open_route_to_tiles(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    map: &str,
    tiles: &[(i32, i32)],
    policy: UnknownPolicy,
) -> RouteResult {
    let places: Vec<Place> = tiles.iter().map(|&(x, y)| Place::tile(map, x, y)).collect();
    plan_with(
        world,
        graph,
        belief,
        from,
        Goal::Tiles(&places),
        policy,
        false,
    )
}

/// [`route`] without the blocked alternatives.
pub fn open_route(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    to: &Place,
    policy: UnknownPolicy,
) -> RouteResult {
    plan_with(world, graph, belief, from, Goal::Place(to), policy, false)
}

fn plan(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    goal: Goal,
    policy: UnknownPolicy,
) -> RouteResult {
    plan_with(world, graph, belief, from, goal, policy, true)
}

fn plan_with(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    goal: Goal,
    policy: UnknownPolicy,
    with_blocked: bool,
) -> RouteResult {
    let start = Place::from(from);
    let open = search(world, graph, belief, &start, goal, policy, Pass::Open, None);
    let open_cost = open.as_ref().map_or(f64::INFINITY, |f| f.cost_s);
    let mut blocked = Vec::new();
    let mut forbidden: Vec<Predicate> = Vec::new();
    while with_blocked && blocked.len() < MAX_BLOCKED_SETS {
        let alt = search(
            world,
            graph,
            belief,
            &start,
            goal,
            policy,
            Pass::Forbid(&forbidden),
            None,
        );
        let Some(alt) = alt else { break };
        if alt.unmet.is_empty() || alt.cost_s >= open_cost {
            break;
        }
        forbidden.extend(alt.unmet.iter().cloned());
        blocked.push((alt.unmet, alt.cost_s));
    }
    match open {
        Some(f) => RouteResult {
            legs: f.legs,
            cost_s: f.cost_s,
            assumes: f.assumes,
            blocked,
        },
        None => RouteResult {
            legs: Vec::new(),
            cost_s: f64::INFINITY,
            assumes: Vec::new(),
            blocked,
        },
    }
}

/// A settled node's way in: the leg, what it assumed and what it lacked.
type Came = HashMap<PlaceKey, (PlaceKey, Leg, Vec<Predicate>, Vec<Predicate>)>;

#[allow(clippy::too_many_arguments)]
fn search(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    start: &Place,
    goal: Goal,
    policy: UnknownPolicy,
    pass: Pass,
    settled: Option<&mut BTreeSet<PlaceKey>>,
) -> Option<Found> {
    let surf_ok = pass.allows(&unmet_of(&check(belief, &surf_requirement()), policy));
    let mut dist: HashMap<PlaceKey, f64> = HashMap::new();
    let mut came: Came = HashMap::new();
    let mut done: std::collections::HashSet<PlaceKey> = std::collections::HashSet::new();
    let mut terrains: BTreeMap<String, Rc<Terrain>> = BTreeMap::new();
    let mut open = BinaryHeap::new();
    dist.insert(start.key(), 0.0);
    open.push(Reverse((Cost(0.0), start.key())));
    let mut relax = |from: &Place,
                     edge: &Edge,
                     g: f64,
                     dist: &mut HashMap<PlaceKey, f64>,
                     open: &mut BinaryHeap<Reverse<(Cost, PlaceKey)>>| {
        let (unmet, assumed, penalty) = if edge.requires.is_empty() {
            (Vec::new(), Vec::new(), 0.0)
        } else {
            let c = check(belief, &edge.requires);
            let unmet = unmet_of(&c, policy);
            if !pass.allows(&unmet) {
                return;
            }
            let mut assumed = Vec::new();
            let mut penalty = 0.0;
            if let UnknownPolicy::Optimistic { penalty_of } = policy {
                for p in &c.unknown {
                    let price = penalty_of(p);
                    if price.is_finite() {
                        penalty += price;
                        assumed.push(p.clone());
                    }
                }
            }
            (unmet, assumed, penalty)
        };
        let ng = g + edge.cost_s + penalty;
        let key = edge.to.key();
        let better = match dist.get(&key) {
            None => true,
            Some(&old) => {
                ng < old
                    || (ng == old
                        && came
                            .get(&key)
                            .is_some_and(|(_, leg, _, _)| edge.kind < leg.kind))
            }
        };
        if better {
            dist.insert(key.clone(), ng);
            came.insert(
                key.clone(),
                (
                    from.key(),
                    Leg {
                        from: from.clone(),
                        to: edge.to.clone(),
                        kind: edge.kind.clone(),
                        cost_s: edge.cost_s,
                        requires: edge.requires.clone(),
                    },
                    assumed,
                    unmet,
                ),
            );
            open.push(Reverse((Cost(ng), key)));
        }
    };
    let mut reached: Option<PlaceKey> = None;
    let mut fly_edges: Option<Vec<Edge>> = None;
    while let Some(Reverse((Cost(g), key))) = open.pop() {
        if done.contains(&key) {
            continue;
        }
        if goal.reached(&key) {
            reached = Some(key);
            break;
        }
        done.insert(key.clone());
        let here = match graph.places.get(&key) {
            Some(p) => p.clone(),
            None if key == start.key() => start.clone(),
            None => continue,
        };
        let Some(map) = world.map(&here.map) else {
            continue;
        };
        if !graph.maps.contains_key(&here.map) {
            continue;
        }
        if graph.intercepted.contains(&here.map) && key != start.key() {
            // Arriving here hands the player to the map's script.
            for edge in graph.edges_from(&here.map, here.x, here.y) {
                if edge.to.map != here.map && !done.contains(&edge.to.key()) {
                    relax(&here, edge, g, &mut dist, &mut open);
                }
            }
            continue;
        }
        let terrain = terrains
            .entry(here.map.clone())
            .or_insert_with(|| graph.cached_terrain(&here.map, belief, policy, pass))
            .clone();
        // Walk to the map's other places (and the goal if it is here).
        let flood = graph.flood(map, (here.x, here.y), surf_ok, &terrain);
        let walk_key = (here.map.clone(), (here.x, here.y), surf_ok, terrain.hash);
        let cached = graph.walks.borrow().get(&walk_key).map(Rc::clone);
        let walks = match cached {
            Some(w) => w,
            None => {
                let w: Vec<Edge> = graph
                    .places_on(&here.map)
                    .iter()
                    .filter(|t| t.key() != key)
                    .filter_map(|t| graph.walk_edge(map, (here.x, here.y), &flood, &terrain, t))
                    .collect();
                let w = Rc::new(w);
                graph.walks.borrow_mut().insert(walk_key, Rc::clone(&w));
                w
            }
        };
        for edge in walks.iter() {
            if !done.contains(&edge.to.key()) {
                relax(&here, edge, g, &mut dist, &mut open);
            }
        }
        for to in goal.tiles() {
            if to.map == here.map
                && !graph.places.contains_key(&to.key())
                && to.key() != key
                && !done.contains(&to.key())
            {
                if let Some(edge) = graph.walk_edge(map, (here.x, here.y), &flood, &terrain, to) {
                    relax(&here, &edge, g, &mut dist, &mut open);
                }
            }
        }
        for edge in graph.edges_from(&here.map, here.x, here.y) {
            if !done.contains(&edge.to.key()) {
                relax(&here, edge, g, &mut dist, &mut open);
            }
        }
        if graph.maps[&here.map].outdoor {
            // The Fly edges this search may take, worked out once: the same
            // from every outdoor place.
            let flights = fly_edges.get_or_insert_with(|| {
                graph
                    .fly_spots
                    .iter()
                    .filter(|spot| {
                        // Flying somewhere never visited (or unlikely to
                        // have been) means going there first: never the
                        // cheaper way, so not even a blocked one. A visit
                        // the belief can't tell stays a blocked
                        // alternative under the pessimistic policy: a look
                        // at the fly map settles it.
                        let visited = Predicate::Visited {
                            map: spot.map.clone(),
                        };
                        let never = match belief.eval(&visited) {
                            Truth::False => true,
                            Truth::Unknown => match policy {
                                UnknownPolicy::Optimistic { penalty_of } => {
                                    !penalty_of(&visited).is_finite()
                                }
                                UnknownPolicy::Pessimistic => false,
                            },
                            Truth::True => false,
                        };
                        let requires = fly_requirement(&spot.map);
                        !never && pass.allows(&unmet_of(&check(belief, &requires), policy))
                    })
                    .map(|spot| Edge {
                        to: spot.clone(),
                        kind: EdgeKind::Fly,
                        cost_s: graph.params.fly_s,
                        requires: fly_requirement(&spot.map),
                    })
                    .collect()
            });
            for edge in flights.iter() {
                if edge.to.key() == key || done.contains(&edge.to.key()) {
                    continue;
                }
                relax(&here, edge, g, &mut dist, &mut open);
            }
        }
    }
    if let Some(out) = settled {
        out.extend(done.iter().cloned());
    }
    let reached = reached?;
    let cost_s = *dist.get(&reached)?;
    let mut legs = Vec::new();
    let mut assumes = Vec::new();
    let mut unmet = Vec::new();
    let mut at = reached;
    while let Some((prev, leg, assumed, lacked)) = came.get(&at) {
        legs.push(leg.clone());
        assumes.extend(assumed.iter().cloned());
        unmet.extend(lacked.iter().cloned());
        at = prev.clone();
    }
    legs.reverse();
    let legs = coalesce_walks(legs);
    assumes.sort();
    assumes.dedup();
    unmet.sort();
    unmet.dedup();
    Some(Found {
        legs,
        cost_s,
        assumes,
        unmet,
    })
}

/// Walks through intermediate places of one map become a single leg (one
/// `Go`); Dijkstra priced them as the sum of shortest paths, which equals
/// the direct one it would otherwise have taken.
fn coalesce_walks(legs: Vec<Leg>) -> Vec<Leg> {
    let mut out: Vec<Leg> = Vec::new();
    for leg in legs {
        if let (Some(last), EdgeKind::Walk { tiles, surf }) = (out.last_mut(), &leg.kind) {
            if let EdgeKind::Walk {
                tiles: t0,
                surf: s0,
            } = &mut last.kind
            {
                if last.to.map == leg.from.map {
                    *t0 += tiles;
                    *s0 |= surf;
                    last.to = leg.to;
                    last.cost_s += leg.cost_s;
                    for p in leg.requires {
                        if !last.requires.contains(&p) {
                            last.requires.push(p);
                        }
                    }
                    continue;
                }
            }
        }
        out.push(leg);
    }
    out
}

/// The predicates an edge lacks under `policy`.
fn unmet_of(c: &crate::predicate::Check, policy: UnknownPolicy) -> Vec<Predicate> {
    let mut unmet = c.failed.clone();
    match policy {
        UnknownPolicy::Pessimistic => unmet.extend(c.unknown.iter().cloned()),
        UnknownPolicy::Optimistic { penalty_of } => unmet.extend(
            c.unknown
                .iter()
                .filter(|p| !penalty_of(p).is_finite())
                .cloned(),
        ),
    }
    unmet.sort();
    unmet.dedup();
    unmet
}
