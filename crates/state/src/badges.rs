//! Badge identities from the card's eight flags; totals from the SAVE panel.
use crate::{GameState, Knowledge, KnowledgeSource};

pub const BADGES: [(&str, &str); 8] = [
    ("FLAG_BADGE01_GET", "BOULDERBADGE"),
    ("FLAG_BADGE02_GET", "CASCADEBADGE"),
    ("FLAG_BADGE03_GET", "THUNDERBADGE"),
    ("FLAG_BADGE04_GET", "RAINBOWBADGE"),
    ("FLAG_BADGE05_GET", "SOULBADGE"),
    ("FLAG_BADGE06_GET", "MARSHBADGE"),
    ("FLAG_BADGE07_GET", "VOLCANOBADGE"),
    ("FLAG_BADGE08_GET", "EARTHBADGE"),
];

/// Keep the viewer's badge list and the planner's flags in agreement.
pub(crate) fn sync(state: &mut GameState, frame: u64) {
    let flags: Vec<_> = BADGES
        .iter()
        .map(|(flag, _)| state.world.flags.get(*flag))
        .collect();
    if flags.iter().all(|k| k.is_none_or(|k| k.value.is_none())) {
        return;
    }
    let earned: Vec<String> = BADGES
        .iter()
        .zip(&flags)
        .filter(|(_, k)| k.is_some_and(|k| k.value == Some(true)))
        .map(|((_, name), _)| (*name).into())
        .collect();
    let source = if flags
        .iter()
        .flatten()
        .all(|k| k.source == KnowledgeSource::Observed)
    {
        KnowledgeSource::Observed
    } else {
        KnowledgeSource::Derived
    };
    state.progression.badges = Knowledge {
        value: Some(earned.clone()),
        source,
        last_verified_frame: Some(frame),
    };
    if flags.iter().all(|k| k.is_some_and(|k| k.value.is_some())) {
        state.progression.badge_count = Knowledge::derived(earned.len() as u8, frame);
        state.progression.badge_inference = Knowledge::unknown();
    }
}

pub(crate) fn observe_count(state: &mut GameState, count: u8, frame: u64) {
    if count > 8 {
        return;
    }
    let confirmed = state.progression.badges.value.as_ref().map_or(0, Vec::len);
    if usize::from(count) < confirmed {
        // A different/older save: the old identities no longer describe it.
        for (flag, _) in BADGES {
            state.world.flags.remove(flag);
        }
        state.progression.badges = Knowledge::unknown();
    } else if usize::from(count) > confirmed {
        // More badges were obtained since the card was read. Negative flags
        // are stale; the count cannot say which of them changed.
        for (flag, _) in BADGES {
            if state
                .world
                .flags
                .get(flag)
                .is_some_and(|k| k.value == Some(false))
            {
                state.world.flags.remove(flag);
            }
        }
    }
    // Zero and eight are exact regardless of gym order. The intermediate
    // count must never become "the first N flags" in the planner.
    if count == 0 || count == 8 {
        for (flag, _) in BADGES {
            state
                .world
                .flags
                .insert(flag.into(), Knowledge::derived(count == 8, frame));
        }
        sync(state, frame);
    }
    state.progression.badge_count = Knowledge::observed(count, frame);
    state.progression.badge_inference = Knowledge::unknown();
    // Normal FireRed progression: Brock first; Giovanni's gym opens after
    // the other gyms. Keep these as visible assumptions until the card is
    // read, and never contradict an already confirmed earned identity.
    if count == 1 || count == 7 {
        let inferred: Vec<String> = BADGES[..usize::from(count)]
            .iter()
            .map(|(_, name)| (*name).into())
            .collect();
        let confirmed = state
            .progression
            .badges
            .value
            .as_deref()
            .unwrap_or_default();
        if confirmed.len() < usize::from(count) && confirmed.iter().all(|b| inferred.contains(b)) {
            state.progression.badge_inference = Knowledge {
                value: Some(inferred),
                source: KnowledgeSource::Assumed,
                last_verified_frame: Some(frame),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefaultReducer, EventRecord, GameEvent, StateReducer};
    fn card(state: &GameState, badges: &[usize], frame: u64) -> GameState {
        let events: Vec<_> = BADGES
            .iter()
            .enumerate()
            .map(|(i, (flag, _))| EventRecord {
                frame_id: frame,
                event: GameEvent::FlagObserved {
                    flag: (*flag).into(),
                    value: badges.contains(&i),
                },
            })
            .collect();
        DefaultReducer.reduce(state, &events)
    }
    #[test]
    fn a_new_total_invalidates_stale_negatives_and_survives_a_checkpoint() {
        let mut state = card(&GameState::default(), &[0], 1);
        observe_count(&mut state, 7, 2);
        assert_eq!(state.world.flags["FLAG_BADGE01_GET"].value, Some(true));
        assert!(!state.world.flags.contains_key("FLAG_BADGE02_GET"));
        let restored = DefaultReducer.reduce(
            &GameState::default(),
            &[EventRecord {
                frame_id: 100,
                event: GameEvent::CheckpointRestored {
                    knowledge: Box::new(state.saved_knowledge()),
                },
            }],
        );
        assert_eq!(restored.progression.badge_count.value, Some(7));
        assert_eq!(
            restored.progression.badge_inference,
            state.progression.badge_inference
        );
        observe_count(&mut state, 0, 3);
        assert!(state.progression.badges.value.as_ref().unwrap().is_empty());
    }

    #[test]
    fn exact_card_identities_and_negative_flags_update_the_display() {
        let mut state = GameState::default();
        observe_count(&mut state, 7, 1);
        assert_eq!(
            state.progression.badge_inference.source,
            KnowledgeSource::Assumed
        );
        assert!(
            state.world.flags.is_empty(),
            "assumptions must not satisfy planner gates"
        );
        let state = card(&state, &[0, 3, 4], 2);
        assert_eq!(
            state.progression.badges.value.unwrap(),
            vec!["BOULDERBADGE", "RAINBOWBADGE", "SOULBADGE"]
        );
        assert_eq!(state.progression.badge_count.value, Some(3));
        assert!(state.progression.badge_inference.value.is_none());
        let cleared = card(&GameState::default(), &[], 3);
        assert_eq!(cleared.progression.badges.value, Some(vec![]));
        assert_eq!(cleared.progression.badge_count.value, Some(0));
    }
    #[test]
    fn totals_do_not_invent_an_order_or_override_confirmed_identities() {
        for count in 0..=8 {
            let mut state = GameState::default();
            observe_count(&mut state, count, 9);
            assert_eq!(state.progression.badge_count, Knowledge::observed(count, 9));
            match count {
                0 | 8 => assert_eq!(
                    state.progression.badges.value.unwrap().len(),
                    usize::from(count)
                ),
                1 | 7 => {
                    assert_eq!(
                        state.progression.badge_inference.value.unwrap().len(),
                        usize::from(count)
                    );
                    assert!(state.progression.badges.value.is_none());
                }
                _ => assert!(state.progression.badge_inference.value.is_none()),
            }
        }
        let mut state = card(&GameState::default(), &[7], 1);
        observe_count(&mut state, 1, 2);
        assert!(state.progression.badge_inference.value.is_none());
        assert_eq!(state.progression.badges.value.unwrap(), vec!["EARTHBADGE"]);
        let mut state = GameState::default();
        observe_count(&mut state, 9, 3);
        assert!(state.progression.badge_count.value.is_none());
    }
}
