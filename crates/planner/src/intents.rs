//! The goal planner's vocabulary (spec §4.1): goal predicates, intents and
//! their preconditions, effects and cost estimates.
//!
//! `Intent` is what a tool carries out. The agent's tool layer defines the
//! same variants; this is the planner's authoritative copy (serde, `Display`),
//! so `crates/agent` can replace its own with it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use pokebot_core::{Error, Result};
use pokebot_gamedata::mechanics::{ball_multiplier, catch_probability};
use pokebot_gamedata::GameData;
use pokebot_state::{PlayerPose, Pocket};
use pokebot_world::events::{Condition, Effect as ScriptEffect, ScriptPath, Val};
use pokebot_world::predicate::{BeliefView, CmpOp, Predicate, Truth};
use pokebot_world::route::{route, Place, PlaceGraph, RouteResult, UnknownPolicy};
use pokebot_world::{MapData, World};
use serde::{Deserialize, Serialize};

use crate::WARP_DOOR;

/// A test on the belief the planner can be asked to make true: the world's
/// predicates plus the planner-level ones (a species caught, a trainer
/// beatable, money, a healed party).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GoalPredicate {
    World(Predicate),
    Caught {
        caught: String,
    },
    /// The party wins against the trainer with P ≥ the planner's confidence.
    CanBeat {
        can_beat: String,
    },
    Money {
        money: u32,
    },
    Healed {
        healed: bool,
    },
}

impl GoalPredicate {
    pub fn flag(name: &str, is: bool) -> GoalPredicate {
        GoalPredicate::World(Predicate::from_flag(name, is))
    }

    pub fn at(map: &str) -> GoalPredicate {
        GoalPredicate::World(Predicate::At {
            map: map.to_string(),
        })
    }

    pub fn has_item(item: &str, n: u32) -> GoalPredicate {
        GoalPredicate::World(Predicate::HasItem {
            item: item.to_string(),
            n,
        })
    }

    pub fn party_has_move(mv: &str) -> GoalPredicate {
        GoalPredicate::World(Predicate::PartyHasMove { mv: mv.to_string() })
    }

    pub fn badge(n: u8) -> GoalPredicate {
        GoalPredicate::World(Predicate::Badge { n })
    }

    pub fn caught(species: &str) -> GoalPredicate {
        GoalPredicate::Caught {
            caught: species.to_string(),
        }
    }

    pub fn can_beat(trainer: &str) -> GoalPredicate {
        GoalPredicate::CanBeat {
            can_beat: trainer.to_string(),
        }
    }

    /// The predicate that rules this one out, when one exists: a flag with
    /// the other value (badges are flags too).
    pub fn negation(&self) -> Option<GoalPredicate> {
        match self {
            GoalPredicate::World(Predicate::Flag { name, is }) => {
                Some(GoalPredicate::flag(name, !is))
            }
            GoalPredicate::World(Predicate::Badge { n }) => {
                Some(GoalPredicate::World(Predicate::Flag {
                    name: Predicate::badge_flag(*n),
                    is: false,
                }))
            }
            _ => None,
        }
    }

