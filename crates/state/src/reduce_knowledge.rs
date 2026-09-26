//! Applying knowledge events (party, bag, money, PC, Pokédex, world belief)
//! to the state. Pure: the same events always give the same state.

use crate::belief::track;
use crate::{
    GameEvent, GameState, HealSpot, Knowledge, KnowledgeSource, MoveSlot, PartyMon, PokedexCounts,
    Status,
};

/// Applies `event` if it is a knowledge event; returns whether it was one.
pub(crate) fn apply(state: &mut GameState, frame: u64, event: &GameEvent) -> bool {
    match event {
        GameEvent::PartyAudited { members } => {
            let old = state.party.value.as_deref().unwrap_or_default();
            let roster = members
                .iter()
                .map(|mon| {
                    let mut mon = mon.clone();
                    let matches: Vec<_> = old.iter().filter(|m| m.matches_member(&mon)).collect();
                    let reading = mon.details.reading();
                    if matches.len() == 1
                        && members.iter().filter(|m| m.matches_member(&mon)).count() == 1
                    {
                        let old = matches[0];
                        mon.details = old.details.clone();
                        mon.history = old.history.clone();
                        mon.training = old.training.clone();
                        mon.observe_details(&reading, frame);
                        if let Some(level) = mon.level.value {
                            mon.training.level_seen(level);
                        }
                        if mon.species.value != old.species.value {
                            mon.training.recalculated();
                        }
                    }
                    observe_summary_stats(&mut mon, &reading, frame);
                    if let Some((_, max)) = mon.hp.value {
                        observe_stat(&mut mon, 0, max, frame);
                    }
                    mon
                })
                .collect();
            state.party = Knowledge::observed(roster, frame);
        }
        GameEvent::PartyDetailsObserved { slot, details } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            m.observe_details(details, frame);
            observe_summary_stats(m, details, frame);
        }
        GameEvent::BadgeCountObserved { count } => {
            crate::badges::observe_count(state, *count, frame);
        }
        GameEvent::PartyReordered { order } => {
            if let Some(old) = &state.party.value {
                let mut sorted = order.clone();
                sorted.sort_unstable();
                if sorted == (0..old.len() as u8).collect::<Vec<_>>() {
                    state.party = Knowledge::observed(
                        order.iter().map(|i| old[usize::from(*i)].clone()).collect(),
                        frame,
                    );
                }
            }
        }
        GameEvent::PartyMonDerived { slot, mon } => {
            *member(state, *slot, KnowledgeSource::Derived, frame) = (**mon).clone();
        }
        GameEvent::PartyObserved {
            slot,
            species,
            nickname,
            level,
            hp,
            status,
            held_item,
        } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            if let Some(v) = species {
                m.species = Knowledge::observed(v.clone(), frame);
            }
            if let Some(v) = nickname {
                m.nickname = Knowledge::observed(v.clone(), frame);
            }
            if let Some(v) = level {
                m.training.level_seen(*v);
                m.level = Knowledge::observed(*v, frame);
            }
            if let Some(v) = hp {
                m.hp = Knowledge::observed(*v, frame);
                observe_stat(m, 0, v.1, frame);
            }
            if let Some(v) = status {
                m.status = Knowledge::observed(*v, frame);
            }
            if let Some(v) = held_item {
                m.held_item = Knowledge::observed(v.clone(), frame);
            }
        }
        GameEvent::MovesObserved { slot, moves } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            for i in 0..4 {
                m.moves[i] = match (moves.get(i), m.moves[i].take()) {
                    (Some(mv), Some(old)) if old.mv.value.as_ref() == Some(mv) => Some(MoveSlot {
                        mv: Knowledge::observed(mv.clone(), frame),
                        pp: old.pp,
                    }),
                    (Some(mv), _) => Some(MoveSlot {
                        mv: Knowledge::observed(mv.clone(), frame),
                        pp: Knowledge::unknown(),
                    }),
                    (None, _) => None,
                };
            }
        }
        GameEvent::MovePpObserved {
            slot,
            move_slot,
            cur,
            max,
        } => {
            if let Some(s) = move_slot_mut(state, *slot, *move_slot) {
                s.pp = Knowledge::observed((*cur, *max), frame);
            }
        }
        GameEvent::MoveUsed { slot, move_slot } => {
            if let Some(s) = move_slot_mut(state, *slot, *move_slot) {
                if let Some((cur, max)) = s.pp.value {
                    s.pp =
                        Knowledge::tracked((cur.saturating_sub(1), max), s.pp.last_verified_frame);
                }
            }
        }
        GameEvent::MoveOutOfPp { slot, move_slot } => {
            if let Some(s) = move_slot_mut(state, *slot, *move_slot) {
                let max = s.pp.value.map_or(0, |(_, max)| max);
                s.pp = Knowledge::observed((0, max), frame);
            }
        }
        GameEvent::MoveLearned {
            slot,
            move_slot,
            mv,
            max_pp,
        }
        | GameEvent::MoveReplaced {
            slot,
            move_slot,
            new: mv,
            max_pp,
            ..
        } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            if let Some(entry) = m.moves.get_mut(usize::from(*move_slot)) {
                *entry = Some(MoveSlot {
                    mv: Knowledge::observed(mv.clone(), frame),
                    pp: Knowledge::derived((*max_pp, *max_pp), frame),
                });
            }
        }
        GameEvent::Evolved { slot, species } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            if m.species.value.as_ref() != Some(species) {
                m.training.recalculated();
            }
            m.species = Knowledge::observed(species.clone(), frame);
        }
        GameEvent::EffortGained {
            slot,
            species,
            level,
            ev_yield,
            exp,
        } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            let macho_brace =
                m.held_item.value.as_ref().and_then(Option::as_deref) == Some("ITEM_MACHO_BRACE");
            m.training
                .award(frame, species.clone(), *level, *ev_yield, *exp, macho_brace);
        }
        GameEvent::LevelUpStatsObserved { slot, stats } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            for (i, v) in stats.iter().enumerate() {
                observe_stat(m, i, *v, frame);
            }
            let [_, attack, defense, speed, sp_attack, sp_defense] = stats.map(Some);
            m.observe_details(
                &crate::SummaryDetails {
                    attack,
                    defense,
                    speed,
                    sp_attack,
                    sp_defense,
                    ..Default::default()
                },
                frame,
            );
            // The game adds the maximum HP's gain to the current HP.
            if let Some((cur, max)) = m.hp.value {
                let cur = (cur + stats[0].saturating_sub(max)).min(stats[0]);
                m.hp = Knowledge::tracked((cur, stats[0]), m.hp.last_verified_frame);
            }
        }
        GameEvent::IvsEstimated { slot, estimate } => {
            member(state, *slot, KnowledgeSource::Derived, frame)
                .training
                .estimate = Some(estimate.clone());
        }
        GameEvent::Healed => {
            for m in state.party.value.iter_mut().flatten() {
                if let Some((_, max)) = m.hp.value {
                    m.hp = Knowledge::derived((max, max), frame);
                }
                m.status = Knowledge::derived(Status::Healthy, frame);
                for s in m.moves.iter_mut().flatten() {
                    if let Some((_, max)) = s.pp.value {
                        s.pp = Knowledge::derived((max, max), frame);
                    }
                }
            }
        }
        GameEvent::ItemsChanged {
            pocket,
            item,
            delta,
            ..
        } => {
            if let Some(k) = state.bag.pockets.get_mut(pocket) {
                if let Some(list) = &k.value {
                    *k = Knowledge::tracked(add_items(list, item, *delta), k.last_verified_frame);
                }
            }
        }
        GameEvent::PocketObserved { pocket, items } => {
            state
                .bag
                .pockets
                .insert(*pocket, Knowledge::observed(items.clone(), frame));
        }
        GameEvent::PocketRowsObserved { pocket, items } => {
            if let Some(k) = state.bag.pockets.get_mut(pocket) {
                // Unknown pocket: what was seen is all that is known, as a
                // lower bound (tracked, so a decision audits it first).
                let mut list = k.value.clone().unwrap_or_default();
                for (item, count) in items {
                    match list.iter_mut().find(|(i, _)| i == item) {
                        Some(entry) => entry.1 = *count,
                        None => list.push((item.clone(), *count)),
                    }
                }
                *k = match k.source {
                    KnowledgeSource::Observed => Knowledge::observed(list, frame),
                    _ => Knowledge::tracked(list, k.last_verified_frame.or(Some(frame))),
                };
            }
        }
        GameEvent::PartySizeObserved { size } => {
            let size = usize::from(*size);
            if size > 0 {
                member(state, size as u8 - 1, KnowledgeSource::Observed, frame);
            }
            if let Some(list) = state.party.value.as_mut() {
                list.truncate(size);
            }
        }
        GameEvent::MoneyObserved { amount } => state.money = Knowledge::observed(*amount, frame),
        GameEvent::MoneyChanged { delta, .. } => {
            if let Some(m) = state.money.value {
                let new = (i64::from(m) + delta).clamp(0, i64::from(u32::MAX)) as u32;
                state.money = Knowledge::tracked(new, state.money.last_verified_frame);
            }
        }
        GameEvent::BoxObserved { box_index, mons } => {
            if let Some(b) = state.pc.boxes.get_mut(usize::from(*box_index)) {
                // The same Pokémon in the same place keeps what was known.
                let mut mons = mons.clone();
                for m in &mut mons {
                    let same = b.value.iter().flatten().find(|o| {
                        o.slot == m.slot
                            && o.species.value == m.species.value
                            && o.level.value == m.level.value
                            && o.nickname.value == m.nickname.value
                    });
                    if let Some(old) = same {
                        m.training = old.training.clone();
                    }
                }
                *b = Knowledge::observed(mons, frame);
            }
        }
        GameEvent::PcItemsObserved { items } => {
            state.pc.items = Knowledge::observed(items.clone(), frame)
        }
        GameEvent::SentToPc {
            box_index: Some(i),
            mon,
        } => {
            if let Some(b) = state.pc.boxes.get_mut(usize::from(*i)) {
                if let Some(list) = &b.value {
                    let mut list = list.clone();
                    list.push((**mon).clone());
                    *b = Knowledge::tracked(list, b.last_verified_frame);
                }
            }
        }
        GameEvent::SentToPc {
            box_index: None, ..
        }
        | GameEvent::ShinySeen { .. } => {}
        GameEvent::MonDeposited {
            party_slot,
            box_index,
        } => {
            let taken = state.party.value.as_mut().and_then(|p| {
                (usize::from(*party_slot) < p.len()).then(|| p.remove(usize::from(*party_slot)))
            });
            if state.party.value.is_some() {
                state.party.source = KnowledgeSource::Tracked;
            }
            if let (Some(mon), Some(b)) = (taken, state.pc.boxes.get_mut(usize::from(*box_index))) {
                if let Some(list) = &b.value {
                    let mut list = list.clone();
                    let slot = (0..30u8)
                        .find(|s| list.iter().all(|m| m.slot != *s))
                        .unwrap_or(0);
                    list.push(crate::BoxMon {
                        slot,
                        species: mon.species,
                        level: mon.level,
                        nickname: mon.nickname,
                        training: mon.training,
                    });
                    *b = Knowledge::tracked(list, b.last_verified_frame);
                }
            }
        }
        GameEvent::MonWithdrawn {
            box_index,
            box_slot,
        } => {
            let mon = state
                .pc
                .boxes
                .get_mut(usize::from(*box_index))
                .and_then(|b| {
                    let list = b.value.as_mut()?;
                    let at = list.iter().position(|m| m.slot == *box_slot)?;
                    let mon = list.remove(at);
                    b.source = KnowledgeSource::Tracked;
                    Some(mon)
                });
            if let Some(mon) = mon {
                let slot = state.party.value.as_ref().map_or(0, |p| p.len()) as u8;
                let m = member(state, slot, KnowledgeSource::Tracked, frame);
                m.species = mon.species;
                m.level = mon.level;
                m.nickname = mon.nickname;
                // Withdrawing computes the stats from the EVs it has.
                m.training = mon.training;
                m.training.level_evs = m.training.evs;
                m.training.recalculated();
            }
        }
        GameEvent::SpeciesSeen { species } => {
            let new = state
                .pokedex
                .seen
                .insert(species.clone(), Knowledge::observed(true, frame))
                .is_none_or(|k| k.value != Some(true));
            if new {
                if let Some(c) = state.pokedex.counts.value.as_mut() {
                    c.seen = c.seen.map(|n| n + 1);
                    state.pokedex.counts.source = KnowledgeSource::Tracked;
                }
            }
        }
        GameEvent::SpeciesCaught { species } => {
            let newly_seen = state
                .pokedex
                .seen
                .insert(species.clone(), Knowledge::observed(true, frame))
                .is_none_or(|k| k.value != Some(true));
            let newly_caught = state
                .pokedex
                .caught
                .insert(species.clone(), Knowledge::observed(true, frame))
                .is_none_or(|k| k.value != Some(true));
            // A read total moves with the catches after it (a species not
            // in the per-species map may still be in the total: only a
            // first mark for the species counts).
            if let Some(c) = state.pokedex.counts.value.as_mut() {
                if newly_seen {
                    c.seen = c.seen.map(|n| n + 1);
                }
                if newly_caught {
                    c.caught += 1;
                }
                if newly_seen || newly_caught {
                    state.pokedex.counts.source = KnowledgeSource::Tracked;
                }
            }
        }
        GameEvent::PokedexCountObserved { seen, caught } => {
            state.pokedex.counts = Knowledge::observed(
                PokedexCounts {
                    seen: *seen,
                    caught: *caught,
                },
                frame,
            );
        }
        GameEvent::CheckpointRestored { knowledge } => {
            let k = (**knowledge).clone();
            let in_control = state.progression.in_control.clone();
            state.progression = k.progression;
            state.progression.in_control = in_control;
            state.party = k.party;
            state.bag = k.bag;
            state.money = k.money;
            state.pc = k.pc;
            state.pokedex = k.pokedex;
            state.world = k.world;
            if state.progression.badges.value.is_none() {
                crate::badges::sync(state, frame);
            }
            // Session-scoped: what was infeasible before the restore may
            // not be any more.
            state.world.infeasible.clear();
        }
        GameEvent::FlagObserved { flag, value } => {
            state
                .world
                .flags
                .insert(flag.clone(), Knowledge::observed(*value, frame));
            if crate::badges::BADGES.iter().any(|(f, _)| f == flag) {
                crate::badges::sync(state, frame);
            }
        }
        GameEvent::FlagTracked { flag, value } => {
            if let Some(k) = track(state.world.flags.get(flag), *value) {
                state.world.flags.insert(flag.clone(), k);
                if crate::badges::BADGES.iter().any(|(f, _)| f == flag) {
                    crate::badges::sync(state, frame);
                }
            }
        }
        GameEvent::VarObserved { var, value } => {
            state
                .world
                .vars
                .insert(var.clone(), Knowledge::observed(*value, frame));
        }
        GameEvent::VarTracked { var, value } => {
            if let Some(k) = track(state.world.vars.get(var), *value) {
                state.world.vars.insert(var.clone(), k);
            }
        }
        GameEvent::MapVisited { map } => {
            state
                .world
                .visited
                .insert(map.clone(), Knowledge::observed(true, frame));
        }
        GameEvent::RespawnSet { map, x, y } => {
            state.world.respawn = Knowledge::observed(
                HealSpot {
                    map: map.clone(),
                    x: *x,
                    y: *y,
                },
                frame,
            );
        }
        GameEvent::NpcSeen {
            map,
            local_id,
            x,
            y,
            facing,
        } => {
            let npc = state.world.npc_mut(map, *local_id);
            npc.pos = Knowledge::observed((*x, *y), frame);
            if let Some(facing) = facing {
                npc.facing = Knowledge::observed(*facing, frame);
            }
            npc.present = Knowledge::observed(true, frame);
        }
        GameEvent::NpcAbsent { map, local_id } => {
            state.world.npc_mut(map, *local_id).present = Knowledge::observed(false, frame);
        }
        GameEvent::ScriptPathRun { script, path } => state.world.record_path(script, *path),
        GameEvent::ScriptPathRetracted {
            script,
            path,
            flags,
            vars,
        } => {
            let w = &mut state.world;
            if let Some(i) = w
                .paths_run
                .iter()
                .rposition(|(s, p)| s == script && p == path)
            {
                w.paths_run.remove(i);
            }
            let tracked = |source: KnowledgeSource| source == KnowledgeSource::Tracked;
            for flag in flags {
                if w.flags.get(flag).is_some_and(|k| tracked(k.source)) {
                    w.flags.remove(flag);
                }
            }
            for var in vars {
                if w.vars.get(var).is_some_and(|k| tracked(k.source)) {
                    w.vars.remove(var);
                }
            }
        }
        GameEvent::IntentInfeasible { intent } => {
            state.world.infeasible.insert(intent.clone());
        }
        // Session-scoped and kept by the agent's navigator, not the belief.
        GameEvent::TileBlocked { .. } | GameEvent::TileUnblocked { .. } => {}
        GameEvent::WhitedOut => {
            let m = member(state, 0, KnowledgeSource::Observed, frame);
            let max = m.hp.value.map_or(0, |(_, max)| max);
            m.hp = Knowledge::observed((0, max), frame);
            m.status = Knowledge::observed(Status::Fainted, frame);
        }
        _ => return false,
    }
    true
}

