//! `Catch { species }` and `Train { level }`: spin on an encounter tile of
//! the map until a battle starts, play it (the catch policy for a catch,
//! fight for training), and repeat until the species is caught or the lead
//! reaches the level. The battle is embedded rather than an interrupt so
//! the step can read what was caught. A weakened lead heals at the nearest
//! Pokémon Center and the hunt resumes. A catch hunt restocks Poké Balls at
//! the nearest mart when fewer than [`HUNT_MIN_BALLS`] are held above the
//! shiny reserve (hunting on with what is held when the mart sells none),
//! and throws at the species hunted whenever the lead is safe: it runs
//! from it only when the lead is at risk, rather than faint it.

use std::sync::Arc;

use pokebot_core::ControllerCommand;

use super::battle::BattleStep;
use super::go::{GoStep, NavParts};
use super::lookup::{encounter_tile, grass_spot};
use super::{
    progress, BattlePlan, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep, SETTLE_FRAMES,
};
use crate::catch::ball_budget;
use crate::nav::{nearest_reachable, Destination};
use crate::party::Party;
use crate::stock::ball_count;
use crate::story::spin_sequence;
use crate::{Action, Decision, Expectation, Outcome};

/// Direction changes per spin action (~2.4 s; a battle cancels it early).
const SPIN_TURNS: usize = 24;
/// Battles before a catch hunt is given up.
const MAX_CATCH_ENCOUNTERS: u32 = 40;
/// Battles before a training hunt is given up.
const MAX_TRAIN_ENCOUNTERS: u32 = 150;
/// Battles run from in a row before a hunt counts as making no progress:
/// a fled battle gives no experience and catches nothing (Switch goal run,
/// Route 22: 1666 training battles, every one run from, the hunt restarted
/// by each replan).
const MAX_FLED_IN_A_ROW: u32 = 8;
/// A hunting lead heals below this share of its HP (per mille). The hunted
/// species is attempted at any HP the risk limit allows, so a catch hunt
/// no longer heals at the catch policy's bar for extras (`CATCH_MIN_HP`;
/// flash-1 passed up a SPEAROW at 39/60 HP before that).
const HEAL_BELOW: u32 = 500;
/// Heals per hunt before it counts as not working.
const MAX_HEALS: u32 = 3;
/// How the step reports a lead that must heal before going on.
const HEAL_FIRST: &str = "heal first";
/// Balls above the shiny reserve a catch hunt needs to go on.
pub const HUNT_MIN_BALLS: u16 = 3;
/// Mart visits per hunt before it counts as not working.
const MAX_BUYS: u32 = 2;
/// How the step reports a hunt short of balls.
const BUY_FIRST: &str = "buy balls first";
/// How the step reports a catch on the way (another species than the
/// target, or one while training): the hunt saves the game before it
/// goes on (spec §8: a save after every belief-changing step; flash-4
/// lost a PIKACHU caught while hunting WEEDLE when the hunt failed later).
const SAVE_FIRST: &str = "save first";

pub struct CatchTool;
pub struct TrainTool;

/// What the hunt is after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunt {
    Species(String),
    Level(u8),
}

pub struct HuntStep {
    hunt: Hunt,
    /// Restock balls when short ([`BUY_FIRST`]); off once a mart sold none
    /// while some are held: the hunted species throws the reserve too.
    restock: bool,
    /// The map to hunt on (the current one when `None`).
    map: Option<String>,
    data: Arc<pokebot_gamedata::GameData>,
    battle: Option<BattleStep>,
    go: Option<GoStep>,
    spin_at: Option<(i32, i32)>,
    encounters: u32,
    /// Battles run from since the last one fought to its end.
    fled_in_a_row: u32,
    nav: NavParts,
}

impl HuntStep {
    pub fn new(ctx: &ToolContext<'_>, hunt: Hunt, map: Option<&str>) -> Self {
        Self {
            hunt,
            restock: true,
            map: map.map(str::to_owned),
            data: Arc::clone(&ctx.data),
            battle: None,
            go: None,
            spin_at: None,
            encounters: 0,
            fled_in_a_row: 0,
            nav: NavParts::of(ctx),
        }
    }