    /// The predicate a compiled script condition demands, when it can be
    /// expressed. Answers and menu choices are not facts about the world
    /// (they are the intent's `answers`); bag space, specials and
    /// command results are assumed.
    pub fn from_condition(c: &Condition) -> Option<GoalPredicate> {
        match c {
            Condition::Flag { flag, is } => Some(GoalPredicate::flag(flag, *is)),
            Condition::Trainer { trainer, defeated } => {
                Some(GoalPredicate::flag(trainer, *defeated))
            }
            Condition::Item { item, count, has } if *has => {
                Some(GoalPredicate::has_item(item, (*count).max(1) as u32))
            }
            Condition::Move { r#move, known } if *known => {
                Some(GoalPredicate::party_has_move(r#move))
            }
            Condition::Var { var, cmp } => {
                let (op, v) = [
                    (CmpOp::Eq, &cmp.eq),
                    (CmpOp::Ne, &cmp.ne),
                    (CmpOp::Lt, &cmp.lt),
                    (CmpOp::Gt, &cmp.gt),
                    (CmpOp::Le, &cmp.le),
                    (CmpOp::Ge, &cmp.ge),
                ]
                .into_iter()
                .find_map(|(op, v)| v.as_ref().and_then(Val::as_int).map(|v| (op, v)))?;
                Some(GoalPredicate::World(Predicate::Var {
                    name: var.clone(),
                    op,
                    value: v,
                }))
            }
            _ => None,
        }
    }

    /// The facts a compiled script effect establishes.
    pub fn from_effect(e: &ScriptEffect) -> Vec<GoalPredicate> {
        match e {
            ScriptEffect::Set { set } => vec![GoalPredicate::flag(set, true)],
            ScriptEffect::Clear { clear } => vec![GoalPredicate::flag(clear, false)],
            ScriptEffect::Defeated { defeated } => vec![GoalPredicate::flag(defeated, true)],
            ScriptEffect::Undefeated { undefeated } => vec![GoalPredicate::flag(undefeated, false)],
            ScriptEffect::Var { var, change } => match change.eq.as_ref().and_then(Val::as_int) {
                Some(v) => vec![GoalPredicate::World(Predicate::Var {
                    name: var.clone(),
                    op: CmpOp::Eq,
                    value: v,
                })],
                None => Vec::new(),
            },
            ScriptEffect::Give { give, count, .. } => {
                vec![GoalPredicate::has_item(give, (*count).max(1) as u32)]
            }
            ScriptEffect::GiveMon { givemon, .. } | ScriptEffect::GiveEgg { giveegg: givemon } => {
                match givemon {
                    Val::Sym(species) => vec![GoalPredicate::caught(species)],
                    Val::Int(_) => Vec::new(),
                }
            }
            ScriptEffect::Heal { heal: true } => vec![GoalPredicate::Healed { healed: true }],
            _ => Vec::new(),
        }
    }

    /// The map name in an `At`.
    pub fn at_map(&self) -> Option<&str> {
        match self {
            GoalPredicate::World(Predicate::At { map }) => Some(map),
            _ => None,
        }
    }
}

impl From<Predicate> for GoalPredicate {
    fn from(p: Predicate) -> Self {
        GoalPredicate::World(p)
    }
}

impl fmt::Display for GoalPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GoalPredicate::World(p) => write!(f, "{p}"),
            GoalPredicate::Caught { caught } => write!(f, "Caught({caught})"),
            GoalPredicate::CanBeat { can_beat } => write!(f, "CanBeat({can_beat})"),
            GoalPredicate::Money { money } => write!(f, "Money(≥{money})"),
            GoalPredicate::Healed { healed: true } => write!(f, "Healed"),
            GoalPredicate::Healed { healed: false } => write!(f, "!Healed"),
        }
    }
}

/// What the belief says about a goal predicate; the world's [`BeliefView`]
/// extended with the planner-level predicates.
pub trait GoalBelief: BeliefView {
    fn eval_goal(&self, p: &GoalPredicate) -> Truth;
}

/// A screen the bot can open to turn an `Unknown` fact into an `Observed`
/// one (spec §4.3), with its measured cost.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeFact {
    /// Badges and money.
    TrainerCard,
    BagPocket(Pocket),
    /// Which towns are lit.
    FlyMap,
    /// Caught and seen species.
    Pokedex,
    /// Species, levels, HP and moves.
    Party,
}

impl ProbeFact {
    pub fn cost_s(&self) -> f64 {
        match self {
            ProbeFact::TrainerCard => 8.0,
            ProbeFact::BagPocket(_) => 10.0,
            ProbeFact::FlyMap => 8.0,
            ProbeFact::Pokedex => 15.0,
            ProbeFact::Party => 8.0,
        }
    }

    /// The probe that settles `p`, if a screen shows it.
    pub fn for_predicate(p: &GoalPredicate, data: &GameData) -> Option<ProbeFact> {
        match p {
            GoalPredicate::World(Predicate::Badge { .. }) => Some(ProbeFact::TrainerCard),
            GoalPredicate::World(Predicate::Flag { name, .. })
                if name.starts_with("FLAG_BADGE") =>
            {
                Some(ProbeFact::TrainerCard)
            }
            GoalPredicate::World(Predicate::HasItem { item, .. }) => {
                Some(ProbeFact::BagPocket(pocket_of(data, item)?))
            }
            GoalPredicate::World(Predicate::Visited { .. }) => Some(ProbeFact::FlyMap),
            GoalPredicate::World(Predicate::PartyHasMove { .. }) => Some(ProbeFact::Party),
            GoalPredicate::Caught { .. } => Some(ProbeFact::Pokedex),
            GoalPredicate::CanBeat { .. } | GoalPredicate::Healed { .. } => Some(ProbeFact::Party),
            GoalPredicate::Money { .. } => Some(ProbeFact::TrainerCard),
            GoalPredicate::World(_) => None,
        }
    }
}

impl fmt::Display for ProbeFact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeFact::TrainerCard => write!(f, "trainer card"),
            ProbeFact::BagPocket(p) => write!(f, "bag {p:?}"),
            ProbeFact::FlyMap => write!(f, "fly map"),
            ProbeFact::Pokedex => write!(f, "pokedex"),
            ProbeFact::Party => write!(f, "party"),
        }
    }
}

