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
    /// Tiles of stationary objects with a way past, by tile.
    blockers: BTreeMap<(i32, i32), Vec<Way>>,
    wander: BTreeSet<(i32, i32)>,
    outdoor: bool,
}

/// A map's walkability for one search: the walls plus the blockers whose
/// ways the pass rules out, and the price of the tiles it may cross.
struct Terrain {
    obstacles: Obstacles,
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
            g.maps.insert(
                map.name.clone(),
                MapInfo {
                    walls,
                    blockers: blockers_by_tile,
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
        }
        for edges in g.edges.values_mut() {
            edges.sort_by(|a, b| (&a.kind, &a.to, &a.requires).cmp(&(&b.kind, &b.to, &b.requires)));
            edges.dedup_by(|a, b| a.kind == b.kind && a.to == b.to && a.requires == b.requires);
        }
        g
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
            let from = self.add_place(&map.name, ax, ay, PlaceKind::Warp);
            let to = self.add_place(&dest.name, dx, dy, PlaceKind::Warp);
            self.add_edge(
                &from,
                Edge {
                    to,
                    kind: EdgeKind::Warp,
                    cost_s: self.params.warp_s,
                    requires: Vec::new(),
                },
            );
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

    /// Object and trigger scripts whose paths warp: an edge per path whose
    /// conditions the belief can answer.
    fn add_script_warps(&mut self, world: &World, events: &crate::events::Events) {
        for (label, script) in &events.scripts {
            let Some(map) = script.map.as_deref().and_then(|m| world.map(m)) else {
                continue;
            };
            let origin = match script.kind.as_str() {
                "object" => script
                    .local_id
                    .and_then(|id| map.objects.iter().find(|o| o.local_id == id))
                    .and_then(|o| Some((o.x?, o.y?)))
                    .and_then(|(x, y)| self.tile_beside(map, x, y)),
                "trigger" => events
                    .triggers
                    .iter()
                    .find(|t| t.map == map.name && t.script.as_deref() == Some(label))
                    .map(|t| (t.x, t.y)),
                _ => None,
            };
            let Some((ox, oy)) = origin else {
                continue;
            };
            for path in &script.paths {
                let Some(requires) = requirement_of(&path.when) else {
                    continue;
                };
                for effect in &path.does {
                    let Effect::Warp {
                        warp,
                        warp_id,
                        x,
                        y,
                        ..
                    } = effect
                    else {
                        continue;
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
                                script: label.clone(),
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
            let mut best: Option<Way> = None;
            for way in ways {
                let c = check(belief, &way.requires);
                if !pass.allows(&unmet_of(&c, policy)) {
                    continue;
                }
                let held = c.truth() == Truth::True;
                let cost_s = if held { 0.0 } else { way.cost_s };
                if best.as_ref().is_none_or(|b| cost_s < b.cost_s) {
                    best = Some(Way {
                        requires: if held {
                            Vec::new()
                        } else {
                            way.requires.clone()
                        },
                        cost_s,
                    });
                }
            }
            match best {
                Some(way) => {
                    passable.insert(tile, way);
                }
                None => {
                    obstacles.insert(tile);
                }
            }
        }
        let hash = {
            let sorted: BTreeSet<_> = obstacles.iter().copied().collect();
            let mut h = std::hash::DefaultHasher::new();
            sorted.hash(&mut h);
            for (tile, way) in &passable {
                tile.hash(&mut h);
                way.requires.hash(&mut h);
                way.cost_s.to_bits().hash(&mut h);
            }
            h.finish()
        };
        Terrain {
            obstacles,
            passable,
            hash,
        }
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
fn requirement_of(when: &[Condition]) -> Option<Requirement> {
    let mut req = Vec::new();
    for c in when {
        match c {
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
}

impl Goal<'_> {
    fn reached(self, key: &PlaceKey) -> bool {
        match self {
            Goal::Place(p) => *key == p.key(),
            Goal::Map(m) => key.0 == m,
        }
    }
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

fn plan(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    from: &PlayerPose,
    goal: Goal,
    policy: UnknownPolicy,
) -> RouteResult {
    let start = Place::from(from);
    let open = search(world, graph, belief, &start, goal, policy, Pass::Open);
    let open_cost = open.as_ref().map_or(f64::INFINITY, |f| f.cost_s);
    let mut blocked = Vec::new();
    let mut forbidden: Vec<Predicate> = Vec::new();
    while blocked.len() < MAX_BLOCKED_SETS {
        let alt = search(
            world,
            graph,
            belief,
            &start,
            goal,
            policy,
            Pass::Forbid(&forbidden),
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
type Came = BTreeMap<PlaceKey, (PlaceKey, Leg, Vec<Predicate>, Vec<Predicate>)>;

fn search(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    start: &Place,
    goal: Goal,
    policy: UnknownPolicy,
    pass: Pass,
) -> Option<Found> {
    let surf_ok = pass.allows(&unmet_of(&check(belief, &surf_requirement()), policy));
    let mut dist: BTreeMap<PlaceKey, f64> = BTreeMap::new();
    let mut came: Came = BTreeMap::new();
    let mut done: BTreeSet<PlaceKey> = BTreeSet::new();
    let mut terrains: BTreeMap<String, Rc<Terrain>> = BTreeMap::new();
    let mut open = BinaryHeap::new();
    dist.insert(start.key(), 0.0);
    open.push(Reverse((Cost(0.0), start.key())));
    let mut relax = |from: &Place,
                     edge: Edge,
                     g: f64,
                     dist: &mut BTreeMap<PlaceKey, f64>,
                     open: &mut BinaryHeap<Reverse<(Cost, PlaceKey)>>| {
        let c = check(belief, &edge.requires);
        let unmet = unmet_of(&c, policy);
        if !pass.allows(&unmet) {
            return;
        }
        let mut assumed = Vec::new();
        let mut penalty = 0.0;
        if let UnknownPolicy::Optimistic { penalty_of } = policy {
            for p in &c.unknown {
                penalty += penalty_of(p);
                assumed.push(p.clone());
            }
        }
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
                        to: edge.to,
                        kind: edge.kind,
                        cost_s: edge.cost_s,
                        requires: edge.requires,
                    },
                    assumed,
                    unmet,
                ),
            );
            open.push(Reverse((Cost(ng), key)));
        }
    };
    let mut reached: Option<PlaceKey> = None;
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
        let terrain = terrains
            .entry(here.map.clone())
            .or_insert_with(|| Rc::new(graph.terrain(&here.map, belief, policy, pass)))
            .clone();
        // Walk to the map's other places (and the goal if it is here).
        let flood = graph.flood(map, (here.x, here.y), surf_ok, &terrain);
        let mut targets: Vec<&Place> = graph.places_on(&here.map).iter().collect();
        if let Goal::Place(to) = goal {
            if to.map == here.map && !graph.places.contains_key(&to.key()) {
                targets.push(to);
            }
        }
        for target in targets {
            if target.key() == key || done.contains(&target.key()) {
                continue;
            }
            if let Some(edge) = graph.walk_edge(map, (here.x, here.y), &flood, &terrain, target) {
                relax(&here, edge, g, &mut dist, &mut open);
            }
        }
        for edge in graph.edges_from(&here.map, here.x, here.y) {
            if !done.contains(&edge.to.key()) {
                relax(&here, edge.clone(), g, &mut dist, &mut open);
            }
        }
        if graph.maps[&here.map].outdoor {
            for spot in &graph.fly_spots {
                if spot.key() == key || done.contains(&spot.key()) {
                    continue;
                }
                let edge = Edge {
                    to: spot.clone(),
                    kind: EdgeKind::Fly,
                    cost_s: graph.params.fly_s,
                    requires: fly_requirement(&spot.map),
                };
                relax(&here, edge, g, &mut dist, &mut open);
            }
        }
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
    if matches!(policy, UnknownPolicy::Pessimistic) {
        unmet.extend(c.unknown.iter().cloned());
    }
    unmet.sort();
    unmet.dedup();
    unmet
}
