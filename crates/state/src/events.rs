use std::time::{Duration, Instant};

use pokebot_core::{CapturedFrame, ControllerCommand};
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
    /// Over a window of `window_ms`, the video device delivered `delivered`
    /// frames and never delivered `missing` (driver drops, empty buffers),
    /// too few for the [`FrameDropPolicy`]. Faster windows are not reported.
    FramesDroppedByCard {
        missing: u64,
        delivered: u64,
        window_ms: u64,
    },
    /// Over a window of `window_ms`, the bot processed `seen` of the frames
    /// the device delivered and `missing` were replaced before it read them,
    /// too many for the [`FrameDropPolicy`].
    FramesDroppedByProcessing {
        missing: u64,
        seen: u64,
        window_ms: u64,
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
    /// The party's last Pokémon fainted (the white-out screens, or HP 0 on
    /// the HUD): the lead is at 0 HP. The game then heals the party and
    /// puts the player at the respawn spot (`Healed` and `PlayerLocated`
    /// follow from the agent).
    WhitedOut,
}

/// An event and the frame it was derived at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    pub frame_id: u64,
    pub event: GameEvent,
}

/// When skipped frames are worth an event. Frames are summed over a window
/// and reported only when the window is slow, per cause: a card that
/// delivers half its frames or a loop that reads every other one is harmless
/// and would otherwise log on every frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameDropPolicy {
    /// Report a window where more than this share of the frames were
    /// dropped: of the frame ids by the card, of the delivered frames by
    /// processing.
    pub max_drop_percent: u32,
    /// Report a window where fewer frames per second than this were
    /// delivered (card) or, with frames dropped, processed (processing).
    pub min_fps: f64,
    pub window: Duration,
}

impl FrameDropPolicy {
    fn exceeds_percent(&self, missing: u64, kept: u64) -> bool {
        missing * 100 > u64::from(self.max_drop_percent) * (missing + kept)
    }

    fn below_min_fps(&self, frames: u64, span: Duration) -> bool {
        (frames as f64) / span.as_secs_f64() < self.min_fps
    }
}

impl Default for FrameDropPolicy {
    fn default() -> Self {
        Self {
            max_drop_percent: 60,
            min_fps: 10.0,
            window: Duration::from_secs(1),
        }
    }
}

/// How a frame arrived: when, and its place in the source's frame ids and
/// in the frames it delivered (see [`CapturedFrame::delivered`]).
#[derive(Debug, Clone, Copy)]
pub struct FrameArrival {
    pub frame_id: u64,
    pub delivered: u64,
    pub captured_at: Instant,
}

impl From<&CapturedFrame> for FrameArrival {
    fn from(frame: &CapturedFrame) -> Self {
        Self {
            frame_id: frame.frame_id,
            delivered: frame.delivered,
            captured_at: frame.captured_at,
        }
    }
}

/// Frames processed and dropped, by cause, since `start`.
#[derive(Debug, Clone)]
struct DropWindow {
    start: Instant,
    seen: u64,
    dropped_by_card: u64,
    dropped_by_processing: u64,
}

impl DropWindow {
    fn new(start: Instant) -> Self {
        Self {
            start,
            seen: 0,
            dropped_by_card: 0,
            dropped_by_processing: 0,
        }
    }
}

