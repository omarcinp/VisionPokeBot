//! Applying knowledge events (party, bag, money, PC, Pokédex) to the state.
//! Pure: the same events always give the same state.

use crate::{GameEvent, GameState, Knowledge, KnowledgeSource, MoveSlot, PartyMon, Status};

/// Applies `event` if it is a knowledge event; returns whether it was one.
pub(crate) fn apply(state: &mut GameState, frame: u64, event: &GameEvent) -> bool {
    match event {
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
                m.level = Knowledge::observed(*v, frame);
            }
            if let Some(v) = hp {
                m.hp = Knowledge::observed(*v, frame);
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
            member(state, *slot, KnowledgeSource::Observed, frame).species =
                Knowledge::observed(species.clone(), frame);
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
        GameEvent::MoneyObserved { amount } => state.money = Knowledge::observed(*amount, frame),
        GameEvent::MoneyChanged { delta, .. } => {
            if let Some(m) = state.money.value {
                let new = (i64::from(m) + delta).clamp(0, i64::from(u32::MAX)) as u32;
                state.money = Knowledge::tracked(new, state.money.last_verified_frame);
            }
        }
        GameEvent::BoxObserved { box_index, mons } => {
            if let Some(b) = state.pc.boxes.get_mut(usize::from(*box_index)) {
                *b = Knowledge::observed(mons.clone(), frame);
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
                    list.push(mon.clone());
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
            }
        }
        GameEvent::SpeciesSeen { species } => {
            state
                .pokedex
                .seen
                .insert(species.clone(), Knowledge::observed(true, frame));
        }
        GameEvent::SpeciesCaught { species } => {
            state
                .pokedex
                .seen
                .insert(species.clone(), Knowledge::observed(true, frame));
            state
                .pokedex
                .caught
                .insert(species.clone(), Knowledge::observed(true, frame));
        }
        GameEvent::CheckpointRestored { knowledge } => {
            let k = (**knowledge).clone();
            state.party = k.party;
            state.bag = k.bag;
            state.money = k.money;
            state.pc = k.pc;
            state.pokedex = k.pokedex;
        }
        _ => return false,
    }
    true
}

/// The party member in `slot`, creating unknown members up to it. The party
/// list's provenance becomes `source` when the list itself was unknown.
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
                mon: boxed(0, "SPECIES_RATTATA"),
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
