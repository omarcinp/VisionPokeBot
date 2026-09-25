//! Goal planner (spec §4): backward-chaining A\* from a goal predicate to
//! the intents that establish it, priced through the route planner and
//! bounded by methods, a depth limit, a node budget and a wall clock.
//!
//! The open set holds partial plans. Each expansion takes the last unmet
//! predicate of a plan and either drops it (the belief plus the plan's
//! effects make it true), settles it (an `Unknown` fact is assumed or
//! probed, §4.3) or establishes it (a method's decomposition first, then
//! every primitive intent whose effects cover it). Ties break on cost, then
//! intent name and map, so the same input always yields the same plan.
//!
//! Reaching a map is priced by the route planner alone (§5.2): its open
//! route, or, when it has none, its blocked alternatives, each of whose
//! unmet requirements becomes a subgoal (a fossil that hides the object in
//! the way, Cut for a tree). A route's unknown requirements are settled
//! like any other unknown; an object hidden by a flag nothing has observed
//! is assumed still there, so the work that removes it is planned.
//!
//! Steps are placed with the least commitment (§4.6.1): what establishes a
//! predicate goes just before the step that needs it (before the `Go` that
//! positions that step), not at the front of the plan. The walking legs of
//! the plan are priced for what they yield on their own (§4.6.2): the new
//! species the catch policy is expected to take on the way, and the
//! experience of the trainers beaten, so explicit `Catch` and `Train` steps
//! cover only the remainder. Audits whose screen is where the plan already
//! goes are added at their screen cost (§4.6.3).

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;

use pokebot_gamedata::mechanics::{
    ball_multiplier, catch_probability, exp_for_level, exp_gain, Stats,
};
use pokebot_gamedata::GameData;
use pokebot_state::priors::NO_EVIDENCE;
use pokebot_state::{Fact, PlayerPose, Priors, SavedKnowledge, WorldBelief};
use pokebot_world::behavior::TALL_GRASS;
use pokebot_world::events::{Condition, DexCount, Effect as ScriptEffect};
use pokebot_world::obstacles::{blockers, static_obstacles, Passage};
use pokebot_world::path::{reach, Reach, Walk};
use pokebot_world::predicate::{BeliefView, Predicate, Truth};
use pokebot_world::route::{EdgeKind, PlaceGraph, RouteParams, RouteResult, UnknownPolicy};
use pokebot_world::{MapData, World};
use serde::{Deserialize, Serialize};

use crate::belief_adapter::{snapshot_id, StateBelief};
use crate::intents::{
    best_ball, expected_throws, first_battle, hm_of_move, path_answers, path_effects_in,
    CostParams, Effect, GoalBelief, GoalPredicate, Intent, Obtain, PlanContext, ProbeFact,
    LEAD_HP_MIN,
};
use crate::methods::Methods;
use crate::prepare::{plan_preparation, Area, PlanStep, Request};

/// Training/catching areas offered to the readiness planner.
const TRAINING_AREAS: usize = 4;
/// Charged when a probed fact has to be assumed because nothing establishes
/// it: beyond any real plan, so the work is always preferred.
const FALLBACK_S: f64 = 1.0e7;
/// Areas assumed when the position is unknown.
const DEFAULT_AREAS: [&str; 4] = ["Route1", "Route22", "Route2", "ViridianForest"];
/// P(set) of a flag the story sets (a trainer beaten, an object removed)
/// when nothing observed bears on it: the game starts with it clear.
const PROGRESS_FLAG_PRIOR: f64 = 0.25;
/// Seconds taken off an encounter area's travel when the area lies on the
/// way to where the plan goes anyway (§4.5 locality).
const LOCALITY_BONUS_S: f64 = 30.0;
/// Encounter tile clusters routed to per map (largest first), one tile each.
const GRASS_CLUSTERS: usize = 4;
/// Slack, in tiles, when deciding whether a trigger tile lies on a walk.
const ON_THE_WAY_SLACK: i32 = 2;
/// Marts and Centers offered per goal: the nearest by maps crossed, so
/// that only their routes are priced.
const NEAREST_SHOPS: usize = 3;
/// Encounter areas priced for new Pokédex entries: the nearest by maps
/// crossed among those with a species still uncaught.
const NEAREST_AREAS: usize = 12;
/// A species the walk is expected to catch with at least this probability
/// is not hunted explicitly (§4.6.2).
const PASSIVE_LIKELY: f64 = 0.5;
/// Expectations below this are not recorded on a step.
const EXPECTED_MIN: f64 = 0.02;
/// Throws the catch policy makes at most for one wild Pokémon.
const MAX_THROWS: u32 = 20;
/// Wild Pokémon average IV, for the catch odds of what the walk meets.
const WILD_IV: u32 = 15;
/// Balls beyond any need, to price what a walk would catch with a full
/// stock.
const PLENTY_OF_BALLS: f64 = 60.0;

#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// A wrong assumption dearer than this makes a probe mandatory (§4.3).
    pub expensive_secs: f64,
    /// Subgoal nesting allowed through intent preconditions (a route's
    /// unknown requirements count as one level each).
    pub max_depth: u8,
    /// Partial plans expanded before giving up.
    pub node_budget: usize,
    /// P(win) a trainer battle must reach.
    pub confidence: f64,
    /// Primitive intents tried per unmet predicate (cheapest first).
    pub candidates_per_goal: usize,
    /// What a wrong guess on a route edge's unknown requirement costs
    /// (a detour), scaled by the fact's improbability.
    pub unknown_edge_alt_s: f64,
    /// Wall-clock seconds a `plan()` call may take before it returns
    /// [`PlanError::Budget`] with the best partial plan.
    pub budget_s: f64,
    /// Set (by a signal handler) to end planning early, the same way.
    pub stop: Option<Arc<AtomicBool>>,
    /// The probe screens the toolbox can open ([`ProbeFact::kind`]);
    /// `None` for all of them. Others are never planned.
    pub supported_probes: Option<BTreeSet<String>>,
}

impl PartialEq for PlanOptions {
    fn eq(&self, other: &Self) -> bool {
        let same_stop = match (&self.stop, &other.stop) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        same_stop
            && self.expensive_secs == other.expensive_secs
            && self.max_depth == other.max_depth
            && self.node_budget == other.node_budget
            && self.confidence == other.confidence
            && self.candidates_per_goal == other.candidates_per_goal
            && self.unknown_edge_alt_s == other.unknown_edge_alt_s
            && self.budget_s == other.budget_s
            && self.supported_probes == other.supported_probes
    }
}

impl Default for PlanOptions {
    fn default() -> Self {
        PlanOptions {
            expensive_secs: 600.0,
            max_depth: 8,
            node_budget: 20_000,
            confidence: 0.9,
            candidates_per_goal: 8,
            unknown_edge_alt_s: 120.0,
            budget_s: 60.0,
            stop: None,
            supported_probes: None,
        }
    }
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedIntent {
    pub intent: Intent,
    pub cost_s: f64,
    /// Unknown facts this step relies on.
    #[serde(default)]
    pub assumes: Vec<GoalPredicate>,
    /// Facts a probe earlier in the plan settles: the step is skipped when
    /// they already hold ("buy balls if needed").
    #[serde(default)]
    pub unless: Vec<GoalPredicate>,
    /// How the estimate was made when it is not the usual one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The legs of the route a `Go` was priced by (§5.3), printed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route: Vec<String>,
    /// What the step is expected to establish on its own, with the
    /// probability (§4.6.2): the new species the catch policy takes on a
    /// `Go`'s walking legs, as `Caught(species)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected: Vec<(GoalPredicate, f64)>,
}

impl PlannedIntent {
    fn new(intent: Intent, cost_s: f64) -> PlannedIntent {
        PlannedIntent {
            intent,
            cost_s,
            assumes: Vec::new(),
            unless: Vec::new(),
            note: None,
            route: Vec::new(),
            expected: Vec::new(),
        }
    }

    /// New Pokédex catches the step is expected to make on its own.
    pub fn expected_new_species(&self) -> f64 {
        self.expected
            .iter()
            .filter(|(p, _)| matches!(p, GoalPredicate::Caught { .. }))
            .map(|(_, p)| *p)
            .sum()
    }

    fn with_route(mut self, route: &RouteResult) -> PlannedIntent {
        self.route = route.legs.iter().map(ToString::to_string).collect();
        self
    }
}

/// The ordered intents that establish a goal (§4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub intents: Vec<PlannedIntent>,
    /// Every unknown fact the plan relies on, sorted.
    pub assumes: Vec<GoalPredicate>,
    /// Expected seconds, penalties for assumptions included.
    pub cost_s: f64,
    /// Digest of the knowledge the plan was made against.
    pub belief_snapshot: u64,
}

impl Plan {
    /// New Pokédex catches the plan's walking legs are expected to make.
    pub fn expected_new_species(&self) -> f64 {
        self.intents
            .iter()
            .map(PlannedIntent::expected_new_species)
            .sum()
    }

