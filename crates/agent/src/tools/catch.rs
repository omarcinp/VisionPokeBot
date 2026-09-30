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
use crate::nav::{nearest_reachable, Destination, MapEntries};
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
/// The same while another member can fight: a faint then costs a SEND OUT
/// and a heal, not the run (the user's rule: Pokémon may faint as long as
/// not all of them do), so the hunt goes on longer between heals.
const BACKED_HEAL_BELOW: u32 = 250;
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
    /// Where the hunt entered maps: its walks are planned afresh on each
    /// map, so only the hunt sees them go round in circles.
    entries: MapEntries,
    /// The training's level when the step began (or last saved): a level
    /// gained is saved before the hunt goes on (the Switch lost an hour
    /// of IVYSAUR's levels to each relaunch: a Train saves only when it
    /// ends).
    saved_level: Option<u8>,
    /// The species a training hunt is for (it leads the battles); `None`:
    /// the lead.
    trainee: Option<String>,
    /// Switch-training: the party slot of the member that fights the
    /// battles the trainee starts (the trainee would lose them). A slot,
    /// not a species: the carrier evolves on the way (fleet worker 6: its
    /// BULBASAUR became IVYSAUR at battle 38, and the hunt lost it).
    carrier: Option<u8>,
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
            entries: MapEntries::default(),
            trainee: None,
            carrier: None,
            nav: NavParts::of(ctx),
            saved_level: None,
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
            Hunt::Level(level) => self
                .trainee
                .as_ref()
                .and_then(|s| party.members.iter().find(|m| &m.species == s))
                .or_else(|| party.lead())
                .filter(|l| l.level >= *level)
                .map(|l| {
                    format!(
                        "{} reached Lv{} (target Lv{level})",
                        l.display_name(),
                        l.level
                    )
                }),
        }
    }

    /// The level of the member being trained (the trainee, else the lead);
    /// `None` when catching.
    fn trained_level(&self, party: &Party) -> Option<u8> {
        if !matches!(self.hunt, Hunt::Level(_)) {
            return None;
        }
        self.trainee
            .as_ref()
            .and_then(|s| party.members.iter().find(|m| &m.species == s))
            .or_else(|| party.lead())
            .map(|m| m.level)
    }

    /// The member whose HP decides heals: the carrier while switch-training
    /// (the trainee doesn't fight), else the lead.
    fn fighter<'p>(&self, party: &'p Party) -> Option<&'p crate::party::Member> {
        carrier_in(party, self.carrier).or_else(|| party.lead())
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
        if self.battle.is_none() {
            if let Some(level) = self.trained_level(&party) {
                match self.saved_level {
                    None => self.saved_level = Some(level),
                    Some(saved) if level > saved => {
                        return Decision::Done(format!("{SAVE_FIRST}: Lv{level} reached"));
                    }
                    Some(_) => {}
                }
            }
        }
        if let Some(battle) = &mut self.battle {
            let decision = battle.next(ctx);
            if let Decision::Done(summary) = &decision {
                self.encounters += 1;
                let caught = battle.caught().map(str::to_owned);
                let fled = battle.ran() && caught.is_none();
                let foe = battle.memory.catch.foe.as_ref().map(|(s, _)| s.clone());
                self.fled_in_a_row = if fled { self.fled_in_a_row + 1 } else { 0 };
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
                // Ran from a battle the hunt is for (any, training; the
                // hunted species, catching) with the lead hurt: the battle's
                // risk rule is stricter than the hunt's heal bar, so heal
                // rather than run from every battle on (fleet workers 2 and
                // 5: SQUIRTLE at 12/23 ran from eight Route 22 battles in a
                // row, twice, and the plan gave up).
                let lead_hp = self.fighter(&party).and_then(|l| l.hp);
                if heal_after_flight(&self.hunt, fled, foe.as_deref(), lead_hp) {
                    let (hp, max) = lead_hp.unwrap_or_default();
                    return Decision::Fail(format!(
                        "{HEAL_FIRST}: ran from a {} battle at {hp}/{max} HP",
                        self.phase().to_lowercase()
                    ));
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
            let carrier_slot = carrier_in(&party, self.carrier).map(|m| m.slot);
            self.battle = Some(match (&self.hunt, carrier_slot) {
                (Hunt::Species(species), _) => battle.sparing(species),
                (Hunt::Level(_), Some(slot)) => battle.shifting_to(slot),
                (Hunt::Level(_), None) => battle,
            });
            return Decision::Wait("a battle starts".into());
        }
        // The carrier gone from the party knowledge: the trainee would
        // start battles it loses with no one to switch to (Switch, Route 4:
        // a misread party size dropped IVYSAUR, and a Lv3 PIDGEY ran from a
        // Lv12 SPEAROW until it fainted).
        if let Some(c) = self
            .carrier
            .filter(|_| carrier_in(&party, self.carrier).is_none())
        {
            return Decision::Fail(format!(
                "the member in slot {c} is to carry the trainee but isn't in the party knowledge"
            ));
        }
        // A fainted trainee earns nothing: the game sends out the next
        // member in its place, and the carrier fights for no one (fleet
        // worker 2, Mt. Moon: PARAS fainted at battle 8 of 150 and MANKEY
        // fought on while PARAS stayed Lv7).
        if let Some(t) = fainted_trainee(&party, self.trainee.as_deref()) {
            return Decision::Fail(format!(
                "{HEAL_FIRST}: {} (training) fainted",
                t.display_name()
            ));
        }
        if let Some(lead) = self.fighter(&party) {
            let backed = party
                .members
                .iter()
                .any(|m| m.slot != lead.slot && m.hp.is_some_and(|(hp, _)| hp > 0));
            let below = if backed {
                BACKED_HEAL_BELOW
            } else {
                HEAL_BELOW
            };
            if lead
                .hp
                .is_some_and(|(hp, max)| u32::from(hp) * 1000 < u32::from(max) * below)
            {
                return Decision::Fail(format!(
                    "{HEAL_FIRST}: {} is at {}/{} HP, below {:.0} %",
                    lead.display_name(),
                    lead.hp.map_or(0, |h| h.0),
                    lead.hp.map_or(0, |h| h.1),
                    f64::from(below) / 10.0
                ));
            }
        }
        // Few balls: restock before the grass (an unknown pocket goes on).
        if self.restock && matches!(self.hunt, Hunt::Species(_)) {
            if let Some(n) =
                ball_count(ctx.state).filter(|n| ball_budget(*n, false) < HUNT_MIN_BALLS)
            {
                // Counted before walking to a mart: money that can't buy
                // enough balls (Potion money kept) fails the hunt here,
                // and the plan looks for another way (fleet workers 2 and
                // 5: ₽680, "the mart sold none", fourteen trips).
                if let Some(money) = ctx.state.money.value {
                    let can_buy = crate::stock::affordable(&self.data, "ITEM_POKE_BALL", money);
                    if ball_budget(n.saturating_add(can_buy), false) < HUNT_MIN_BALLS {
                        return Decision::Fail(format!(
                            "can't afford the balls: {n} held, ₽{money} buys {can_buy} (₽{} kept for Potions), {HUNT_MIN_BALLS} above the shiny reserve needed",
                            money - crate::stock::spendable(&self.data, money)
                        ));
                    }
                }
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
        if let Some(why) = self.entries.note(&pose, o.frame_id) {
            return Decision::Fail(format!("{:?}: {why}", self.hunt));
        }
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
                whiteout: false,
            }
        }
    }
}

