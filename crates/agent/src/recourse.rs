//! Recourses: the ways a stuck goal tries to get unstuck, stacked by a
//! priority computed each time one is needed and updated by how each has
//! paid off ([`crate::ledger`]).
//!
//! A recourse is offered with a chance (that it changes what the belief
//! knows, which is what lets the planner find another way) and the
//! seconds it is expected to take; the priority is the chance per second.
//! Nothing is scripted:
//!
//! - `Probe`: open the screens that settle the plan's unobserved
//!   assumptions (menus, cheap and exact).
//! - `Explore { map }`: talk to every person and read every sign of a map
//!   that can still teach something (never tried, tried before the belief
//!   changed, or a question's other answer left), the stuck map first and
//!   then outward, nearest first, up to [`MAX_HOPS`] maps away. A map the
//!   failing plan named counts double.
//!
//! The chance of one talk is the share of all logged talks that taught
//! something; a kind's own record scales its offers ([`weight`]). So a
//! kind that keeps failing sinks below the others, and one that works
//! rises, across restarts. Adding a recourse is an [`Recourse`] variant,
//! its offers here and its run in the goal loop.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use pokebot_planner::ProbeFact;
use pokebot_state::{GameState, PlayerPose};
use pokebot_world::route::{self, PlaceGraph, UnknownPolicy};
use pokebot_world::World;
use serde::Serialize;

use crate::belief_view::StateBelief;
use crate::ledger::Ledger;
use crate::tools::explore;

/// Maps away from the stuck one an `Explore` may go.
pub const MAX_HOPS: u32 = 3;
/// Seconds a talk takes (walk up, the conversation, the way back).
const TALK_S: f64 = 25.0;
/// Seconds a map hop takes when there is no route graph to price it.
const HOP_S: f64 = 40.0;
/// Seconds a probe (one screen opened and closed) takes.
const PROBE_S: f64 = 20.0;
/// Chance one talk teaches something, before any talk is logged.
const PRIOR_TALK: f64 = 0.1;
/// Chance a probe of the plan's assumptions changes the belief.
const PRIOR_PROBE: f64 = 0.25;
/// A map the failing plan names is this much likelier to hold the answer.
const RELEVANT: f64 = 2.0;

/// A way to get unstuck.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Recourse {
    Probe { facts: Vec<ProbeFact> },
    Explore { map: String },
}

impl Recourse {
    /// The kind its record is kept under.
    pub fn kind(&self) -> &'static str {
        match self {
            Recourse::Probe { .. } => "probe",
            Recourse::Explore { .. } => "explore",
        }
    }
}

impl std::fmt::Display for Recourse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Recourse::Probe { facts } => write!(f, "probe {facts:?}"),
            Recourse::Explore { map } => write!(f, "explore {map}"),
        }
    }
}

/// A recourse on offer, priced.
#[derive(Debug, Clone, Serialize)]
pub struct Offer {
    pub recourse: Recourse,
    /// That it changes the belief.
    pub chance: f64,
    pub cost_s: f64,
    /// Why this chance and cost (for the log).
    pub why: String,
}

impl Offer {
    /// Chance per second.
    pub fn priority(&self) -> f64 {
        self.chance / self.cost_s.max(1.0)
    }
}

/// What the goal loop knows about where it is stuck.
#[derive(Debug, Clone, Default)]
pub struct Stall {
    /// Maps the failing plan walks to or acts on.
    pub relevant: BTreeSet<String>,
    /// Probes that settle the plan's unobserved assumptions.
    pub probes: Vec<ProbeFact>,
}

/// How a kind's record scales its offers: its smoothed success rate over
/// the prior, between ¼ and 4.
pub fn weight(ledger: &Ledger, kind: &str, prior: f64) -> f64 {
    (ledger.stats(kind).rate(prior) / prior).clamp(0.25, 4.0)
}