    /// Why the plan can't be carried out as is: the reasons of its
    /// `Unsupported` intents.
    pub fn blocked(&self) -> Vec<String> {
        self.intents
            .iter()
            .filter_map(|s| match &s.intent {
                Intent::Unsupported { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanError {
    /// No intent chain establishes the goal within the depth limit.
    NoPlan { goal: GoalPredicate },
    /// The node budget, the wall clock or a stop request ended the search.
    Budget {
        goal: GoalPredicate,
        /// Partial plans expanded.
        nodes: usize,
        elapsed_s: f64,
        /// The cheapest partial plan when the search stopped: its steps so
        /// far, with an `Unsupported` placeholder per predicate still open.
        best_partial: Option<Plan>,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NoPlan { goal } => write!(f, "no plan establishes {goal}"),
            PlanError::Budget {
                goal,
                nodes,
                elapsed_s,
                ..
            } => write!(
                f,
                "planning {goal} exceeded the budget ({nodes} nodes, {elapsed_s:.1} s)"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Parses the CLI goal grammar: `catch SPECIES | flag FLAG | badge N |
/// at MAP | item ITEM N`. Species and items take the game's constant with
/// or without its prefix.
pub fn parse_goal(text: &str) -> Result<GoalPredicate, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let usage = "expected: catch SPECIES | flag FLAG_NAME | badge N | at MAP | item ITEM N";
    match words.as_slice() {
        ["catch", species] => Ok(GoalPredicate::caught(&constant("SPECIES_", species))),
        ["flag", flag] => Ok(GoalPredicate::flag(&flag.to_ascii_uppercase(), true)),
        ["badge", n] => n
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=8).contains(n))
            .map(GoalPredicate::badge)
            .ok_or_else(|| format!("badge {n}: expected 1-8")),
        ["at", map] => Ok(GoalPredicate::at(map)),
        ["item", item, n] => n
            .parse::<u32>()
            .map(|n| GoalPredicate::has_item(&constant("ITEM_", item), n))
            .map_err(|e| format!("item count {n}: {e}")),
        ["item", item] => Ok(GoalPredicate::has_item(&constant("ITEM_", item), 1)),
        _ => Err(format!("goal {text:?}: {usage}")),
    }
}

fn constant(prefix: &str, name: &str) -> String {
    let upper = name.trim().to_ascii_uppercase().replace([' ', '-'], "_");
    if upper.starts_with(prefix) {
        upper
    } else {
        format!("{prefix}{upper}")
    }
}

/// P(edge requirement) → the seconds a wrong optimistic guess costs.
type PenaltyModel = Rc<dyn Fn(&Predicate) -> f64>;

thread_local! {
    /// The penalty model the route planner's `Optimistic` policy consults
    /// (its callback is a plain `fn`, so the priors reach it this way).
    static EDGE_PENALTY: RefCell<Option<PenaltyModel>> = const { RefCell::new(None) };
}

fn edge_penalty(p: &Predicate) -> f64 {
    EDGE_PENALTY.with(|cell| cell.borrow().as_ref().map_or(0.0, |f| f(p)))
}

/// The goal planner over one world, game data and rule set.
pub struct Planner<'a> {
    pub world: &'a World,
    pub graph: &'a PlaceGraph,
    pub data: &'a GameData,
    pub obtain: Option<&'a Obtain>,
    pub priors: Option<&'a Priors>,
    pub methods: &'a Methods,
    pub options: PlanOptions,
    pub params: CostParams,
    /// Predicate → (script, path) whose effects establish it.
    scripts_by_effect: BTreeMap<GoalPredicate, Vec<(String, usize)>>,
    /// Map → items sold there.
    marts: BTreeMap<String, Vec<String>>,
    /// Pokémon Center maps.
    centers: Vec<String>,
    /// Trainer id → maps with an object whose script fights it.
    trainer_maps: BTreeMap<String, Vec<String>>,
    /// Map → trigger tiles whose script fights a trainer not yet beaten.
    battle_triggers: BTreeMap<String, Vec<BattleTrigger>>,
    /// Map → the tiles of objects a walk may cross (trainers, objects a
    /// flag hides) and what crossing costs; other stationary objects are
    /// walls.
    crossings: BTreeMap<String, BTreeMap<(i32, i32), Crossing>>,
    /// (map, local id) → an object's tile.
    objects: BTreeMap<(String, u32), (i32, i32)>,
    /// Flags the story sets (scripts, removed objects, trainers): clear
    /// until then, so unlikely without evidence.
    progress_flags: BTreeSet<String>,
    /// Map → encounter tiles to route to (two per cluster, largest first).
    grass: BTreeMap<String, Vec<(i32, i32)>>,
}

impl<'a> Planner<'a> {
    pub fn new(
        world: &'a World,
        graph: &'a PlaceGraph,
        data: &'a GameData,
        obtain: Option<&'a Obtain>,
        priors: Option<&'a Priors>,
        methods: &'a Methods,
        options: PlanOptions,
    ) -> Planner<'a> {
        let mut scripts_by_effect: BTreeMap<GoalPredicate, Vec<(String, usize)>> = BTreeMap::new();
        let mut trainer_maps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut battle_triggers: BTreeMap<String, Vec<BattleTrigger>> = BTreeMap::new();
        let mut objects = BTreeMap::new();
        let mut progress_flags = BTreeSet::new();
        if let Some(events) = world.events() {
            for (label, script) in &events.scripts {
                for path in &script.paths {
                    for e in &path.does {
                        match e {
                            ScriptEffect::Set { set } => {
                                progress_flags.insert(set.clone());
                            }
                            ScriptEffect::Defeated { defeated } => {
                                progress_flags.insert(defeated.clone());
                            }
                            _ => {}
                        }
                    }
                }
                let Some(map) = &script.map else {
                    continue;
                };
                for (i, path) in script.paths.iter().enumerate() {
                    for p in path_effects_in(path, map, world) {
                        scripts_by_effect
                            .entry(p)
                            .or_default()
                            .push((label.clone(), i));
                    }
                    if script.kind == "object" {
                        if let Some(trainer) = crate::intents::first_battle(path) {
                            let maps = trainer_maps.entry(trainer.to_string()).or_default();
                            if !maps.contains(map) {
                                maps.push(map.clone());
                            }
                        }
                    }
                }
            }
            for t in &events.triggers {
                let Some(script) = t.script.as_deref().and_then(|s| events.script(s)) else {
                    continue;
                };
                let trainer = script.paths.iter().find_map(|path| {
                    let undefeated = path.when.iter().any(|c| {
                        matches!(
                            c,
                            Condition::Trainer {
                                defeated: false,
                                ..
                            }
                        )
                    });
                    undefeated
                        .then(|| crate::intents::first_battle(path))
                        .flatten()
                });
                if let Some(trainer) = trainer {
                    battle_triggers
                        .entry(t.map.clone())
                        .or_default()
                        .push(((t.x, t.y), trainer.to_string()));
                }
            }
            for o in &events.objects {
                if let (Some(x), Some(y)) = (o.x, o.y) {
                    objects.insert((o.map.clone(), o.local_id), (x, y));
                }
                if let Some(flag) = &o.hidden_by {
                    progress_flags.insert(flag.clone());
                }
            }
        }
        for v in battle_triggers.values_mut() {
            v.sort();
            v.dedup();
        }
        let mut grass = BTreeMap::new();
        let mut crossings = BTreeMap::new();
        let trainer_tiles =
            (RouteParams::default().battle_s / RouteParams::default().tile_s) as i32;
        for map in world.maps() {
            if data
                .wild
                .get(&map.name)
                .is_some_and(|t| t.contains_key("land"))
            {
                let tiles = encounter_tiles(map);
                if !tiles.is_empty() {
                    grass.insert(map.name.clone(), tiles);
                }
            }
            let mut ways: BTreeMap<(i32, i32), Crossing> = BTreeMap::new();
            for b in blockers(map, world.events()) {
                // The cheapest way past, as the route planner's flood
                // prices it: a hidden object one tile, a trainer a battle.
                let way = b.passages.iter().map(|p| match p {
                    Passage::Hidden { .. } => (1, None),
                    Passage::Trainer { trainer } => (trainer_tiles, Some(trainer.clone())),
                });
                if let Some(w) = way.min_by_key(|w| w.0) {
                    ways.insert((b.x, b.y), w);
                }
            }
            if !ways.is_empty() {
                crossings.insert(map.name.clone(), ways);
            }
        }
        let mut marts: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (map, items) in &data.marts {
            marts
                .entry(map.clone())
                .or_default()
                .extend(items.iter().cloned());
        }
        if let Some(places) = world.places() {
            for (map, items) in &places.marts {
                marts
                    .entry(map.clone())
                    .or_default()
                    .extend(items.iter().cloned());
            }
        }
        for items in marts.values_mut() {
            items.sort();
            items.dedup();
        }
        let mut centers: Vec<String> = world
            .places()
            .map(|p| p.heal_spots.iter().map(|h| h.respawn_map.clone()).collect())
            .unwrap_or_default();
        centers.sort();
        centers.dedup();
        Planner {
            world,
            graph,
            data,
            obtain,
            priors,
            methods,
            options,
            params: CostParams::default(),
            scripts_by_effect,
            marts,
            centers,
            trainer_maps,
            battle_triggers,
            crossings,
            objects,
            progress_flags,
            grass,
        }
    }

    /// Plans `goal` from `knowledge` at `pose`. An already-true goal yields
    /// an empty plan.
    pub fn plan(
        &self,
        goal: &GoalPredicate,
        knowledge: &SavedKnowledge,
        pose: Option<PlayerPose>,
    ) -> Result<Plan, PlanError> {
        let mut base = StateBelief::new(knowledge, self.data, pose.clone());
        base.confidence = self.options.confidence;
        let session = Session {
            planner: self,
            base,
            expanded: Cell::new(0),
            started: Instant::now(),
            nesting: Cell::new(0),
            active: RefCell::new(Vec::new()),
            partial: RefCell::new(None),
            prefix: RefCell::new(Vec::new()),
            routes: RefCell::new(HashMap::new()),
            reaches: RefCell::new(HashMap::new()),
            hops: pose
                .as_ref()
                .map(|p| hops(self.world, &p.map))
                .unwrap_or_default(),
            grass_routes: RefCell::new(HashMap::new()),
            min_costs: RefCell::new(BTreeMap::new()),
            readiness: RefCell::new(BTreeMap::new()),
            methods: RefCell::new(BTreeMap::new()),
            seq: Cell::new(0),
            trace: std::env::var_os("POKEBOT_PLAN_TRACE").is_some(),
            spent: RefCell::new([0.0; 4]),
        };
        let penalty = self.penalty_model(&knowledge.world);
        EDGE_PENALTY.with(|cell| *cell.borrow_mut() = Some(penalty));
        let root = OpenGoal {
            p: goal.clone(),
            depth: 0,
            before: None,
            unless: None,
            establish: true,
        };
        let result = session.search(vec![root], &BTreeSet::new(), None);
        EDGE_PENALTY.with(|cell| *cell.borrow_mut() = None);
        if session.trace {
            let spent = session.spent.borrow();
            eprintln!(
                "[plan] {} expansions, {} routes ({:.1} s), {} grass routes ({:.1} s), {} floods ({:.1} s), {} methods, readiness {:.1} s, {:.2} s",
                session.expanded.get(),
                session.routes.borrow().len(),
                spent[0],
                session.grass_routes.borrow().len(),
                spent[1],
                session.reaches.borrow().len(),
                spent[2],
                session.methods.borrow().len(),
                spent[3],
                session.started.elapsed().as_secs_f64()
            );
        }
        let sub = result.map_err(|e| match e {
            Failure::NoPlan => PlanError::NoPlan { goal: goal.clone() },
            Failure::Budget => PlanError::Budget {
                goal: goal.clone(),
                nodes: session.expanded.get(),
                elapsed_s: session.started.elapsed().as_secs_f64(),
                best_partial: session.partial_plan(knowledge),
            },
        })?;
        let mut assumes = sub.assumes;
        assumes.sort();
        assumes.dedup();
        if session.trace {
            for (i, s) in sub.steps.iter().enumerate() {
                eprintln!("[plan] raw {:>3} {}", i + 1, s.planned.intent);
            }
        }
        let (steps, repeated) = dedupe_steps(sub.steps);
        let mut steps = session.fill_routes(steps);
        session.passive_walk(&mut steps);
        let steps: Vec<PlannedIntent> = steps.into_iter().map(|s| s.planned).collect();
        let (mut intents, saved) = dedupe_probes(steps, self.data);
        let audits = session.audits(goal, &mut intents, knowledge);
        Ok(Plan {
            intents,
            assumes,
            cost_s: sub.cost - saved - repeated + audits,
            belief_snapshot: snapshot_id(knowledge),
        })
    }

    /// Whether planning `goal` turns on the party: what it is made of
    /// (battles, an HM carrier) or the Pokédex counts.
    fn depends_on_party(goal: &GoalPredicate, intents: &[PlannedIntent]) -> bool {
        matches!(
            goal,
            GoalPredicate::CanBeat { .. }
                | GoalPredicate::Healed { .. }
                | GoalPredicate::PokedexCaught { .. }
                | GoalPredicate::PokedexSeen { .. }
                | GoalPredicate::World(Predicate::PartyHasMove { .. })
        ) || intents.iter().any(|s| {
            matches!(
                s.intent,
                Intent::Beat { .. } | Intent::Train { .. } | Intent::Teach { .. }
            )
        })
    }

    /// P(`p` holds) from the priors: flags and visits through the rules;
    /// a story flag no rule covers is likelier clear than set; everything
    /// else the no-evidence rate.
    pub fn prior(&self, belief: &WorldBelief, p: &GoalPredicate) -> f64 {
        match p {
            GoalPredicate::World(w) => world_prior(self.priors, &self.progress_flags, belief, w),
            _ => NO_EVIDENCE,
        }
    }

    fn penalty_model(&self, belief: &WorldBelief) -> PenaltyModel {
        let priors = self.priors.cloned();
        let progress = self.progress_flags.clone();
        let belief = belief.clone();
        let alt = self.options.unknown_edge_alt_s;
        Rc::new(move |p: &Predicate| {
            let prob = world_prior(priors.as_ref(), &progress, &belief, p);
            (1.0 - prob) * alt
        })
    }
}

/// [`Planner::prior`] for a world predicate.
fn world_prior(
    priors: Option<&Priors>,
    progress: &BTreeSet<String>,
    belief: &WorldBelief,
    p: &Predicate,
) -> f64 {
    let flag = |name: &str| {
        let p = priors.map_or(NO_EVIDENCE, |pr| pr.probability(belief, &Fact::flag(name)));
        if p == NO_EVIDENCE && progress.contains(name) {
            PROGRESS_FLAG_PRIOR
        } else {
            p
        }
    };
    match p {
        Predicate::Flag { name, is } => {
            let t = flag(name);
            if *is {
                t
            } else {
                1.0 - t
            }
        }
        Predicate::Badge { n } => flag(&Predicate::badge_flag(*n)),
        Predicate::Visited { map } => priors.map_or(NO_EVIDENCE, |pr| {
            pr.probability(belief, &Fact::visited(map.clone()))
        }),
        _ => NO_EVIDENCE,
    }
}

/// Whether a step onto `(x, y)` of `map` rolls for a wild encounter: tall
/// grass, or cave floor with land encounters.
fn is_encounter_tile(map: &MapData, x: i32, y: i32) -> bool {
    const CAVE: u16 = 0x08;
    const SAND_CAVE: u16 = 0x2B;
    map.tile(x, y).is_some_and(|t| {
        t.collision == 0
            && (t.behavior == TALL_GRASS
                || (t.encounter == 1 && matches!(t.behavior, 0x00 | CAVE | SAND_CAVE)))
    })
}

/// P(one throw catches `species` at `level`, weakened to half HP, with a
/// ball of `mult`) and the throws expected.
fn catch_odds(data: &GameData, species: &str, level: u8, mult: u32) -> Option<(f64, u32)> {
    let sp = data.species(species)?;
    let max_hp = Stats::compute(&sp.base, level, WILD_IV).hp();
    let p = catch_probability(sp.catch_rate, max_hp, (max_hp / 2).max(1), mult);
    if p <= 0.0 {
        return None;
    }
    Some((p, (1.0 / p).ceil() as u32))
}

/// Experience one participant gains for beating `trainer`'s whole party.
fn trainer_exp(data: &GameData, trainer: &str) -> f64 {
    data.trainers
        .get(trainer)
        .map(|t| {
            t.party
                .iter()
                .map(|m| exp_gain(data, &m.species, m.level, true) as f64)
                .sum()
        })
        .unwrap_or(0.0)
}

/// The level `exp` total experience reaches on `growth`.
fn level_for_exp(growth: &str, exp: u64) -> u8 {
    (1..=100u8)
        .rev()
        .find(|l| exp_for_level(growth, *l) <= exp)
        .unwrap_or(1)
}

/// Tiles wild Pokémon appear on when walked: tall grass, or cave floor
/// with land encounters. Two per connected cluster (first and last in
/// row-major order), the largest [`GRASS_CLUSTERS`] clusters.
fn encounter_tiles(map: &MapData) -> Vec<(i32, i32)> {
    let is_encounter = |x: i32, y: i32| is_encounter_tile(map, x, y);
    let mut seen: BTreeSet<(i32, i32)> = BTreeSet::new();
    let mut clusters: Vec<Vec<(i32, i32)>> = Vec::new();
    for y in 0..map.height {
        for x in 0..map.width {
            if !is_encounter(x, y) || seen.contains(&(x, y)) {
                continue;
            }
            let mut cluster = Vec::new();
            let mut queue = VecDeque::from([(x, y)]);
            seen.insert((x, y));
            while let Some((cx, cy)) = queue.pop_front() {
                cluster.push((cx, cy));
                for (nx, ny) in [(cx + 1, cy), (cx - 1, cy), (cx, cy + 1), (cx, cy - 1)] {
                    if is_encounter(nx, ny) && seen.insert((nx, ny)) {
                        queue.push_back((nx, ny));
                    }
                }
            }
            cluster.sort_by_key(|&(x, y)| (y, x));
            clusters.push(cluster);
        }
    }
    clusters.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
    clusters.iter().take(GRASS_CLUSTERS).map(|c| c[0]).collect()
}

/// One planned step with the facts it establishes.
#[derive(Debug, Clone)]
struct Step {
    planned: PlannedIntent,
    effects: Vec<GoalPredicate>,
    /// The route a `Go` was priced by, for what its legs yield (§4.6.2).
    route: Option<Rc<RouteResult>>,
}

impl Step {
    fn new(planned: PlannedIntent, effects: Vec<GoalPredicate>) -> Step {
        Step {
            planned,
            effects,
            route: None,
        }
    }
}

/// A primitive way to establish a predicate: one or more steps with what
/// they need first.
#[derive(Debug, Clone)]
struct Candidate {
    steps: Vec<Step>,
    /// Steps that go at the plan's start, ahead of the walk they equip
    /// (balls for what the way yields, §4.6.2).
    lead: Vec<Step>,
    preconditions: Vec<GoalPredicate>,
    assumes: Vec<GoalPredicate>,
    cost: f64,
}

impl Candidate {
    fn single(intent: Intent, ctx: &PlanContext<'_>, cost: f64) -> Candidate {
        let preconditions = intent.preconditions(ctx);
        let effects = intent
            .effects(ctx)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Establishes(p) => Some(p),
                Effect::Observes(_) => None,
            })
            .collect();
        Candidate {
            steps: vec![Step::new(PlannedIntent::new(intent, cost), effects)],
            lead: Vec::new(),
            preconditions,
            assumes: Vec::new(),
            cost,
        }
    }

    /// The last step (the one that establishes the goal) also does `p`.
    fn add_effect(&mut self, p: GoalPredicate) {
        if let Some(last) = self.steps.last_mut() {
            if !last.effects.contains(&p) {
                last.effects.push(p);
            }
        }
    }

    fn effects(&self) -> Vec<GoalPredicate> {
        let mut out: Vec<GoalPredicate> =
            self.steps.iter().flat_map(|s| s.effects.clone()).collect();
        out.sort();
        out.dedup();
        out
    }

    fn sort_key(&self) -> (OrdF64, String) {
        let names: Vec<String> = self
            .lead
            .iter()
            .chain(self.steps.iter())
            .map(|s| s.planned.intent.to_string())
            .collect();
        (OrdF64(self.cost), names.join(";"))
    }
}

/// A sub-plan: the steps, what they assumed and their cost.
#[derive(Clone)]
struct SubPlan {
    steps: Vec<Step>,
    assumes: Vec<GoalPredicate>,
    cost: f64,
}

impl SubPlan {
    fn effects(&self) -> impl Iterator<Item = &GoalPredicate> {
        self.steps.iter().flat_map(|s| s.effects.iter())
    }
}

#[derive(Clone, Copy)]
enum Failure {
    NoPlan,
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OrdF64(f64);

impl Eq for OrdF64 {}

impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// An unmet predicate on a partial plan's stack.
#[derive(Debug, Clone)]
struct OpenGoal {
    p: GoalPredicate,
    depth: u8,
    /// The step needing it (its id), or the plan's end for the goal
    /// itself: only steps before it can establish it.
    before: Option<u64>,
    /// The steps establishing it are conditional on this probed fact.
    unless: Option<GoalPredicate>,
    /// Establish it even if the belief can't rule it out (the root goal,
    /// and facts a probe will settle).
    establish: bool,
}

#[derive(Debug, Clone)]
struct Node {
    g: f64,
    f: f64,
    seq: u64,
    /// In execution order.
    plan: Vec<Step>,
    /// Ids of the plan's steps, in the same order.
    ids: Vec<u64>,
    /// Unmet predicates; the last is worked on next.
    open: Vec<OpenGoal>,
    /// Facts assumed about the starting state.
    assumed: BTreeSet<GoalPredicate>,
    assumes: Vec<GoalPredicate>,
    /// Step id → the id of the first step of the group last planned for
    /// it: the next thing it needs goes ahead of that group, so everything
    /// planned for a step sits together just ahead of it, its trip last
    /// (§4.6.1).
    anchors: BTreeMap<u64, u64>,
    /// Facts the plan's own way is expected to establish without a step
    /// (the trainers beaten on the way level the lead enough, §4.6.2).
    expected: BTreeSet<GoalPredicate>,
}

impl Node {
    fn key(&self) -> Reverse<(OrdF64, u64)> {
        Reverse((OrdF64(self.f), self.seq))
    }