/// The bag pocket an item lives in, from the game data.
pub fn pocket_of(data: &GameData, item: &str) -> Option<Pocket> {
    data.items
        .get(item)
        .and_then(|i| i.pocket.as_deref())
        .and_then(Pocket::from_decomp)
}

/// What an intent does to the belief.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Establishes(GoalPredicate),
    /// The facts a probe reveals become `Observed`.
    Observes(ProbeFact),
}

/// Something a tool can carry out.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// Reach a map (any tile of it).
    Go {
        dest: String,
    },
    /// Talk to an object and answer its prompts.
    Talk {
        map: String,
        object: u32,
        answers: Vec<String>,
    },
    /// Run one compiled path of a script to completion.
    RunScript {
        script: String,
        path: usize,
        answers: Vec<String>,
        map: String,
    },
    Heal {
        center: String,
    },
    /// Fight the battle on screen with a policy (`fight`, `flee`).
    Battle {
        policy: String,
    },
    /// Win a scripted trainer battle.
    Beat {
        trainer: String,
        map: String,
    },
    /// Raise a member to `level` in the wild on `map`.
    Train {
        map: String,
        species: String,
        level: u8,
    },
    /// Encounter the species on the map's table and catch it with `balls`
    /// at hand (expected throws plus the reserve).
    Catch {
        species: String,
        map: String,
        slot: String,
        balls: u32,
    },
    Buy {
        item: String,
        count: u32,
        map: String,
    },
    /// Teach the HM in the bag to a party member.
    Teach {
        hm: String,
        mon: String,
    },
    Probe {
        fact: ProbeFact,
    },
    Save,
    Unstick,
    /// Nothing on this console establishes the predicate (link trades,
    /// breeding); the plan reports it as blocked.
    Unsupported {
        reason: String,
        establishes: GoalPredicate,
    },
}

/// Timing model of the intents, in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct CostParams {
    pub script_s: f64,
    pub say_s: f64,
    pub answer_s: f64,
    pub trainer_battle_s: f64,
    pub give_s: f64,
    pub heal_s: f64,
    pub buy_s: f64,
    pub buy_item_s: f64,
    pub teach_s: f64,
    pub save_s: f64,
    pub throw_s: f64,
    pub weaken_s: f64,
    pub wild_intro_s: f64,
    pub flee_s: f64,
    pub tile_s: f64,
    /// Steps between wild encounters at rate 1 (the game rolls rate×16 of
    /// 2880 per step).
    pub encounter_steps_at_rate_1: f64,
    /// Balls kept beyond the expected throws.
    pub ball_reserve: u32,
    /// Charged for an `Unsupported` intent so plans that avoid it win.
    pub unsupported_s: f64,
}

impl Default for CostParams {
    fn default() -> Self {
        CostParams {
            script_s: 4.0,
            say_s: 2.0,
            answer_s: 3.0,
            trainer_battle_s: 90.0,
            give_s: 3.0,
            heal_s: 12.0,
            buy_s: 15.0,
            buy_item_s: 1.0,
            teach_s: 20.0,
            save_s: 8.0,
            throw_s: 8.0,
            weaken_s: 30.0,
            wild_intro_s: 8.0,
            flee_s: 5.0,
            tile_s: 0.268,
            encounter_steps_at_rate_1: 180.0,
            ball_reserve: 5,
            unsupported_s: 6000.0,
        }
    }
}

/// What intents are priced and checked against.
pub struct PlanContext<'a> {
    pub world: &'a World,
    pub graph: &'a PlaceGraph,
    pub data: &'a GameData,
    pub belief: &'a dyn GoalBelief,
    /// Where the player is; `None` when the position is unknown.
    pub pose: Option<PlayerPose>,
    pub params: CostParams,
    /// How the route planner treats unknown edge requirements.
    pub policy: UnknownPolicy,
    pub entries: &'a Entries,
}

impl PlanContext<'_> {
    /// The cheapest route to any tile of `map` from the current pose.
    pub fn route_to(&self, map: &str) -> Option<RouteResult> {
        let pose = self.pose.as_ref()?;
        self.entries
            .route(self.world, self.graph, self.belief, pose, map, self.policy)
    }

    /// The script path an intent runs, when it is a `RunScript`.
    pub fn script_path(&self, script: &str, path: usize) -> Option<&ScriptPath> {
        self.world.events()?.script(script)?.paths.get(path)
    }
}