/// The party member in `slot`, creating unknown members up to it. The party
/// list's provenance becomes `source` when the list itself was unknown.
/// The level the member's stats were computed at: the highest seen.
fn stats_level(m: &PartyMon) -> Option<u8> {
    m.training.level.or(m.level.value)
}

/// A stat read at the member's current level (index in the game's order).
fn observe_stat(m: &mut PartyMon, index: usize, value: u16, frame: u64) {
    if let (Some(species), Some(level)) = (m.species.value.clone(), stats_level(m)) {
        let mut stats = [None; 6];
        stats[index] = Some(value);
        m.training.observe_stats(frame, &species, level, stats);
    }
}

/// The Skills page's stats and experience, for the member's training.
fn observe_summary_stats(m: &mut PartyMon, d: &crate::SummaryDetails, frame: u64) {
    if let (Some(species), Some(level)) = (m.species.value.clone(), stats_level(m)) {
        let stats = [
            None,
            d.attack,
            d.defense,
            d.speed,
            d.sp_attack,
            d.sp_defense,
        ];
        m.training.observe_stats(frame, &species, level, stats);
    }
    if let Some(exp) = d.exp_points {
        m.training.observe_exp(exp);
    }
}

fn member(state: &mut GameState, slot: u8, source: KnowledgeSource, frame: u64) -> &mut PartyMon {
    if state.party.value.is_none() {
        state.party = Knowledge {
            value: Some(Vec::new()),
            source,
            last_verified_frame: Some(frame),
        };
    }
    let list = state.party.value.as_mut().expect("set above");
    while list.len() <= usize::from(slot) {
        list.push(PartyMon::default());
    }
    &mut list[usize::from(slot)]
}

