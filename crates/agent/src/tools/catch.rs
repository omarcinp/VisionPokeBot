//! `Catch { species }`: spin on an encounter tile of the current map until
//! a battle starts, play it with the catch policy, and repeat until the
//! species is caught. The battle is embedded rather than an interrupt so
//! the step can read what was caught.

use std::sync::Arc;

use pokebot_core::ControllerCommand;

use super::battle::BattleStep;
use super::go::{GoStep, NavParts};
use super::lookup::grass_spot;
use super::{
    progress, BattlePlan, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep, SETTLE_FRAMES,
};
use crate::nav::Destination;
use crate::party::Party;
use crate::story::spin_sequence;
use crate::{Action, Decision, Expectation, Outcome};

/// Direction changes per spin action (~2.4 s; a battle cancels it early).
const SPIN_TURNS: usize = 24;
/// Battles before the hunt is given up.
const MAX_ENCOUNTERS: u32 = 40;
/// The lead heals below this share of its HP (per mille).
const HEAL_BELOW: u32 = 500;

pub struct CatchTool;

pub struct CatchStep {
    species: String,
    data: Arc<pokebot_gamedata::GameData>,
    battle: Option<BattleStep>,
    go: Option<GoStep>,
    spin_at: Option<(i32, i32)>,
    encounters: u32,
    caught: bool,
    nav: NavParts,
}

impl ToolStep for CatchStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if let Some(battle) = &mut self.battle {
            let decision = battle.next(ctx);
            if let Decision::Done(summary) = &decision {
                self.encounters += 1;
                let caught = battle.caught().map(str::to_owned);
                ctx.events.push(progress(
                    "Catch",
                    format!("{summary} ({}/{MAX_ENCOUNTERS})", self.encounters),
                ));
                self.battle = None;
                self.go = None;
                if caught.as_deref() == Some(self.species.as_str()) {
                    self.caught = true;
                    return Decision::Done(format!("caught {}", self.species));
                }
                if self.encounters >= MAX_ENCOUNTERS {
                    return Decision::Fail(format!(
                        "{} not caught in {MAX_ENCOUNTERS} battles",
                        self.species
                    ));
                }
                return Decision::Wait("battle over".into());
            }
            return decision;
        }
        if o.battle.is_some() {
            self.battle = Some(BattleStep::new(
                Arc::clone(&self.data),
                BattlePlan::Auto,
                false,
            ));
            return Decision::Wait("a battle starts".into());
        }
        let party = Party::from_state(ctx.state);
        if let Some(lead) = party.lead() {
            if lead
                .hp
                .is_some_and(|(hp, max)| u32::from(hp) * 1000 < u32::from(max) * HEAL_BELOW)
            {
                return Decision::Fail(format!(
                    "the lead is at {}/{} HP: heal first",
                    lead.hp.map_or(0, |h| h.0),
                    lead.hp.map_or(0, |h| h.1)
                ));
            }
        }
        if ctx.quiet_frames < SETTLE_FRAMES {
            return Decision::Wait("letting the scene settle".into());
        }
        let Some(pose) = o.player.as_ref().map(|p| p.pose.clone()) else {
            return Decision::Wait("locating".into());
        };
        let target = match self.spin_at {
            Some(t) => t,
            None => {
                let Some(t) = grass_spot(&self.nav.world, &pose.map, (pose.x, pose.y)) else {
                    return Decision::Fail(format!("no encounter tiles on {}", pose.map));
                };
                self.spin_at = Some(t);
                t
            }
        };
        if (pose.x, pose.y) == target {
            self.go = None;
            return Decision::Act(
                Action::new(
                    "spin in the grass for encounters",
                    vec![ControllerCommand::Sequence(spin_sequence(SPIN_TURNS))],
                    Expectation::InputsDone,
                    10,
                )
                .interruptible(),
            );
        }
        let dest = Destination::Tile {
            map: pose.map.clone(),
            x: target.0,
            y: target.1,
        };
        if self.go.as_ref().is_none_or(|go| *go.destination() != dest) {
            self.go = Some(GoStep::with(&self.nav, dest));
        }
        match self.go.as_mut().expect("set above").next(ctx) {
            Decision::Done(_) => Decision::Wait("at the spin tile".into()),
            d => d,
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        if let Some(battle) = &mut self.battle {
            battle.on_outcome(action, outcome, ctx);
        } else if let Some(go) = &mut self.go {
            go.on_outcome(action, outcome, ctx);
        }
    }

    fn expects(&self) -> Expects {
        // The battle is played here, not as an interrupt.
        Expects::BATTLE
    }
}

impl CatchStep {
    fn new(ctx: &ToolContext<'_>, species: &str) -> Self {
        Self {
            species: species.to_owned(),
            data: Arc::clone(&ctx.data),
            battle: None,
            go: None,
            spin_at: None,
            encounters: 0,
            caught: false,
            nav: NavParts::of(ctx),
        }
    }
}

pub fn catch(ctx: &mut ToolContext<'_>, species: &str) -> Result<(), ToolError> {
    let mut step = CatchStep::new(ctx, species);
    ctx.drive(&mut step).map(|_| ())
}

impl Tool for CatchTool {
    fn name(&self) -> &str {
        "Catch"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Catch { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Catch { species } = intent else {
            return ToolOutcome::failed("not a Catch");
        };
        catch(ctx, species).into()
    }
}