/// The switch-training carrier in `slot`, whatever it evolved into.
fn carrier_in(party: &Party, slot: Option<u8>) -> Option<&crate::party::Member> {
    let slot = slot?;
    party.members.iter().find(|m| m.slot == slot)
}

/// The member a training hunt is for, when it fainted.
fn fainted_trainee<'p>(
    party: &'p Party,
    trainee: Option<&str>,
) -> Option<&'p crate::party::Member> {
    let species = trainee?;
    party
        .members
        .iter()
        .find(|m| m.species == species)
        .filter(|m| m.fainted())
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

/// Whether a battle the hunt ran from calls for a heal: one it is for (any
/// while training; the hunted species while catching; running from others
/// is how a catch hunt goes on), with the lead hurt below
/// [`FLIGHT_HEAL_PCT`] %. Nearly full, the flight is the matchup's, and a
/// heal changes nothing (fleet worker 4: a Lv7 CATERPIE at 47/50 ran from
/// Route 3's Pokémon, healed three times and gave the training up).
fn heal_after_flight(
    hunt: &Hunt,
    fled: bool,
    foe: Option<&str>,
    lead_hp: Option<(u16, u16)>,
) -> bool {
    let for_the_hunt = match hunt {
        Hunt::Level(_) => true,
        Hunt::Species(s) => foe == Some(s.as_str()),
    };
    fled && for_the_hunt
        && lead_hp.is_some_and(|(hp, max)| u32::from(hp) * 100 < u32::from(max) * FLIGHT_HEAL_PCT)
}

