//! Goal planner (spec §4): backward-chaining A\* from a goal predicate to
//! the intents that establish it, priced through the route planner and
//! bounded by methods, a depth limit and a node budget.
//!
//! The open set holds partial plans. Each expansion takes the last unmet
//! predicate of a plan and either drops it (the belief plus the plan's
//! effects make it true), settles it (an `Unknown` fact is assumed or
//! probed, §4.3) or establishes it (a method's decomposition first, then
//! every primitive intent whose effects cover it). Ties break on cost, then
//! intent name and map, so the same input always yields the same plan.

use std::cell::{Cell, RefCell};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque};
use std::fmt;
use std::rc::Rc;

use pokebot_gamedata::mechanics::ball_multiplier;
use pokebot_gamedata::GameData;
use pokebot_state::priors::NO_EVIDENCE;
use pokebot_state::{Fact, PlayerPose, Priors, SavedKnowledge, WorldBelief};
use pokebot_world::predicate::{BeliefView, Predicate, Truth};
use pokebot_world::route::{PlaceGraph, RouteResult, UnknownPolicy};
use pokebot_world::World;
use serde::{Deserialize, Serialize};

use crate::belief_adapter::{snapshot_id, StateBelief};
use crate::intents::{
    best_ball, expected_throws, hm_of_move, path_answers, path_effects, CostParams, Effect,
    Entries, GoalBelief, GoalPredicate, Intent, Obtain, PlanContext, ProbeFact,
};
use crate::methods::Methods;
use crate::prepare::{plan_preparation, Area, PlanStep, Request};

/// Minutes per map crossed, for the readiness planner's travel estimates.
const MINUTES_PER_MAP: f64 = 0.6;
/// Training/catching areas offered to the readiness planner.
const TRAINING_AREAS: usize = 4;
/// Charged when a probed fact has to be assumed because nothing establishes
/// it: beyond any real plan, so the work is always preferred.
const FALLBACK_S: f64 = 1.0e7;
/// Areas assumed when the position is unknown.
const DEFAULT_AREAS: [&str; 4] = ["Route1", "Route22", "Route2", "ViridianForest"];

#[derive(Debug, Clone, PartialEq)]
pub struct PlanOptions {
    /// A wrong assumption dearer than this makes a probe mandatory (§4.3).
    pub expensive_secs: f64,
    /// Subgoal nesting allowed through intent preconditions.
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
}