/// Where `At{map}` is routed to, and how buildings are entered.
///
/// The destination is the tile an edge into the map lands on (a warp's
/// landing, a connection crossing), so it is a tile the route can reach —
/// a map's own warp tiles are left alone by the walk flood.
///
/// Outdoor door tiles are collision tiles the flood never steps on (the
/// walker stands below one and pushes Up), so the route planner can't enter
/// a building on its own: [`Entries::route`] goes to the tile in front of
/// the building's door, charges the warp, and routes on from the landing.
pub struct Entries {
    /// Map → the tiles edges into it land on, sorted.
    landing: BTreeMap<String, Vec<Place>>,
    /// Interior map → (outdoor tile in front of its building's door, the
    /// landing inside the building's ground floor).
    building: BTreeMap<String, (Place, PlayerPose)>,
}

impl Entries {
    pub fn build(world: &World, graph: &PlaceGraph) -> Entries {
        let mut landing: BTreeMap<String, Vec<Place>> = BTreeMap::new();
        for from in graph.places() {
            for edge in graph.edges_from(&from.map, from.x, from.y) {
                let to = &edge.to;
                landing.entry(to.map.clone()).or_default().push(to.clone());
            }
        }
        for places in landing.values_mut() {
            places.sort();
            places.dedup();
        }
        // Outdoor doors first, then every interior reached from those
        // buildings through interior warps (stairs, doors inside).
        let mut maps: Vec<&MapData> = world.maps().collect();
        maps.sort_by(|a, b| a.name.cmp(&b.name));
        let mut building: BTreeMap<String, (Place, PlayerPose)> = BTreeMap::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for map in &maps {
            if !map.is_outdoor() {
                continue;
            }
            for w in &map.warps {
                let Some((dest, dx, dy)) = world.warp_destination(w) else {
                    continue;
                };
                if dest.is_outdoor() || building.contains_key(&dest.name) {
                    continue;
                }
                let is_door = map
                    .tile(w.x, w.y)
                    .is_some_and(|t| t.collision != 0 || t.behavior == WARP_DOOR);
                if !is_door {
                    continue;
                }
                let front = Place::tile(&map.name, w.x, w.y + 1);
                let inside = PlayerPose {
                    map: dest.name.clone(),
                    x: dx,
                    y: dy,
                };
                building.insert(dest.name.clone(), (front, inside));
                queue.push_back(dest.name.clone());
            }
        }
        while let Some(name) = queue.pop_front() {
            let Some(map) = world.map(&name) else {
                continue;
            };
            let entry = building[&name].clone();
            for w in &map.warps {
                let Some((dest, _, _)) = world.warp_destination(w) else {
                    continue;
                };
                if dest.is_outdoor() || building.contains_key(&dest.name) {
                    continue;
                }
                building.insert(dest.name.clone(), entry.clone());
                queue.push_back(dest.name.clone());
            }
        }
        Entries { landing, building }
    }

    /// The tiles `At{map}` may be routed to: the map's entry landings (some
    /// may lie in a part of the map only reachable from another side), else
    /// its first place, else its middle tile.
    pub fn destinations(&self, world: &World, graph: &PlaceGraph, map: &str) -> Vec<Place> {
        if let Some(p) = self.landing.get(map) {
            return p.clone();
        }
        if let Some(p) = graph.places_on(map).first() {
            return vec![p.clone()];
        }
        world
            .map(map)
            .map(|m| vec![Place::tile(map, m.width / 2, m.height / 2)])
            .unwrap_or_default()
    }

    /// The cheapest of the routes to `dests` (ties: the first).
    fn best_route(
        world: &World,
        graph: &PlaceGraph,
        belief: &dyn BeliefView,
        pose: &PlayerPose,
        dests: &[Place],
        policy: UnknownPolicy,
    ) -> Option<RouteResult> {
        let mut best: Option<RouteResult> = None;
        for dest in dests {
            let r = route(world, graph, belief, pose, dest, policy);
            let better = match &best {
                None => true,
                Some(b) => {
                    r.cost_s < b.cost_s
                        || (!b.found()
                            && r.blocked.first().map(|x| x.1) < b.blocked.first().map(|x| x.1))
                }
            };
            if better {
                best = Some(r);
            }
        }
        best
    }