fn move_slot_mut(state: &mut GameState, slot: u8, move_slot: u8) -> Option<&mut MoveSlot> {
    state
        .party
        .value
        .as_mut()?
        .get_mut(usize::from(slot))?
        .moves
        .get_mut(usize::from(move_slot))?
        .as_mut()
}

/// `list` with `delta` of `item` added (entries reaching 0 are removed; new
/// items go to the end, as the game lists them).
fn add_items(list: &crate::ItemList, item: &str, delta: i32) -> crate::ItemList {
    let mut out = list.clone();
    match out.iter().position(|(i, _)| i == item) {
        Some(at) => {
            let n = (i32::from(out[at].1) + delta).max(0) as u16;
            if n == 0 {
                out.remove(at);
            } else {
                out[at].1 = n;
            }
        }
        None if delta > 0 => out.push((item.to_owned(), delta.min(i32::from(u16::MAX)) as u16)),
        None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefaultReducer, EventRecord, StateReducer};

    fn run(events: Vec<GameEvent>) -> GameState {
        let records: Vec<EventRecord> = events
            .into_iter()
            .enumerate()
            .map(|(i, event)| EventRecord {
                frame_id: i as u64 + 1,
                event,
            })
            .collect();
        DefaultReducer.reduce(&GameState::default(), &records)
    }

    fn starter() -> GameEvent {
        let mut mon = PartyMon {
            species: Knowledge::derived("SPECIES_BULBASAUR".into(), 0),
            level: Knowledge::derived(5, 0),
            ..Default::default()
        };
        mon.moves[0] = Some(MoveSlot {
            mv: Knowledge::derived("MOVE_TACKLE".into(), 0),
            pp: Knowledge::derived((35, 35), 0),
        });
        GameEvent::PartyMonDerived {
            slot: 0,
            mon: Box::new(mon),
        }
    }

    #[test]
    fn pp_is_observed_then_tracked_then_healed() {
        let s = run(vec![
            starter(),
            GameEvent::MovePpObserved {
                slot: 0,
                move_slot: 0,
                cur: 30,
                max: 35,
            },
            GameEvent::MoveUsed {
                slot: 0,
                move_slot: 0,
            },
        ]);
        let pp = &s.party.value.as_ref().unwrap()[0].moves[0]
            .as_ref()
            .unwrap()
            .pp;
        assert_eq!(pp.value, Some((29, 35)));
        assert_eq!(pp.source, KnowledgeSource::Tracked);
        assert_eq!(pp.last_verified_frame, Some(2));
        let healed = run(vec![
            starter(),
            GameEvent::MoveUsed {
                slot: 0,
                move_slot: 0,
            },
            GameEvent::Healed,
        ]);
        let pp = &healed.party.value.as_ref().unwrap()[0].moves[0]
            .as_ref()
            .unwrap()
            .pp;
        assert_eq!(pp.value, Some((35, 35)));
    }

    #[test]
    fn a_heal_does_not_invent_an_unknown_hp_total() {
        let healed = run(vec![starter(), GameEvent::Healed]);
        let hp = &healed.party.value.as_ref().unwrap()[0].hp;
        assert_eq!(hp.value, None);
    }

    #[test]
    fn replacing_a_move_keeps_the_slot() {
        let s = run(vec![
            starter(),
            GameEvent::MoveReplaced {
                slot: 0,
                move_slot: 0,
                old: "MOVE_TACKLE".into(),
                new: "MOVE_VINE_WHIP".into(),
                max_pp: 10,
            },
        ]);
        let m = s.party.value.as_ref().unwrap()[0].moves[0]
            .as_ref()
            .unwrap();
        assert_eq!(m.mv.value.as_deref(), Some("MOVE_VINE_WHIP"));
        assert_eq!(m.pp.value, Some((10, 10)));
    }

    #[test]
    fn observed_moves_keep_known_pp_for_the_same_move() {
        let s = run(vec![
            starter(),
            GameEvent::MovePpObserved {
                slot: 0,
                move_slot: 0,
                cur: 12,
                max: 35,
            },
            GameEvent::MovesObserved {
                slot: 0,
                moves: vec!["MOVE_TACKLE".into(), "MOVE_GROWL".into()],
            },
        ]);
        let mon = &s.party.value.as_ref().unwrap()[0];
        assert_eq!(mon.moves[0].as_ref().unwrap().pp.value, Some((12, 35)));
        assert_eq!(mon.moves[1].as_ref().unwrap().pp.value, None);
        assert!(mon.moves[2].is_none());
    }

    #[test]
    fn out_of_pp_and_evolution_are_observed() {
        let s = run(vec![
            starter(),
            GameEvent::MoveOutOfPp {
                slot: 0,
                move_slot: 0,
            },
            GameEvent::Evolved {
                slot: 0,
                species: "SPECIES_IVYSAUR".into(),
            },
        ]);
        let mon = &s.party.value.as_ref().unwrap()[0];
        assert_eq!(mon.moves[0].as_ref().unwrap().pp.value, Some((0, 35)));
        assert_eq!(mon.species.value.as_deref(), Some("SPECIES_IVYSAUR"));
        assert_eq!(mon.species.source, KnowledgeSource::Observed);
    }

    use crate::{BoxMon, Pocket, SavedKnowledge};

    #[test]
    fn tracked_deltas_need_a_known_base() {
        let s = run(vec![GameEvent::ItemsChanged {
            pocket: Pocket::PokeBalls,
            item: "ITEM_POKE_BALL".into(),
            delta: 5,
            reason: "bought".into(),
        }]);
        assert_eq!(s.bag.pockets[&Pocket::PokeBalls], Knowledge::unknown());
        let s = run(vec![
            GameEvent::PocketObserved {
                pocket: Pocket::PokeBalls,
                items: vec![("ITEM_POKE_BALL".into(), 3)],
            },
            GameEvent::ItemsChanged {
                pocket: Pocket::PokeBalls,
                item: "ITEM_POKE_BALL".into(),
                delta: -3,
                reason: "thrown".into(),
            },
            GameEvent::ItemsChanged {
                pocket: Pocket::PokeBalls,
                item: "ITEM_GREAT_BALL".into(),
                delta: 2,
                reason: "bought".into(),
            },
        ]);
        let balls = &s.bag.pockets[&Pocket::PokeBalls];
        assert_eq!(
            balls.value,
            Some(vec![("ITEM_GREAT_BALL".to_owned(), 2u16)])
        );
        assert!(balls.is_stale());
        let s = run(vec![
            GameEvent::MoneyObserved { amount: 4600 },
            GameEvent::MoneyChanged {
                delta: -1000,
                reason: "bought".into(),
            },
        ]);
        assert_eq!(s.money.value, Some(3600));
        assert!(s.money.is_stale());
    }

    fn boxed(slot: u8, species: &str) -> BoxMon {
        BoxMon {
            slot,
            species: Knowledge::observed(species.into(), 1),
            level: Knowledge::observed(5, 1),
            nickname: Knowledge::unknown(),
            training: Default::default(),
        }
    }

    #[test]
    fn pc_moves_mons_between_party_and_boxes() {
        let s = run(vec![
            starter(),
            GameEvent::BoxObserved {
                box_index: 0,
                mons: vec![boxed(0, "SPECIES_PIDGEY")],
            },
            GameEvent::MonWithdrawn {
                box_index: 0,
                box_slot: 0,
            },
        ]);
        assert_eq!(s.pc.boxes[0].value.as_ref().unwrap().len(), 0);
        let party = s.party.value.as_ref().unwrap();
        assert_eq!(party[1].species.value.as_deref(), Some("SPECIES_PIDGEY"));
        let s = run(vec![
            starter(),
            GameEvent::BoxObserved {
                box_index: 0,
                mons: vec![],
            },
            GameEvent::SentToPc {
                box_index: Some(0),
                mon: Box::new(boxed(0, "SPECIES_RATTATA")),
            },
            GameEvent::SpeciesCaught {
                species: "SPECIES_RATTATA".into(),
            },
        ]);
        assert_eq!(s.pc.boxes[0].value.as_ref().unwrap().len(), 1);
        assert_eq!(s.pokedex.caught["SPECIES_RATTATA"].value, Some(true));
        assert_eq!(s.pokedex.seen["SPECIES_RATTATA"].value, Some(true));
    }

    #[test]
    fn pokedex_counts_are_observed_from_the_trainer_card() {
        let s = run(vec![starter()]);
        assert_eq!(s.pokedex.counts, Knowledge::unknown());
        let s = run(vec![
            starter(),
            GameEvent::SpeciesCaught {
                species: "SPECIES_RATTATA".into(),
            },
            GameEvent::PokedexCountObserved {
                seen: Some(12),
                caught: 7,
            },
        ]);
        assert_eq!(
            s.pokedex.counts.value,
            Some(PokedexCounts {
                seen: Some(12),
                caught: 7
            })
        );
        assert_eq!(s.pokedex.counts.source, KnowledgeSource::Observed);
        // The per-species map is untouched: it is a lower bound, not the total.
        assert_eq!(s.pokedex.caught.len(), 1);
        // The card gives the caught total alone; catches after a read move
        // the totals (a repeat of a known species doesn't).
        let s = run(vec![
            starter(),
            GameEvent::PokedexCountObserved {
                seen: None,
                caught: 5,
            },
            GameEvent::SpeciesCaught {
                species: "SPECIES_SPEAROW".into(),
            },
            GameEvent::SpeciesCaught {
                species: "SPECIES_SPEAROW".into(),
            },
            GameEvent::SpeciesSeen {
                species: "SPECIES_ZUBAT".into(),
            },
        ]);
        assert_eq!(
            s.pokedex.counts.value,
            Some(PokedexCounts {
                seen: None,
                caught: 6
            })
        );
        assert_eq!(s.pokedex.counts.source, KnowledgeSource::Tracked);
        // Round trip through the saved knowledge; older files load as unknown.
        let saved = s.saved_knowledge();
        let json = serde_json::to_string(&saved).unwrap();
        let back: crate::SavedKnowledge = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pokedex.counts, saved.pokedex.counts);
        let old: crate::Pokedex = serde_json::from_str(r#"{"caught":{},"seen":{}}"#).unwrap();
        assert_eq!(old.counts, Knowledge::unknown());
    }

    #[test]
    fn checkpoint_restore_replaces_knowledge() {
        let saved = run(vec![starter()]).saved_knowledge();
        let s = run(vec![
            starter(),
            GameEvent::MoveUsed {
                slot: 0,
                move_slot: 0,
            },
            GameEvent::MoneyObserved { amount: 1 },
            GameEvent::CheckpointRestored {
                knowledge: Box::new(saved.clone()),
            },
        ]);
        assert_eq!(s.saved_knowledge(), saved);
        assert_eq!(SavedKnowledge::default().money, Knowledge::unknown());
    }
}