impl Default for PlanOptions {
    fn default() -> Self {
        PlanOptions {
            expensive_secs: 600.0,
            max_depth: 6,
            node_budget: 20_000,
            confidence: 0.9,
            candidates_per_goal: 8,
            unknown_edge_alt_s: 120.0,
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
}

impl PlannedIntent {
    fn new(intent: Intent, cost_s: f64) -> PlannedIntent {
        PlannedIntent {
            intent,
            cost_s,
            assumes: Vec::new(),
            unless: Vec::new(),
            note: None,
        }
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
    /// The node budget ran out first.
    Budget { goal: GoalPredicate, nodes: usize },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NoPlan { goal } => write!(f, "no plan establishes {goal}"),
            PlanError::Budget { goal, nodes } => {
                write!(f, "planning {goal} exceeded the budget of {nodes} nodes")
            }
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
    entries: Entries,
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
        if let Some(events) = world.events() {
            for (label, script) in &events.scripts {
                if script.map.is_none() {
                    continue;
                }
                for (i, path) in script.paths.iter().enumerate() {
                    for p in path_effects(path) {
                        scripts_by_effect
                            .entry(p)
                            .or_default()
                            .push((label.clone(), i));
                    }
                }
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
            entries: Entries::build(world, graph),
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
            routes: RefCell::new(HashMap::new()),
            min_costs: RefCell::new(BTreeMap::new()),
            readiness: RefCell::new(BTreeMap::new()),
            methods: RefCell::new(BTreeMap::new()),
            hops: pose
                .as_ref()
                .map(|p| hops(self.world, &p.map))
                .unwrap_or_default(),
            seq: Cell::new(0),
            trace: std::env::var_os("POKEBOT_PLAN_TRACE").is_some(),
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
        let sub = result.map_err(|e| match e {
            Failure::NoPlan => PlanError::NoPlan { goal: goal.clone() },
            Failure::Budget => PlanError::Budget {
                goal: goal.clone(),
                nodes: self.options.node_budget,
            },
        })?;
        let mut assumes = sub.assumes;
        assumes.sort();
        assumes.dedup();
        let steps: Vec<PlannedIntent> = sub.steps.into_iter().map(|s| s.planned).collect();
        let (intents, saved) = dedupe_probes(steps, self.data);
        Ok(Plan {
            intents,
            assumes,
            cost_s: sub.cost - saved,
            belief_snapshot: snapshot_id(knowledge),
        })
    }

    /// P(`p` holds) from the priors: flags and visits through the rules,
    /// everything else the no-evidence rate.
    pub fn prior(&self, belief: &WorldBelief, p: &GoalPredicate) -> f64 {
        let Some(priors) = self.priors else {
            return NO_EVIDENCE;
        };
        match p {
            GoalPredicate::World(Predicate::Flag { name, is }) => {
                let p = priors.probability(belief, &Fact::flag(name.clone()));
                if *is {
                    p
                } else {
                    1.0 - p
                }
            }
            GoalPredicate::World(Predicate::Badge { n }) => {
                priors.probability(belief, &Fact::flag(Predicate::badge_flag(*n)))
            }
            GoalPredicate::World(Predicate::Visited { map }) => {
                priors.probability(belief, &Fact::visited(map.clone()))
            }
            _ => NO_EVIDENCE,
        }
    }

    fn penalty_model(&self, belief: &WorldBelief) -> PenaltyModel {
        let priors = self.priors.cloned();
        let belief = belief.clone();
        let alt = self.options.unknown_edge_alt_s;
        Rc::new(move |p: &Predicate| {
            let prob = match (&priors, p) {
                (Some(priors), Predicate::Flag { name, is }) => {
                    let t = priors.probability(&belief, &Fact::flag(name.clone()));
                    if *is {
                        t
                    } else {
                        1.0 - t
                    }
                }
                (Some(priors), Predicate::Badge { n }) => {
                    priors.probability(&belief, &Fact::flag(Predicate::badge_flag(*n)))
                }
                (Some(priors), Predicate::Visited { map }) => {
                    priors.probability(&belief, &Fact::visited(map.clone()))
                }
                _ => NO_EVIDENCE,
            };
            (1.0 - prob) * alt
        })
    }
}

/// One planned step with the facts it establishes.
#[derive(Debug, Clone)]
struct Step {
    planned: PlannedIntent,
    effects: Vec<GoalPredicate>,
}

/// A primitive way to establish a predicate: one or more steps with what
/// they need first.
#[derive(Debug, Clone)]
struct Candidate {
    steps: Vec<Step>,
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
            steps: vec![Step {
                planned: PlannedIntent::new(intent, cost),
                effects,
            }],
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
            .steps
            .iter()
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
}

impl Node {
    fn key(&self) -> Reverse<(OrdF64, u64)> {
        Reverse((OrdF64(self.f), self.seq))
    }

    /// What holds when the step `before` runs: the base facts, the
    /// assumptions and the effects of the steps ahead of it.
    fn established_at(
        &self,
        base: &BTreeSet<GoalPredicate>,
        before: Option<u64>,
    ) -> BTreeSet<GoalPredicate> {
        let mut out = base.clone();
        out.extend(self.assumed.iter().cloned());
        let end = before
            .and_then(|id| self.ids.iter().position(|i| *i == id))
            .unwrap_or(self.plan.len());
        for step in &self.plan[..end] {
            out.extend(step.effects.iter().cloned());
        }
        out
    }

    /// Records that `p` is relied on by the step that needed it.
    fn assume(&mut self, p: GoalPredicate, before: Option<u64>) {
        let at = before
            .and_then(|id| self.ids.iter().position(|i| *i == id))
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

    /// Prepends `steps` (they run before everything planned so far).
    fn prepend(&mut self, steps: Vec<Step>, ids: Vec<u64>) {
        let mut plan = steps;
        plan.append(&mut self.plan);
        self.plan = plan;
        let mut all = ids;
        all.append(&mut self.ids);
        self.ids = all;
    }
}

type RouteKey = (String, Vec<GoalPredicate>);
type MethodKey = (String, Vec<GoalPredicate>, Option<GoalPredicate>);

/// One planning call: the belief, the shared budget and the caches.
struct Session<'p, 'a> {
    planner: &'p Planner<'a>,
    base: StateBelief<'a>,
    expanded: Cell<usize>,
    routes: RefCell<HashMap<RouteKey, Rc<RouteResult>>>,
    /// Cheapest primitive cost per predicate (the heuristic).
    min_costs: RefCell<BTreeMap<GoalPredicate, f64>>,
    /// Readiness plans per trainer.
    readiness: RefCell<BTreeMap<String, Option<Candidate>>>,
    /// Method decompositions per (method, facts held, condition).
    methods: RefCell<BTreeMap<MethodKey, Option<Rc<SubPlan>>>>,
    /// Maps crossed from the pose's map, for unroutable destinations.
    hops: BTreeMap<String, u32>,
    seq: Cell<u64>,
    /// `POKEBOT_PLAN_TRACE=1` prints every expansion.
    trace: bool,
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
            entries: &self.planner.entries,
        }
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
        };
        heap.push((root.key(), root.seq));
        nodes.insert(root.seq, root);
        while let Some((_, seq)) = heap.pop() {
            let mut node = nodes.remove(&seq).expect("queued node");
            let Some(goal) = node.open.pop() else {
                return Ok(SubPlan {
                    steps: node.plan,
                    assumes: node.assumes,
                    cost: node.g,
                });
            };
            if self.expanded.get() >= self.planner.options.node_budget {
                return Err(Failure::Budget);
            }
            self.expanded.set(self.expanded.get() + 1);
            let established = node.established_at(base, goal.before);
            let belief = self.belief(&established);
            let mut truth = belief.eval_goal(&goal.p);
            if goal.establish && truth == Truth::Unknown {
                truth = Truth::False;
            }
            if self.trace {
                eprintln!(
                    "[plan] {:>6} g={:>8.1} d={} {:?} {} (plan {} steps, open {})",
                    self.expanded.get(),
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
                    if goal.depth >= self.planner.options.max_depth {
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
        let probe = ProbeFact::for_predicate(&p, self.planner.data);
        let alt_cost = if goal.depth < opts.max_depth {
            self.candidates(&p, belief, false)
                .first()
                .map_or(f64::INFINITY, |c| self.alternative_cost(c, belief))
        } else {
            f64::INFINITY
        };
        let expected_loss = if alt_cost.is_finite() {
            (1.0 - prior) * alt_cost
        } else {
            0.0
        };
        let probe_cost = probe.as_ref().map_or(f64::INFINITY, ProbeFact::cost_s);
        let expensive = alt_cost > opts.expensive_secs;
        let assume = !expensive && expected_loss <= probe_cost;
        let Some(fact) = probe.filter(|_| !assume) else {
            node.g += expected_loss;
            node.assume(p, goal.before);
            return vec![node];
        };
        let probe_id = self.next_seq();
        node.prepend(
            vec![Step {
                planned: PlannedIntent::new(Intent::Probe { fact }, probe_cost),
                effects: Vec::new(),
            }],
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
        fallback.prepend(
            vec![Step {
                planned: PlannedIntent::new(unsupported, FALLBACK_S),
                effects: vec![p.clone()],
            }],
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
        if no_method != Some(p) {
            if let Some(method) = self.planner.methods.for_goal(p) {
                match self.plan_method(method, goal, &belief.established) {
                    Ok(sub) => {
                        let mut n = node.clone();
                        let ids: Vec<u64> = sub.steps.iter().map(|_| self.next_seq()).collect();
                        n.prepend(sub.steps.clone(), ids);
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
        let candidates = self.candidates(p, belief, true);
        let mut out = Vec::new();
        for mut c in candidates {
            let mut n = node.clone();
            c.add_effect(p.clone());
            for s in &mut c.steps {
                s.planned.assumes.extend(c.assumes.iter().cloned());
                s.planned.unless.extend(goal.unless.iter().cloned());
            }
            let ids: Vec<u64> = c.steps.iter().map(|_| self.next_seq()).collect();
            let first = ids[0];
            n.prepend(c.steps, ids);
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
        for sub in &method.subgoals {
            let r = self.search(vec![open(sub, false)], &est, None)?;
            est.extend(r.effects().cloned());
            est.extend(r.assumes.iter().cloned());
            est.insert(sub.clone());
            steps.extend(r.steps);
            assumes.extend(r.assumes);
            cost += r.cost;
        }
        if !method.complete {
            let r = self.search(vec![open(&goal.p, true)], &est, Some(&goal.p))?;
            steps.extend(r.steps);
            assumes.extend(r.assumes);
            cost += r.cost;
        }
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
        let ctx = self.context(belief);
        let r = Rc::new(ctx.route_to(map)?);
        self.routes.borrow_mut().insert(key, Rc::clone(&r));
        Some(r)
    }

    /// Primitive intents establishing `p`, cheapest first, at most
    /// `candidates_per_goal`. Records the cheapest cost for the heuristic.
    fn candidates(
        &self,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        full: bool,
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
                let mut v = self.script_candidates(p, &ctx);
                v.extend(self.buy_candidates(item, *n, belief, &ctx));
                v
            }
            GoalPredicate::World(Predicate::PartyHasMove { mv }) => {
                let mut v = self.script_candidates(p, &ctx);
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
            GoalPredicate::World(_) => self.script_candidates(p, &ctx),
            GoalPredicate::Caught { caught } => self.catch_candidates(caught, p, &ctx),
            GoalPredicate::CanBeat { can_beat } => self
                .readiness_candidate(can_beat, p, belief, &ctx)
                .into_iter()
                .collect(),
            GoalPredicate::Money { money } => {
                vec![self.unsupported(format!("earning ₽{money} is not planned"), p, &ctx)]
            }
            GoalPredicate::Healed { .. } => self
                .planner
                .centers
                .iter()
                .map(|center| {
                    let intent = Intent::Heal {
                        center: center.clone(),
                    };
                    let cost = intent.cost_s(&ctx);
                    Candidate::single(intent, &ctx, cost)
                })
                .collect(),
        };
        out.retain(|c| c.cost.is_finite());
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

    fn unsupported(&self, reason: String, p: &GoalPredicate, ctx: &PlanContext<'_>) -> Candidate {
        let intent = Intent::Unsupported {
            reason,
            establishes: p.clone(),
        };
        let cost = intent.cost_s(ctx);
        Candidate::single(intent, ctx, cost)
    }

    /// Seconds to reach `map`: the open route, else the hop estimate (with
    /// its note), else nothing is known.
    fn go_cost(&self, map: &str, belief: &StateBelief<'a>) -> (f64, Option<String>) {
        if let Some(r) = self.route_to(map, belief).filter(|r| r.found()) {
            return (r.cost_s, None);
        }
        match self.hops.get(map) {
            Some(h) => (
                f64::from(*h) * MINUTES_PER_MAP * 60.0,
                Some(format!("route unverified: {h} maps by hops")),
            ),
            None => (0.0, Some("route unknown".to_string())),
        }
    }

    /// `Go` over the open route, plus one `Go` per blocked alternative
    /// needing its requirement first.
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
            c.assumes = route
                .assumes
                .iter()
                .cloned()
                .map(GoalPredicate::World)
                .collect();
            out.push(c);
        }
        for (req, cost) in &route.blocked {
            let mut c = Candidate::single(intent.clone(), ctx, *cost);
            c.preconditions = req.iter().cloned().map(GoalPredicate::World).collect();
            out.push(c);
        }
        if !route.found() {
            // The route planner finds no way in (a corridor held by NPCs it
            // treats as walls, a door it can't step on): estimate by maps
            // crossed and say so, so the tool's own routing decides.
            if let Some(hops) = self.hops.get(map) {
                let cost = f64::from(*hops) * MINUTES_PER_MAP * 60.0;
                let mut c = Candidate::single(intent, ctx, cost);
                c.steps[0].planned.note = Some(format!("route unverified: {hops} maps by hops"));
                out.push(c);
            }
        }
        out
    }

    fn script_candidates(&self, p: &GoalPredicate, ctx: &PlanContext<'_>) -> Vec<Candidate> {
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
            out.push(Candidate::single(intent, ctx, cost));
        }
        out
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
        self.planner
            .marts
            .iter()
            .filter(|(_, items)| items.iter().any(|i| i == item))
            .map(|(map, _)| {
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

    fn catch_candidates(
        &self,
        species: &str,
        p: &GoalPredicate,
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
                    let intent = Intent::Catch {
                        species: species.to_string(),
                        map: map.to_string(),
                        slot: slot.to_string(),
                        balls: throws + self.planner.params.ball_reserve,
                    };
                    let cost = intent.cost_s(ctx);
                    out.push(Candidate::single(intent, ctx, cost));
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
                        .position(|path| path_effects(path).contains(p))
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

    /// The readiness planner's cheapest preparation for `trainer`, as
    /// `Go`/`Train`/`Catch` steps.
    fn readiness_candidate(
        &self,
        trainer: &str,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
    ) -> Option<Candidate> {
        if let Some(c) = self.readiness.borrow().get(trainer) {
            return c.clone();
        }
        let built = self.build_readiness(trainer, p, belief, ctx);
        self.readiness
            .borrow_mut()
            .insert(trainer.to_string(), built.clone());
        built
    }

    fn build_readiness(
        &self,
        trainer: &str,
        p: &GoalPredicate,
        belief: &StateBelief<'a>,
        ctx: &PlanContext<'_>,
    ) -> Option<Candidate> {
        let Some(party) = belief.party_members() else {
            return Some(self.unsupported(
                format!("the party is unknown, so readiness for {trainer} can't be planned"),
                p,
                ctx,
            ));
        };
        if !self.planner.data.trainers.contains_key(trainer) {
            return None;
        }
        let areas = self.training_areas();
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
                let (go_cost, note) = self.go_cost(&map, belief);
                let mut planned = PlannedIntent::new(go, go_cost);
                planned.note = note;
                steps.push(Step {
                    planned,
                    effects: vec![
                        GoalPredicate::at(&map),
                        GoalPredicate::World(Predicate::Visited { map: map.clone() }),
                    ],
                });
                cost += go_cost;
                here = Some(map.clone());
            }
            let step_cost = minutes * 60.0;
            steps.push(Step {
                planned: PlannedIntent::new(intent, step_cost),
                effects: Vec::new(),
            });
            cost += step_cost;
        }
        if plan.min_confidence() < self.planner.options.confidence {
            let reason = format!(
                "training reaches only {:.0}% against {trainer}",
                plan.min_confidence() * 100.0
            );
            let u = Intent::Unsupported {
                reason,
                establishes: p.clone(),
            };
            let u_cost = u.cost_s(ctx);
            steps.push(Step {
                planned: PlannedIntent::new(u, u_cost),
                effects: Vec::new(),
            });
            cost += u_cost;
        }
        if steps.is_empty() {
            return None;
        }
        let mut c = Candidate {
            steps,
            preconditions: Vec::new(),
            assumes: Vec::new(),
            cost,
        };
        c.add_effect(p.clone());
        Some(c)
    }

    /// Nearest maps with a land encounter table, for the readiness planner.
    fn training_areas(&self) -> Vec<Area> {
        let world = self.planner.world;
        let has_land = |map: &str| {
            self.planner
                .data
                .wild
                .get(map)
                .is_some_and(|t| t.contains_key("land"))
        };
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
        near.truncate(TRAINING_AREAS);
        near.into_iter()
            .map(|(d, map)| {
                let around = hops(world, map);
                let center = around
                    .iter()
                    .filter(|(m, _)| m.contains("PokemonCenter"))
                    .map(|(_, d)| *d)
                    .min()
                    .unwrap_or(4);
                Area {
                    map: map.clone(),
                    travel_minutes: f64::from(d) * MINUTES_PER_MAP,
                    heal_minutes: 2.0 * f64::from(center) * MINUTES_PER_MAP + 0.5,
                }
            })
            .collect()
    }
}

/// Keeps the first probe of each fact (the screen stays read) and moves it
/// before the first step that relies on what it shows; returns the steps
/// and the cost of the probes dropped.
fn dedupe_probes(steps: Vec<PlannedIntent>, data: &GameData) -> (Vec<PlannedIntent>, f64) {
    let mut seen: BTreeSet<ProbeFact> = BTreeSet::new();
    let mut saved = 0.0;
    let mut out: Vec<PlannedIntent> = Vec::with_capacity(steps.len());
    let mut probes: Vec<PlannedIntent> = Vec::new();
    for s in steps {
        if let Intent::Probe { fact } = &s.intent {
            if !seen.insert(fact.clone()) {
                saved += s.cost_s;
            } else {
                probes.push(s);
            }
            continue;
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
}