    /// The cheapest route from `pose` to any tile of `map`: direct when the
    /// route planner finds one, else through the building's front door.
    pub fn route(
        &self,
        world: &World,
        graph: &PlaceGraph,
        belief: &dyn BeliefView,
        pose: &PlayerPose,
        map: &str,
        policy: UnknownPolicy,
    ) -> Option<RouteResult> {
        let dests = self.destinations(world, graph, map);
        if pose.map == map {
            return Some(RouteResult {
                legs: Vec::new(),
                cost_s: 0.0,
                assumes: Vec::new(),
                blocked: Vec::new(),
            });
        }
        let direct = Self::best_route(world, graph, belief, pose, &dests, policy)?;
        if direct.found() {
            return Some(direct);
        }
        let Some((front, inside)) = self.building.get(map) else {
            return Some(direct);
        };
        let warp_s = graph.params().warp_s;
        let outside = route(world, graph, belief, pose, front, policy);
        let onward = if inside.map == map {
            RouteResult {
                legs: Vec::new(),
                cost_s: 0.0,
                assumes: Vec::new(),
                blocked: Vec::new(),
            }
        } else {
            Self::best_route(world, graph, belief, inside, &dests, policy)?
        };
        let mut legs = outside.legs;
        legs.extend(onward.legs);
        let mut assumes = outside.assumes;
        assumes.extend(onward.assumes);
        assumes.sort();
        assumes.dedup();
        let mut blocked = outside.blocked;
        for b in &mut blocked {
            b.1 += warp_s + onward.cost_s;
        }
        Some(RouteResult {
            legs,
            cost_s: outside.cost_s + warp_s + onward.cost_s,
            assumes,
            blocked,
        })
    }
}

