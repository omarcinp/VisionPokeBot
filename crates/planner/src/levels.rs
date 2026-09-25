//! Story reachability (spec §4.2): how far into the story each fact lies,
//! from the compiled events alone.
//!
//! A relaxed forward pass over the world: starting from what holds now,
//! every map the route planner reaches with the facts established so far
//! is entered, every script path (object, sign, trigger, map entry) on a
//! reached map whose conditions hold adds its effects, HMs in the bag teach
//! their move once the badge allows it, marts on reached maps sell their
//! items; repeat until nothing new appears. A fact's **level** is the pass
//! it first appeared in; facts that never do are out of reach.
//!
//! The pass is relaxed (effects only add; a flag set stays settable to its
//! other value), so a level is a lower bound on how much of the story must
//! happen first. The goal planner uses it to order what it establishes
//! (badge 1 before badge 2, the Silph Scope before the ghost), to route
//! only through what can be had before a goal (the Secret House is not
//! reached by Surf, which it gives), and to drop candidates whose
//! conditions the story never provides.

use std::collections::{BTreeMap, BTreeSet};

use pokebot_state::PlayerPose;
use pokebot_world::predicate::{BeliefView, Predicate, Truth};
use pokebot_world::route::{reachable, PlaceGraph, UnknownPolicy};
use pokebot_world::World;

/// Passes run at most (the story is a few dozen steps deep).
const MAX_PASSES: u32 = 200;

/// Something the story can do: on `map` (anywhere when `None`), once `pre`
/// holds, `effects` hold.
#[derive(Debug, Clone)]
pub struct Achiever {
    pub map: Option<String>,
    pub pre: Vec<Predicate>,
    pub effects: Vec<Predicate>,
    /// The script path it is, when it is one.
    pub script: Option<(String, usize)>,
    /// Where the player stands to run it, on a map whose passages the
    /// story opens (empty elsewhere: being on the map is enough).
    pub spots: Vec<(i32, i32)>,
}

/// A fact as the pass tracks it: flags (badges too) with their value,
/// items held, moves known, maps reached. Vars are tracked by value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Atom {
    Flag(String, bool),
    Item(String),
    Move(String),
    Map(String),
}

fn atom_of(p: &Predicate) -> Option<Atom> {
    Some(match p {
        Predicate::Flag { name, is } => Atom::Flag(name.clone(), *is),
        Predicate::Badge { n } => Atom::Flag(Predicate::badge_flag(*n), true),
        Predicate::HasItem { item, .. } => Atom::Item(item.clone()),
        Predicate::PartyHasMove { mv } => Atom::Move(mv.clone()),
        Predicate::Visited { map } | Predicate::At { map } => Atom::Map(map.clone()),
        Predicate::Var { .. } => return None,
    })
}

/// What holds before anything is done: the facts the belief knows, and
/// for the rest the story's start (a new game's initial flags set, every
/// other flag clear, every var at 0, nothing held, nowhere visited).
#[derive(Debug, Clone, Default)]
pub struct Start {
    /// Flags whose value the belief knows.
    pub flags: BTreeMap<String, bool>,
    /// Flags a new game starts with set.
    pub initial: BTreeSet<String>,
    /// Vars whose value the belief knows.
    pub vars: BTreeMap<String, i64>,
    /// Items known held, with the count.
    pub items: BTreeMap<String, u32>,
    pub moves: BTreeSet<String>,
    /// Maps known visited, and the one the player is on.
    pub maps: BTreeSet<String>,
    /// Nothing is known about the start: every fact holds (no pruning).
    pub everything: bool,
}

impl Start {
    fn holds(&self, p: &Predicate) -> bool {
        if self.everything {
            return true;
        }
        match p {
            Predicate::Flag { name, is } => {
                self.flags
                    .get(name)
                    .copied()
                    .unwrap_or(self.initial.contains(name))
                    == *is
            }
            Predicate::Badge { n } => self
                .flags
                .get(&Predicate::badge_flag(*n))
                .copied()
                .unwrap_or(false),
            Predicate::HasItem { item, n } => self.items.get(item).is_some_and(|c| c >= n),
            Predicate::PartyHasMove { mv } => self.moves.contains(mv),
            Predicate::Visited { map } | Predicate::At { map } => self.maps.contains(map),
            Predicate::Var { name, op, value } => {
                op.holds(self.vars.get(name).copied().unwrap_or(0), *value)
            }
        }
    }
}

