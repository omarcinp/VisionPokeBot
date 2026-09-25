//! Event-driven needs, independent of the campaign goal. Urgent needs
//! suspend the running tool at an overworld boundary; soon needs wait for
//! an inexpensive detour. Recovery never discards the campaign plan.
use crate::{belief_view::StateBelief, tools::lookup};
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, GameState, PartyMon, PlayerPose, Status};
use pokebot_world::{
    route::{self, EdgeKind, Place, PlaceGraph, RouteParams, UnknownPolicy},
    World,
};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Need {
    AuditParty,
    HealUrgent,
    HealSoon,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Layer {
    Motion,
    Tool,
    Plan,
    Goals,
}

pub fn layer(event: &GameEvent) -> Option<Layer> {
    match event {
        GameEvent::PartyAudited { .. }
        | GameEvent::PartyObserved { .. }
        | GameEvent::MovePpObserved { .. }
        | GameEvent::MoveUsed { .. }
        | GameEvent::MovesObserved { .. }
        | GameEvent::Healed
        | GameEvent::BattleEnded
        | GameEvent::Evolved { .. } => Some(Layer::Goals),
        GameEvent::FlagObserved { .. }
        | GameEvent::FlagTracked { .. }
        | GameEvent::VarObserved { .. }
        | GameEvent::VarTracked { .. }
        | GameEvent::PocketObserved { .. }
        | GameEvent::ItemsChanged { .. }
        | GameEvent::MoneyObserved { .. }
        | GameEvent::MoneyChanged { .. } => Some(Layer::Plan),
        GameEvent::BattleStarted => Some(Layer::Tool),
        GameEvent::MapVisited { .. } => Some(Layer::Motion),
        _ => None,
    }
}

fn valid_hp(mon: &PartyMon) -> Option<(u16, u16)> {
    mon.hp.value.filter(|&(cur, max)| {
        cur <= max
            && max > 0
            && (max >= u16::from(mon.level.value.unwrap_or(1)) + 10
                || (max == 1 && mon.species.value.as_deref() == Some("SPECIES_SHEDINJA")))
    })
}

pub fn health(party: Option<&[PartyMon]>, data: &GameData) -> Option<Need> {
    let Some(party) = party.filter(|p| !p.is_empty()) else {
        return Some(Need::AuditParty);
    };
    // Unknown fields must be observed before estimating risk.
    if party.iter().any(|m| {
        valid_hp(m).is_none()
            || m.status.value.is_none()
            || m.moves
                .iter()
                .flatten()
                .any(|s| s.pp.value.is_none() || s.mv.value.is_none())
            || m.moves.iter().all(Option::is_none)
    }) {
        return Some(Need::AuditParty);
    }
    let attacks = |m: &PartyMon| {
        m.moves
            .iter()
            .flatten()
            .filter(|s| {
                s.mv.value
                    .as_deref()
                    .and_then(|mv| data.move_(mv))
                    .is_some_and(|mv| mv.power > 0)
            })
            .fold((0u32, 0u32), |(cur, max), s| {
                let pp = s.pp.value.unwrap_or_default();
                (cur + u32::from(pp.0), max + u32::from(pp.1))
            })
    };
    let usable = party
        .iter()
        .filter(|m| {
            valid_hp(m).is_some_and(|p| p.0 > 0)
                && attacks(m).0 > 0
                && !matches!(
                    m.status.value,
                    Some(Status::Fainted | Status::Frozen | Status::Asleep)
                )
        })
        .count();
    let lead = &party[0];
    let (hp, max) = valid_hp(lead).unwrap();
    let pct = u32::from(hp) * 100 / u32::from(max);
    let (pp, total) = attacks(lead);
    let lead_sick = !matches!(lead.status.value, Some(Status::Healthy));
    if usable == 0
        || hp == 0
        || pp == 0
        || pct < if usable <= 1 { 50 } else { 35 }
        || (usable <= 1 && lead_sick)
    {
        return Some(Need::HealUrgent);
    }
    if party.iter().any(|m| {
        valid_hp(m).is_some_and(|(hp, max)| u32::from(hp) * 100 < u32::from(max) * 75)
            || m.status.value != Some(Status::Healthy)
    }) || (total > 0 && pp * 4 < total)
    {
        Some(Need::HealSoon)
    } else {
        None
    }
}

#[derive(Default)]
pub struct Scheduler {
    pub enabled: bool,
    pub recovering: bool,
    pub queue: VecDeque<Need>,
    pub destination: Option<String>,
    pub graph: Option<PlaceGraph>,
    pub assumptions: Vec<pokebot_planner::GoalPredicate>,
    pub invalidated: Option<String>,
}
impl Scheduler {
    pub fn event(&mut self, event: &GameEvent, state: &GameState, data: &GameData) -> bool {
        if !self.enabled {
            return false;
        }
        if layer(event) == Some(Layer::Plan) {
            use pokebot_planner::GoalBelief;
            let knowledge = state.saved_knowledge();
            let belief = pokebot_planner::StateBelief::new(
                &knowledge,
                data,
                state.player.pose.value.clone(),
            );
            if let Some(p) = self
                .assumptions
                .iter()
                .find(|p| belief.eval_goal(p) == pokebot_world::predicate::Truth::False)
            {
                self.invalidated = Some(format!("event contradicted active assumption: {p}"));
                return true;
            }
        }
        if layer(event) != Some(Layer::Goals) {
            return false;
        }
        let old = self.queue.clone();
        self.queue.clear();
        let need = health(state.party.value.as_deref(), data);
        // Battle HUD, status text and PP events already update persistent
        // party facts. BattleEnded reuses those readings; only missing or
        // inconsistent facts require another menu audit.
        if need == Some(Need::HealUrgent) {
            self.queue.push_front(Need::HealUrgent);
        } else if need == Some(Need::AuditParty) {
            self.queue.push_front(Need::AuditParty);
        }
        if need == Some(Need::HealSoon) {
            self.queue.push_back(Need::HealSoon);
        }
        old != self.queue
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Recovery {
    pub map: String,
    pub medicine: Option<String>,
    pub added_travel_s: f64,
    pub encounter_risk_s: f64,
    pub service_s: f64,
    pub item_cost_s: f64,
    pub score_s: f64,
}

/// Price nearby healers using gated tile routes and the added cost of
/// returning toward the interrupted destination. Encounter exposure is a
/// map-density heuristic; it is deliberately not an exact probability.
pub fn recovery(
    world: &World,
    graph: &PlaceGraph,
    state: &GameState,
    data: &GameData,
    from: &PlayerPose,
    destination: Option<&str>,
    urgent: bool,
) -> Option<Recovery> {
    let belief = StateBelief(state);
    let mut distance = BTreeMap::from([(from.map.clone(), 0usize)]);
    let mut queue = VecDeque::from([from.map.clone()]);
    while let Some(name) = queue.pop_front() {
        let d = distance[&name];
        if d >= 5 {
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
    let baseline = destination
        .map(|d| {
            route::route_to_map(world, graph, &belief, from, d, UnknownPolicy::Pessimistic).cost_s
        })
        .unwrap_or(0.0);
    let mut maps: Vec<_> = distance
        .into_iter()
        .filter(|(map, _)| lookup::healer_on(world, map).is_some())
        .collect();
    maps.sort_by_key(|(map, d)| (*d, map.clone()));
    let mut candidates = Vec::new();
    for (name, _) in maps.into_iter().take(8) {
        let map = world.map(&name)?;
        let id = lookup::healer_on(world, &name)?;
        let (x, y) = lookup::object_tile(world, &name, id)?;
        for ((x, y), _) in crate::nav::facing_spots(map, x, y) {
            let to = Place::tile(&name, x, y);
            let out = route::route(world, graph, &belief, from, &to, UnknownPolicy::Pessimistic);
            if !out.found() {
                continue;
            }
            let return_cost = destination
                .map(|d| {
                    route::route_to_map(
                        world,
                        graph,
                        &belief,
                        &PlayerPose {
                            map: name.clone(),
                            x,
                            y,
                        },
                        d,
                        UnknownPolicy::Pessimistic,
                    )
                    .cost_s
                })
                .unwrap_or(0.0);
            let travel = if urgent || !baseline.is_finite() {
                out.cost_s
            } else {
                (out.cost_s + return_cost - baseline).max(0.0)
            };
            if !travel.is_finite() {
                continue;
            }
            let risk = out
                .legs
                .iter()
                .map(|leg| {
                    let EdgeKind::Walk { tiles, .. } = leg.kind else {
                        return 0.0;
                    };
                    let Some(m) = world.map(&leg.from.map) else {
                        return 0.0;
                    };
                    let mut grass = 0;
                    let mut walkable = 0;
                    for y in 0..m.height {
                        for x in 0..m.width {
                            if let Some(t) = m.tile(x, y) {
                                if t.collision == 0 {
                                    walkable += 1;
                                    if lookup::encounter_tile(&t) {
                                        grass += 1;
                                    }
                                }
                            }
                        }
                    }
                    f64::from(tiles) * grass as f64 / (walkable.max(1) as f64)
                        * if urgent { 4.0 } else { 1.0 }
                })
                .sum::<f64>();
            candidates.push(Recovery {
                map: name.clone(),
                medicine: None,
                added_travel_s: travel,
                encounter_risk_s: risk,
                service_s: 12.0,
                item_cost_s: 0.0,
                score_s: travel + risk + 12.0,
            });
        }
    }
    candidates.extend(medicines(state, data, &from.map));
    candidates
        .into_iter()
        .min_by(|a, b| a.score_s.total_cmp(&b.score_s).then(a.map.cmp(&b.map)))
}

/// Only executable medicines that resolve the queued health need.
/// Price is converted at 10 money units per second of avoided travel.
fn medicines(state: &GameState, data: &GameData, map: &str) -> Vec<Recovery> {
    let Some(party) = state.party.value.as_ref().filter(|p| !p.is_empty()) else {
        return vec![];
    };
    if party[0].status.value != Some(Status::Healthy) {
        return vec![];
    }
    let Some((hp, max)) = valid_hp(&party[0]) else {
        return vec![];
    };
    let items = state
        .bag
        .pockets
        .get(&pokebot_state::Pocket::Items)
        .and_then(|k| k.value.as_ref());
    let mut candidates = Vec::new();
    for (item, heal) in [
        ("ITEM_POTION", 20u16),
        ("ITEM_SUPER_POTION", 50),
        ("ITEM_HYPER_POTION", 200),
        ("ITEM_MAX_POTION", u16::MAX),
    ] {
        if !items.is_some_and(|items| items.iter().any(|(i, n)| i == item && *n > 0)) {
            continue;
        }
        let Some(price) = data.items.get(item).map(|i| i.price) else {
            continue;
        };
        let mut after = party.clone();
        after[0].hp.value = Some((hp.saturating_add(heal).min(max), max));
        if health(Some(&after), data).is_some() {
            continue;
        }
        let item_cost_s = f64::from(price) / 10.0;
        candidates.push(Recovery {
            map: map.into(),
            medicine: Some(item.into()),
            added_travel_s: 0.0,
            encounter_risk_s: 0.0,
            service_s: 15.0,
            item_cost_s,
            score_s: 15.0 + item_cost_s,
        });
    }
    candidates
}

pub fn graph(world: &World) -> PlaceGraph {
    PlaceGraph::build(world, RouteParams::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_state::{Knowledge, MoveSlot};
    fn data() -> GameData {
        serde_json::from_value(serde_json::json!({"species":{},"moves":{"MOVE_TACKLE":{"name":"TACKLE","effect":null,"power":35,"type":null,"accuracy":100,"pp":35,"priority":0,"secondary_chance":0}},"items":{},"type_chart":[],"trainers":{},"wild":{},"map_trainers":{},"marts":{}})).unwrap()
    }
    fn mon(hp: u16) -> PartyMon {
        PartyMon {
            hp: Knowledge::observed((hp, 100), 1),
            level: Knowledge::observed(20, 1),
            status: Knowledge::observed(Status::Healthy, 1),
            moves: [
                Some(MoveSlot {
                    mv: Knowledge::observed("MOVE_TACKLE".into(), 1),
                    pp: Knowledge::observed((20, 35), 1),
                }),
                None,
                None,
                None,
            ],
            ..Default::default()
        }
    }
    #[test]
    fn last_usable_mon_is_more_urgent_than_one_wounded_member() {
        let d = data();
        assert_eq!(health(Some(&[mon(45)]), &d), Some(Need::HealUrgent));
        assert_eq!(health(Some(&[mon(45), mon(100)]), &d), Some(Need::HealSoon));
        assert_eq!(
            health(Some(&[mon(34), mon(100)]), &d),
            Some(Need::HealUrgent)
        );
        let mut unknown = mon(100);
        unknown.hp = Knowledge::unknown();
        assert_eq!(health(Some(&[unknown]), &d), Some(Need::AuditParty));
    }
    #[test]
    fn events_coalesce_and_recovery_clears_the_queue() {
        let d = data();
        let mut state = GameState {
            party: Knowledge::observed(vec![mon(45)], 1),
            ..Default::default()
        };
        let mut s = Scheduler {
            enabled: true,
            ..Default::default()
        };
        s.event(&GameEvent::BattleEnded, &state, &d);
        s.event(&GameEvent::BattleEnded, &state, &d);
        assert_eq!(s.queue, VecDeque::from([Need::HealUrgent]));
        state.party = Knowledge::observed(vec![mon(100)], 2);
        s.event(
            &GameEvent::PartyAudited {
                members: vec![mon(100)],
            },
            &state,
            &d,
        );
        assert!(s.queue.is_empty());
    }

    #[test]
    fn battle_end_reuses_health_and_only_audits_unknown_facts() {
        let d = data();
        let mut state = GameState {
            party: Knowledge::observed(vec![mon(100)], 1),
            ..Default::default()
        };
        let mut s = Scheduler {
            enabled: true,
            ..Default::default()
        };
        s.event(&GameEvent::BattleEnded, &state, &d);
        assert!(s.queue.is_empty());

        // The last battle's HUD replaces HP; the inactive roster is retained.
        state.party.value.as_mut().unwrap()[0].hp = Knowledge::observed((60, 100), 2);
        s.event(&GameEvent::BattleEnded, &state, &d);
        assert_eq!(s.queue, VecDeque::from([Need::HealSoon]));
        state.party.value.as_mut().unwrap()[0].hp = Knowledge::observed((40, 100), 3);
        s.event(&GameEvent::BattleEnded, &state, &d);
        assert_eq!(s.queue, VecDeque::from([Need::HealUrgent]));
        state.party.value.as_mut().unwrap()[0].hp = Knowledge::unknown();
        s.event(&GameEvent::BattleEnded, &state, &d);
        assert_eq!(s.queue, VecDeque::from([Need::AuditParty]));
    }

    #[test]
    fn medicines_must_resolve_health_without_hiding_pp_or_status_needs() {
        let mut d = data();
        d.items.insert(
            "ITEM_SUPER_POTION".into(),
            pokebot_gamedata::Item {
                price: 700,
                name: "SUPER POTION".into(),
                pocket: Some("POCKET_ITEMS".into()),
            },
        );
        let mut state = GameState {
            party: Knowledge::observed(vec![mon(45)], 1),
            ..Default::default()
        };
        state.bag.pockets.insert(
            pokebot_state::Pocket::Items,
            Knowledge::observed(vec![("ITEM_SUPER_POTION".into(), 1)], 1),
        );
        let choices = medicines(&state, &d, "Route1");
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].item_cost_s, 70.0);
        state.party.value.as_mut().unwrap()[0].status = Knowledge::observed(Status::Poisoned, 2);
        assert!(medicines(&state, &d, "Route1").is_empty());
        let m = &mut state.party.value.as_mut().unwrap()[0];
        m.status = Knowledge::observed(Status::Healthy, 2);
        m.moves[0].as_mut().unwrap().pp = Knowledge::observed((0, 35), 2);
        assert!(medicines(&state, &d, "Route1").is_empty());
    }

    #[test]
    fn a_fact_contradiction_invalidates_the_plan_but_health_only_queues_recovery() {
        let d = data();
        let mut state = GameState::default();
        let p = pokebot_planner::GoalPredicate::flag("FLAG_A", true);
        let mut s = Scheduler {
            enabled: true,
            assumptions: vec![p],
            ..Default::default()
        };
        state.party = Knowledge::observed(vec![mon(45)], 1);
        s.event(&GameEvent::BattleEnded, &state, &d);
        assert!(s.invalidated.is_none());
        state
            .world
            .flags
            .insert("FLAG_A".into(), Knowledge::observed(false, 2));
        s.event(
            &GameEvent::FlagObserved {
                flag: "FLAG_A".into(),
                value: false,
            },
            &state,
            &d,
        );
        assert!(s.invalidated.is_some());
    }

    #[test]
    fn recovery_prices_tile_routes_and_includes_mom() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(world) = World::load(root.join("data/world")) else {
            return;
        };
        let d = data();
        let state = GameState::default();
        let graph = graph(&world);
        let from = PlayerPose {
            map: "PalletTown_PlayersHouse_1F".into(),
            x: 7,
            y: 6,
        };
        let choice = recovery(&world, &graph, &state, &d, &from, None, true).unwrap();
        assert_eq!(choice.map, from.map);
        assert!(choice.medicine.is_none());
        assert!(choice.score_s.is_finite());
    }
}
