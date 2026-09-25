use pokebot_core::ControllerCommand;
use serde::{Deserialize, Serialize};

use crate::{
    BoxMon, Direction, Gender, ItemList, Observation, PartyMon, PlayerPose, Pocket, SavedKnowledge,
    ScreenState, Status,
};

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
    /// A party member known without seeing it (story, legacy data).
    PartyMonDerived {
        slot: u8,
        mon: Box<PartyMon>,
    },
    /// Fields of a party member read from the screen (`None` = not shown).
    PartyObserved {
        slot: u8,
        species: Option<String>,
        nickname: Option<String>,
        level: Option<u8>,
        hp: Option<(u16, u16)>,
        status: Option<Status>,
        held_item: Option<Option<String>>,
    },
    /// A member's complete move list, in menu order.
    MovesObserved {
        slot: u8,
        moves: Vec<String>,
    },
    MovePpObserved {
        slot: u8,
        move_slot: u8,
        cur: u8,
        max: u8,
    },
    /// A move was used (one PP spent).
    MoveUsed {
        slot: u8,
        move_slot: u8,
    },
    /// "X learned M!" into an empty slot.
    MoveLearned {
        slot: u8,
        move_slot: u8,
        mv: String,
        max_pp: u8,
    },
    /// "X forgot O … learned N!"
    MoveReplaced {
        slot: u8,
        move_slot: u8,
        old: String,
        new: String,
        max_pp: u8,
    },
    /// "There's no PP left for this move!"
    MoveOutOfPp {
        slot: u8,
        move_slot: u8,
    },
    Evolved {
        slot: u8,
        species: String,
    },
    /// The nurse restored the party.
    Healed,
    /// Items gained (+) or spent (−) as told by text or a confirmed action.
    ItemsChanged {
        pocket: Pocket,
        item: String,
        delta: i32,
        reason: String,
    },
    PocketObserved {
        pocket: Pocket,
        items: ItemList,
    },
    MoneyObserved {
        amount: u32,
    },
    MoneyChanged {
        delta: i64,
        reason: String,
    },
    BoxObserved {
        box_index: u8,
        mons: Vec<BoxMon>,
    },
    PcItemsObserved {
        items: ItemList,
    },
    /// A caught Pokémon went to the PC ("transferred to BILL's PC").
    SentToPc {
        box_index: Option<u8>,
        mon: BoxMon,
    },
    MonDeposited {
        party_slot: u8,
        box_index: u8,
    },
    MonWithdrawn {
        box_index: u8,
        box_slot: u8,
    },
    SpeciesSeen {
        species: String,
    },
    SpeciesCaught {
        species: String,
    },
    /// The Pokédex totals as the Pokédex shows them, or the caught total
    /// alone from the Trainer Card.
    PokedexCountObserved {
        seen: Option<u16>,
        caught: u16,
    },
    /// A shiny appeared (for the log and the catch policy).
    ShinySeen {
        species: String,
    },
    /// "RED received the BOULDERBADGE from BROCK." (decomp name, e.g.
    /// `BOULDERBADGE`).
    BadgeEarned {
        badge: String,
    },
    /// The knowledge stored with the save that was just loaded.
    CheckpointRestored {
        knowledge: Box<SavedKnowledge>,
    },
    /// A flag's value read from the screen (trainer card, a dialogue branch
    /// that implies it, an NPC at its tile).
    FlagObserved {
        flag: String,
        value: bool,
    },
    /// A flag a script path is known to have set or cleared.
    FlagTracked {
        flag: String,
        value: bool,
    },
    VarObserved {
        var: String,
        value: u16,
    },
    VarTracked {
        var: String,
        value: u16,
    },
    /// The player was on `map` (a transition seen, or the Fly map lit).
    MapVisited {
        map: String,
    },
    /// Where a blackout now lands (the last Pokémon Center used).
    RespawnSet {
        map: String,
        x: i32,
        y: i32,
    },
    /// An NPC was seen at a tile of `map`.
    NpcSeen {
        map: String,
        local_id: u32,
        x: i32,
        y: i32,
        facing: Direction,
    },
    /// An NPC's scripted tile was looked at and it was not there.
    NpcAbsent {
        map: String,
        local_id: u32,
    },
    /// The agent ran path `path` of the compiled script `script` to
    /// completion. The reducer only records it: the agent emits the
    /// `FlagTracked`/`VarTracked`/`ItemsChanged` the path's effects imply,
    /// so the reducer needs no world data.
    ScriptPathRun {
        script: String,
        path: usize,
    },
    /// An intent failed in a way that replanning it would repeat; forgotten
    /// on `CheckpointRestored`.
    IntentInfeasible {
        intent: String,
    },
    /// A tile the walker found blocked (an NPC met by bumping): the
    /// session's legs route around it (log only; the agent keeps the set).
    TileBlocked {
        map: String,
        x: i32,
        y: i32,
    },
    /// A `TileBlocked` taken back: something else stopped the step.
    TileUnblocked {
        map: String,
        x: i32,
        y: i32,
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