/// Turns the observation stream into events. Deterministic: the same
/// observations and capture times always produce the same events.
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
    last_arrival: Option<FrameArrival>,
    drop_policy: FrameDropPolicy,
    drop_window: Option<DropWindow>,
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
            last_arrival: None,
            drop_policy: FrameDropPolicy::default(),
            drop_window: None,
            pose: None,
            pose_candidate: None,
        }
    }

    pub fn with_drop_policy(mut self, policy: FrameDropPolicy) -> Self {
        self.drop_policy = policy;
        self
    }

    /// `arrival` is how the observed frame came from the video source; it
    /// feeds the dropped-frame windows.
    pub fn observe(
        &mut self,
        observation: &Observation,
        arrival: FrameArrival,
    ) -> Vec<EventRecord> {
        let frame_id = observation.frame_id;
        self.last_frame_id = Some(frame_id);
        let mut events = self.count_frame(arrival);

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

    /// Adds a frame to the dropped-frame window; closes the window once it
    /// spans the policy's duration, returning events for the causes that
    /// made it slow.
    fn count_frame(&mut self, arrival: FrameArrival) -> Vec<GameEvent> {
        let (Some(last), Some(window)) =
            (self.last_arrival.replace(arrival), &mut self.drop_window)
        else {
            self.drop_window = Some(DropWindow::new(arrival.captured_at));
            return Vec::new();
        };
        let ids = arrival.frame_id.saturating_sub(last.frame_id);
        let delivered = arrival.delivered.saturating_sub(last.delivered);
        window.seen += 1;
        window.dropped_by_card += ids.saturating_sub(delivered);
        window.dropped_by_processing += delivered.saturating_sub(1);
        let span = arrival.captured_at.saturating_duration_since(window.start);
        if span < self.drop_policy.window {
            return Vec::new();
        }
        let closed = std::mem::replace(window, DropWindow::new(arrival.captured_at));
        let policy = &self.drop_policy;
        let window_ms = span.as_millis() as u64;
        let delivered = closed.seen + closed.dropped_by_processing;
        let mut events = Vec::new();
        if policy.exceeds_percent(closed.dropped_by_card, delivered)
            || policy.below_min_fps(delivered, span)
        {
            events.push(GameEvent::FramesDroppedByCard {
                missing: closed.dropped_by_card,
                delivered,
                window_ms,
            });
        }
        if closed.dropped_by_processing > 0
            && (policy.exceeds_percent(closed.dropped_by_processing, closed.seen)
                || policy.below_min_fps(closed.seen, span))
        {
            events.push(GameEvent::FramesDroppedByProcessing {
                missing: closed.dropped_by_processing,
                seen: closed.seen,
                window_ms,
            });
        }
        events
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
        let at = |id| FrameArrival {
            frame_id: id,
            delivered: id,
            captured_at: Instant::now(),
        };
        let mut extractor = EventExtractor::new(2);
        assert!(extractor
            .observe(&obs(0, ScreenState::Unknown), at(0))
            .is_empty());
        assert!(extractor
            .observe(&obs(1, ScreenState::Transition), at(1))
            .is_empty());
        assert!(extractor
            .observe(&obs(2, ScreenState::Unknown), at(2))
            .is_empty());
        assert!(extractor
            .observe(&obs(3, ScreenState::Transition), at(3))
            .is_empty());
        let events = extractor.observe(&obs(4, ScreenState::Transition), at(4));
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

    /// Feeds `seconds` of a 60 fps source whose card delivers every
    /// `card_step`-th frame id to a loop that reads every `bot_step`-th
    /// delivered frame; returns the dropped-frame events.
    fn run_source(
        extractor: &mut EventExtractor,
        card_step: u64,
        bot_step: u64,
        seconds: u64,
    ) -> Vec<GameEvent> {
        let start = Instant::now();
        (0..seconds * 60)
            .step_by((card_step * bot_step) as usize)
            .flat_map(|id| {
                let arrival = FrameArrival {
                    frame_id: id,
                    delivered: id / card_step,
                    captured_at: start + Duration::from_micros(id * 1_000_000 / 60),
                };
                extractor.observe(&obs(id, ScreenState::Unknown), arrival)
            })
            .map(|record| record.event)
            .filter(|event| {
                matches!(
                    event,
                    GameEvent::FramesDroppedByCard { .. }
                        | GameEvent::FramesDroppedByProcessing { .. }
                )
            })
            .collect()
    }

    #[test]
    fn dropping_half_the_frames_is_not_reported() {
        assert!(run_source(&mut EventExtractor::new(1), 2, 1, 10).is_empty());
        assert!(run_source(&mut EventExtractor::new(1), 1, 2, 10).is_empty());
    }

    #[test]
    fn card_drops_are_told_apart_from_processing_drops() {
        let events = run_source(&mut EventExtractor::new(1), 3, 1, 3);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            GameEvent::FramesDroppedByCard {
                missing: 40,
                delivered: 20,
                window_ms: 1000
            }
        );
        let events = run_source(&mut EventExtractor::new(1), 1, 3, 3);
        assert_eq!(
            events[0],
            GameEvent::FramesDroppedByProcessing {
                missing: 40,
                seen: 20,
                window_ms: 1000
            }
        );
    }

    #[test]
    fn reports_windows_below_the_minimum_fps() {
        let policy = FrameDropPolicy {
            max_drop_percent: 100,
            ..FrameDropPolicy::default()
        };
        let quiet = |card_step, bot_step| {
            let mut extractor = EventExtractor::new(1).with_drop_policy(policy);
            run_source(&mut extractor, card_step, bot_step, 3)
        };
        assert!(quiet(6, 1).is_empty()); // the card delivers 10 fps
        assert!(matches!(
            quiet(12, 1)[..],
            [GameEvent::FramesDroppedByCard { delivered: 5, .. }, _]
        ));
        assert!(matches!(
            quiet(1, 12)[..],
            [GameEvent::FramesDroppedByProcessing { seen: 5, .. }, _]
        ));
        // A card at 5 fps read in full is the card's fault only.
        assert!(quiet(12, 1)
            .iter()
            .all(|e| matches!(e, GameEvent::FramesDroppedByCard { .. })));
    }

    #[test]
    fn a_stall_is_reported_once() {
        let start = Instant::now();
        let at = |id, ms| FrameArrival {
            frame_id: id,
            delivered: id,
            captured_at: start + Duration::from_millis(ms),
        };
        let mut extractor = EventExtractor::new(1);
        extractor.observe(&obs(0, ScreenState::Unknown), at(0, 0));
        extractor.observe(&obs(1, ScreenState::Unknown), at(1, 17));
        let events = extractor.observe(&obs(180, ScreenState::Unknown), at(180, 3000));
        assert_eq!(
            events[0].event,
            GameEvent::FramesDroppedByProcessing {
                missing: 178,
                seen: 2,
                window_ms: 3000
            }
        );
    }
}