    fn max_encounters(&self) -> u32 {
        match self.hunt {
            Hunt::Species(_) => MAX_CATCH_ENCOUNTERS,
            Hunt::Level(_) => MAX_TRAIN_ENCOUNTERS,
        }
    }

    fn plan(&self) -> BattlePlan {
        match self.hunt {
            Hunt::Species(_) => BattlePlan::Auto,
            Hunt::Level(_) => BattlePlan::Fight,
        }
    }

    fn phase(&self) -> &'static str {
        match self.hunt {
            Hunt::Species(_) => "Catch",
            Hunt::Level(_) => "Train",
        }
    }

    /// Whether the hunt is over, given the party knowledge, the species
    /// the last battle caught and the Pokédex's caught marks (a catch a
    /// failed battle step didn't report shows as the mark on the next
    /// encounter's HUD; flash-5 hunted a MANKEY already caught).
    fn reached(
        &self,
        party: &Party,
        caught: Option<&str>,
        state: &pokebot_state::GameState,
    ) -> Option<String> {
        match &self.hunt {
            Hunt::Species(species) => {
                if caught == Some(species.as_str()) {
                    return Some(format!("caught {species}"));
                }
                let marked = state
                    .pokedex
                    .caught
                    .get(species)
                    .is_some_and(|k| k.value == Some(true));
                marked.then(|| format!("{species} is marked caught in the Pokédex"))
            }
            Hunt::Level(level) => party.lead().filter(|l| l.level >= *level).map(|l| {
                format!(
                    "{} reached Lv{} (target Lv{level})",
                    l.display_name(),
                    l.level
                )
            }),
        }
    }

    /// Walks to the grass of `map` (from another map) or to the spin tile.
    fn walk(&mut self, ctx: &mut StepContext<'_>, dest: Destination) -> Decision {
        if self.go.as_ref().is_none_or(|go| *go.destination() != dest) {
            self.go = Some(GoStep::with(&self.nav, dest));
        }
        match self.go.as_mut().expect("set above").next(ctx) {
            Decision::Done(_) => Decision::Wait("at the grass".into()),
            d => d,
        }
    }
}

