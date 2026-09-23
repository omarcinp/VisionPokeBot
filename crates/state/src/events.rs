use pokebot_core::ControllerCommand;
use serde::{Deserialize, Serialize};

use crate::{Gender, Observation, PlayerPose, ScreenState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameEvent {
    /// The screen classification changed and held for the confirmation window.
    ScreenChanged {
        from: ScreenState,
        to: ScreenState,
        detector: String,
    },
    /// The bot sent input. Only video can confirm what it did.
    InputIssued {
        command_id: u64,
        command: ControllerCommand,
    },
    /// A goal changed phase (emitted by the agent).
    GoalProgress {
        goal: String,
        phase: String,
        detail: String,
    },
    /// A goal finished; `success` is false if it was abandoned.
    GoalFinished {
        goal: String,
        success: bool,
        detail: String,
    },
    GenderChosen {
        gender: Gender,
    },
    PlayerNamed {
        name: String,
    },
    RivalNamed {
        name: String,
    },
    /// Control of the character was confirmed (Start menu opened and closed).
    ControlConfirmed,
    GameSaved,
    BattleStarted,
    BattleEnded,
    /// The player was located on the map (first fix, or after losing track).
    PlayerLocated {
        pose: PlayerPose,
    },
    /// The player's tile changed within a map.
    PlayerMoved {
        from: PlayerPose,
        to: PlayerPose,
    },
    /// The player is now on another map (warp or connection).
    MapChanged {
        from: PlayerPose,
        to: PlayerPose,
    },
    /// The video source skipped frame ids (the bot read too slowly or the
    /// device dropped frames).
    FramesDropped {
        missing: u64,
    },
}

/// An event and the frame it was derived at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    pub frame_id: u64,
    pub event: GameEvent,
}

/// Turns the observation stream into events. Deterministic: the same
/// observations always produce the same events.
///
/// A new screen classification must persist for `confirm_frames` consecutive
/// frames before `ScreenChanged` is emitted, so single-frame flicker during
/// animations does not reach the state.
#[derive(Debug, Clone)]
pub struct EventExtractor {
    confirm_frames: u32,
    confirmed: ScreenState,
    candidate: Option<(ScreenState, u32)>,
    last_frame_id: Option<u64>,
    pose: Option<PlayerPose>,
    pose_candidate: Option<(PlayerPose, u32)>,
}

impl EventExtractor {
    pub fn new(confirm_frames: u32) -> Self {
        Self {
            confirm_frames: confirm_frames.max(1),
            confirmed: ScreenState::Unknown,
            candidate: None,
            last_frame_id: None,
            pose: None,
            pose_candidate: None,
        }
    }

    pub fn observe(&mut self, observation: &Observation) -> Vec<EventRecord> {
        let frame_id = observation.frame_id;
        let mut events = Vec::new();
        if let Some(last) = self.last_frame_id {
            let missing = frame_id.saturating_sub(last + 1);
            if missing > 0 {
                events.push(GameEvent::FramesDropped { missing });
            }
        }
        self.last_frame_id = Some(frame_id);

        let seen = observation.screen.value;
        if seen == self.confirmed {
            self.candidate = None;
        } else {
            let count = match self.candidate {
                Some((screen, count)) if screen == seen => count + 1,
                _ => 1,
            };
            if count >= self.confirm_frames {
                events.push(GameEvent::ScreenChanged {
                    from: self.confirmed,
                    to: seen,
                    detector: observation.screen.detector.clone(),
                });
                self.confirmed = seen;
                self.candidate = None;
            } else {
                self.candidate = Some((seen, count));
            }
        }
        // A pose must be seen on consecutive frames (walking frames between
        // tiles are not located) before it becomes an event.
        if let Some(seen) = observation.player.as_ref().map(|p| &p.pose) {
            if self.pose.as_ref() == Some(seen) {
                self.pose_candidate = None;
            } else {
                let count = match &self.pose_candidate {
                    Some((pose, count)) if pose == seen => count + 1,
                    _ => 1,
                };
                if count >= self.confirm_frames {
                    events.push(match self.pose.take() {
                        None => GameEvent::PlayerLocated { pose: seen.clone() },
                        Some(from) if from.map != seen.map => GameEvent::MapChanged {
                            from,
                            to: seen.clone(),
                        },
                        Some(from) => GameEvent::PlayerMoved {
                            from,
                            to: seen.clone(),
                        },
                    });
                    self.pose = Some(seen.clone());
                    self.pose_candidate = None;
                } else {
                    self.pose_candidate = Some((seen.clone(), count));
                }
            }
        }
        events
            .into_iter()
            .map(|event| EventRecord { frame_id, event })
            .collect()
    }

    pub fn input(&self, command_id: u64, command: &ControllerCommand) -> EventRecord {
        EventRecord {
            frame_id: self.last_frame_id.unwrap_or(0),
            event: GameEvent::InputIssued {
                command_id,
                command: command.clone(),
            },
        }
    }
}

impl Default for EventExtractor {
    fn default() -> Self {
        Self::new(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameMetrics, Observed};

    fn obs(frame_id: u64, screen: ScreenState) -> Observation {
        Observation::bare(
            frame_id,
            Observed {
                value: screen,
                detector: "test".into(),
            },
            FrameMetrics::default(),
        )
    }

    #[test]
    fn screen_change_needs_confirmation() {
        let mut extractor = EventExtractor::new(2);
        assert!(extractor.observe(&obs(0, ScreenState::Unknown)).is_empty());
        assert!(extractor
            .observe(&obs(1, ScreenState::Transition))
            .is_empty());
        assert!(extractor.observe(&obs(2, ScreenState::Unknown)).is_empty());
        assert!(extractor
            .observe(&obs(3, ScreenState::Transition))
            .is_empty());
        let events = extractor.observe(&obs(4, ScreenState::Transition));
        assert_eq!(
            events,
            vec![EventRecord {
                frame_id: 4,
                event: GameEvent::ScreenChanged {
                    from: ScreenState::Unknown,
                    to: ScreenState::Transition,
                    detector: "test".into()
                }
            }]
        );
    }

    #[test]
    fn reports_dropped_frames() {
        let mut extractor = EventExtractor::new(1);
        extractor.observe(&obs(10, ScreenState::Unknown));
        let events = extractor.observe(&obs(13, ScreenState::Unknown));
        assert_eq!(events[0].event, GameEvent::FramesDropped { missing: 2 });
    }
}