/// An `Explore` of `fresh` targets, each teaching with chance `q`,
/// `travel_s` away: the chance one of them teaches something, and the
/// seconds expected until one does or all were tried (the tool stops at
/// the first, so a map rich in people isn't charged for all of them).
pub fn explore_price(q: f64, fresh: usize, travel_s: f64) -> (f64, f64) {
    let q = q.clamp(0.0, 0.95);
    let n = fresh.min(explore::MAX_TARGETS);
    let miss = (1.0 - q).powi(i32::try_from(n).unwrap_or(i32::MAX));
    // Talks expected: the sum over k < n of (1 - q)^k.
    let talks = if q > 0.0 { (1.0 - miss) / q } else { n as f64 };
    (1.0 - miss, travel_s + TALK_S * talks)
}

/// Maps within `max_hops` warps or connections of `from`, with their
/// distance, nearest first.
pub fn nearby_maps(world: &World, from: &str, max_hops: u32) -> Vec<(String, u32)> {
    let mut distance = BTreeMap::from([(from.to_owned(), 0u32)]);
    let mut queue = VecDeque::from([from.to_owned()]);
    while let Some(name) = queue.pop_front() {
        let d = distance[&name];
        if d >= max_hops {
            continue;
        }
        let Some(map) = world.map(&name) else {
            continue;
        };
        for next in map
            .warps
            .iter()
            .filter_map(|w| world.name_of(&w.dest_map))
            .chain(map.connections.iter().filter_map(|c| world.name_of(&c.map)))
        {
            if !distance.contains_key(next) {
                distance.insert(next.to_owned(), d + 1);
                queue.push_back(next.to_owned());
            }
        }
    }
    let mut maps: Vec<_> = distance.into_iter().collect();
    maps.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    maps
}