    fn position(&self, id: u64) -> Option<usize> {
        self.ids.iter().position(|i| *i == id)
    }

    /// Where steps establishing something for the step `before` go: ahead
    /// of what was last planned for it, else straight ahead of it; the
    /// plan's end for the goal itself.
    fn insert_pos(&self, before: Option<u64>) -> usize {
        let Some(id) = before else {
            return self.plan.len();
        };
        let at = self.anchors.get(&id).copied().unwrap_or(id);
        self.position(at)
            .or_else(|| self.position(id))
            .unwrap_or(self.plan.len())
    }

    /// What holds when the step at `pos` runs: the base facts, the
    /// assumptions and the effects of the steps ahead of it.
    fn established_before(
        &self,
        base: &BTreeSet<GoalPredicate>,
        pos: usize,
    ) -> BTreeSet<GoalPredicate> {
        let mut out = base.clone();
        out.extend(self.assumed.iter().cloned());
        out.extend(self.expected.iter().cloned());
        for step in &self.plan[..pos.min(self.plan.len())] {
            out.extend(step.effects.iter().cloned());
        }
        out
    }

    /// Records that `p` is relied on by the step that needed it.
    fn assume(&mut self, p: GoalPredicate, before: Option<u64>) {
        let at = before
            .and_then(|id| self.position(id))
            .or_else(|| self.plan.len().checked_sub(1));
        if let Some(i) = at {
            let step = &mut self.plan[i].planned;
            if !step.assumes.contains(&p) {
                step.assumes.push(p.clone());
            }
        }
        if !self.assumes.contains(&p) {
            self.assumes.push(p.clone());
        }
        self.assumed.insert(p);
    }

    /// Inserts `steps` so they run just ahead of the step at `at` (§4.6.1:
    /// as late as what needs them allows).
    fn insert(&mut self, at: usize, steps: Vec<Step>, ids: Vec<u64>) {
        let at = at.min(self.plan.len());
        let tail = self.plan.split_off(at);
        let tail_ids = self.ids.split_off(at);
        self.plan.extend(steps);
        self.ids.extend(ids);
        self.plan.extend(tail);
        self.ids.extend(tail_ids);
    }
}

/// What surrounds an open goal in the plan being built: the maps the plan
/// passes anyway (§4.5 locality) and what its walking legs and battles
/// ahead of the goal's place are expected to yield (§4.6.2).
#[derive(Debug, Clone, Default)]
struct Surround {
    waypoints: BTreeSet<String>,
    passive: Passive,
    /// The steps ahead, for pricing the walk again with other means.
    ahead: Vec<Step>,
}

/// What a run of steps yields on its own: per species not yet caught, the
/// probability the catch policy takes one on the way and the probability
/// one is met at all; the species explicit `Catch`es take; the experience
/// the lead gains from the trainers beaten.
#[derive(Debug, Clone, Default)]
struct Passive {
    caught: BTreeMap<String, f64>,
    seen: BTreeMap<String, f64>,
    explicit: BTreeSet<String>,
    exp: f64,
    /// Balls left at the end.
    balls: f64,
    /// The level the lead was trained to on the way, when a `Train` did.
    lead_level: Option<u8>,
}

impl Passive {
    fn map(&self, which: DexCount) -> &BTreeMap<String, f64> {
        match which {
            DexCount::Caught => &self.caught,
            DexCount::Seen => &self.seen,
        }
    }

    /// Expected new entries beyond the explicit catches and `skip`.
    fn expected(&self, which: DexCount, skip: &BTreeSet<String>) -> f64 {
        self.map(which)
            .iter()
            .filter(|(s, _)| !self.explicit.contains(*s) && !skip.contains(*s))
            .map(|(_, p)| *p)
            .sum()
    }

    fn probability(&self, which: DexCount, species: &str) -> f64 {
        self.map(which).get(species).copied().unwrap_or(0.0)
    }
}

type RouteKey = (String, Vec<GoalPredicate>);
/// A trigger tile and the trainer its script fights.
type BattleTrigger = ((i32, i32), String);
/// What a stationary object's tile costs to cross, in tiles, and the
/// trainer that must be beaten to cross it (none for a hidden object).
type Crossing = (i32, Option<String>);
type MethodKey = (String, Vec<GoalPredicate>, Option<GoalPredicate>);
/// The encounter tile chosen on a map and the route to it.
type GrassRoute = Option<((i32, i32), Rc<RouteResult>)>;
/// Floods by (map, start tile).
type Reaches = HashMap<(String, (i32, i32)), Rc<Reach>>;

/// One planning call: the belief, the shared budget and the caches.
struct Session<'p, 'a> {
    planner: &'p Planner<'a>,
    base: StateBelief<'a>,
    expanded: Cell<usize>,
    started: Instant,
    /// Nested searches in progress (methods plan their subgoals with their
    /// own search); the outermost one keeps the partial plan.
    nesting: Cell<u32>,
    /// Subgoals being planned up the method nesting: meeting one again is
    /// a cycle (Cut needs the S.S. Anne, whose route wants Cut).
    active: RefCell<Vec<GoalPredicate>>,
    /// The outermost search's last expanded node, for the budget error.
    partial: RefCell<Option<Node>>,
    /// The steps that run ahead of the plan a nested search builds (a
    /// method's earlier subgoals and what surrounds the method), for what
    /// their legs yield.
    prefix: RefCell<Vec<Step>>,
    routes: RefCell<HashMap<RouteKey, Rc<RouteResult>>>,
    /// Floods from a tile of a map over its static obstacles (trigger
    /// checks): the map as the navigator sees it, no belief involved.
    reaches: RefCell<Reaches>,
    /// Maps crossed from the pose's map: the cheap distance that picks
    /// which shops and areas are worth routing to.
    hops: BTreeMap<String, u32>,
    grass_routes: RefCell<HashMap<RouteKey, GrassRoute>>,
    /// Cheapest primitive cost per predicate (the heuristic).
    min_costs: RefCell<BTreeMap<GoalPredicate, f64>>,
    /// Readiness plans per (trainer, experience the lead gains first, the
    /// level it is trained to first).
    readiness: RefCell<BTreeMap<(String, u64, u8), Option<Candidate>>>,
    /// Method decompositions per (method, facts held, condition).
    methods: RefCell<BTreeMap<MethodKey, Option<Rc<SubPlan>>>>,
    seq: Cell<u64>,
    /// `POKEBOT_PLAN_TRACE=1` prints every expansion.
    trace: bool,
    /// Seconds spent in (routes, grass routes, tile walks, readiness).
    spent: RefCell<[f64; 4]>,
}

impl<'p, 'a> Session<'p, 'a> {
    fn next_seq(&self) -> u64 {
        let s = self.seq.get();
        self.seq.set(s + 1);
        s
    }

    fn belief(&self, established: &BTreeSet<GoalPredicate>) -> StateBelief<'a> {
        self.base.with_established(established.clone())
    }

    /// What a route needs of the plan: the requirements of its legs that
    /// the base knowledge doesn't meet (unknown ones, and the ones met only
    /// by facts the plan establishes). They become the step's
    /// preconditions, so it is ordered after them (flash-5: a trip to
    /// Diglett's Cave priced with Cut established was placed first and
    /// walked into the Route 2 Cut tree).
    fn route_needs(&self, route: &RouteResult, into: &mut Vec<GoalPredicate>) {
        for leg in &route.legs {
            for p in &leg.requires {
                if self.base.eval(p) == Truth::True {
                    continue;
                }
                let g = GoalPredicate::World(p.clone());
                if !into.contains(&g) {
                    into.push(g);
                }
            }
        }
    }

