//! Frame → Observation → Events → Reducer → GameState.
//!
//! Observations describe what is visible now. Events are the only input to
//! persistent state. The reducer is pure, so any session can be rebuilt
//! from its event log.
//!
//! `WorldBelief` (flags, vars, visited maps, respawn, NPCs) is fed by the
//! same events; `inference` derives more facts from observed ones and
//! `priors` gives a probability for the rest, both from rule files under
//! [`RULES_DIR`].

mod belief;
mod change;
mod events;
pub mod inference;
mod inventory;
mod knowledge;
mod observation;
mod party;
pub mod priors;
mod reduce_knowledge;
mod reducer;
mod screen;
mod state;
mod view;
pub use observation::{PartyMenuObservation, SummaryObservation, SummaryPage};

pub use belief::{Fact, HealSpot, NpcBelief, WorldBelief, PATHS_RUN_KEPT};
pub use change::{diff, ChangeRecord, StateChange};
pub use events::{EventExtractor, EventRecord, FrameArrival, FrameDropPolicy, GameEvent};
pub use inference::{Condition, InferenceRule, InferenceRules};
pub use inventory::{
    Bag, BoxMon, ItemList, PcStorage, Pocket, Pokedex, PokedexCounts, SavedKnowledge, BOXES,
};
pub use knowledge::{Knowledge, KnowledgeSource};
pub use observation::{
    BagObservation, BattleMenu, BattleObservation, DialogueKind, DialogueObservation, Direction,
    FlyMapObservation, FrameMetrics, KeyboardFocus, MenuObservation, MoveListObservation,
    NamingObservation, Observation, Observed, PartyRowObservation, PlayerPose,
    PokedexListObservation, PoseObservation, Region, ShinyReading, ShopObservation,
    SpriteObservation, TrainerCardObservation,
};
pub use party::{MoveSlot, PartyMon, Status};
pub use priors::{PriorRule, Priors};
pub use reducer::{DefaultReducer, StateReducer};
pub use screen::ScreenState;
pub use view::{MenuView, OpponentView, ViewState, VisibleNpc};

/// Where the rule files (`inference.json`, `priors.json`) live by default,
/// relative to the repository root. The copies under `data/world/` are
/// not tracked by git.
pub const RULES_DIR: &str = "data/rules";
pub use state::{
    GameState, Gender, GoalStatus, InputState, PlayerState, Progression, SynchronizationState,
};