/// Every recourse on offer from `pose`, best first.
pub fn offers(
    world: &World,
    graph: Option<&PlaceGraph>,
    ledger: &Ledger,
    state: &GameState,
    pose: &PlayerPose,
    stall: &Stall,
) -> Vec<Offer> {
    let mut out = Vec::new();
    if !stall.probes.is_empty() {
        let chance = (PRIOR_PROBE * weight(ledger, "probe", PRIOR_PROBE)).min(0.95);
        out.push(Offer {
            recourse: Recourse::Probe {
                facts: stall.probes.clone(),
            },
            chance,
            cost_s: PROBE_S * stall.probes.len() as f64,
            why: format!("{} unobserved assumption(s)", stall.probes.len()),
        });
    }
    let (learnt, talks) = ledger.talk_rate();
    let q = (f64::from(learnt) + 2.0 * PRIOR_TALK) / (f64::from(talks) + 2.0);
    let w = weight(ledger, "explore", PRIOR_TALK);
    let unnamed: Vec<(i32, i32)> = state
        .view
        .npcs
        .iter()
        .filter(|n| n.map == pose.map && n.local_id.is_none())
        .map(|n| (n.x, n.y))
        .collect();
    let belief = StateBelief(state);
    for (map, hops) in nearby_maps(world, &pose.map, MAX_HOPS) {
        let here = map == pose.map;
        let at = if here {
            pose.clone()
        } else {
            PlayerPose {
                map: map.clone(),
                x: 0,
                y: 0,
            }
        };
        let fresh = explore::fresh(world, ledger, state, &at, if here { &unnamed } else { &[] });
        if fresh.is_empty() {
            continue;
        }
        let travel_s = if here {
            0.0
        } else if let Some(graph) = graph {
            let r = route::route_to_map(
                world,
                graph,
                &belief,
                pose,
                &map,
                UnknownPolicy::Pessimistic,
            );
            if !r.found() || !r.cost_s.is_finite() {
                continue;
            }
            r.cost_s
        } else {
            f64::from(hops) * HOP_S
        };
        let relevant = here || stall.relevant.contains(&map);
        let per_talk = q * w * if relevant { RELEVANT } else { 1.0 };
        let (chance, cost_s) = explore_price(per_talk, fresh.len(), travel_s);
        out.push(Offer {
            recourse: Recourse::Explore { map: map.clone() },
            chance,
            cost_s,
            why: format!(
                "{} fresh target(s), {hops} hop(s), {travel_s:.0} s away, talk rate {learnt}/{talks}{}",
                fresh.len(),
                if relevant { ", relevant" } else { "" }
            ),
        });
    }
    out.sort_by(|a, b| b.priority().total_cmp(&a.priority()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(map: &str, chance: f64, cost_s: f64) -> Offer {
        Offer {
            recourse: Recourse::Explore { map: map.into() },
            chance,
            cost_s,
            why: String::new(),
        }
    }

    fn priced(map: &str, q: f64, fresh: usize, travel_s: f64) -> Offer {
        let (chance, cost_s) = explore_price(q, fresh, travel_s);
        offer(map, chance, cost_s)
    }

    /// The stuck map's people beat a map two minutes away, few or many;
    /// once they are spent, the neighbour is next.
    #[test]
    fn near_and_relevant_comes_before_far() {
        for (here_n, far_n) in [(3, 6), (8, 1), (1, 8)] {
            let here = priced("Here", 0.2, here_n, 0.0);
            let far = priced("Far", 0.1, far_n, 120.0);
            assert!(here.priority() > far.priority(), "{here_n} vs {far_n}");
            assert!(far.priority() > 0.0);
        }
        // Equal odds and travel: the richer map is the better bet.
        assert!(priced("A", 0.1, 6, 60.0).priority() > priced("B", 0.1, 1, 60.0).priority());
    }

    #[test]
    fn more_fresh_targets_raise_the_chance_but_never_to_certainty() {
        assert!(explore_price(0.1, 5, 0.0).0 > explore_price(0.1, 1, 0.0).0);
        assert!(explore_price(1.0, 50, 0.0).0 < 1.0);
        assert_eq!(explore_price(0.1, 0, 0.0), (0.0, 0.0));
        // A sure first talk costs one talk, however many are fresh.
        assert!((explore_price(0.95, 8, 0.0).1 - TALK_S * 1.05).abs() < 1.0);
    }

    /// A kind that keeps failing sinks; one that pays off rises.
    #[test]
    fn a_kinds_record_moves_its_weight() {
        let mut l = Ledger::default();
        assert!((weight(&l, "probe", PRIOR_PROBE) - 1.0).abs() < 1e-9);
        for _ in 0..6 {
            l.tried("probe", false, 20.0);
        }
        assert!(weight(&l, "probe", PRIOR_PROBE) < 0.5);
        for _ in 0..6 {
            l.tried("explore", true, 60.0);
        }
        assert!(weight(&l, "explore", PRIOR_TALK) > 2.0);
    }

    /// On the real world: the maps around Bill's cottage, nearest first,
    /// and the cottage itself offered while its people are untried.
    #[test]
    fn the_stuck_map_is_offered_first_then_its_neighbours() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = World::load(&dir) else {
            return;
        };
        if world.events().is_none() {
            return;
        }
        let maps = nearby_maps(&world, "Route25_SeaCottage", 2);
        assert_eq!(maps[0], ("Route25_SeaCottage".to_owned(), 0));
        assert!(
            maps.iter().any(|(m, d)| m == "Route25" && *d == 1),
            "{maps:?}"
        );
        let pose = PlayerPose {
            map: "Route25_SeaCottage".into(),
            x: 4,
            y: 6,
        };
        let state = GameState::default();
        let o = offers(
            &world,
            None,
            &Ledger::default(),
            &state,
            &pose,
            &Stall::default(),
        );
        assert_eq!(
            o.first().map(|o| &o.recourse),
            Some(&Recourse::Explore {
                map: "Route25_SeaCottage".into()
            }),
            "{o:#?}"
        );
    }
}
