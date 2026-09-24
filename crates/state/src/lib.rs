//! Frame → Observation → Events → Reducer → GameState.
//!
//! Observations describe what is visible now. Events are the only input to
//! persistent state. The reducer is pure, so any session can be rebuilt
//! from its event log.

mod events;
mod inventory;
mod knowledge;
mod observation;
mod party;
mod reduce_knowledge;
mod reducer;
mod screen;
mod state;

pub use events::{EventExtractor, EventRecord, GameEvent};
pub use inventory::{Bag, BoxMon, ItemList, PcStorage, Pocket, Pokedex, SavedKnowledge, BOXES};
pub use knowledge::{Knowledge, KnowledgeSource};
pub use observation::{
    BagObservation, BattleMenu, BattleObservation, DialogueKind, DialogueObservation, Direction,
    FrameMetrics, KeyboardFocus, MenuObservation, MoveListObservation, NamingObservation,
    Observation, Observed, PlayerPose, PoseObservation, Region, ShinyReading, ShopObservation,
};
pub use party::{MoveSlot, PartyMon, Status};
pub use reducer::{DefaultReducer, StateReducer};
pub use screen::ScreenState;
pub use state::{
    GameState, Gender, GoalStatus, InputState, PlayerState, Progression, SynchronizationState,
};