impl ToolStep for HuntStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        let party = Party::from_state(ctx.state);
        if let Some(battle) = &mut self.battle {
            let decision = battle.next(ctx);
            if let Decision::Done(summary) = &decision {
                self.encounters += 1;
                let caught = battle.caught().map(str::to_owned);
                self.fled_in_a_row = if battle.ran() && caught.is_none() {
                    self.fled_in_a_row + 1
                } else {
                    0
                };
                ctx.events.push(progress(
                    self.phase(),
                    format!("{summary} ({}/{})", self.encounters, self.max_encounters()),
                ));
                self.battle = None;
                self.go = None;
                // The state the step sees was cloned before this battle's
                // last events; the caught species is read from the battle.
                if let Some(done) = self.reached(&party, caught.as_deref(), ctx.state) {
                    return Decision::Done(done);
                }
                if self.fled_in_a_row >= MAX_FLED_IN_A_ROW {
                    return Decision::Fail(format!(
                        "{:?}: ran from the last {} battles, no progress",
                        self.hunt, self.fled_in_a_row
                    ));
                }
                if self.encounters >= self.max_encounters() {
                    return Decision::Fail(format!(
                        "{:?} not reached in {} battles",
                        self.hunt,
                        self.max_encounters()
                    ));
                }
                if let Some(species) = caught {
                    return Decision::Done(format!("{SAVE_FIRST}: caught {species}"));
                }
                return Decision::Wait("battle over".into());
            }
            return decision;
        }
        if let Some(done) = self.reached(&party, None, ctx.state) {
            return Decision::Done(done);
        }
        if o.battle.is_some() {
            let battle = BattleStep::new(Arc::clone(&self.data), self.plan(), false);
            self.battle = Some(match &self.hunt {
                Hunt::Species(species) => battle.sparing(species),
                Hunt::Level(_) => battle,
            });
            return Decision::Wait("a battle starts".into());
        }
        if let Some(lead) = party.lead() {
            if lead
                .hp
                .is_some_and(|(hp, max)| u32::from(hp) * 1000 < u32::from(max) * HEAL_BELOW)
            {
                return Decision::Fail(format!(
                    "{HEAL_FIRST}: the lead is at {}/{} HP, below {:.0} %",
                    lead.hp.map_or(0, |h| h.0),
                    lead.hp.map_or(0, |h| h.1),
                    f64::from(HEAL_BELOW) / 10.0
                ));
            }
        }
        // Few balls: restock before the grass (an unknown pocket goes on).
        if self.restock && matches!(self.hunt, Hunt::Species(_)) {
            if let Some(n) =
                ball_count(ctx.state).filter(|n| ball_budget(*n, false) < HUNT_MIN_BALLS)
            {
                return Decision::Fail(format!(
                    "{BUY_FIRST}: {n} held, {HUNT_MIN_BALLS} above the shiny reserve needed"
                ));
            }
        }
        if ctx.quiet_frames < SETTLE_FRAMES {
            return Decision::Wait("letting the scene settle".into());
        }
        let Some(pose) = o.player.as_ref().map(|p| p.pose.clone()) else {
            return Decision::Wait("locating".into());
        };
        if let Some(map) = self.map.clone().filter(|m| *m != pose.map) {
            // Another map: head for its grass first.
            let Some(m) = self.nav.world.map(&map) else {
                return Decision::Fail(format!("unknown map {map}"));
            };
            let Some((x, y)) = grass_spot(&self.nav.world, &map, (m.width / 2, m.height / 2))
            else {
                return Decision::Fail(format!("no encounter tiles on {map}"));
            };
            return self.walk(ctx, Destination::Tile { map, x, y });
        }
        let target = match self.spin_at {
            Some(t) => t,
            None => {
                let Some(t) = grass_spot(&self.nav.world, &pose.map, (pose.x, pose.y)) else {
                    return Decision::Fail(format!("no encounter tiles on {}", pose.map));
                };
                // A map split in parts (Route 4 around Mt. Moon) may keep
                // all its grass on the other side: say so before walking.
                if !encounter_tiles_reachable(&self.nav, &pose) {
                    return Decision::Fail(format!(
                        "no encounter tile of {} is reachable from {pose}",
                        pose.map
                    ));
                }
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
        self.walk(ctx, dest)
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        if let Some(battle) = &mut self.battle {
            battle.on_outcome(action, outcome, ctx);
        } else if let Some(go) = &mut self.go {
            go.on_outcome(action, outcome, ctx);
        }
    }

    fn expects(&self) -> Expects {
        // The battle is played here, not as an interrupt. Outside one,
        // dialogue is a trainer's challenge (or an NPC's words) on the way
        // to the grass: the interrupt wrapper follows it and fights the
        // trainer, so the hunt must not claim it (flash-1: the hunt waited
        // for the scene to settle in front of "Excuse me! You looked at
        // me, didn't you?" until the stuck rule pressed B, then tried to
        // catch the trainer's PIDGEY).
        if self.battle.is_some() {
            Expects::BATTLE
        } else {
            Expects {
                dialogue: false,
                battle: true,
                menu: false,
            }
        }
    }
}

/// Whether any encounter tile of the player's map can be walked to.
fn encounter_tiles_reachable(nav: &NavParts, pose: &pokebot_state::PlayerPose) -> bool {
    let Some(m) = nav.world.map(&pose.map) else {
        return false;
    };
    let tiles: std::collections::HashSet<(i32, i32)> = (0..m.height)
        .flat_map(|y| (0..m.width).map(move |x| (x, y)))
        .filter(|&(x, y)| m.tile(x, y).is_some_and(|t| encounter_tile(&t)))
        .collect();
    let goals = std::collections::BTreeMap::from([(pose.map.clone(), tiles)]);
    !nearest_reachable(&nav.world, pose, &goals, &nav.gone).is_empty()
}