/// The level of every fact the story reaches.
#[derive(Debug, Clone, Default)]
pub struct Levels {
    start: Start,
    atoms: BTreeMap<Atom, u32>,
    /// Var → value → level it is first set at.
    vars: BTreeMap<String, BTreeMap<i64, u32>>,
    /// Script path → the pass it could first run in.
    fired: BTreeMap<(String, usize), u32>,
    /// Passes run.
    pub passes: u32,
}

impl Levels {
    /// No knowledge of the story: every fact is taken as reachable now.
    pub fn unknown() -> Levels {
        Levels {
            start: Start {
                everything: true,
                ..Start::default()
            },
            ..Levels::default()
        }
    }

    /// The pass the script path could first run in; `None` when never.
    pub fn fired(&self, script: &str, path: usize) -> Option<u32> {
        self.fired.get(&(script.to_string(), path)).copied()
    }

    /// Whether the pass ran (a pose to start from was known).
    pub fn known(&self) -> bool {
        !self.start.everything
    }

    /// The pass `p` first holds in; `None` when the story never provides it.
    pub fn level(&self, p: &Predicate) -> Option<u32> {
        if self.start.holds(p) {
            return Some(0);
        }
        match p {
            Predicate::Var { name, op, value } => self
                .vars
                .get(name)?
                .iter()
                .filter(|(v, _)| op.holds(**v, *value))
                .map(|(_, l)| *l)
                .min(),
            _ => self.atoms.get(&atom_of(p)?).copied(),
        }
    }

    fn holds(&self, p: &Predicate) -> bool {
        self.level(p).is_some()
    }

    fn add(&mut self, p: &Predicate, level: u32) -> bool {
        match p {
            Predicate::Var {
                name,
                op: pokebot_world::predicate::CmpOp::Eq,
                value,
            } => {
                let values = self.vars.entry(name.clone()).or_default();
                if values.contains_key(value) {
                    return false;
                }
                values.insert(*value, level);
                true
            }
            Predicate::Var { .. } => false,
            _ => match atom_of(p) {
                // Counts are relaxed: once an item can be had, any number.
                Some(a) if !self.atoms.contains_key(&a) => {
                    self.atoms.insert(a, level);
                    true
                }
                _ => false,
            },
        }
    }
}

/// The facts of the pass so far, as a belief the route planner reads.
struct PassBelief<'l>(&'l Levels);

impl BeliefView for PassBelief<'_> {
    fn eval(&self, p: &Predicate) -> Truth {
        if self.0.holds(p) {
            Truth::True
        } else {
            Truth::False
        }
    }
}

/// Runs the pass from `pose`.
pub fn compute(
    world: &World,
    graph: &PlaceGraph,
    pose: &PlayerPose,
    achievers: &[Achiever],
    start: Start,
) -> Levels {
    let mut levels = Levels {
        start,
        atoms: BTreeMap::new(),
        vars: BTreeMap::new(),
        fired: BTreeMap::new(),
        passes: 0,
    };
    let mut done = vec![false; achievers.len()];
    let mut reached: BTreeSet<String> = BTreeSet::new();
    for pass in 1..=MAX_PASSES {
        levels.passes = pass;
        let (maps, tiles) = reachable(
            world,
            graph,
            &PassBelief(&levels),
            pose,
            UnknownPolicy::Pessimistic,
        );
        let mut changed = false;
        for m in maps {
            if reached.insert(m.clone()) {
                changed |= levels.add(&Predicate::At { map: m }, pass);
            }
        }
        // Effects of this pass count from the next one, so a level is the
        // number of rounds of the story before the fact.
        let mut added: Vec<Predicate> = Vec::new();
        for (i, a) in achievers.iter().enumerate() {
            if done[i] {
                continue;
            }
            if a.map.as_ref().is_some_and(|m| !reached.contains(m)) {
                continue;
            }
            // On a gated map, one of its spots must be reached too.
            if let (false, Some(m)) = (a.spots.is_empty(), &a.map) {
                if let Some(t) = tiles.get(m) {
                    if !a.spots.iter().any(|s| t.contains(s)) {
                        continue;
                    }
                }
            }
            if a.pre.iter().all(|p| levels.holds(p)) {
                done[i] = true;
                added.extend(a.effects.iter().cloned());
                if let Some(id) = &a.script {
                    levels.fired.entry(id.clone()).or_insert(pass);
                }
            }
        }
        for p in &added {
            changed |= levels.add(p, pass);
        }
        if !changed {
            break;
        }
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_levels_take_everything_as_reachable() {
        let l = Levels::unknown();
        assert_eq!(l.level(&Predicate::Badge { n: 8 }), Some(0));
    }
}
