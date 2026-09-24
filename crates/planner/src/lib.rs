//! Planning: how likely is the party to win a trainer battle, what is the
//! cheapest way (training, catching) to make that likely enough, and which
//! intents establish a goal predicate from the current belief (spec §4).
//!
//! Everything is computed exactly from the game's formulas and data — no
//! random sampling — so the same state always yields the same plan.

pub mod belief_adapter;
pub mod evaluate;
pub mod goals;
pub mod intents;
pub mod methods;
pub mod prepare;

pub use belief_adapter::{load_checkpoint, snapshot_id, StateBelief};
pub use evaluate::{battle_vs_trainer, matchup, BattleEstimate, Combatant};
pub use goals::{parse_goal, Plan, PlanError, PlanOptions, PlannedIntent, Planner};
pub use intents::{
    CostParams, Effect, GoalBelief, GoalPredicate, Intent, Obtain, PlanContext, ProbeFact,
};
pub use methods::{Method, Methods};

pub(crate) use pokebot_world::behavior::WARP_DOOR;
pub use prepare::{
    plan_preparation, plan_training, Area, PartyMember, PlanStep, PreparationPlan, Request, OUR_IV,
};
