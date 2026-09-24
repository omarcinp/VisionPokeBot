//! Readiness planning: how likely is the party to win a trainer battle, and
//! what is the cheapest way (training, catching) to make that likely enough?
//!
//! Everything is computed exactly from the game's formulas and data — no
//! random sampling — so the same state always yields the same plan.

pub mod evaluate;
pub mod prepare;

pub use evaluate::{battle_vs_trainer, matchup, BattleEstimate, Combatant};
pub use prepare::{
    plan_preparation, plan_training, Area, PartyMember, PlanStep, PreparationPlan, Request, OUR_IV,
};