impl Intent {
    /// The variant name, for tie-breaking and tables.
    pub fn name(&self) -> &'static str {
        match self {
            Intent::Go { .. } => "Go",
            Intent::Talk { .. } => "Talk",
            Intent::RunScript { .. } => "RunScript",
            Intent::Heal { .. } => "Heal",
            Intent::Battle { .. } => "Battle",
            Intent::Beat { .. } => "Beat",
            Intent::Train { .. } => "Train",
            Intent::Catch { .. } => "Catch",
            Intent::Buy { .. } => "Buy",
            Intent::Teach { .. } => "Teach",
            Intent::Probe { .. } => "Probe",
            Intent::Save => "Save",
            Intent::Unstick => "Unstick",
            Intent::Unsupported { .. } => "Unsupported",
        }
    }

    /// What must hold before the intent runs.
    pub fn preconditions(&self, ctx: &PlanContext<'_>) -> Vec<GoalPredicate> {
        match self {
            Intent::Go { .. }
            | Intent::Battle { .. }
            | Intent::Probe { .. }
            | Intent::Save
            | Intent::Unstick
            | Intent::Unsupported { .. } => Vec::new(),
            Intent::Talk { map, .. } => vec![GoalPredicate::at(map)],
            Intent::RunScript {
                script, path, map, ..
            } => {
                let mut pre = vec![GoalPredicate::at(map)];
                if let Some(p) = ctx.script_path(script, *path) {
                    pre.extend(path_preconditions(p));
                    if let Some(trainer) = first_battle(p) {
                        pre.push(GoalPredicate::can_beat(trainer));
                    }
                }
                pre
            }
            Intent::Heal { center } => vec![GoalPredicate::at(center)],
            Intent::Beat { trainer, map } => {
                vec![GoalPredicate::at(map), GoalPredicate::can_beat(trainer)]
            }
            Intent::Train { map, .. } => vec![GoalPredicate::at(map)],
            Intent::Catch {
                map, slot, balls, ..
            } => {
                let mut pre = vec![GoalPredicate::at(map)];
                if slot == "water" {
                    pre.push(GoalPredicate::party_has_move("MOVE_SURF"));
                }
                pre.push(GoalPredicate::has_item(&best_ball(ctx), *balls));
                pre
            }
            Intent::Buy { item, count, map } => {
                let price = ctx.data.items.get(item).map_or(0, |i| i.price);
                vec![
                    GoalPredicate::at(map),
                    GoalPredicate::Money {
                        money: price * count,
                    },
                ]
            }
            Intent::Teach { hm, .. } => {
                let mut pre = vec![GoalPredicate::has_item(hm, 1)];
                if let Some(n) = hm_badge(hm) {
                    pre.push(GoalPredicate::badge(n));
                }
                pre
            }
        }
    }

    /// What holds after the intent ran.
    pub fn effects(&self, ctx: &PlanContext<'_>) -> Vec<Effect> {
        match self {
            Intent::Go { dest } => vec![
                Effect::Establishes(GoalPredicate::at(dest)),
                Effect::Establishes(GoalPredicate::World(Predicate::Visited {
                    map: dest.clone(),
                })),
            ],
            Intent::Talk { .. } | Intent::Battle { .. } | Intent::Save | Intent::Unstick => {
                Vec::new()
            }
            Intent::RunScript { script, path, .. } => ctx
                .script_path(script, *path)
                .map(path_effects)
                .unwrap_or_default()
                .into_iter()
                .map(Effect::Establishes)
                .collect(),
            Intent::Heal { .. } => {
                vec![Effect::Establishes(GoalPredicate::Healed { healed: true })]
            }
            Intent::Beat { trainer, .. } => {
                vec![Effect::Establishes(GoalPredicate::flag(trainer, true))]
            }
            Intent::Train { .. } => Vec::new(),
            Intent::Catch { species, .. } => {
                vec![Effect::Establishes(GoalPredicate::caught(species))]
            }
            Intent::Buy { item, count, .. } => {
                vec![Effect::Establishes(GoalPredicate::has_item(item, *count))]
            }
            Intent::Teach { hm, .. } => hm_move(hm)
                .map(|mv| vec![Effect::Establishes(GoalPredicate::party_has_move(mv))])
                .unwrap_or_default(),
            Intent::Probe { fact } => vec![Effect::Observes(fact.clone())],
            Intent::Unsupported { establishes, .. } => {
                vec![Effect::Establishes(establishes.clone())]
            }
        }
    }

    /// Seconds the intent is expected to take from the current pose.
    pub fn cost_s(&self, ctx: &PlanContext<'_>) -> f64 {
        let p = &ctx.params;
        match self {
            Intent::Go { dest } => ctx.route_to(dest).map_or(f64::INFINITY, |r| r.cost_s),
            Intent::Talk { answers, .. } => p.script_s + p.answer_s * answers.len() as f64,
            Intent::RunScript {
                script,
                path,
                answers,
                ..
            } => {
                let mut cost = p.script_s + p.answer_s * answers.len() as f64;
                if let Some(sp) = ctx.script_path(script, *path) {
                    for e in &sp.does {
                        cost += match e {
                            ScriptEffect::Say { .. } => p.say_s,
                            ScriptEffect::Battle { .. } => p.trainer_battle_s,
                            ScriptEffect::Give { .. } => p.give_s,
                            _ => 0.0,
                        };
                    }
                }
                cost
            }
            Intent::Heal { .. } => p.heal_s,
            Intent::Battle { .. } | Intent::Beat { .. } => p.trainer_battle_s,
            Intent::Train { .. } => 0.0,
            Intent::Catch {
                species,
                map,
                slot,
                balls,
            } => catch_cost(ctx, species, map, slot, *balls),
            Intent::Buy { count, .. } => p.buy_s + p.buy_item_s * f64::from(*count),
            Intent::Teach { .. } => p.teach_s,
            Intent::Probe { fact } => fact.cost_s(),
            Intent::Save => p.save_s,
            Intent::Unstick => p.script_s,
            Intent::Unsupported { .. } => p.unsupported_s,
        }
    }
}

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Intent::Go { dest } => write!(f, "Go({dest})"),
            Intent::Talk {
                map,
                object,
                answers,
            } => write!(f, "Talk({map} #{object}{})", answers_suffix(answers)),
            Intent::RunScript {
                script,
                path,
                answers,
                ..
            } => write!(f, "RunScript({script}[{path}]{})", answers_suffix(answers)),
            Intent::Heal { center } => write!(f, "Heal({center})"),
            Intent::Battle { policy } => write!(f, "Battle({policy})"),
            Intent::Beat { trainer, .. } => write!(f, "Beat({trainer})"),
            Intent::Train {
                map,
                species,
                level,
            } => write!(f, "Train({species} to Lv{level} on {map})"),
            Intent::Catch {
                species,
                map,
                slot,
                balls,
            } => write!(f, "Catch({species} on {map} {slot}, {balls} balls)"),
            Intent::Buy { item, count, map } => write!(f, "Buy({item} x{count} at {map})"),
            Intent::Teach { hm, mon } => write!(f, "Teach({hm} to {mon})"),
            Intent::Probe { fact } => write!(f, "Probe({fact})"),
            Intent::Save => write!(f, "Save"),
            Intent::Unstick => write!(f, "Unstick"),
            Intent::Unsupported { reason, .. } => write!(f, "Unsupported({reason})"),
        }
    }
}

fn answers_suffix(answers: &[String]) -> String {
    if answers.is_empty() {
        String::new()
    } else {
        format!(" answers {}", answers.join(","))
    }
}

/// The path's `when` as predicates. The compiler cuts loops after one
/// iteration, which leaves paths that test a flag both ways; the first test
/// of each fact is the entry condition and wins.
pub fn path_preconditions(path: &ScriptPath) -> Vec<GoalPredicate> {
    let mut seen: BTreeMap<String, GoalPredicate> = BTreeMap::new();
    let mut out = Vec::new();
    for c in &path.when {
        let Some(p) = GoalPredicate::from_condition(c) else {
            continue;
        };
        let key = match &p {
            GoalPredicate::World(Predicate::Flag { name, .. }) => format!("flag:{name}"),
            GoalPredicate::World(Predicate::Badge { n }) => {
                format!("flag:{}", Predicate::badge_flag(*n))
            }
            GoalPredicate::World(Predicate::Var { name, .. }) => format!("var:{name}"),
            other => format!("p:{other}"),
        };
        if seen.contains_key(&key) {
            continue;
        }
        seen.insert(key, p.clone());
        out.push(p);
    }
    out
}