/// Below this share of its HP, a lead that ran from a battle the hunt is
/// for heals first.
const FLIGHT_HEAL_PCT: u32 = 75;

/// Runs the hunt, healing at the nearest Center (and coming back) when the
/// lead is too weak to go on, and saving after a catch on the way when
/// the context keeps a checkpoint (`--save-game`).
fn hunt(ctx: &mut ToolContext<'_>, hunt: Hunt, map: Option<&str>) -> Result<(), ToolError> {
    hunt_for(ctx, hunt, map, None, None)
}

fn hunt_for(
    ctx: &mut ToolContext<'_>,
    hunt: Hunt,
    map: Option<&str>,
    trainee: Option<&str>,
    carrier: Option<u8>,
) -> Result<(), ToolError> {
    let mut heals = 0;
    let mut buys = 0;
    let mut encounters = 0;
    let mut fled_in_a_row = 0;
    let mut restock = true;
    loop {
        let mut step = HuntStep::new(ctx, hunt.clone(), map);
        step.trainee = trainee.map(str::to_owned);
        step.carrier = carrier;
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
                        Err(e) => ctx.info(format!("save on the way: {e}; hunting on")),
                    }
                }
            }
            Ok(_) => return Ok(()),
            Err(ToolError::Failed(reason)) if reason.starts_with(HEAL_FIRST) => {
                // Heals in a row without a battle fought between them are
                // what doesn't work; a hunt that fights heals as it needs.
                if fled_in_a_row == 0 {
                    heals = 0;
                }
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

/// Trains `species` to `level` on `map`: it leads the battles (the lead
/// is the one that fights, and gains the experience; fleet workers'
/// "Train MANKEY to Lv12" was done at once, their CHARMANDER lead being
/// Lv14 already, and MANKEY never fought).
pub fn train(
    ctx: &mut ToolContext<'_>,
    map: &str,
    species: &str,
    level: u8,
) -> Result<(), ToolError> {
    // A fainted trainee is healed first: skipped, the hunt trained no one
    // (the game sends out the next member for it).
    let party = Party::from_state(ctx.state());
    let mut trainees = party.members.iter().filter(|m| m.species == species);
    if trainees.clone().next().is_some() && trainees.all(|m| m.fainted()) {
        ctx.emit(progress(
            "Train",
            format!(
                "{} fainted: healing before it trains",
                crate::party::display_name(species)
            ),
        ))?;
        ctx.invoke(&Intent::Heal { center: None }).result?;
    }
    let party = Party::from_state(ctx.state());
    let slot = party
        .members
        .iter()
        .filter(|m| !m.fainted())
        .find(|m| m.species == species)
        .map(|m| m.slot);
    // One that would lose its first battles here is switch-trained: it
    // starts each battle and the strongest member that can fight here
    // comes out to win it (fleet worker 4: a Lv2 MANKEY led on Route 1
    // and fainted to a PIDGEY, twice). One that wins has the carrier too,
    // for the battles it would win only slowly (the battle weighs each).
    let as_planned = |m: &crate::party::Member| pokebot_planner::PartyMember {
        species: m.species.clone(),
        level: m.level,
        exp: None,
        moves: m.moves.iter().filter(|mv| *mv != "?").cloned().collect(),
    };
    let trainee = party.members.iter().find(|m| Some(m.slot) == slot);
    let alone =
        trainee.map(|t| pokebot_planner::prepare::trains_alone(&ctx.data, &as_planned(t), map));
    let carrier = trainee.map(|t| {
        party
            .members
            .iter()
            .filter(|m| m.slot != t.slot && m.hp.is_none_or(|(hp, _)| hp > 0))
            .filter(|m| {
                pokebot_planner::prepare::trains_alone(&ctx.data, &as_planned(m), map) == Some(true)
            })
            .max_by_key(|m| m.level)
            .map(|m| m.species.clone())
    });
    let carrier = match carrier {
        Some(None) if alone == Some(Some(true)) => None,
        Some(None) => {
            return Err(ToolError::Failed(format!(
                "{} can't train on {map} and no member can carry it there",
                crate::party::display_name(species)
            )))
        }
        Some(Some(c)) => Some(c),
        None => None,
    };
    if let Some(slot) = slot.filter(|s| *s != 0) {
        ctx.emit(progress(
            "Party",
            match &carrier {
                Some(c) => format!(
                    "{} leads to train, {} fights",
                    crate::party::display_name(species),
                    crate::party::display_name(c)
                ),
                None => format!("{} leads to train", crate::party::display_name(species)),
            },
        ))?;
        ctx.drive(&mut super::party_order::LeadWith::new(slot, species))?;
    }
    // Where the carrier stands once the trainee leads.
    let carrier = match carrier {
        Some(c) => Some(
            Party::from_state(ctx.state())
                .members
                .iter()
                .find(|m| m.species == c)
                .map(|m| m.slot)
                .ok_or_else(|| {
                    ToolError::Failed(format!(
                        "{} is to carry {} but isn't in the party knowledge",
                        crate::party::display_name(&c),
                        crate::party::display_name(species)
                    ))
                })?,
        ),
        None => None,
    };
    hunt_for(
        ctx,
        Hunt::Level(level),
        Some(map),
        slot.map(|_| species),
        carrier,
    )
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
        let Intent::Train {
            map,
            species,
            level,
        } = intent
        else {
            return ToolOutcome::failed("not a Train");
        };
        train(ctx, map, species, *level).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::Gone;
    use pokebot_state::PlayerPose;
    use pokebot_world::World;

    /// Fleet worker 6: the carrier (slot 1) evolved from BULBASAUR into
    /// IVYSAUR at battle 38; it is still the carrier.
    #[test]
    fn an_evolved_carrier_still_carries() {
        let Ok(data) = pokebot_gamedata::GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let member = |slot, species: &str| crate::party::Member {
            slot,
            ..crate::party::Member::new(&data, species, 16)
        };
        let party = Party {
            members: vec![member(0, "SPECIES_CATERPIE"), member(1, "SPECIES_IVYSAUR")],
        };
        let c = carrier_in(&party, Some(1)).expect("the carrier");
        assert_eq!(c.species, "SPECIES_IVYSAUR");
        assert!(carrier_in(&party, Some(2)).is_none());
        assert!(carrier_in(&party, None).is_none());
    }

    #[test]
    fn a_hunt_heals_after_running_from_what_it_is_for() {
        let train = Hunt::Level(13);
        let mankey = Hunt::Species("SPECIES_MANKEY".into());
        // Fleet workers 2 and 5: SQUIRTLE at 12/23 ran from every battle.
        assert!(heal_after_flight(
            &train,
            true,
            Some("SPECIES_RATTATA"),
            Some((12, 23))
        ));
        assert!(!heal_after_flight(
            &train,
            true,
            Some("SPECIES_RATTATA"),
            Some((23, 23))
        ));
        // Fleet worker 4: a Lv7 CATERPIE at 47/50 runs from Route 3's
        // Pokémon for the matchup; a heal changes nothing.
        assert!(!heal_after_flight(
            &train,
            true,
            Some("SPECIES_SPEAROW"),
            Some((47, 50))
        ));
        assert!(!heal_after_flight(
            &train,
            false,
            Some("SPECIES_RATTATA"),
            Some((12, 23))
        ));
        // A catch hunt runs from other species as it goes.
        assert!(!heal_after_flight(
            &mankey,
            true,
            Some("SPECIES_RATTATA"),
            Some((12, 23))
        ));
        assert!(heal_after_flight(
            &mankey,
            true,
            Some("SPECIES_MANKEY"),
            Some((12, 23))
        ));
    }

    /// Fleet worker 2, Mt. Moon: PARAS, switch-trained with MANKEY
    /// carrying, fainted at battle 8 of 150; the hunt went on, the game
    /// sending MANKEY out for it every battle. A fainted trainee heals.
    #[test]
    fn a_fainted_trainee_is_healed_before_the_next_battle() {
        let Ok(data) = pokebot_gamedata::GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let mut paras = crate::party::Member::new(&data, "SPECIES_PARAS", 7);
        paras.hp = Some((0, 23));
        let mankey = crate::party::Member {
            slot: 1,
            hp: Some((39, 39)),
            ..crate::party::Member::new(&data, "SPECIES_MANKEY", 14)
        };
        let party = Party {
            members: vec![paras.clone(), mankey],
        };
        let t = fainted_trainee(&party, Some("SPECIES_PARAS")).expect("PARAS fainted");
        assert_eq!(t.slot, 0);
        // The carrier's faint is the heal bar's; a living trainee trains.
        assert!(fainted_trainee(&party, Some("SPECIES_MANKEY")).is_none());
        assert!(fainted_trainee(&party, None).is_none());
        paras.hp = Some((5, 23));
        let party = Party {
            members: vec![paras],
        };
        assert!(fainted_trainee(&party, Some("SPECIES_PARAS")).is_none());
    }

    /// The Switch lost an hour of IVYSAUR's levels to each relaunch: a
    /// Train saved only when it ended. Between battles, a level gained
    /// since the step began is saved first ("save first", as a catch).
    #[test]
    fn a_level_gained_while_training_is_saved() {
        use pokebot_state::{GameState, Knowledge, Observation, Observed, PartyMon, ScreenState};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let (Ok(world), Ok(data)) = (
            World::load(&dir),
            pokebot_gamedata::GameData::load(dir.join("gamedata.json")),
        ) else {
            return;
        };
        let mut step = HuntStep {
            hunt: Hunt::Level(34),
            restock: true,
            map: Some("Route4".into()),
            data: Arc::new(data),
            battle: None,
            go: None,
            spin_at: None,
            encounters: 0,
            fled_in_a_row: 0,
            entries: MapEntries::default(),
            saved_level: None,
            trainee: None,
            carrier: None,
            nav: NavParts {
                world: Arc::new(world),
                gone: Gone::new(),
                syncer: None,
                blocked: Default::default(),
                gates: Default::default(),
                data: None,
            },
        };
        let state = |level: u8| GameState {
            party: Knowledge::observed(
                vec![PartyMon {
                    species: Knowledge::observed("SPECIES_IVYSAUR".into(), 1),
                    level: Knowledge::observed(level, 1),
                    hp: Knowledge::observed((80, 80), 1),
                    ..PartyMon::default()
                }],
                1,
            ),
            ..GameState::default()
        };
        let o = Observation::bare(
            1,
            Observed {
                value: ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        );
        let mut run = |state: &GameState| {
            let mut events = Vec::new();
            step.next(&mut StepContext {
                observation: &o,
                state,
                events: &mut events,
                quiet_frames: 0,
                frame: None,
                learned: &[],
            })
        };
        let saves = |d: &Decision| matches!(d, Decision::Done(s) if s.starts_with(SAVE_FIRST));
        assert!(!saves(&run(&state(28))));
        assert!(!saves(&run(&state(28))));
        assert!(saves(&run(&state(29))));
    }

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
            data: None,
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