    fn context<'c>(&'c self, belief: &'c StateBelief<'a>) -> PlanContext<'c> {
        PlanContext {
            world: self.planner.world,
            graph: self.planner.graph,
            data: self.planner.data,
            belief,
            pose: self.base.pose.clone(),
            params: self.planner.params.clone(),
            policy: UnknownPolicy::Optimistic {
                penalty_of: edge_penalty,
            },
        }
    }

    /// Whether a probe of `fact` can be planned: the toolbox opens that
    /// screen and it hasn't failed twice this session.
    fn probe_allowed(&self, fact: &ProbeFact) -> bool {
        let opts = &self.planner.options;
        if opts
            .supported_probes
            .as_ref()
            .is_some_and(|s| !s.contains(fact.kind()))
        {
            return false;
        }
        let intent = Intent::Probe { fact: fact.clone() };
        !self
            .base
            .knowledge
            .world
            .infeasible
            .contains(&intent.to_string())
    }

    /// Whether the session found this exact intent infeasible.
    fn infeasible(&self, intent: &Intent) -> bool {
        self.base
            .knowledge
            .world
            .infeasible
            .contains(&intent.to_string())
    }

    /// The wall clock, the stop flag or the node budget ran out.
    fn out_of_budget(&self) -> bool {
        let opts = &self.planner.options;
        if self.expanded.get() >= opts.node_budget {
            return true;
        }
        if opts
            .stop
            .as_ref()
            .is_some_and(|s| s.load(AtomicOrdering::Relaxed))
        {
            return true;
        }
        self.started.elapsed().as_secs_f64() > opts.budget_s
    }

    /// The legs of every `Go` priced by a blocked alternative (its
    /// requirement was planned before it), routed again with what the
    /// steps before it establish.
    fn fill_routes(&self, mut steps: Vec<Step>) -> Vec<Step> {
        let mut established: BTreeSet<GoalPredicate> = BTreeSet::new();
        for step in &mut steps {
            if let Intent::Go { dest } = &step.planned.intent {
                if step.route.is_none() {
                    let belief = self.belief(&established);
                    if let Some(r) = self.route_to(dest, &belief).filter(|r| r.found()) {
                        step.planned = step.planned.clone().with_route(&r);
                        step.route = Some(r);
                    }
                }
            }
            established.extend(step.effects.iter().cloned());
        }
        steps
    }

    /// The steps that run ahead of position `pos` of `node`'s plan: the
    /// nested prefix, then the plan so far.
    fn steps_before(&self, node: &Node, pos: usize) -> Vec<Step> {
        let mut out = self.prefix.borrow().clone();
        out.extend(node.plan[..pos.min(node.plan.len())].iter().cloned());
        out
    }

    /// What surrounds a goal placed at `pos`: the waypoints and what the
    /// steps ahead of it yield on their own.
    fn surround(&self, node: &Node, pos: usize, belief: &StateBelief<'a>) -> Surround {
        let mut steps = self.steps_before(node, pos);
        let passive = self.passive_walk(&mut steps);
        Surround {
            waypoints: self.waypoints(node, belief),
            passive,
            ahead: steps,
        }
    }

    /// Whether a route's walking legs step on wild encounter tiles.
    fn crosses_encounters(&self, route: &RouteResult) -> bool {
        route.legs.iter().any(|leg| {
            matches!(leg.kind, EdgeKind::Walk { .. })
                && leg.from.map == leg.to.map
                && self
                    .planner
                    .data
                    .wild
                    .get(&leg.from.map)
                    .is_some_and(|t| t.contains_key("land"))
                && self.planner.world.map(&leg.from.map).is_some_and(|m| {
                    self.encounter_tiles_on(m, (leg.from.x, leg.from.y), (leg.to.x, leg.to.y)) > 0
                })
        })
    }

    /// Tiles with wild encounters stepped on walking from `from` to `to`
    /// on `map` (the flood's path, around the stationary objects). Either
    /// end may be a tile the flood can't use: its neighbours stand in.
    fn encounter_tiles_on(&self, map: &MapData, from: (i32, i32), to: (i32, i32)) -> u32 {
        if from == to {
            return 0;
        }
        let around = |t: (i32, i32)| {
            [
                t,
                (t.0, t.1 + 1),
                (t.0, t.1 - 1),
                (t.0 - 1, t.1),
                (t.0 + 1, t.1),
            ]
        };
        for f in around(from) {
            if !map.in_bounds(f.0, f.1) {
                continue;
            }
            let reach = self.reach_from(map, f);
            let path = around(to)
                .into_iter()
                .filter_map(|t| reach.path(t))
                .min_by_key(Vec::len);
            if let Some(path) = path {
                return path
                    .iter()
                    .filter(|s| is_encounter_tile(map, s.to.0, s.to.1))
                    .count() as u32;
            }
        }
        0
    }

    /// What `steps` yield on their own, in order (§4.6.2): on every `Go`,
    /// for each species of the areas its legs walk that is not caught yet,
    /// P(one is met on the legs' encounter tiles) × P(the catch policy
    /// takes it with the balls held), recorded on the step and summed per
    /// species over the run; the balls those catches use; the experience
    /// of the trainers beaten. Explicit catches and buys move the counts
    /// along the way.
    fn passive_walk(&self, steps: &mut [Step]) -> Passive {
        self.passive_walk_with(steps, None)
    }

    /// [`Session::passive_walk`] starting with `balls` in the bag (the
    /// known stock when `None`).
    fn passive_walk_with(&self, steps: &mut [Step], balls: Option<f64>) -> Passive {
        let data = self.planner.data;
        let mut out = Passive::default();
        let base_ctx = self.context(&self.base);
        let ball = best_ball(&base_ctx);
        let mult = ball_multiplier(&ball).unwrap_or(10);
        let reserve = f64::from(self.planner.params.ball_reserve);
        let mut balls = balls.unwrap_or_else(|| self.base.item_count(&ball).map_or(0.0, f64::from));
        let caught_already =
            |species: &str| self.base.eval_goal(&GoalPredicate::caught(species)) == Truth::True;
        let seen_already = |species: &str| {
            caught_already(species)
                || self
                    .base
                    .knowledge
                    .pokedex
                    .seen
                    .get(species)
                    .and_then(|k| k.value)
                    == Some(true)
        };
        let mut established: BTreeSet<GoalPredicate> = BTreeSet::new();
        for step in steps.iter_mut() {
            // The catches are priced afresh; other expectations (a
            // readiness judged again after training) stay.
            step.planned
                .expected
                .retain(|(p, _)| !matches!(p, GoalPredicate::Caught { .. }));
            match &step.planned.intent {
                Intent::Buy { item, count, .. } if *item == ball => {
                    balls += f64::from(*count);
                }
                Intent::Catch {
                    species, balls: b, ..
                } => {
                    out.explicit.insert(species.clone());
                    out.caught.insert(species.clone(), 1.0);
                    out.seen.insert(species.clone(), 1.0);
                    let throws = b.saturating_sub(self.planner.params.ball_reserve);
                    balls = (balls - f64::from(throws)).max(0.0);
                }
                Intent::Beat { trainer, .. } => out.exp += trainer_exp(data, trainer),
                Intent::Train { species, level, .. } => {
                    let lead = self
                        .base
                        .party_members()
                        .and_then(|m| m.first().map(|m| m.species.clone()));
                    if lead.as_deref() == Some(species.as_str()) {
                        out.lead_level = Some(out.lead_level.map_or(*level, |l| l.max(*level)));
                    }
                }
                Intent::RunScript { script, path, .. } => {
                    let trainer = self
                        .planner
                        .world
                        .events()
                        .and_then(|e| e.script(script))
                        .and_then(|s| s.paths.get(*path))
                        .and_then(first_battle);
                    if let Some(trainer) = trainer {
                        out.exp += trainer_exp(data, trainer);
                    }
                }
                Intent::Go { dest } => {
                    let route = match &step.route {
                        Some(r) => Some(Rc::clone(r)),
                        None => {
                            let belief = self.belief(&established);
                            self.route_to(dest, &belief).filter(|r| r.found())
                        }
                    };
                    let mut expected: BTreeMap<String, f64> = BTreeMap::new();
                    for leg in route.iter().flat_map(|r| r.legs.iter()) {
                        if !matches!(leg.kind, EdgeKind::Walk { .. }) || leg.from.map != leg.to.map
                        {
                            continue;
                        }
                        let Some(table) = data.wild.get(&leg.from.map).and_then(|t| t.get("land"))
                        else {
                            continue;
                        };
                        let Some(map) = self.planner.world.map(&leg.from.map) else {
                            continue;
                        };
                        let tiles = self.encounter_tiles_on(
                            map,
                            (leg.from.x, leg.from.y),
                            (leg.to.x, leg.to.y),
                        );
                        if tiles == 0 {
                            continue;
                        }
                        // Encounters expected on the leg: the game rolls
                        // rate×16 of 2880 on every encounter tile stepped on.
                        let lambda = f64::from(tiles) * f64::from(table.rate.max(1))
                            / self.planner.params.encounter_steps_at_rate_1;
                        let mut shares: BTreeMap<&str, (f64, u8)> = BTreeMap::new();
                        for slot in &table.slots {
                            let e = shares.entry(slot.species.as_str()).or_insert((0.0, 0));
                            e.0 += f64::from(slot.chance) / 100.0;
                            e.1 = e.1.max(slot.max_level);
                        }
                        for (species, (share, level)) in shares {
                            let meet = 1.0 - (-lambda * share).exp();
                            if !seen_already(species) {
                                let p = out.seen.entry(species.to_string()).or_insert(0.0);
                                *p += (1.0 - *p) * meet;
                            }
                            if caught_already(species) || out.explicit.contains(species) {
                                continue;
                            }
                            // The policy catches a new species only with the
                            // reserve to spare beyond the expected throws,
                            // then throws until it is caught.
                            let spare = (balls - reserve).floor().max(0.0);
                            let Some((p1, throws)) = catch_odds(data, species, level, mult) else {
                                continue;
                            };
                            if spare < f64::from(throws) {
                                continue;
                            }
                            let tries = spare.min(f64::from(MAX_THROWS));
                            let catch = 1.0 - (1.0 - p1).powf(tries);
                            let before = out.caught.get(species).copied().unwrap_or(0.0);
                            let gain = (1.0 - before) * meet * catch;
                            if gain < EXPECTED_MIN {
                                continue;
                            }
                            out.caught.insert(species.to_string(), before + gain);
                            *expected.entry(species.to_string()).or_insert(0.0) += gain;
                            balls = (balls - gain * f64::from(throws)).max(0.0);
                        }
                    }
                    step.planned.expected.extend(
                        expected
                            .into_iter()
                            .map(|(s, p)| (GoalPredicate::caught(&s), p)),
                    );
                }
                _ => {}
            }
            established.extend(step.effects.iter().cloned());
        }
        out.balls = balls;
        out
    }

    /// Audits at the plan's own screens (§4.6.3). At bootstrap, with the
    /// PC boxes unknown and the party mattering to the goal, the Center
    /// the player is in or next to is visited first for a box audit; else
    /// the audit rides on the first `Heal` the plan makes. Returns the
    /// seconds added.
    fn audits(
        &self,
        goal: &GoalPredicate,
        intents: &mut Vec<PlannedIntent>,
        knowledge: &SavedKnowledge,
    ) -> f64 {
        if intents.is_empty() || !Planner::depends_on_party(goal, intents) {
            return 0.0;
        }
        if knowledge.pc.boxes.iter().any(|b| b.value.is_some()) {
            return 0.0;
        }
        let probe = Intent::Probe {
            fact: ProbeFact::PcBoxes,
        };
        if !self.probe_allowed(&ProbeFact::PcBoxes) || intents.iter().any(|s| s.intent == probe) {
            return 0.0;
        }
        let mut audit = PlannedIntent::new(probe, ProbeFact::PcBoxes.cost_s());
        let mut added = audit.cost_s;
        // In or next to a Center: the audit comes first.
        let near = self
            .planner
            .centers
            .iter()
            .filter_map(|c| Some((*self.hops.get(c)?, c)))
            .filter(|(d, _)| *d <= 1)
            .min();
        if let Some((hops, center)) = near {
            let mut first = Vec::new();
            if hops > 0 {
                let Some(route) = self.route_to(center, &self.base).filter(|r| r.found()) else {
                    return 0.0;
                };
                let go = Intent::Go {
                    dest: center.clone(),
                };
                first.push(PlannedIntent::new(go, route.cost_s).with_route(&route));
                added += route.cost_s;
            }
            audit.note = Some("bootstrap: the boxes are unknown and the Center is here".into());
            first.push(audit);
            intents.splice(0..0, first);
            return added;
        }
        // Else at the first Center the plan heals at.
        let Some(heal) = intents
            .iter()
            .position(|s| matches!(s.intent, Intent::Heal { .. }))
        else {
            return 0.0;
        };
        audit.note = Some("the boxes are unknown; the plan heals here anyway".into());
        intents.insert(heal + 1, audit);
        added
    }

    /// The outermost search's best partial plan as a `Plan`: its steps,
    /// preceded by a placeholder per predicate still open.
    fn partial_plan(&self, knowledge: &SavedKnowledge) -> Option<Plan> {
        let node = self.partial.borrow().clone()?;
        let mut intents: Vec<PlannedIntent> = node
            .open
            .iter()
            .rev()
            .map(|g| {
                PlannedIntent::new(
                    Intent::Unsupported {
                        reason: format!("{} was not planned within the budget", g.p),
                        establishes: g.p.clone(),
                    },
                    0.0,
                )
            })
            .collect();
        intents.extend(node.plan.into_iter().map(|s| s.planned));
        let mut assumes = node.assumes;
        assumes.sort();
        assumes.dedup();
        Some(Plan {
            intents,
            assumes,
            cost_s: node.g,
            belief_snapshot: snapshot_id(knowledge),
        })
    }

    /// A\* from `open` (last worked first) with `base` already holding.
    /// `no_method` blocks the method for that goal (a method's own final
    /// step is planned from primitives).
    fn search(
        &self,
        open: Vec<OpenGoal>,
        base: &BTreeSet<GoalPredicate>,
        no_method: Option<&GoalPredicate>,
    ) -> Result<SubPlan, Failure> {
        let mut heap: BinaryHeap<(Reverse<(OrdF64, u64)>, u64)> = BinaryHeap::new();
        let mut nodes: BTreeMap<u64, Node> = BTreeMap::new();
        let root = Node {
            g: 0.0,
            f: self.heuristic(&open),
            seq: self.next_seq(),
            plan: Vec::new(),
            ids: Vec::new(),
            open,
            assumed: BTreeSet::new(),
            assumes: Vec::new(),
            anchors: BTreeMap::new(),
            expected: BTreeSet::new(),
        };
        heap.push((root.key(), root.seq));
        nodes.insert(root.seq, root);
        while let Some((_, seq)) = heap.pop() {
            let mut node = nodes.remove(&seq).expect("queued node");
            if self.nesting.get() == 0 {
                *self.partial.borrow_mut() = Some(node.clone());
            }
            let Some(goal) = node.open.pop() else {
                return Ok(SubPlan {
                    steps: node.plan,
                    assumes: node.assumes,
                    cost: node.g,
                });
            };
            if self.out_of_budget() {
                return Err(Failure::Budget);
            }
            self.expanded.set(self.expanded.get() + 1);
            // Whether the goal holds is judged where the step that needs
            // it runs (everything planned for that step so far counts);
            // what establishes it goes ahead of all that (§4.6.1).
            let at = goal
                .before
                .and_then(|id| node.position(id))
                .unwrap_or(node.plan.len());
            let established = node.established_before(base, at);
            let belief = self.belief(&established);
            let mut truth = belief.eval_goal(&goal.p);
            if goal.establish && truth == Truth::Unknown {
                truth = Truth::False;
            }
            if self.trace {
                eprintln!(
                    "[plan] {:>6} t={:>7.2} g={:>8.1} d={} {:?} {} (plan {} steps, open {})",
                    self.expanded.get(),
                    self.started.elapsed().as_secs_f64(),
                    node.g,
                    goal.depth,
                    truth,
                    goal.p,
                    node.plan.len(),
                    node.open.len()
                );
            }
            let mut successors: Vec<Node> = Vec::new();
            match truth {
                Truth::True => successors.push(node),
                Truth::Unknown => {
                    successors = self.settle_unknown(node, goal, &belief);
                }
                Truth::False => {
                    if goal.depth >= self.planner.options.max_depth
                        || self.active.borrow().contains(&goal.p)
                    {
                        if self.trace {
                            eprintln!(
                                "[plan]        dropped {} (depth {} / active {})",
                                goal.p,
                                goal.depth,
                                self.active.borrow().contains(&goal.p)
                            );
                        }
                        continue;
                    }
                    successors = self.expand(&node, &goal, &belief, no_method)?;
                }
            }
            for mut n in successors {
                n.seq = self.next_seq();
                n.f = n.g + self.heuristic(&n.open);
                heap.push((n.key(), n.seq));
                nodes.insert(n.seq, n);
            }
        }
        Err(Failure::NoPlan)
    }

    fn heuristic(&self, open: &[OpenGoal]) -> f64 {
        open.iter()
            .map(|g| self.min_costs.borrow().get(&g.p).copied().unwrap_or(0.0))
            .sum()
    }

    /// §4.3: assume the fact (at the expected cost of being wrong) or
    /// probe it first. After a probe the plan follows the likelier outcome;
    /// when the evidence is even it includes the work, marked conditional.
    fn settle_unknown(
        &self,
        mut node: Node,
        goal: OpenGoal,
        belief: &StateBelief<'a>,
    ) -> Vec<Node> {
        let opts = &self.planner.options;
        let p = goal.p.clone();
        let prior = self.planner.prior(&self.base.knowledge.world, &p);
        let probe =
            ProbeFact::for_predicate(&p, self.planner.data).filter(|fact| self.probe_allowed(fact));
        let pos = node.insert_pos(goal.before);
        // What a wrong guess costs is the explicit work: nothing the walk
        // was expected to yield can be counted on then.
        let surround = Surround {
            waypoints: self.waypoints(&node, belief),
            passive: Passive::default(),
            ahead: Vec::new(),
        };
        let best = if goal.depth < opts.max_depth {
            self.candidates(&p, belief, false, &surround)
                .into_iter()
                .next()
        } else {
            None
        };
        let plannable = best.as_ref().is_some_and(|c| {
            !c.steps
                .iter()
                .any(|s| matches!(s.planned.intent, Intent::Unsupported { .. }))
        });
        let alt_cost = best
            .as_ref()
            .map_or(f64::INFINITY, |c| self.alternative_cost(c, belief));
        let expected_loss = if alt_cost.is_finite() {
            (1.0 - prior) * alt_cost
        } else {
            0.0
        };
        let probe_cost = probe.as_ref().map_or(f64::INFINITY, ProbeFact::cost_s);
        let expensive = alt_cost > opts.expensive_secs;
        let assume = !expensive && expected_loss <= probe_cost;
        if probe.is_none() && plannable && prior < NO_EVIDENCE {
            // Nothing shows it and it is likelier absent (a story flag
            // never observed): do the work, skipped should it turn out to
            // hold.
            node.open.push(OpenGoal {
                p: p.clone(),
                depth: goal.depth,
                before: goal.before,
                unless: Some(p),
                establish: true,
            });
            return vec![node];
        }
        let Some(fact) = probe.filter(|_| !assume) else {
            node.g += expected_loss;
            node.assume(p, goal.before);
            return vec![node];
        };
        let probe_id = self.next_seq();
        node.insert(
            pos,
            vec![Step::new(
                PlannedIntent::new(Intent::Probe { fact }, probe_cost),
                Vec::new(),
            )],
            vec![probe_id],
        );
        node.g += probe_cost;
        if prior > NO_EVIDENCE {
            node.assume(p, goal.before);
            return vec![node];
        }
        // The likelier outcome is that the work is needed: plan it,
        // conditional on the probe. Should nothing establish it, fall back
        // to assuming, reported as blocked and priced so it only wins when
        // the work can't be planned at all.
        let mut fallback = node.clone();
        let unsupported = Intent::Unsupported {
            reason: format!("{p} could not be planned; the plan assumes it"),
            establishes: p.clone(),
        };
        fallback.g += FALLBACK_S;
        fallback.insert(
            pos,
            vec![Step::new(
                PlannedIntent::new(unsupported, FALLBACK_S),
                vec![p.clone()],
            )],
            vec![self.next_seq()],
        );
        fallback.assume(p.clone(), goal.before);
        node.open.push(OpenGoal {
            p: p.clone(),
            depth: goal.depth,
            before: goal.before,
            unless: Some(p),
            establish: true,
        });
        vec![node, fallback]
    }

    /// What recovering from a wrong assumption costs: the cheapest
    /// candidate plus the trip its `At` needs and its other preconditions'
    /// known floor.
    fn alternative_cost(&self, c: &Candidate, belief: &StateBelief<'a>) -> f64 {
        let mut cost = c.cost;
        for pre in &c.preconditions {
            if belief.eval_goal(pre) == Truth::True {
                continue;
            }
            cost += match pre.at_map() {
                Some(map) => self
                    .route_to(map, belief)
                    .filter(|r| r.found())
                    .map_or(self.planner.options.expensive_secs, |r| r.cost_s),
                None => self.min_costs.borrow().get(pre).copied().unwrap_or(0.0),
            };
        }
        cost
    }

    /// Successors that establish the goal: the method's decomposition when
    /// one applies and can be planned, else every primitive candidate.
    fn expand(
        &self,
        node: &Node,
        goal: &OpenGoal,
        belief: &StateBelief<'a>,
        no_method: Option<&GoalPredicate>,
    ) -> Result<Vec<Node>, Failure> {
        let p = &goal.p;
        let pos = node.insert_pos(goal.before);
        if no_method != Some(p) {
            if let Some(method) = self.planner.methods.for_goal(p) {
                // The method's nested searches see what runs ahead of it.
                let mark = self.prefix.borrow().len();
                let ahead: Vec<Step> = node.plan[..pos.min(node.plan.len())].to_vec();
                self.prefix.borrow_mut().extend(ahead);
                let planned = self.plan_method(method, goal, &belief.established);
                self.prefix.borrow_mut().truncate(mark);
                match planned {
                    Ok(sub) => {
                        let mut n = node.clone();
                        let ids: Vec<u64> = sub.steps.iter().map(|_| self.next_seq()).collect();
                        let first = ids[0];
                        n.insert(pos, sub.steps.clone(), ids);
                        if let Some(c) = goal.before {
                            n.anchors.insert(c, first);
                        }
                        for a in &sub.assumes {
                            if !n.assumes.contains(a) {
                                n.assumes.push(a.clone());
                            }
                        }
                        n.g += sub.cost;
                        return Ok(vec![n]);
                    }
                    Err(Failure::Budget) => return Err(Failure::Budget),
                    Err(Failure::NoPlan) => {}
                }
            }
        }
        let surround = self.surround(node, pos, belief);
        let candidates = self.candidates(p, belief, true, &surround);
        let mut out = Vec::new();
        for mut c in candidates {
            let mut n = node.clone();
            if c.steps.is_empty() {
                // Nothing to do there: the way yields it. The step that
                // needs it says so.
                n.expected.insert(p.clone());
                if let Some(i) = goal.before.and_then(|id| n.position(id)) {
                    n.plan[i].planned.expected.push((p.clone(), 1.0));
                }
                if !c.lead.is_empty() {
                    let lead_ids: Vec<u64> = c.lead.iter().map(|_| self.next_seq()).collect();
                    n.insert(0, c.lead, lead_ids);
                }
                n.g += c.cost;
                out.push(n);
                continue;
            }
            c.add_effect(p.clone());
            for s in c.lead.iter_mut().chain(c.steps.iter_mut()) {
                s.planned.assumes.extend(c.assumes.iter().cloned());
                s.planned.unless.extend(goal.unless.iter().cloned());
            }
            let mut pos = pos;
            if !c.lead.is_empty() {
                let lead_ids: Vec<u64> = c.lead.iter().map(|_| self.next_seq()).collect();
                pos += c.lead.len();
                n.insert(0, c.lead, lead_ids);
            }
            let ids: Vec<u64> = c.steps.iter().map(|_| self.next_seq()).collect();
            let first = ids[0];
            n.insert(pos, c.steps, ids);
            // What the step needs next goes ahead of this group (its trip,
            // planned first, stays last).
            if let Some(c_id) = goal.before {
                n.anchors.insert(c_id, first);
            }
            for a in c.assumes {
                if !n.assumes.contains(&a) {
                    n.assumes.push(a);
                }
            }
            n.g += c.cost;
            // Preconditions in reverse so the first listed is worked first;
            // they must hold before the candidate's first step.
            for pre in c.preconditions.into_iter().rev() {
                n.open.push(OpenGoal {
                    p: pre,
                    depth: goal.depth + 1,
                    before: Some(first),
                    unless: goal.unless.clone(),
                    establish: false,
                });
            }
            out.push(n);
        }
        Ok(out)
    }

    /// Plans a method's subgoals in order, then the goal from primitives
    /// unless the method is complete. Cached per facts held.
    fn plan_method(
        &self,
        method: &crate::methods::Method,
        goal: &OpenGoal,
        established: &BTreeSet<GoalPredicate>,
    ) -> Result<SubPlan, Failure> {
        let key: MethodKey = (
            method.name.clone(),
            established.iter().cloned().collect(),
            goal.unless.clone(),
        );
        if let Some(cached) = self.methods.borrow().get(&key) {
            return cached
                .as_ref()
                .map(|s| (**s).clone())
                .ok_or(Failure::NoPlan);
        }
        let result = self.build_method(method, goal, established);
        if self.trace {
            eprintln!(
                "[plan]        method {} for {}: {}",
                method.name,
                goal.p,
                match &result {
                    Ok(s) => format!(
                        "{} steps, {:.1} s: {}",
                        s.steps.len(),
                        s.cost,
                        s.steps
                            .iter()
                            .map(|st| st.planned.intent.to_string())
                            .collect::<Vec<_>>()
                            .join(" | ")
                    ),
                    Err(Failure::NoPlan) => "no plan".to_string(),
                    Err(Failure::Budget) => "budget".to_string(),
                }
            );
        }
        match &result {
            Ok(sub) => {
                self.methods
                    .borrow_mut()
                    .insert(key, Some(Rc::new(sub.clone())));
            }
            Err(Failure::NoPlan) => {
                self.methods.borrow_mut().insert(key, None);
            }
            Err(Failure::Budget) => {}
        }
        result
    }

    fn build_method(
        &self,
        method: &crate::methods::Method,
        goal: &OpenGoal,
        established: &BTreeSet<GoalPredicate>,
    ) -> Result<SubPlan, Failure> {
        let mut steps: Vec<Step> = Vec::new();
        let mut assumes = Vec::new();
        let mut cost = 0.0;
        let mut est = established.clone();
        let open = |p: &GoalPredicate, establish: bool| OpenGoal {
            p: p.clone(),
            depth: goal.depth,
            before: None,
            unless: goal.unless.clone(),
            establish,
        };
        // While the subgoals are planned the goal itself is off limits:
        // meeting it again (Cut needs the S.S. Anne, whose route would
        // like Cut) is a cycle, not a plan.
        self.nesting.set(self.nesting.get() + 1);
        self.active.borrow_mut().push(goal.p.clone());
        let subgoals = (|| {
            for sub in &method.subgoals {
                let mark = self.prefix.borrow().len();
                self.prefix.borrow_mut().extend(steps.iter().cloned());
                let r = self.search(vec![open(sub, false)], &est, None);
                self.prefix.borrow_mut().truncate(mark);
                let r = r?;
                est.extend(r.effects().cloned());
                est.extend(r.assumes.iter().cloned());
                est.insert(sub.clone());
                steps.extend(r.steps);
                assumes.extend(r.assumes);
                cost += r.cost;
            }
            Ok(())
        })();
        self.active.borrow_mut().pop();
        let result = subgoals.and_then(|()| {
            if !method.complete {
                let mark = self.prefix.borrow().len();
                self.prefix.borrow_mut().extend(steps.iter().cloned());
                let r = self.search(vec![open(&goal.p, true)], &est, Some(&goal.p));
                self.prefix.borrow_mut().truncate(mark);
                let r = r?;
                steps.extend(r.steps);
                assumes.extend(r.assumes);
                cost += r.cost;
            }
            Ok(())
        });
        self.nesting.set(self.nesting.get() - 1);
        result?;
        if let Some(last) = steps.last_mut() {
            if !last.effects.contains(&goal.p) {
                last.effects.push(goal.p.clone());
            }
        }
        Ok(SubPlan {
            steps,
            assumes,
            cost,
        })
    }

    fn route_to(&self, map: &str, belief: &StateBelief<'a>) -> Option<Rc<RouteResult>> {
        let key: RouteKey = (
            map.to_string(),
            belief
                .established
                .iter()
                .filter(|p| matches!(p, GoalPredicate::World(_)))
                .cloned()
                .collect(),
        );
        if let Some(r) = self.routes.borrow().get(&key) {
            return Some(Rc::clone(r));
        }
        let t = Instant::now();
        let ctx = self.context(belief);
        let r = ctx.route_to(map);
        self.spent.borrow_mut()[0] += t.elapsed().as_secs_f64();
        if self.trace {
            eprintln!(
                "[route] {map}: found={} in {:.2} s (t={:.1})",
                r.as_ref().is_some_and(|r| r.found()),
                t.elapsed().as_secs_f64(),
                self.started.elapsed().as_secs_f64()
            );
        }
        let r = Rc::new(r?);
        self.routes.borrow_mut().insert(key, Rc::clone(&r));
        Some(r)
    }

    fn established_key(belief: &StateBelief<'a>) -> Vec<GoalPredicate> {
        belief
            .established
            .iter()
            .filter(|p| matches!(p, GoalPredicate::World(_)))
            .cloned()
            .collect()
    }

    /// The cheapest encounter tile of `map` to reach and the route to it
    /// (a map's grass may lie past a cave the landings don't cross).
    fn grass_route(&self, map: &str, belief: &StateBelief<'a>) -> GrassRoute {
        let key: RouteKey = (map.to_string(), Self::established_key(belief));
        if let Some(r) = self.grass_routes.borrow().get(&key) {
            return r.clone();
        }
        let t = Instant::now();
        let ctx = self.context(belief);
        let mut best: GrassRoute = None;
        for &(x, y) in self
            .planner
            .grass
            .get(map)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let Some(r) = ctx.route_to_tile(map, x, y) else {
                break;
            };
            if !r.found() {
                continue;
            }
            if best.as_ref().is_none_or(|(_, b)| r.cost_s < b.cost_s) {
                best = Some(((x, y), Rc::new(r)));
            }
        }
        self.grass_routes.borrow_mut().insert(key, best.clone());
        self.spent.borrow_mut()[1] += t.elapsed().as_secs_f64();
        if self.trace {
            eprintln!(
                "[grass] {map}: {:?} in {:.2} s",
                best.as_ref().map(|(t, r)| (*t, r.cost_s)),
                t.elapsed().as_secs_f64()
            );
        }
        best
    }

    /// The flood from `from` on `map` as the route planner sees it:
    /// stationary objects are walls unless a flag hides them or they are
    /// trainers, whose tiles cost what crossing them does.
    fn reach_from(&self, map: &MapData, from: (i32, i32)) -> Rc<Reach> {
        let key = (map.name.clone(), from);
        let cached = self.reaches.borrow().get(&key).map(Rc::clone);
        if let Some(r) = cached {
            return r;
        }
        let t = Instant::now();
        let ways = self.planner.crossings.get(&map.name);
        let mut obstacles = static_obstacles(map);
        if let Some(ways) = ways {
            obstacles.retain(|t| !ways.contains_key(t));
        }
        let walk = Walk {
            obstacles: &obstacles,
            surf: false,
        };
        let extra = |t: (i32, i32)| ways.and_then(|w| w.get(&t)).map_or(0, |w| w.0);
        let r = Rc::new(reach(map, from, &walk, extra));
        self.reaches.borrow_mut().insert(key, Rc::clone(&r));
        self.spent.borrow_mut()[2] += t.elapsed().as_secs_f64();
        r
    }

    /// Tiles walked from `from` to `to` on `map` around its stationary
    /// objects; `None` when no path stays on the map. Either end may be a
    /// tile the flood can't start on or enter (a ladder, an object's own
    /// tile): its neighbours stand in, a tile dearer.
    fn tile_walk(&self, map: &MapData, from: (i32, i32), to: (i32, i32)) -> Option<i32> {
        if from == to {
            return Some(0);
        }
        let around = |t: (i32, i32)| {
            [
                t,
                (t.0, t.1 + 1),
                (t.0, t.1 - 1),
                (t.0 - 1, t.1),
                (t.0 + 1, t.1),
            ]
        };
        for (i, f) in around(from).into_iter().enumerate() {
            if !map.in_bounds(f.0, f.1) {
                continue;
            }
            let reach = self.reach_from(map, f);
            let best = around(to)
                .into_iter()
                .enumerate()
                .filter_map(|(j, t)| Some(reach.cost(t)? + (i > 0) as i32 + (j > 0) as i32))
                .min();
            if best.is_some() {
                return best;
            }
        }
        None
    }

    /// Trainers met on the walk from `from` to `to` on `map`: those whose
    /// trigger tile lies on the way (the Super Nerd before Mt. Moon's
    /// fossils) and those standing on it. Beating them is a precondition.
    fn trainers_between(&self, map: &str, from: (i32, i32), to: (i32, i32)) -> Vec<String> {
        let Some(m) = self.planner.world.map(map) else {
            return Vec::new();
        };
        let Some(direct) = self.tile_walk(m, from, to) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(ways) = self.planner.crossings.get(map) {
            if let Some(path) = self.reach_from(m, from).path(to) {
                for step in path {
                    if let Some((_, Some(trainer))) = ways.get(&step.to) {
                        if !out.contains(trainer) {
                            out.push(trainer.clone());
                        }
                    }
                }
            }
        }
        let triggers = self
            .planner
            .battle_triggers
            .get(map)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for (tile, trainer) in triggers {
            let (Some(a), Some(b)) = (self.tile_walk(m, from, *tile), self.tile_walk(m, *tile, to))
            else {
                continue;
            };
            if self.trace {
                eprintln!(
                    "[plan]        trigger {trainer} on {map} {tile:?}: {from:?}->{to:?} direct {direct}, via {}",
                    a + b
                );
            }
            if a + b <= direct + ON_THE_WAY_SLACK && !out.contains(trainer) {
                out.push(trainer.clone());
            }
        }
        out
    }

    /// Where the player stands on `map` once there: the pose when on it,
    /// else the landing of the route to it.
    fn landing_on(&self, map: &str, belief: &StateBelief<'a>) -> Option<(i32, i32)> {
        let pose = self.base.pose.as_ref()?;
        if pose.map == map {
            return Some((pose.x, pose.y));
        }
        let r = self.route_to(map, belief).filter(|r| r.found())?;
        r.legs.last().map(|l| (l.to.x, l.to.y))
    }

    /// Maps the plan passes through anyway: on the routes to the `At`s
    /// still open and to the `Go`s already planned.
    fn waypoints(&self, node: &Node, belief: &StateBelief<'a>) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let dests = node
            .open
            .iter()
            .filter_map(|g| g.p.at_map().map(str::to_string))
            .chain(node.plan.iter().filter_map(|s| match &s.planned.intent {
                Intent::Go { dest } => Some(dest.clone()),
                _ => None,
            }));
        for dest in dests {
            if let Some(r) = self.route_to(&dest, belief) {
                for leg in &r.legs {
                    out.insert(leg.from.map.clone());
                    out.insert(leg.to.map.clone());
                }
            }
            out.insert(dest);
        }
        out
    }

    /// The [`NEAREST_SHOPS`] of `maps` by maps crossed from the pose (all
    /// of them, in name order, when the position is unknown).
    fn nearest<'m>(&self, maps: Vec<&'m String>) -> Vec<&'m String> {
        if self.hops.is_empty() {
            return maps;
        }
        let mut near: Vec<(u32, &'m String)> = maps
            .into_iter()
            .filter_map(|m| Some((*self.hops.get(m)?, m)))
            .collect();
        near.sort();
        near.truncate(NEAREST_SHOPS);
        near.into_iter().map(|(_, m)| m).collect()
    }

    /// A floor on what establishing `p` costs: its cheapest candidate, the
    /// trips its `At`s need (a map with no open route counts as
    /// `expensive_secs`) and, `depth` levels down, the same for its other
    /// preconditions; infinite when nothing establishes it.
    fn establish_bound(&self, p: &GoalPredicate, belief: &StateBelief<'a>, depth: u8) -> f64 {
        if belief.eval_goal(p) == Truth::True {
            return 0.0;
        }
        let Some(c) = self
            .candidates(p, belief, false, &Surround::default())
            .into_iter()
            .next()
        else {
            return f64::INFINITY;
        };
        let mut cost = c.cost;
        for pre in &c.preconditions {
            if belief.eval_goal(pre) == Truth::True {
                continue;
            }
            cost += match pre {
                GoalPredicate::World(Predicate::At { map }) => self
                    .route_to(map, belief)
                    .filter(|r| r.found())
                    .map_or(self.planner.options.expensive_secs, |r| r.cost_s),
                GoalPredicate::World(_) if depth > 0 => {
                    self.establish_bound(pre, belief, depth - 1)
                }
                _ => self.min_costs.borrow().get(pre).copied().unwrap_or(0.0),
            };
        }
        cost
    }

    /// Primitive intents establishing `p`, cheapest first, at most
    /// `candidates_per_goal`. Records the cheapest cost for the heuristic.
    fn candidates(
        &self,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        full: bool,
        surround: &Surround,
    ) -> Vec<Candidate> {
        let ctx = self.context(belief);
        let mut out: Vec<Candidate> = match p {
            GoalPredicate::World(Predicate::At { map }) => self.go_candidates(map, belief, &ctx),
            GoalPredicate::World(Predicate::Visited { map }) => {
                let mut v = self.go_candidates(map, belief, &ctx);
                for c in &mut v {
                    c.add_effect(p.clone());
                }
                v
            }
            GoalPredicate::World(Predicate::HasItem { item, n }) => {
                let mut v = self.script_candidates(p, belief, &ctx);
                v.extend(self.buy_candidates(item, *n, belief, &ctx));
                v
            }
            GoalPredicate::World(Predicate::PartyHasMove { mv }) => {
                let mut v = self.script_candidates(p, belief, &ctx);
                if let Some(hm) = hm_of_move(mv) {
                    let mon = belief
                        .party_members()
                        .and_then(|m| m.first().map(|m| m.species.clone()))
                        .unwrap_or_else(|| "lead".to_string());
                    let intent = Intent::Teach {
                        hm: hm.to_string(),
                        mon,
                    };
                    let cost = intent.cost_s(&ctx);
                    v.push(Candidate::single(intent, &ctx, cost));
                }
                v
            }
            GoalPredicate::World(Predicate::Flag { name, is: true })
                if self.planner.trainer_maps.contains_key(name) =>
            {
                let mut v = self.script_candidates(p, belief, &ctx);
                v.extend(self.beat_candidates(name, &ctx));
                v
            }
            GoalPredicate::World(_) => self.script_candidates(p, belief, &ctx),
            GoalPredicate::Caught { caught } => self.catch_candidates(caught, p, belief, &ctx),
            GoalPredicate::PokedexCaught { ge } => {
                self.catch_new_candidates(*ge, DexCount::Caught, p, belief, &ctx, surround)
            }
            GoalPredicate::PokedexSeen { ge } => {
                self.catch_new_candidates(*ge, DexCount::Seen, p, belief, &ctx, surround)
            }
            GoalPredicate::CanBeat { can_beat } => self
                .readiness_candidate(can_beat, p, belief, &ctx, &surround.passive)
                .into_iter()
                .collect(),
            GoalPredicate::Money { money } => {
                vec![self.unsupported(format!("earning ₽{money} is not planned"), p, &ctx)]
            }
            GoalPredicate::Healed { .. } | GoalPredicate::LeadHp { .. } => {
                self.heal_candidates(belief, &ctx)
            }
        };
        out.retain(|c| c.cost.is_finite());
        // Intents that failed this session the same way twice (spec §8,
        // `IntentInfeasible`) are left to the other branches.
        let infeasible = &belief.knowledge.world.infeasible;
        if !infeasible.is_empty() {
            out.retain(|c| {
                !c.steps
                    .iter()
                    .any(|s| infeasible.contains(&s.planned.intent.to_string()))
            });
        }
        out.sort_by_key(Candidate::sort_key);
        out.dedup_by(|a, b| a.preconditions == b.preconditions && a.effects() == b.effects());
        if let Some(best) = out.first() {
            self.min_costs
                .borrow_mut()
                .entry(p.clone())
                .or_insert(best.cost);
        }
        if full {
            out.truncate(self.planner.options.candidates_per_goal);
        } else {
            out.truncate(1);
        }
        out
    }

    /// `Heal` at the heal spots (Pokémon Centers, and Mom's house) the
    /// route planner reaches, the cheapest [`NEAREST_SHOPS`] trips first.
    /// A heal the session found infeasible is left out, so the others
    /// still stand (the first Switch goal run lost every plan when the
    /// heal at home failed twice).
    fn heal_candidates(&self, belief: &StateBelief<'a>, ctx: &PlanContext<'_>) -> Vec<Candidate> {
        let mut spots: Vec<(OrdF64, &String, Intent)> = self
            .planner
            .centers
            .iter()
            .filter_map(|center| {
                let intent = Intent::Heal {
                    center: center.clone(),
                };
                if self.infeasible(&intent) {
                    return None;
                }
                let trip = if self.hops.is_empty() {
                    0.0
                } else {
                    self.go_cost(center, belief)?
                };
                Some((OrdF64(trip), center, intent))
            })
            .collect();
        spots.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        spots.truncate(NEAREST_SHOPS);
        spots
            .into_iter()
            .map(|(_, _, intent)| {
                let cost = intent.cost_s(ctx);
                Candidate::single(intent, ctx, cost)
            })
            .collect()
    }

    fn unsupported(&self, reason: String, p: &GoalPredicate, ctx: &PlanContext<'_>) -> Candidate {
        let intent = Intent::Unsupported {
            reason,
            establishes: p.clone(),
        };
        let cost = intent.cost_s(ctx);
        Candidate::single(intent, ctx, cost)
    }

    /// Seconds to reach `map` by the open route; `None` when there is none.
    fn go_cost(&self, map: &str, belief: &StateBelief<'a>) -> Option<f64> {
        self.route_to(map, belief)
            .filter(|r| r.found())
            .map(|r| r.cost_s)
    }

    /// `Go` over the open route (its unknown requirements and the trainers
    /// whose trigger it crosses become preconditions), plus one `Go` per
    /// blocked alternative whose requirement is worth establishing: always
    /// when nothing is open, else only when the saving could exceed the
    /// floor of what establishing it costs.
    fn go_candidates(
        &self,
        map: &str,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
    ) -> Vec<Candidate> {
        let Some(route) = self.route_to(map, belief) else {
            return Vec::new();
        };
        let intent = Intent::Go {
            dest: map.to_string(),
        };
        let mut out = Vec::new();
        if route.found() {
            let mut c = Candidate::single(intent.clone(), ctx, route.cost_s);
            c.steps[0].planned = c.steps[0].planned.clone().with_route(&route);
            c.steps[0].route = Some(Rc::clone(&route));
            c.preconditions = route
                .assumes
                .iter()
                .cloned()
                .map(GoalPredicate::World)
                .collect();
            self.route_needs(&route, &mut c.preconditions);
            // A trip to a heal spot is the way to a fit lead, not something
            // that waits for one.
            if !self.planner.centers.iter().any(|c| c == map) && self.crosses_encounters(&route) {
                c.preconditions.push(GoalPredicate::lead_hp(LEAD_HP_MIN));
            }
            for leg in &route.legs {
                if !matches!(leg.kind, EdgeKind::Walk { .. }) || leg.from.map != leg.to.map {
                    continue;
                }
                if !self.planner.battle_triggers.contains_key(&leg.from.map) {
                    continue;
                }
                for trainer in self.trainers_between(
                    &leg.from.map,
                    (leg.from.x, leg.from.y),
                    (leg.to.x, leg.to.y),
                ) {
                    let need = GoalPredicate::flag(&trainer, true);
                    if !c.preconditions.contains(&need) {
                        c.preconditions.push(need);
                    }
                }
            }
            out.push(c);
        }
        let open_cost = if route.found() {
            route.cost_s
        } else {
            f64::INFINITY
        };
        for (req, cost) in &route.blocked {
            if open_cost.is_finite() {
                let saving = open_cost - cost;
                let floor: f64 = req
                    .iter()
                    .map(|p| self.establish_bound(&GoalPredicate::World(p.clone()), belief, 2))
                    .sum();
                if floor >= saving {
                    continue;
                }
            }
            let mut c = Candidate::single(intent.clone(), ctx, *cost);
            c.preconditions = req.iter().cloned().map(GoalPredicate::World).collect();
            // Its legs are not known yet; a trip that needs opening is long
            // enough to cross grass somewhere.
            if !self.planner.centers.iter().any(|c| c == map) {
                c.preconditions.push(GoalPredicate::lead_hp(LEAD_HP_MIN));
            }
            out.push(c);
        }
        if out.is_empty() {
            // Nothing the route planner knows leads in: the plan says so
            // rather than guessing.
            out.push(self.unsupported(
                format!("no route into {map}"),
                &GoalPredicate::at(map),
                ctx,
            ));
        }
        out
    }

    /// `Beat` for every map with an object whose script fights `trainer`.
    fn beat_candidates(&self, trainer: &str, ctx: &PlanContext<'_>) -> Vec<Candidate> {
        self.planner
            .trainer_maps
            .get(trainer)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|map| {
                let intent = Intent::Beat {
                    trainer: trainer.to_string(),
                    map: map.clone(),
                };
                let cost = intent.cost_s(ctx);
                Candidate::single(intent, ctx, cost)
            })
            .collect()
    }

    fn script_candidates(
        &self,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
    ) -> Vec<Candidate> {
        let Some(events) = self.planner.world.events() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (label, idx) in self
            .planner
            .scripts_by_effect
            .get(p)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let Some(script) = events.script(label) else {
                continue;
            };
            // Only what the player can start by talking: map scripts run on
            // their own and triggers on their tile.
            if script.kind != "object" {
                continue;
            }
            let (Some(map), Some(path)) = (&script.map, script.paths.get(*idx)) else {
                continue;
            };
            let intent = Intent::RunScript {
                script: label.clone(),
                path: *idx,
                answers: path_answers(path),
                map: map.clone(),
            };
            let cost = intent.cost_s(ctx);
            let mut c = Candidate::single(intent, ctx, cost);
            // Trainers whose trigger lies between where the player lands
            // on the map and the object.
            let object = script
                .local_id
                .filter(|_| {
                    self.planner.battle_triggers.contains_key(map)
                        || self.planner.crossings.contains_key(map)
                })
                .and_then(|id| self.planner.objects.get(&(map.clone(), id)))
                .copied();
            if let (Some((ox, oy)), Some(from)) = (object, self.landing_on(map, belief)) {
                if let Some(beside) = self.beside(map, ox, oy) {
                    for trainer in self.trainers_between(map, from, beside) {
                        let need = GoalPredicate::flag(&trainer, true);
                        if !c.preconditions.contains(&need) {
                            c.preconditions.push(need);
                        }
                    }
                }
            }
            out.push(c);
        }
        out
    }

    /// The tile the player talks to an object at `(x, y)` from: below,
    /// above, left, right, the first that is walkable.
    fn beside(&self, map: &str, x: i32, y: i32) -> Option<(i32, i32)> {
        let m = self.planner.world.map(map)?;
        [(x, y + 1), (x, y - 1), (x - 1, y), (x + 1, y)]
            .into_iter()
            .find(|&(nx, ny)| m.tile(nx, ny).is_some_and(|t| t.collision == 0))
    }

    fn buy_candidates(
        &self,
        item: &str,
        n: u32,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
    ) -> Vec<Candidate> {
        if !self.planner.data.items.contains_key(item) {
            return Vec::new();
        }
        let held = (1..n)
            .rev()
            .find(|k| {
                belief.eval(&Predicate::HasItem {
                    item: item.to_string(),
                    n: *k,
                }) == Truth::True
            })
            .unwrap_or(0);
        let count = n.saturating_sub(held).max(1);
        let marts: Vec<&String> = self
            .planner
            .marts
            .iter()
            .filter(|(_, items)| items.iter().any(|i| i == item))
            .map(|(map, _)| map)
            .collect();
        self.nearest(marts)
            .into_iter()
            .map(|map| {
                let intent = Intent::Buy {
                    item: item.to_string(),
                    count,
                    map: map.clone(),
                };
                let cost = intent.cost_s(ctx);
                let mut c = Candidate::single(intent, ctx, cost);
                // Buying `count` on top of what is held reaches `n`.
                c.add_effect(GoalPredicate::has_item(item, n));
                c
            })
            .collect()
    }

    /// A wild catch on `map` with the walk to its encounter tiles: the
    /// `Go` to the tile chosen (its own step, priced by the route to that
    /// tile, not to any landing of the map: Route 4's grass lies past Mt.
    /// Moon) and the `Catch`.
    fn wild_catch(
        &self,
        species: &str,
        map: &str,
        slot: &str,
        throws: u32,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
    ) -> Option<(Candidate, f64)> {
        let catch = Intent::Catch {
            species: species.to_string(),
            map: map.to_string(),
            slot: slot.to_string(),
            balls: throws + self.planner.params.ball_reserve,
        };
        let catch_cost = catch.cost_s(ctx);
        if !catch_cost.is_finite() {
            return None;
        }
        let mut c = Candidate::single(catch, ctx, catch_cost);
        c.preconditions.retain(|p| p.at_map().is_none());
        let travel = if slot == "land" {
            let ((x, y), route) = self.grass_route(map, belief)?;
            let go = Intent::Go {
                dest: map.to_string(),
            };
            let mut planned = PlannedIntent::new(go, route.cost_s).with_route(&route);
            planned.note = Some(format!("to the grass at ({x}, {y})"));
            let mut go = Step::new(
                planned,
                vec![
                    GoalPredicate::at(map),
                    GoalPredicate::World(Predicate::Visited {
                        map: map.to_string(),
                    }),
                ],
            );
            go.route = Some(Rc::clone(&route));
            c.steps.insert(0, go);
            for p in &route.assumes {
                let p = GoalPredicate::World(p.clone());
                if !c.preconditions.contains(&p) {
                    c.preconditions.push(p);
                }
            }
            self.route_needs(&route, &mut c.preconditions);
            route.cost_s
        } else {
            c.preconditions.insert(0, GoalPredicate::at(map));
            0.0
        };
        c.cost += travel;
        Some((c, travel))
    }

    fn catch_candidates(
        &self,
        species: &str,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
    ) -> Vec<Candidate> {
        let Some(obtain) = self.planner.obtain else {
            return vec![self.unsupported("no obtain.json".into(), p, ctx)];
        };
        let Some(entry) = obtain.species.get(species) else {
            return vec![self.unsupported(format!("{species} is not obtainable"), p, ctx)];
        };
        let ball = best_ball(ctx);
        let mult = ball_multiplier(&ball).unwrap_or(10);
        let mut out = Vec::new();
        let mut unsupported: Vec<String> = Vec::new();
        for m in &entry.methods {
            match m.method.as_str() {
                "wild" | "static" => {
                    let (Some(map), Some(slot)) = (
                        m.map.as_deref(),
                        if m.method == "static" {
                            Some("static")
                        } else {
                            m.slot.as_deref()
                        },
                    ) else {
                        continue;
                    };
                    if !matches!(slot, "land" | "water" | "static") {
                        continue;
                    }
                    if self.planner.world.map(map).is_none() {
                        continue;
                    }
                    let level = m.max_level.or(m.level).unwrap_or(5);
                    let Some(throws) = expected_throws(self.planner.data, species, level, mult)
                    else {
                        continue;
                    };
                    if let Some((c, _)) = self.wild_catch(species, map, slot, throws, belief, ctx) {
                        out.push(c);
                    }
                }
                "gift" | "trade" | "fossil" | "prize" => {
                    let (Some(script), Some(events)) =
                        (m.script.as_deref(), self.planner.world.events())
                    else {
                        continue;
                    };
                    let Some(s) = events.script(script) else {
                        continue;
                    };
                    let Some(map) = s.map.as_deref() else {
                        continue;
                    };
                    let idx = s
                        .paths
                        .iter()
                        .position(|path| path_effects_in(path, map, self.planner.world).contains(p))
                        .unwrap_or(0);
                    let Some(path) = s.paths.get(idx) else {
                        continue;
                    };
                    let intent = Intent::RunScript {
                        script: script.to_string(),
                        path: idx,
                        answers: path_answers(path),
                        map: map.to_string(),
                    };
                    let cost = intent.cost_s(ctx);
                    let mut c = Candidate::single(intent, ctx, cost);
                    if m.method == "trade" {
                        if let Some(give) = &m.give {
                            c.preconditions.push(GoalPredicate::caught(give));
                        }
                    }
                    if m.method == "fossil" {
                        if let Some(item) = &m.item {
                            c.preconditions.push(GoalPredicate::has_item(item, 1));
                        }
                    }
                    c.add_effect(p.clone());
                    out.push(c);
                }
                "evolve" => unsupported.push(format!(
                    "{species}: evolving {} ({} {}) is not planned yet",
                    m.from.as_deref().unwrap_or("?"),
                    m.how.as_deref().unwrap_or("?"),
                    m.level.map_or(String::new(), |l| l.to_string())
                )),
                "breed" => unsupported.push(format!("{species}: breeding is not planned yet")),
                _ => {}
            }
        }
        if out.is_empty() {
            let reason = unsupported
                .first()
                .cloned()
                .unwrap_or_else(|| format!("{species}: {}", entry.reasons.join("; ")));
            out.push(self.unsupported(reason, p, ctx));
        }
        out
    }

    /// New species for the Pokédex (`PokedexCaught(≥n)`): the cheapest
    /// species not yet caught, from the wild land tables, each priced as
    /// the route to its grass, the expected encounters and the catch;
    /// areas on the way to where the plan goes get a small bonus. What the
    /// walk ahead is expected to catch on its own (§4.6.2) counts toward
    /// `n`: species it likely takes are not hunted, and explicit catches
    /// cover only the remainder. One candidate: a `Go` per area then its
    /// `Catch`es, with the balls for all of them as the precondition; when
    /// the walk alone is expected to reach `n`, a look at the Trainer Card
    /// before the step that needs the count.
    fn catch_new_candidates(
        &self,
        ge: u16,
        which: DexCount,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
        surround: &Surround,
    ) -> Vec<Candidate> {
        let (known, _) = belief.pokedex_count(which);
        let need = usize::from(ge).saturating_sub(known as usize);
        if need == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(c) = self.catch_new_plan(need, which, p, belief, ctx, surround, None) {
            out.push(c);
        }
        // With balls bought first, the walk ahead catches on its own.
        if which == DexCount::Caught && !surround.ahead.is_empty() {
            if let Some(stock) = self.stock_for_walk(need, which, p, belief, ctx, surround) {
                if let Some(c) =
                    self.catch_new_plan(need, which, p, belief, ctx, surround, Some(stock))
                {
                    out.push(c);
                }
            }
        }
        out
    }

    /// Balls to buy at the plan's start so the walk ahead catches what it
    /// meets (§4.6.2): what the walk would use with a full stock plus the
    /// explicit remainder's throws and the reserve, beyond what is held;
    /// bounded by the money known. Returns the mart, the count, the stock
    /// held before and the walk priced with the purchase.
    fn stock_for_walk(
        &self,
        need: usize,
        which: DexCount,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
        surround: &Surround,
    ) -> Option<(String, u32, u32, Passive)> {
        let ball = best_ball(ctx);
        let price = self.planner.data.items.get(&ball)?.price.max(1);
        let held = self.base.item_count(&ball).unwrap_or(0);
        let reserve = self.planner.params.ball_reserve;
        let marts: Vec<&String> = self
            .planner
            .marts
            .iter()
            .filter(|(_, items)| items.contains(&ball))
            .map(|(map, _)| map)
            .collect();
        // The cheapest of the nearest marts to reach.
        let mart = self
            .nearest(marts)
            .into_iter()
            .filter_map(|m| Some((OrdF64(self.go_cost(m, belief)?), m)))
            .min()?
            .1
            .clone();
        let mut ahead = surround.ahead.clone();
        let plenty = self.passive_walk_with(&mut ahead, Some(f64::from(held) + PLENTY_OF_BALLS));
        let used = (f64::from(held) + PLENTY_OF_BALLS - plenty.balls).ceil() as u32;
        if used == 0 {
            return None;
        }
        // The remainder's throws under that walk.
        let remainder = self
            .catch_new_plan(
                need,
                which,
                p,
                belief,
                ctx,
                &Surround {
                    waypoints: surround.waypoints.clone(),
                    passive: plenty,
                    ahead: Vec::new(),
                },
                None,
            )
            .map_or(0, |c| {
                c.steps
                    .iter()
                    .filter_map(|s| match &s.planned.intent {
                        Intent::Catch { balls, .. } => Some(balls.saturating_sub(reserve)),
                        _ => None,
                    })
                    .sum()
            });
        let wanted = (used + remainder + reserve).saturating_sub(held).max(1);
        let affordable = belief.knowledge.money.value.map_or(u32::MAX, |m| m / price);
        let count = wanted.min(affordable);
        if count == 0 {
            return None;
        }
        let passive = self.passive_walk_with(&mut ahead, Some(f64::from(held + count)));
        Some((mart, count, held, passive))
    }

    /// One way to reach the count: `stock` bought at the plan's start (a
    /// `Go` to the mart and the `Buy`), then the walk's expected catches,
    /// then explicit catches for the remainder just before the step that
    /// needs the count; a look at the Trainer Card there when the walk
    /// alone is expected to reach it.
    #[allow(clippy::too_many_arguments)]
    fn catch_new_plan(
        &self,
        need: usize,
        which: DexCount,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
        surround: &Surround,
        stock: Option<(String, u32, u32, Passive)>,
    ) -> Option<Candidate> {
        let Some(obtain) = self.planner.obtain else {
            return Some(self.unsupported("no obtain.json".into(), p, ctx));
        };
        let waypoints = &surround.waypoints;
        let ball = best_ball(ctx);
        let (lead, lead_cost, passive): (Vec<Step>, f64, Cow<'_, Passive>) = match stock {
            Some((mart, count, held, passive)) => {
                let go = Intent::Go { dest: mart.clone() };
                let route = self.route_to(&mart, belief).filter(|r| r.found())?;
                let mut go_step = Step::new(
                    PlannedIntent::new(go, route.cost_s).with_route(&route),
                    vec![
                        GoalPredicate::at(&mart),
                        GoalPredicate::World(Predicate::Visited { map: mart.clone() }),
                    ],
                );
                go_step.route = Some(Rc::clone(&route));
                let buy = Intent::Buy {
                    item: ball.clone(),
                    count,
                    map: mart.clone(),
                };
                let buy_cost = buy.cost_s(ctx);
                let mut buy_planned = PlannedIntent::new(buy, buy_cost);
                buy_planned.note = Some("for what the way there catches".into());
                // Buying `count` on top of what is held.
                let buy_step = Step::new(
                    buy_planned,
                    vec![GoalPredicate::has_item(&ball, held + count)],
                );
                (
                    vec![go_step, buy_step],
                    route.cost_s + buy_cost,
                    Cow::Owned(passive),
                )
            }
            None => (Vec::new(), 0.0, Cow::Borrowed(&surround.passive)),
        };
        let expected_alone = passive.expected(which, &BTreeSet::new());
        let expected_note = |c: &mut Candidate| {
            if expected_alone >= EXPECTED_MIN {
                if let Some(s) = c.steps.first_mut() {
                    let text = format!(
                        "the way here is expected to catch {expected_alone:.1} new species"
                    );
                    s.planned.note = Some(match s.planned.note.take() {
                        Some(n) => format!("{n}; {text}"),
                        None => text,
                    });
                }
            }
        };
        if expected_alone.floor() as usize >= need {
            if !self.probe_allowed(&ProbeFact::TrainerCard) {
                // Nothing to confirm it with: expected, and judged again
                // when the step that needs the count runs.
                return Some(Candidate {
                    steps: Vec::new(),
                    lead,
                    preconditions: Vec::new(),
                    assumes: Vec::new(),
                    cost: lead_cost,
                });
            }
            let probe = Intent::Probe {
                fact: ProbeFact::TrainerCard,
            };
            let cost = probe.cost_s(ctx);
            let mut c = Candidate::single(probe, ctx, cost);
            c.steps[0].planned.note = Some(format!(
                "the walk is expected to catch {expected_alone:.1} new species; the card confirms"
            ));
            c.add_effect(p.clone());
            c.lead = lead;
            c.cost += lead_cost;
            if let Some(buy) = c.lead.get(1) {
                if let Intent::Buy { item, count, .. } = &buy.planned.intent {
                    let price = self.planner.data.items.get(item).map_or(0, |i| i.price);
                    c.preconditions.push(GoalPredicate::Money {
                        money: price * count,
                    });
                }
            }
            return Some(c);
        }
        let ball = best_ball(ctx);
        let mult = ball_multiplier(&ball).unwrap_or(10);
        let uncaught =
            |species: &str| belief.eval_goal(&GoalPredicate::caught(species)) != Truth::True;
        // The areas worth pricing: the nearest with something new.
        let mut areas: BTreeSet<&String> = BTreeSet::new();
        for (species, entry) in &obtain.species {
            if !uncaught(species) {
                continue;
            }
            for m in &entry.methods {
                if m.method == "wild" && m.slot.as_deref() == Some("land") {
                    if let Some(map) = m
                        .map
                        .as_ref()
                        .filter(|m| self.planner.grass.contains_key(*m))
                    {
                        areas.insert(map);
                    }
                }
            }
        }
        let areas: BTreeSet<&String> = if self.hops.is_empty() {
            areas
        } else {
            let mut near: Vec<(u32, &String)> = areas
                .into_iter()
                .filter_map(|m| Some((*self.hops.get(m)?, m)))
                .collect();
            near.sort();
            near.truncate(NEAREST_AREAS);
            near.into_iter().map(|(_, m)| m).collect()
        };
        // (priced cost, species, map, throws, candidate)
        let mut options: Vec<(OrdF64, String, String, u32, Candidate)> = Vec::new();
        for (species, entry) in &obtain.species {
            if !uncaught(species) {
                continue;
            }
            let mut best: Option<(OrdF64, String, u32, Candidate)> = None;
            for m in &entry.methods {
                if m.method != "wild" || m.slot.as_deref() != Some("land") {
                    continue;
                }
                let Some(map) = m.map.as_deref() else {
                    continue;
                };
                if !areas.contains(&map.to_string()) {
                    continue;
                }
                let level = m.max_level.or(m.level).unwrap_or(5);
                let Some(throws) = expected_throws(self.planner.data, species, level, mult) else {
                    continue;
                };
                let Some((c, travel)) = self.wild_catch(species, map, "land", throws, belief, ctx)
                else {
                    continue;
                };
                let mut priced = c.cost;
                if waypoints.contains(map) {
                    priced -= travel.min(LOCALITY_BONUS_S);
                }
                let key = (OrdF64(priced), map.to_string());
                if best
                    .as_ref()
                    .is_none_or(|(b, bm, _, _)| (*b, bm.as_str()) > (key.0, key.1.as_str()))
                {
                    best = Some((key.0, key.1, throws, c));
                }
            }
            if let Some((cost, map, throws, c)) = best {
                options.push((cost, species.clone(), map, throws, c));
            }
        }
        options.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        // Cheapest first, leaving out what the walk likely catches anyway,
        // until the explicit catches plus what the walk is still expected
        // to add reach the shortfall.
        let mut chosen: BTreeSet<String> = BTreeSet::new();
        let mut order: Vec<usize> = Vec::new();
        loop {
            let expected = passive.expected(which, &chosen);
            if order.len() + expected.floor() as usize >= need {
                break;
            }
            let next = options
                .iter()
                .position(|o| {
                    !chosen.contains(&o.1) && passive.probability(which, &o.1) < PASSIVE_LIKELY
                })
                .or_else(|| options.iter().position(|o| !chosen.contains(&o.1)));
            let Some(i) = next else {
                return Some(self.unsupported(
                    format!(
                        "only {} new species can be caught in the wild; {need} are needed",
                        options.len()
                    ),
                    p,
                    ctx,
                ));
            };
            chosen.insert(options[i].1.clone());
            order.push(i);
        }
        order.sort_unstable();
        let options: Vec<_> = order.into_iter().map(|i| options[i].clone()).collect();
        // One `Go` per area, nearest area first.
        let mut areas: Vec<(OrdF64, String)> = Vec::new();
        for (_, _, map, _, c) in &options {
            if !areas.iter().any(|(_, m)| m == map) {
                areas.push((OrdF64(c.steps[0].planned.cost_s), map.clone()));
            }
        }
        areas.sort();
        let mut steps: Vec<Step> = Vec::new();
        let mut cost = 0.0;
        let mut throws_total = 0;
        // What the trips need (a gate's Cut, a trainer on the way) and
        // assume, kept from every species' own candidate: without them
        // the trip was ordered before what opens it (flash-5: Diglett's
        // Cave through the Route 2 Cut tree, 24 steps before Cut).
        let mut preconditions = vec![GoalPredicate::has_item(&ball, 0)];
        let mut assumes = Vec::new();
        for (_, area) in &areas {
            let mut first = true;
            for (_, _, map, throws, c) in &options {
                if map != area {
                    continue;
                }
                for (i, step) in c.steps.iter().enumerate() {
                    if i == 0 && !first {
                        // The area's `Go` is already in.
                        continue;
                    }
                    steps.push(step.clone());
                    cost += step.planned.cost_s;
                }
                first = false;
                throws_total += throws;
                for pre in &c.preconditions {
                    if !preconditions.contains(pre) {
                        preconditions.push(pre.clone());
                    }
                }
                for a in &c.assumes {
                    if !assumes.contains(a) {
                        assumes.push(a.clone());
                    }
                }
            }
        }
        preconditions[0] =
            GoalPredicate::has_item(&ball, throws_total + self.planner.params.ball_reserve);
        let mut c = Candidate {
            steps,
            lead,
            preconditions,
            assumes,
            cost: cost + lead_cost,
        };
        if let Some(buy) = c.lead.get(1) {
            if let Intent::Buy { item, count, .. } = &buy.planned.intent {
                let price = self.planner.data.items.get(item).map_or(0, |i| i.price);
                c.preconditions.push(GoalPredicate::Money {
                    money: price * count,
                });
            }
        }
        expected_note(&mut c);
        c.add_effect(p.clone());
        Some(c)
    }

    /// The readiness planner's cheapest preparation for `trainer`, as
    /// `Go`/`Train`/`Catch` steps, with the lead as the way there leaves
    /// it (§4.6.2): trained to the level an earlier `Train` reached and
    /// the experience of the trainers beaten on top.
    fn readiness_candidate(
        &self,
        trainer: &str,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
        passive: &Passive,
    ) -> Option<Candidate> {
        let key = (
            trainer.to_string(),
            passive.exp as u64,
            passive.lead_level.unwrap_or(0),
        );
        if let Some(c) = self.readiness.borrow().get(&key) {
            return c.clone();
        }
        let t = Instant::now();
        let built = self.build_readiness(trainer, p, belief, ctx, key.1, passive.lead_level);
        self.spent.borrow_mut()[3] += t.elapsed().as_secs_f64();
        if self.trace {
            eprintln!(
                "[plan]        readiness for {trainer} (+{} exp, trained to {:?}): {:.2} s",
                key.1,
                passive.lead_level,
                t.elapsed().as_secs_f64()
            );
        }
        self.readiness.borrow_mut().insert(key, built.clone());
        built
    }

    fn build_readiness(
        &self,
        trainer: &str,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
        exp: u64,
        trained: Option<u8>,
    ) -> Option<Candidate> {
        let Some(mut party) = belief.party_members() else {
            return Some(self.unsupported(
                format!("the party is unknown, so readiness for {trainer} can't be planned"),
                p,
                ctx,
            ));
        };
        if !self.planner.data.trainers.contains_key(trainer) {
            return None;
        }
        // The lead trains and fights the trainers on the way: it starts
        // the preparation that much further on (from the floor of its
        // level, its exact total being unknown).
        if let Some(lead) = party.first_mut() {
            let was = lead.level;
            if let Some(level) = trained {
                lead.level = lead.level.max(level);
                lead.exp = None;
            }
            if exp > 0 {
                if let Some(sp) = self.planner.data.species(&lead.species) {
                    let total = exp_for_level(&sp.growth_rate, lead.level) + exp;
                    lead.exp = Some(total);
                    lead.level = level_for_exp(&sp.growth_rate, total).max(lead.level);
                }
            }
            if lead.level > was {
                // Levelled on the way, it learnt its moves on the way too
                // (every new move is accepted): the species' level-up set.
                lead.moves.clear();
            }
        }
        let areas = self.training_areas(belief);
        let request = Request {
            party,
            targets: vec![trainer.to_string()],
            areas,
            confidence: self.planner.options.confidence,
            money: belief.knowledge.money.value.unwrap_or(0),
            data: self.planner.data,
        };
        let plans = plan_preparation(&request, 1);
        let Some(plan) = plans.first() else {
            return Some(self.unsupported(
                format!("no training or catching readies the party for {trainer}"),
                p,
                ctx,
            ));
        };
        let mut steps: Vec<Step> = Vec::new();
        let mut cost = 0.0;
        let mut here: Option<String> = None;
        for step in &plan.steps {
            let (map, intent, minutes) = match step {
                PlanStep::Train {
                    species,
                    to,
                    map,
                    minutes,
                    ..
                } => (
                    map.clone(),
                    Intent::Train {
                        map: map.clone(),
                        species: species.clone(),
                        level: *to,
                    },
                    *minutes,
                ),
                PlanStep::Catch {
                    species,
                    map,
                    minutes,
                    balls,
                    ..
                } => (
                    map.clone(),
                    Intent::Catch {
                        species: species.clone(),
                        map: map.clone(),
                        slot: "land".to_string(),
                        balls: *balls,
                    },
                    *minutes,
                ),
            };
            if here.as_deref() != Some(map.as_str()) {
                let go = Intent::Go { dest: map.clone() };
                let Some(go_cost) = self.go_cost(&map, belief) else {
                    return Some(self.unsupported(
                        format!("no route to {map} to ready the party for {trainer}"),
                        p,
                        ctx,
                    ));
                };
                let planned = PlannedIntent::new(go, go_cost);
                steps.push(Step::new(
                    planned,
                    vec![
                        GoalPredicate::at(&map),
                        GoalPredicate::World(Predicate::Visited { map: map.clone() }),
                    ],
                ));
                cost += go_cost;
                here = Some(map.clone());
            }
            let step_cost = minutes * 60.0;
            steps.push(Step::new(PlannedIntent::new(intent, step_cost), Vec::new()));
            cost += step_cost;
        }
        let confidence = self.planner.options.confidence;
        if plan.min_confidence() < confidence {
            // The best the window reaches falls short: the party trains
            // that far anyway and readiness is judged again after (the
            // goal loop replans after every `Train`); the shortfall is
            // priced so a way that gets there wins when there is one.
            let Some(last) = steps.last_mut() else {
                return Some(self.unsupported(
                    format!("no training or catching readies the party for {trainer}"),
                    p,
                    ctx,
                ));
            };
            let shortfall = (confidence - plan.min_confidence()) / confidence;
            last.planned.note = Some(format!(
                "reaches only {:.0}% against {trainer}: readiness is judged again after",
                plan.min_confidence() * 100.0
            ));
            last.planned
                .expected
                .push((p.clone(), plan.min_confidence()));
            cost += shortfall * self.planner.params.unsupported_s;
        }
        let mut preconditions = Vec::new();
        if steps.iter().any(|s| {
            matches!(
                s.planned.intent,
                Intent::Train { .. } | Intent::Catch { .. }
            )
        }) {
            preconditions.push(GoalPredicate::lead_hp(LEAD_HP_MIN));
        }
        // No steps and the confidence met: the way there readies the
        // lead by itself.
        let mut c = Candidate {
            steps,
            lead: Vec::new(),
            preconditions,
            assumes: Vec::new(),
            cost,
        };
        c.add_effect(p.clone());
        Some(c)
    }

    /// Nearest maps with a land encounter table, for the readiness
    /// planner: the closest by maps crossed, priced by the route to their
    /// grass (those with none are left out).
    fn training_areas(&self, belief: &StateBelief<'a>) -> Vec<Area> {
        let world = self.planner.world;
        let has_land = |map: &str| self.planner.grass.contains_key(map);
        let Some(pose) = &self.base.pose else {
            return DEFAULT_AREAS
                .iter()
                .filter(|m| has_land(m))
                .map(|m| Area {
                    map: (*m).to_string(),
                    travel_minutes: 3.0,
                    heal_minutes: 3.0,
                })
                .collect();
        };
        let from_here = hops(world, &pose.map);
        let mut near: Vec<(u32, &String)> = from_here
            .iter()
            .filter(|(m, _)| has_land(m))
            .map(|(m, d)| (*d, m))
            .collect();
        near.sort();
        near.truncate(TRAINING_AREAS * 2);
        // The nearest Center from here stands in for each area's.
        let center = self
            .planner
            .centers
            .iter()
            .filter_map(|c| {
                let r = self.route_to(c, belief)?;
                r.found().then_some(r.cost_s)
            })
            .min_by(f64::total_cmp)
            .unwrap_or(120.0);
        let mut areas: Vec<(OrdF64, Area)> = near
            .into_iter()
            .filter_map(|(_, map)| {
                let (_, route) = self.grass_route(map, belief)?;
                Some((
                    OrdF64(route.cost_s),
                    Area {
                        map: map.clone(),
                        travel_minutes: route.cost_s / 60.0,
                        heal_minutes: (2.0 * center) / 60.0 + 0.5,
                    },
                ))
            })
            .collect();
        areas.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.map.cmp(&b.1.map)));
        areas.truncate(TRAINING_AREAS);
        areas.into_iter().map(|(_, a)| a).collect()
    }
}