/// The answers a path takes at its prompts, in order (`yes`, `no`, or the
/// multichoice option as `MENU=n`).
pub fn path_answers(path: &ScriptPath) -> Vec<String> {
    path.when
        .iter()
        .filter_map(|c| match c {
            Condition::Answer { answer } => Some(answer.clone()),
            Condition::Choice { choice, cmp } => cmp
                .eq
                .as_ref()
                .and_then(Val::as_int)
                .map(|v| format!("{choice}={v}")),
            _ => None,
        })
        .collect()
}

/// The facts a path establishes (a `set` after a `clear` of the same flag
/// wins, so later effects override earlier ones).
pub fn path_effects(path: &ScriptPath) -> Vec<GoalPredicate> {
    let mut out: Vec<GoalPredicate> = Vec::new();
    for e in &path.does {
        for p in GoalPredicate::from_effect(e) {
            if let Some(neg) = p.negation() {
                out.retain(|q| *q != neg);
            }
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// The trainer of the path's first battle; later battles on the same path
/// are the compiler's cut loops (rematches).
pub fn first_battle(path: &ScriptPath) -> Option<&str> {
    path.does.iter().find_map(|e| match e {
        ScriptEffect::Battle { battle, .. } => Some(battle.as_str()),
        _ => None,
    })
}

/// HM item → badge needed to use the move in the field (FireRed).
pub fn hm_badge(hm: &str) -> Option<u8> {
    Some(match hm {
        "ITEM_HM01" => 2,
        "ITEM_HM02" => 3,
        "ITEM_HM03" => 5,
        "ITEM_HM04" => 4,
        "ITEM_HM05" => 1,
        "ITEM_HM06" => 6,
        "ITEM_HM07" => 7,
        _ => return None,
    })
}

/// HM item → the move it teaches.
pub fn hm_move(hm: &str) -> Option<&'static str> {
    Some(match hm {
        "ITEM_HM01" => "MOVE_CUT",
        "ITEM_HM02" => "MOVE_FLY",
        "ITEM_HM03" => "MOVE_SURF",
        "ITEM_HM04" => "MOVE_STRENGTH",
        "ITEM_HM05" => "MOVE_FLASH",
        "ITEM_HM06" => "MOVE_ROCK_SMASH",
        "ITEM_HM07" => "MOVE_WATERFALL",
        _ => return None,
    })
}

/// The move's HM item.
pub fn hm_of_move(mv: &str) -> Option<&'static str> {
    [
        "ITEM_HM01",
        "ITEM_HM02",
        "ITEM_HM03",
        "ITEM_HM04",
        "ITEM_HM05",
        "ITEM_HM06",
        "ITEM_HM07",
    ]
    .into_iter()
    .find(|hm| hm_move(hm) == Some(mv))
}

/// The best ball the belief knows the bag holds (highest multiplier, the
/// Master Ball excluded); a Poké Ball when none is known.
pub fn best_ball(ctx: &PlanContext<'_>) -> String {
    let mut best: Option<(u32, String)> = None;
    for item in ctx.data.items.keys() {
        let Some(mult) = ball_multiplier(item) else {
            continue;
        };
        if ctx.belief.eval(&Predicate::HasItem {
            item: item.clone(),
            n: 1,
        }) != Truth::True
        {
            continue;
        }
        let better = match &best {
            None => true,
            Some((m, name)) => mult > *m || (mult == *m && item < name),
        };
        if better {
            best = Some((mult, item.clone()));
        }
    }
    best.map_or_else(|| "ITEM_POKE_BALL".to_string(), |(_, name)| name)
}

/// Expected throws to catch `species` at `level`, weakened to half HP, with
/// `ball` (its ×10 multiplier): `ceil(1 / P(one throw))`.
pub fn expected_throws(data: &GameData, species: &str, level: u8, ball: u32) -> Option<u32> {
    let s = data.species(species)?;
    let stats = pokebot_gamedata::mechanics::Stats::compute(&s.base, level, 15);
    let max_hp = stats.hp();
    let p = catch_probability(s.catch_rate, max_hp, (max_hp / 2).max(1), ball);
    if p <= 0.0 {
        return None;
    }
    Some((1.0 / p).ceil() as u32)
}

/// Encounter rate and slot chance of `species` on `map`'s `slot` table.
pub fn encounter_odds(data: &GameData, species: &str, map: &str, slot: &str) -> Option<(u16, u32)> {
    let table = data.wild.get(map)?.get(slot)?;
    let chance: u32 = table
        .slots
        .iter()
        .filter(|s| s.species == species)
        .map(|s| u32::from(s.chance))
        .sum();
    (chance > 0).then_some((table.rate, chance))
}

fn catch_cost(ctx: &PlanContext<'_>, species: &str, map: &str, slot: &str, balls: u32) -> f64 {
    let p = &ctx.params;
    let throws = balls.saturating_sub(p.ball_reserve).max(1);
    let catching = p.weaken_s + p.throw_s * f64::from(throws);
    if slot == "static" {
        return p.wild_intro_s + catching;
    }
    let Some((rate, chance)) = encounter_odds(ctx.data, species, map, slot) else {
        return f64::INFINITY;
    };
    let encounters = 100.0 / f64::from(chance);
    let steps = p.encounter_steps_at_rate_1 / f64::from(rate.max(1));
    let walk = steps * p.tile_s;
    let others = (encounters - 1.0).max(0.0) * (p.wild_intro_s + p.flee_s);
    encounters * walk + others + p.wild_intro_s + catching
}

/// One way to obtain a species (`data/world/obtain.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct ObtainMethod {
    pub method: String,
    #[serde(default)]
    pub map: Option<String>,
    #[serde(default)]
    pub slot: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub level: Option<u8>,
    #[serde(default)]
    pub min_level: Option<u8>,
    #[serde(default)]
    pub max_level: Option<u8>,
    #[serde(default)]
    pub rate: Option<u32>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub how: Option<String>,
    #[serde(default)]
    pub give: Option<String>,
    #[serde(default)]
    pub item: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObtainSpecies {
    pub methods: Vec<ObtainMethod>,
    #[serde(default)]
    pub reasons: Vec<String>,
}

/// Every species with the ways it can be obtained on this console.
#[derive(Debug, Clone, Deserialize)]
pub struct Obtain {
    pub rom: String,
    pub species: BTreeMap<String, ObtainSpecies>,
}

impl Obtain {
    /// Reads `<dir>/obtain.json`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Obtain> {
        let path = dir.as_ref().join("obtain.json");
        let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
        serde_json::from_str(&text)
            .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_predicates_round_trip_and_print() {
        let p = GoalPredicate::caught("SPECIES_RATTATA");
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#"{"caught":"SPECIES_RATTATA"}"#);
        assert_eq!(serde_json::from_str::<GoalPredicate>(&json).unwrap(), p);
        let w: GoalPredicate = serde_json::from_str(r#"{"badge":{"n":3}}"#).unwrap();
        assert_eq!(w, GoalPredicate::badge(3));
        assert_eq!(w.to_string(), "Badge(3)");
        assert_eq!(p.to_string(), "Caught(SPECIES_RATTATA)");
        assert_eq!(
            GoalPredicate::badge(2).negation(),
            Some(GoalPredicate::flag("FLAG_BADGE02_GET", false))
        );
    }

    #[test]
    fn intents_serialize_by_variant() {
        let i = Intent::Probe {
            fact: ProbeFact::BagPocket(Pocket::PokeBalls),
        };
        let json = serde_json::to_string(&i).unwrap();
        assert_eq!(json, r#"{"probe":{"fact":{"bag_pocket":"PokeBalls"}}}"#);
        assert_eq!(serde_json::from_str::<Intent>(&json).unwrap(), i);
        assert_eq!(i.to_string(), "Probe(bag PokeBalls)");
        assert_eq!(hm_of_move("MOVE_CUT"), Some("ITEM_HM01"));
        assert_eq!(hm_badge("ITEM_HM03"), Some(5));
    }

    #[test]
    fn contradictory_path_conditions_keep_the_entry_test() {
        let path = ScriptPath {
            when: vec![
                Condition::Flag {
                    flag: "FLAG_A".into(),
                    is: false,
                },
                Condition::Flag {
                    flag: "FLAG_A".into(),
                    is: true,
                },
                Condition::Answer {
                    answer: "yes".into(),
                },
            ],
            does: vec![
                ScriptEffect::Clear {
                    clear: "FLAG_B".into(),
                },
                ScriptEffect::Set {
                    set: "FLAG_B".into(),
                },
            ],
            opaque: Vec::new(),
        };
        assert_eq!(
            path_preconditions(&path),
            vec![GoalPredicate::flag("FLAG_A", false)]
        );
        assert_eq!(path_answers(&path), vec!["yes".to_string()]);
        assert_eq!(
            path_effects(&path),
            vec![GoalPredicate::flag("FLAG_B", true)]
        );
    }
}
