use crate::{
    EventRecord, GameEvent, GameState, GoalStatus, Knowledge, ScreenState, SynchronizationState,
};

pub trait StateReducer {
    fn reduce(&self, previous: &GameState, events: &[EventRecord]) -> GameState;
}

/// Pure reducer: `(state, events) → state`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultReducer;

impl StateReducer for DefaultReducer {
    fn reduce(&self, previous: &GameState, events: &[EventRecord]) -> GameState {
        let mut state = previous.clone();
        for record in events {
            if crate::reduce_knowledge::apply(&mut state, record.frame_id, &record.event) {
                continue;
            }
            match &record.event {
                // Classifying a screen as Unknown means we no longer know it.
                GameEvent::ScreenChanged {
                    to: ScreenState::Unknown,
                    ..
                } => state.screen = Knowledge::unknown(),
                GameEvent::ScreenChanged { to, .. } => {
                    state.screen = Knowledge::observed(*to, record.frame_id);
                }
                GameEvent::InputIssued { command, .. } => {
                    state.input.commands_issued += 1;
                    state.input.last_command = Some(command.clone());
                    state.input.last_command_frame = Some(record.frame_id);
                }
                GameEvent::FramesDroppedByCard { missing, .. } => {
                    state.frames_dropped_by_card += missing;
                }
                GameEvent::FramesDroppedByProcessing { missing, .. } => {
                    state.frames_dropped_by_processing += missing;
                }
                GameEvent::GoalProgress {
                    goal,
                    phase,
                    detail,
                } => {
                    state.active_goal = Some(GoalStatus {
                        goal: goal.clone(),
                        phase: phase.clone(),
                        detail: detail.clone(),
                    });
                }
                GameEvent::GoalFinished { .. } => state.active_goal = None,
                // The agent saw these choices confirmed on screen.
                GameEvent::GenderChosen { gender } => {
                    state.progression.gender = Knowledge::observed(*gender, record.frame_id)
                }
                GameEvent::PlayerNamed { name } => {
                    state.progression.player_name =
                        Knowledge::observed(name.clone(), record.frame_id)
                }
                GameEvent::RivalNamed { name } => {
                    state.progression.rival_name =
                        Knowledge::observed(name.clone(), record.frame_id)
                }
                GameEvent::PlayerLocated { pose: to }
                | GameEvent::PlayerMoved { to, .. }
                | GameEvent::MapChanged { to, .. } => {
                    state.player.pose = Knowledge::observed(to.clone(), record.frame_id);
                }
                GameEvent::BadgeEarned { badge } => {
                    let mut badges = state.progression.badges.value.take().unwrap_or_default();
                    if !badges.contains(badge) {
                        badges.push(badge.clone());
                    }
                    state.progression.badges = Knowledge::observed(badges, record.frame_id);
                }
                GameEvent::GameSaved => {}
                GameEvent::BattleStarted => state.in_battle = true,
                GameEvent::BattleEnded => state.in_battle = false,
                GameEvent::ControlConfirmed => {
                    state.progression.in_control = Knowledge::observed(true, record.frame_id);
                    state.synchronization = SynchronizationState::Synchronized;
                }
                GameEvent::PartyMonDerived { .. }
                | GameEvent::PartyAudited { .. }
                | GameEvent::PartyObserved { .. }
                | GameEvent::MovesObserved { .. }
                | GameEvent::MovePpObserved { .. }
                | GameEvent::MoveUsed { .. }
                | GameEvent::MoveLearned { .. }
                | GameEvent::MoveReplaced { .. }
                | GameEvent::MoveOutOfPp { .. }
                | GameEvent::Evolved { .. }
                | GameEvent::Healed
                | GameEvent::ItemsChanged { .. }
                | GameEvent::PocketObserved { .. }
                | GameEvent::MoneyObserved { .. }
                | GameEvent::MoneyChanged { .. }
                | GameEvent::BoxObserved { .. }
                | GameEvent::PcItemsObserved { .. }
                | GameEvent::SentToPc { .. }
                | GameEvent::MonDeposited { .. }
                | GameEvent::MonWithdrawn { .. }
                | GameEvent::SpeciesSeen { .. }
                | GameEvent::SpeciesCaught { .. }
                | GameEvent::PokedexCountObserved { .. }
                | GameEvent::ShinySeen { .. }
                | GameEvent::CheckpointRestored { .. }
                | GameEvent::FlagObserved { .. }
                | GameEvent::FlagTracked { .. }
                | GameEvent::VarObserved { .. }
                | GameEvent::VarTracked { .. }
                | GameEvent::MapVisited { .. }
                | GameEvent::RespawnSet { .. }
                | GameEvent::NpcSeen { .. }
                | GameEvent::NpcAbsent { .. }
                | GameEvent::ScriptPathRun { .. }
                | GameEvent::IntentInfeasible { .. }
                | GameEvent::TileBlocked { .. }
                | GameEvent::TileUnblocked { .. }
                | GameEvent::WhitedOut => {
                    unreachable!("handled by reduce_knowledge")
                }
            }
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use pokebot_core::{Button, ControllerCommand};

    use super::*;
    use crate::KnowledgeSource;

    #[test]
    fn replaying_events_rebuilds_state() {
        let events = vec![
            EventRecord {
                frame_id: 5,
                event: GameEvent::ScreenChanged {
                    from: ScreenState::Unknown,
                    to: ScreenState::Transition,
                    detector: "uniform".into(),
                },
            },
            EventRecord {
                frame_id: 6,
                event: GameEvent::InputIssued {
                    command_id: 0,
                    command: ControllerCommand::Press(Button::A),
                },
            },
            EventRecord {
                frame_id: 9,
                event: GameEvent::FramesDroppedByProcessing {
                    missing: 3,
                    seen: 1,
                    window_ms: 1000,
                },
            },
        ];
        let reducer = DefaultReducer;
        let all_at_once = reducer.reduce(&GameState::default(), &events);
        let one_by_one = events.iter().fold(GameState::default(), |state, event| {
            reducer.reduce(&state, std::slice::from_ref(event))
        });
        assert_eq!(all_at_once, one_by_one);
        assert_eq!(all_at_once.screen.value, Some(ScreenState::Transition));
        assert_eq!(all_at_once.screen.source, KnowledgeSource::Observed);
        assert_eq!(all_at_once.screen.last_verified_frame, Some(5));
        assert_eq!(all_at_once.input.commands_issued, 1);
        assert_eq!(all_at_once.frames_dropped_by_processing, 3);

        let back_to_unknown = reducer.reduce(
            &all_at_once,
            &[EventRecord {
                frame_id: 10,
                event: GameEvent::ScreenChanged {
                    from: ScreenState::Transition,
                    to: ScreenState::Unknown,
                    detector: "none".into(),
                },
            }],
        );
        assert_eq!(back_to_unknown.screen, Knowledge::unknown());

        let json = serde_json::to_string(&all_at_once).unwrap();
        assert_eq!(
            serde_json::from_str::<GameState>(&json).unwrap(),
            all_at_once
        );
    }
}