/// Drops the work the plan already did: a step whose every effect an
/// earlier step established (the same fact needed by two later steps is
/// planned once for each, the second before the first is known to hold),
/// a `Go` to the map the steps so far leave the player on, and a `Go`
/// straight before another `Go`. Returns the steps and the cost dropped.
fn dedupe_steps(steps: Vec<Step>) -> (Vec<Step>, f64) {
    let mut done: BTreeSet<GoalPredicate> = BTreeSet::new();
    let mut kept: Vec<Step> = Vec::new();
    let mut dropped = 0.0;
    // The map the steps so far leave the player on.
    let mut here: Option<String> = None;
    for step in steps {
        let intent = &step.planned.intent;
        let repeat = !step.effects.is_empty()
            && !matches!(intent, Intent::Go { .. } | Intent::Heal { .. })
            && step.effects.iter().all(|e| done.contains(e));
        if repeat {
            dropped += step.planned.cost_s;
            continue;
        }
        if let Intent::Go { dest } = intent {
            if here.as_deref() == Some(dest) {
                dropped += step.planned.cost_s;
                continue;
            }
            if let Some(last) = kept.last() {
                if matches!(last.planned.intent, Intent::Go { .. }) {
                    dropped += last.planned.cost_s;
                    kept.pop();
                }
            }
        }
        match intent {
            Intent::Go { dest } => here = Some(dest.clone()),
            Intent::Talk { map, .. }
            | Intent::RunScript { map, .. }
            | Intent::Beat { map, .. }
            | Intent::Train { map, .. }
            | Intent::Catch { map, .. }
            | Intent::Buy { map, .. } => here = Some(map.clone()),
            Intent::Heal { center } => here = Some(center.clone()),
            Intent::Battle { .. }
            | Intent::Teach { .. }
            | Intent::Probe { .. }
            | Intent::Save
            | Intent::Unstick
            | Intent::Unsupported { .. } => {}
        }
        done.extend(step.effects.iter().cloned());
        kept.push(step);
    }
    (kept, dropped)
}