/// Runs the hunt, healing at the nearest Center (and coming back) when the
/// lead is too weak to go on, and saving after a catch on the way when
/// the context keeps a checkpoint (`--save-game`).
fn hunt(ctx: &mut ToolContext<'_>, hunt: Hunt, map: Option<&str>) -> Result<(), ToolError> {
    let mut heals = 0;
    let mut buys = 0;
    let mut encounters = 0;
    let mut fled_in_a_row = 0;
    let mut restock = true;
    loop {
        let mut step = HuntStep::new(ctx, hunt.clone(), map);
        step.restock = restock;
        step.encounters = encounters;
        step.fled_in_a_row = fled_in_a_row;
        let result = ctx.drive(&mut step);
        encounters = step.encounters;
        fled_in_a_row = step.fled_in_a_row;
        match result {
            Ok(summary) if summary.starts_with(SAVE_FIRST) => {
                if ctx.checkpoint.is_some() {
                    ctx.emit(progress(step.phase(), format!("{summary}: saving")))?;
                    match ctx.invoke(&Intent::Save).result {
                        Ok(()) => {}
                        Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
                        Err(e) => ctx.info(format!("save after a catch: {e}; hunting on")),
                    }
                }
            }
            Ok(_) => return Ok(()),
            Err(ToolError::Failed(reason)) if reason.starts_with(HEAL_FIRST) => {
                heals += 1;
                if heals > MAX_HEALS {
                    return Err(ToolError::Failed(format!(
                        "{reason}; healed {MAX_HEALS} times already"
                    )));
                }
                ctx.emit(progress(step.phase(), format!("{reason}: healing")))?;
                ctx.invoke(&Intent::Heal { center: None }).result?;
            }
            Err(ToolError::Failed(reason)) if reason.starts_with(BUY_FIRST) => {
                buys += 1;
                if buys > MAX_BUYS {
                    return Err(ToolError::Failed(format!(
                        "{reason}; went to a mart {MAX_BUYS} times already"
                    )));
                }
                let before = ball_count(ctx.state());
                ctx.emit(progress(step.phase(), format!("{reason}: to the mart")))?;
                // Count 0: restock to the stock policy's target, keeping
                // the Potion money.
                ctx.invoke(&Intent::Buy {
                    item: "ITEM_POKE_BALL".into(),
                    count: 0,
                })
                .result?;
                let after = ball_count(ctx.state());
                if after <= before && after.is_some_and(|n| n > 0) {
                    // The hunted species throws the reserve too: hunt on
                    // with what is held rather than give the hunt up.
                    ctx.info(format!(
                        "{reason}; the mart sold none: hunting on with {after:?} balls"
                    ));
                    restock = false;
                } else if after <= before {
                    return Err(ToolError::Failed(format!(
                        "{reason}; the mart sold none ({before:?} → {after:?} balls)"
                    )));
                }
            }
            Err(e) => return Err(e),
        }
    }
}

pub fn catch(ctx: &mut ToolContext<'_>, species: &str, map: Option<&str>) -> Result<(), ToolError> {
    hunt(ctx, Hunt::Species(species.to_owned()), map)
}

pub fn train(ctx: &mut ToolContext<'_>, map: &str, level: u8) -> Result<(), ToolError> {
    hunt(ctx, Hunt::Level(level), Some(map))
}

impl Tool for CatchTool {
    fn name(&self) -> &str {
        "Catch"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Catch { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Catch { species, map } = intent else {
            return ToolOutcome::failed("not a Catch");
        };
        catch(ctx, species, map.as_deref()).into()
    }
}

impl Tool for TrainTool {
    fn name(&self) -> &str {
        "Train"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Train { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Train { map, level, .. } = intent else {
            return ToolOutcome::failed("not a Train");
        };
        train(ctx, map, *level).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::Gone;
    use pokebot_state::PlayerPose;
    use pokebot_world::World;

    #[test]
    fn route_4_grass_is_out_of_reach_from_its_west_part() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = World::load(&dir) else { return };
        let nav = NavParts {
            world: Arc::new(world),
            gone: Gone::new(),
            syncer: None,
            blocked: Default::default(),
            gates: Default::default(),
        };
        let at = |map: &str, x, y| PlayerPose {
            map: map.into(),
            x,
            y,
        };
        // The Pokémon Center door: west of Mt. Moon, no grass this side.
        assert!(!encounter_tiles_reachable(&nav, &at("Route4", 12, 5)));
        // Route 2 has grass south of Viridian Forest's gate.
        assert!(encounter_tiles_reachable(&nav, &at("Route2", 8, 60)));
        assert!(encounter_tiles_reachable(&nav, &at("Route1", 8, 20)));
    }
}