/// Keeps the first probe of each fact (the screen stays read) and moves it
/// before the first step that relies on what it shows; returns the steps
/// and the cost of the probes dropped. A probe with a note is deliberate
/// (a look at the card where the walk should have reached a count): it
/// stays where it is and stands for the fact's other probes.
fn dedupe_probes(steps: Vec<PlannedIntent>, data: &GameData) -> (Vec<PlannedIntent>, f64) {
    let deliberate: BTreeSet<ProbeFact> = steps
        .iter()
        .filter_map(|s| match &s.intent {
            Intent::Probe { fact } if s.note.is_some() => Some(fact.clone()),
            _ => None,
        })
        .collect();
    let mut seen: BTreeSet<ProbeFact> = BTreeSet::new();
    let mut saved = 0.0;
    let mut out: Vec<PlannedIntent> = Vec::with_capacity(steps.len());
    let mut probes: Vec<PlannedIntent> = Vec::new();
    for s in steps {
        if let Intent::Probe { fact } = &s.intent {
            if s.note.is_some() {
                if !seen.insert(fact.clone()) {
                    saved += s.cost_s;
                    continue;
                }
            } else if deliberate.contains(fact) || !seen.insert(fact.clone()) {
                saved += s.cost_s;
                continue;
            } else {
                probes.push(s);
                continue;
            }
        }
        out.push(s);
    }
    for probe in probes.into_iter().rev() {
        let Intent::Probe { fact } = &probe.intent else {
            unreachable!()
        };
        let relies = |s: &PlannedIntent| {
            s.assumes
                .iter()
                .chain(s.unless.iter())
                .any(|p| ProbeFact::for_predicate(p, data).as_ref() == Some(fact))
        };
        let at = out.iter().position(relies).unwrap_or(0);
        out.insert(at, probe);
    }
    (out, saved)
}

/// Maps crossed from `from` to every map (breadth-first over warps and
/// connections), in name order for equal distances.
fn hops(world: &World, from: &str) -> BTreeMap<String, u32> {
    let mut dist = BTreeMap::from([(from.to_owned(), 0)]);
    let mut queue = VecDeque::from([from.to_owned()]);
    while let Some(name) = queue.pop_front() {
        let Some(map) = world.map(&name) else {
            continue;
        };
        let d = dist[&name];
        let next: BTreeSet<String> = map
            .warps
            .iter()
            .filter_map(|w| world.name_of(&w.dest_map))
            .chain(map.connections.iter().filter_map(|c| world.name_of(&c.map)))
            .map(str::to_owned)
            .collect();
        for n in next {
            if !dist.contains_key(&n) {
                dist.insert(n.clone(), d + 1);
                queue.push_back(n);
            }
        }
    }
    dist
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_grammar() {
        assert_eq!(
            parse_goal("catch rattata").unwrap(),
            GoalPredicate::caught("SPECIES_RATTATA")
        );
        assert_eq!(
            parse_goal("catch SPECIES_RATTATA").unwrap(),
            GoalPredicate::caught("SPECIES_RATTATA")
        );
        assert_eq!(
            parse_goal("flag FLAG_SYS_GAME_CLEAR").unwrap(),
            GoalPredicate::flag("FLAG_SYS_GAME_CLEAR", true)
        );
        assert_eq!(parse_goal("badge 3").unwrap(), GoalPredicate::badge(3));
        assert_eq!(
            parse_goal("flag FLAG_BADGE03_GET").unwrap(),
            GoalPredicate::badge(3)
        );
        assert_eq!(
            parse_goal("at Route2").unwrap(),
            GoalPredicate::at("Route2")
        );
        assert_eq!(
            parse_goal("item poke_ball 10").unwrap(),
            GoalPredicate::has_item("ITEM_POKE_BALL", 10)
        );
        assert!(parse_goal("badge 9").is_err());
        assert!(parse_goal("win").is_err());
    }

    #[test]
    fn experience_maps_back_to_a_level() {
        let growth = "GROWTH_MEDIUM_SLOW";
        assert_eq!(level_for_exp(growth, 0), 1);
        let at_20 = exp_for_level(growth, 20);
        assert_eq!(level_for_exp(growth, at_20), 20);
        assert_eq!(level_for_exp(growth, at_20 + 1), 20);
        assert_eq!(level_for_exp(growth, exp_for_level(growth, 21)), 21);
    }

    #[test]
    fn a_noted_probe_stands_for_the_facts_other_probes() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(data) = GameData::load(root.join("data/world/gamedata.json")) else {
            return;
        };
        let card = |note: Option<&str>| {
            let mut p = PlannedIntent::new(
                Intent::Probe {
                    fact: ProbeFact::TrainerCard,
                },
                8.0,
            );
            p.note = note.map(str::to_owned);
            p
        };
        let go = PlannedIntent::new(
            Intent::Go {
                dest: "Route3".into(),
            },
            10.0,
        );
        let steps = vec![card(None), go.clone(), card(Some("confirms"))];
        let (out, saved) = dedupe_probes(steps, &data);
        assert_eq!(saved, 8.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], go);
        assert_eq!(out[1].note.as_deref(), Some("confirms"));
        // Without a note the first probe is kept and the later one dropped.
        let steps = vec![card(None), go.clone(), card(None)];
        let (out, saved) = dedupe_probes(steps, &data);
        assert_eq!(saved, 8.0);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0].intent, Intent::Probe { .. }));
    }

    #[test]
    fn steps_go_ahead_of_what_needs_them() {
        let step = |name: &str| {
            Step::new(
                PlannedIntent::new(
                    Intent::Go {
                        dest: name.to_string(),
                    },
                    1.0,
                ),
                Vec::new(),
            )
        };
        let mut node = Node {
            g: 0.0,
            f: 0.0,
            seq: 0,
            plan: vec![step("goal")],
            ids: vec![1],
            open: Vec::new(),
            assumed: BTreeSet::new(),
            assumes: Vec::new(),
            anchors: BTreeMap::new(),
            expected: BTreeSet::new(),
        };
        // The goal's trip goes just ahead of it and anchors it.
        let pos = node.insert_pos(Some(1));
        assert_eq!(pos, 0);
        node.insert(pos, vec![step("trip")], vec![2]);
        node.anchors.insert(1, 2);
        // The next thing the goal needs goes ahead of the trip, and
        // becomes the anchor in turn.
        let pos = node.insert_pos(Some(1));
        assert_eq!(pos, 0);
        node.insert(pos, vec![step("need")], vec![3]);
        node.anchors.insert(1, 3);
        // What the trip needs goes ahead of the trip, after the need.
        let pos = node.insert_pos(Some(2));
        assert_eq!(pos, 1);
        node.insert(pos, vec![step("trip-need")], vec![4]);
        let names: Vec<String> = node
            .plan
            .iter()
            .map(|s| s.planned.intent.to_string())
            .collect();
        assert_eq!(names, ["Go(need)", "Go(trip-need)", "Go(trip)", "Go(goal)"]);
        assert_eq!(node.ids, [3, 4, 2, 1]);
        assert_eq!(node.insert_pos(None), 4);
    }
}
