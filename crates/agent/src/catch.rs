//! Whether and how to catch a wild Pokémon (the pure decision).
//!
//! A shiny is always caught, with any ball including the reserve. Otherwise
//! a species not caught yet is caught when balls above the shiny reserve
//! are held and the odds ([`crate::catch_odds`]) are good: with those
//! balls, the best mix of throws, a status move and attacks catches it
//! with at least [`MIN_CATCH_CHANCE`], while our lead faints with at most
//! [`RISK_LIMIT`]. Every turn the odds are solved again from the HUD and
//! the best action taken: throw, sleep or paralyse it, an attack unlikely
//! to faint it, or RUN. Trainers' Pokémon are never caught, and unknown
//! facts (caught flag, ball stock) decline the catch rather than guess.
//!
//! The flow in battle: [`identify`] reads the wild opponent once (two
//! agreeing command-menu frames), emits what the Pokédex learns and plans
//! the catch; the battle decision then plays the odds' choice each turn
//! (`attempt_decision`); [`Thrower`] goes through the battle bag;
//! [`CatchMemory::observe`] reads the result, the foe's status and the PC
//! box from battle text; [`caught_events`] records the catch when the
//! battle ends.
//!
//! The species a hunt is after ([`CatchMemory::wanted`]) is the goal, not
//! an extra: running from it throws the encounter away, so only our lead's
//! risk declines it. It may throw the shiny reserve too (the hunt restocks
//! afterwards), and neither a low chance, a worn lead nor an unread caught
//! icon or pocket stops the attempt.

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::mechanics::ball_multiplier;
use pokebot_gamedata::{printed_name, GameData};
use pokebot_planner::evaluate::faint_probability;
use pokebot_planner::Combatant;
use pokebot_state::{
    BattleMenu, GameEvent, GameState, Observation, Pocket, ScreenState, ShinyReading, Status,
};

use crate::bag::{
    by_item, is_cancel, item_key, pocket_from_title, pocket_index, read_rows, Rows, POCKETS,
};
use crate::battle::{identify_opponent, step_toward, BattleMemory, BattlePolicy};
use crate::catch_odds::{self, Choice, FoeView, Means, Odds, Weights};
use crate::party::{Member, Party};
use crate::stock::{ball_count, SHINY_RESERVE};
use crate::{Action, Decision, Expectation};

/// Largest accepted P(our lead faints during the attempt).
pub const RISK_LIMIT: f64 = 0.02;
/// Smallest P(catch) a non-shiny attempt starts with.
pub const MIN_CATCH_CHANCE: f64 = 0.2;
/// A fainted lead costs this many catches; a shiny is worth far more
/// than a fainted lead (the next Pokémon fights on).
const LEAD_FAINT: Weights = Weights { lead_faint: 20.0 };
const SHINY_LEAD_FAINT: Weights = Weights { lead_faint: 0.25 };
/// The species a hunt is after is worth half a fainted lead: a low chance
/// is still taken while the lead is safe ([`RISK_LIMIT`] caps it).
const WANTED_LEAD_FAINT: Weights = Weights { lead_faint: 2.0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoeStatus {
    None,
    Asleep,
    Paralyzed,
}

/// The wild opponent as read from the HUD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foe {
    pub species: String,
    pub level: u8,
    /// HP bar fill, 0–1000.
    pub hp_per_mille: u16,
    pub status: FoeStatus,
    pub shiny: bool,
    /// Pokédex caught flag; `None` when unknown.
    pub caught: Option<bool>,
}

/// Our battling Pokémon and its current/maximum HP.
#[derive(Debug, Clone, Copy)]
pub struct Lead<'a> {
    pub member: &'a Member,
    pub hp: (u16, u16),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatchPlan {
    pub ball: String,
    /// The first action and the odds of the whole attempt.
    pub odds: Odds,
    pub shiny: bool,
    /// The species a hunt is after ([`plan_catch_wanted`]).
    pub wanted: bool,
}

impl CatchPlan {
    /// "first Sleep Powder, P(catch) 0.95, P(KO) 0.03, ~1.3 balls, risk 0.0004".
    pub fn summary(&self) -> String {
        let o = &self.odds.outlook;
        format!(
            "first {}, P(catch) {:.2}, P(KO) {:.2}, ~{:.1} balls, risk {:.4}",
            self.odds.choice, o.catch, o.foe_faints, o.balls, o.lead_faints
        )
    }
}

/// The Poké Balls pocket, `None` when unknown. A tracked list (e.g. after a
/// throw) counts as known.
pub fn balls_held(state: &GameState) -> Option<Vec<(String, u16)>> {
    state.bag.pockets.get(&Pocket::PokeBalls)?.value.clone()
}

/// The held ball with the highest multiplier; ties: cheapest, then name.
/// The Master Ball is never chosen.
pub fn best_ball(data: &GameData, balls: &[(String, u16)]) -> Option<String> {
    let price = |item: &str| data.items.get(item).map_or(u32::MAX, |i| i.price);
    balls
        .iter()
        .filter(|(_, n)| *n > 0)
        .filter_map(|(item, _)| Some((ball_multiplier(item)?, item)))
        .min_by(|(ma, a), (mb, b)| mb.cmp(ma).then(price(a).cmp(&price(b))).then(a.cmp(b)))
        .map(|(_, item)| item.clone())
}

/// Balls an attempt may throw out of `count` held: all for a shiny, those
/// above the shiny reserve otherwise.
pub fn ball_budget(count: u16, shiny: bool) -> u16 {
    if shiny {
        count
    } else {
        count.saturating_sub(SHINY_RESERVE)
    }
}

/// P(our lead faints within `turns` attacks of the foe's most damaging move),
/// the foe at iv 31 and us at iv 0 from our current HP. 1 when either side
/// is unknown to the game data.
pub fn risk(data: &GameData, lead: &Lead, foe: &Foe, turns: u32) -> f64 {
    let (Some(them), Some(us)) = (foe_combatant(data, foe, 31), our_combatant(data, lead, 0))
    else {
        return 1.0;
    };
    faint_probability(data, &them, &us, turns as usize)
}

/// Share of max HP (per mille) the lead needs to try a non-shiny catch.
pub const CATCH_MIN_HP: u32 = 750;

/// The lead's status as known in the game state (from battle text).
fn lead_status(state: &GameState) -> Option<Status> {
    state.party.value.as_ref()?.first()?.status.value
}

/// The odds with `means`; `asleep_for`: our actions since the foe was
/// seen asleep; `wanted`: the species a hunt is after.
fn odds(
    data: &GameData,
    lead: &Lead,
    foe: &Foe,
    asleep_for: u8,
    means: &Means,
    wanted: bool,
) -> Option<Odds> {
    let view = FoeView {
        species: &foe.species,
        level: foe.level,
        hp_per_mille: foe.hp_per_mille,
        status: foe.status,
        asleep_for,
    };
    let weights = if foe.shiny {
        SHINY_LEAD_FAINT
    } else if wanted {
        WANTED_LEAD_FAINT
    } else {
        LEAD_FAINT
    };
    catch_odds::best(data, lead, &view, means, weights)
}

/// What an attempt may do: throw `ball`, `balls` of them.
fn means<'a>(ball: &str, balls: u16, status_allowed: bool, disabled: Option<&'a str>) -> Means<'a> {
    Means {
        ball: ball_multiplier(ball).unwrap_or(10),
        balls,
        status_allowed,
        disabled,
    }
}

/// Decide whether to catch `foe`, and how. `Err` carries the reason not to.
pub fn plan_catch(
    data: &GameData,
    state: &GameState,
    lead: &Lead,
    foe: &Foe,
    trainer: bool,
) -> Result<CatchPlan, String> {
    plan_catch_wanted(data, state, lead, foe, trainer, false)
}

/// [`plan_catch`] for the species a hunt is after (`wanted`): declined
/// only when it is caught already, no ball is held, or the attempt puts
/// our lead's risk above [`RISK_LIMIT`]. Switch, Route 24: a hunt ran from
/// every WEEDLE it met, the catch declined for the reserve or the odds.
pub fn plan_catch_wanted(
    data: &GameData,
    state: &GameState,
    lead: &Lead,
    foe: &Foe,
    trainer: bool,
    wanted: bool,
) -> Result<CatchPlan, String> {
    if trainer {
        return Err("a trainer's Pokémon can't be caught".into());
    }
    // The hunt ends once its species is marked caught: an unread icon on
    // the wanted one is not a reason to let it go.
    let caught_ok = foe.caught == Some(false) || (wanted && foe.caught != Some(true));
    if !foe.shiny && !caught_ok {
        return Err(format!("{}: caught flag {:?}", foe.species, foe.caught));
    }
    // Throwing all balls (the reserve too): a shiny, or the hunt's species.
    let any_ball = foe.shiny || wanted;
    // A catch costs the lead HP (weakening turns, throws): only a healthy
    // lead tries for a non-shiny, so the story's battles aren't fought
    // worn down. The wanted species is the goal: the risk limit alone
    // guards the lead.
    if !any_ball {
        let (hp, max) = lead.hp;
        if u32::from(hp) * 1000 < u32::from(max) * CATCH_MIN_HP {
            return Err(format!("the lead is at {hp}/{max} HP, below 75 %"));
        }
        if let Some(status) = lead_status(state).filter(|s| !matches!(s, Status::Healthy)) {
            return Err(format!("the lead is {status:?}"));
        }
    }
    let ball = match balls_held(state) {
        Some(balls) => best_ball(data, &balls).ok_or("no usable ball held")?,
        None if any_ball => "ITEM_POKE_BALL".to_owned(),
        None => return Err("ball count unknown (pocket not audited)".into()),
    };
    // An unknown pocket (a shiny or the wanted species): plan with a
    // reserve's worth.
    let count = ball_count(state).unwrap_or(SHINY_RESERVE);
    let budget = ball_budget(count, any_ball);
    if budget == 0 {
        return Err(if any_ball {
            "no ball held".into()
        } else {
            format!("{count} balls: none above the shiny reserve {SHINY_RESERVE}")
        });
    }
    let odds = odds(
        data,
        lead,
        foe,
        0,
        &means(&ball, budget, true, None),
        wanted,
    )
    .ok_or_else(|| format!("{} unknown to the game data", foe.species))?;
    let plan = CatchPlan {
        ball,
        odds,
        shiny: foe.shiny,
        wanted,
    };
    if !foe.shiny {
        let o = &plan.odds.outlook;
        // Any chance at the wanted species beats running from it, unless
        // the odds say RUN: every attempt risks the lead more than the
        // catch is worth.
        if wanted && plan.odds.choice == Choice::Run {
            return Err(format!(
                "the odds say RUN: the lead's risk outweighs the catch ({})",
                plan.summary()
            ));
        }
        if !wanted && (plan.odds.choice == Choice::Run || o.catch < MIN_CATCH_CHANCE) {
            return Err(format!(
                "P(catch) {:.2} with {budget} balls, below {MIN_CATCH_CHANCE} ({})",
                o.catch,
                plan.summary()
            ));
        }
        if o.lead_faints > RISK_LIMIT {
            return Err(format!(
                "risk {:.4} above {RISK_LIMIT} ({})",
                o.lead_faints,
                plan.summary()
            ));
        }
    }
    Ok(plan)
}

/// Our side: the summary's stats when they fit the level, else all at `iv`.
fn our_combatant(data: &GameData, lead: &Lead, iv: u32) -> Option<Combatant> {
    let m = lead.member;
    let mut us = Combatant::new(data, &m.species, m.level, m.moves.clone(), iv)?;
    if let Some(stats) = m.stats(data) {
        us.stats = stats;
    }
    us.hp = u32::from(lead.hp.0);
    Some(us)
}

fn foe_combatant(data: &GameData, foe: &Foe, iv: u32) -> Option<Combatant> {
    let moves = data.default_moves(&foe.species, foe.level);
    Combatant::new(data, &foe.species, foe.level, moves, iv)
}

// ---- The catch in battle (flow) ----

/// Frames on the command menu to identify the wild opponent; after that the
/// battle goes on without a catch.
const IDENTIFY_FRAMES: u64 = 180;
/// Retries (unmet expectations, or [`BAG_WAIT_FRAMES`] of waiting) in one
/// phase of the battle bag, and in the whole throw.
const BAG_RETRIES: u32 = 12;
const BAG_TOTAL_RETRIES: u32 = 40;
const BAG_WAIT_FRAMES: u64 = 60;
/// B presses to leave the bag after giving up a throw.
const MAX_CLOSES: u32 = 8;
/// Frames an attempt may wait on a battle menu for HUD numbers that read
/// (and agree on two frames) before it is abandoned.
const HUD_WAIT_FRAMES: u64 = 300;

/// A catch attempt in progress.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub plan: CatchPlan,
    /// The status move was used (it is used once, hit or miss).
    pub opened: bool,
    /// Balls that broke free so far.
    pub throws: u32,
    /// The foe's status as told by battle text.
    pub foe_status: FoeStatus,
    /// Balls held (Master Ball excluded): the tracked count when the
    /// attempt began, one less per ball that broke free, and the battle
    /// bag's own reading once a throw reads it. A non-shiny attempt stops
    /// at [`SHINY_RESERVE`].
    pub balls: Option<u16>,
    /// Turns (command menus) the foe has been seen asleep, this one
    /// included.
    pub asleep_turns: u8,
}

/// What the odds of a turn were solved from: (our HP, the foe's bar, its
/// status, turns asleep, balls to throw, status move allowed, disabled
/// move).
type OddsKey = ((u16, u16), u16, FoeStatus, u8, u16, bool, Option<String>);

/// (species, level, caught icon, shiny reading) as read on the command menu.
type Identity = (String, u8, bool, Option<ShinyReading>);
/// (on the command menu, our HP numbers, the foe's HP bar).
type HudReading = (bool, (u16, u16), u16);

/// Per-battle catching memory, owned by BattleMemory.
#[derive(Debug, Default, Clone)]
pub struct CatchMemory {
    /// The wild opponent was identified (or given up on) and the catch
    /// decided.
    pub decided: bool,
    pub attempt: Option<Attempt>,
    /// The species caught in this battle ("Gotcha!").
    pub caught: Option<String>,
    /// The identified wild opponent: (species, level).
    pub foe: Option<(String, u8)>,
    /// The PC box the catch was placed in (0-based), read from text.
    pub box_index: Option<u8>,
    /// An attempt was abandoned for its risk: RUN.
    pub flee: bool,
    /// The species the hunt is after: caught unless our lead is at risk.
    pub wanted: Option<String>,
    /// "The TRAINER blocked the BALL!": this is a trainer battle.
    pub blocked: bool,
    /// The current throw through the battle bag.
    pub thrower: Option<Thrower>,
    /// The move being chosen is the status opener.
    opener_chosen: bool,
    identity: Option<(u64, Identity)>,
    identify_since: Option<u64>,
    hud: Option<(u64, HudReading)>,
    /// First frame of the current wait for the HUD.
    hud_waiting_since: Option<u64>,
    /// This turn's command menu was counted (for the sleep turns).
    turn_counted: bool,
    /// The last odds solved and what from (solved once per reading).
    odds: Option<(OddsKey, Odds)>,
    /// The last page read once (awaiting a second frame) and the last page
    /// applied.
    page: Option<(u64, String)>,
    applied: String,
    pokedex_seen: Option<u64>,
}

impl CatchMemory {
    /// The attempt's current choice: the last odds solved, else the plan's
    /// first action.
    fn choice(&self) -> Option<&Choice> {
        self.odds
            .as_ref()
            .map(|(_, odds)| &odds.choice)
            .or_else(|| self.attempt.as_ref().map(|a| &a.plan.odds.choice))
    }

    /// Reads battle text on every battle frame: the throw's result, the
    /// foe's status, the nickname question (a catch) and the PC box. A page
    /// counts once two frames read the same text, and only once; that page
    /// is returned for the other battle-text readers.
    pub fn observe(&mut self, o: &Observation) -> Option<String> {
        // The battle is back after USE: that throw is over.
        if o.battle.is_some() && self.thrower.as_ref().is_some_and(Thrower::used) {
            self.thrower = None;
        }
        // A battle frame without a menu (text, animation) ends the turn's
        // menu: the next menu's HUD must be confirmed afresh, never by a
        // reading from before the turn.
        if o.battle.as_ref().is_some_and(|b| b.menu.is_none()) {
            self.hud = None;
            self.turn_counted = false;
        }
        let Some(d) = &o.dialogue else {
            return None;
        };
        let page = d.lines.join(" ");
        if page.trim().is_empty() {
            return None;
        }
        match &self.page {
            Some((frame, seen)) if *seen == page && *frame < o.frame_id => {}
            Some((_, seen)) if *seen == page => return None,
            _ => {
                self.page = Some((o.frame_id, page));
                return None;
            }
        }
        if page == self.applied {
            return None;
        }
        self.applied = page.clone();
        let species = self.foe.as_ref().map(|(s, _)| s.clone());
        match throw_text(&page) {
            Some(ThrowOutcome::Caught) => self.caught = species.clone(),
            Some(ThrowOutcome::BrokeFree) => {
                if let Some(a) = &mut self.attempt {
                    a.throws += 1;
                    a.balls = a.balls.map(|n| n.saturating_sub(1));
                }
            }
            None => {}
        }
        if let (Some(a), Some(species)) = (&mut self.attempt, &species) {
            if let Some(status) = foe_status_text(&page, &printed_name(species)) {
                if status != FoeStatus::Asleep {
                    a.asleep_turns = 0;
                }
                a.foe_status = status;
            }
        }
        // Only a caught Pokémon gets the nickname question.
        if self.caught.is_none() && is_nickname_question(&page) {
            self.caught = species;
        }
        if let Some(index) = box_from_text(&page) {
            self.box_index = Some(index);
        }
        // Only a trainer blocks a ball: no catch in this battle.
        if page.contains("blocked the BALL") {
            self.blocked = true;
            self.attempt = None;
            self.thrower = None;
        }
        Some(page)
    }

    /// A move choice was confirmed: the opener, if it was one, is used.
    pub fn on_move_confirmed(&mut self) {
        if std::mem::take(&mut self.opener_chosen) {
            if let Some(a) = &mut self.attempt {
                a.opened = true;
            }
        }
    }
}

/// Reads battle text into the battle's memory ([`CatchMemory::observe`]);
/// a blocked ball marks the battle as a trainer's. Returns the page newly
/// read on two frames, if any.
pub fn observe(memory: &mut BattleMemory, o: &Observation) -> Option<String> {
    let page = memory.catch.observe(o);
    if memory.catch.blocked {
        memory.trainer = true;
    }
    page
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrowOutcome {
    Caught,
    BrokeFree,
}

/// The game's four "broke free" pages (0–3 shakes), as (first, second) line.
const BROKE_FREE: [(&str, &str); 4] = [
    ("Oh, no!", "The POKéMON broke free!"),
    ("Aww!", "It appeared to be caught!"),
    ("Aargh!", "Almost had it!"),
    ("Shoot!", "It was so close, too!"),
];

/// Facts in battle text about the throw: "Gotcha! X was caught!" or one of
/// the broke-free pages. Anything else (half a page included) is `None`.
pub fn throw_text(page: &str) -> Option<ThrowOutcome> {
    if page.contains("Gotcha!") && page.contains(" was caught!") {
        return Some(ThrowOutcome::Caught);
    }
    BROKE_FREE
        .iter()
        .any(|(a, b)| page.contains(a) && page.contains(b))
        .then_some(ThrowOutcome::BrokeFree)
}

/// The wild foe's status as told by battle text: "Wild X fell asleep!" /
/// "is fast asleep." → asleep, "Wild X is paralyzed!…" → paralysed,
/// "Wild X woke up!" → `FoeStatus::None`. `foe_name` is the printed name
/// (`?` in the reading matches any letter).
pub fn foe_status_text(page: &str, foe_name: &str) -> Option<FoeStatus> {
    let after = &page[page.find("Wild ")? + "Wild ".len()..];
    let len = foe_name.chars().count();
    let name: String = after.chars().take(len).collect();
    if !crate::bag::fits(foe_name, &name) {
        return None;
    }
    let rest = after[name.len()..].trim_start();
    let starts = |phrases: &[&str]| phrases.iter().any(|p| rest.starts_with(p));
    if starts(&["fell asleep!", "is fast asleep", "is already asleep"]) {
        Some(FoeStatus::Asleep)
    } else if starts(&["is paralyzed!", "is already paralyzed"]) {
        Some(FoeStatus::Paralyzed)
    } else if starts(&["woke up!"]) {
        Some(FoeStatus::None)
    } else {
        None
    }
}

/// "Give a nickname to the captured X?"
pub fn is_nickname_question(page: &str) -> bool {
    page.contains("Give a nickname to the")
}

/// The PC pages after a catch, which the game prints while the YES/NO box
/// of the nickname question is still drawn (flash-4: the battle step took
/// the lingering box for a question it did not know and failed).
pub use pokebot_sense::text::{box_from_text, is_pc_transfer_text};

fn log(detail: String) -> GameEvent {
    GameEvent::GoalProgress {
        goal: "Story".into(),
        phase: "Catch".into(),
        detail,
    }
}

/// Identifies the wild opponent once per battle, on the command menu, once
/// two frames agree on its species (the HUD name resolving to exactly one),
/// level, caught icon and shiny reading. Emits `SpeciesSeen` or
/// `SpeciesCaught` (and `ShinySeen`), then plans the catch.
pub fn identify(
    o: &Observation,
    data: &GameData,
    state: &GameState,
    party: &Party,
    memory: &mut BattleMemory,
    events: &mut Vec<GameEvent>,
) {
    if memory.trainer || memory.catch.decided {
        return;
    }
    let Some(battle) = &o.battle else { return };
    if !matches!(battle.menu, Some(BattleMenu::Command { .. })) {
        return;
    }
    let c = &mut memory.catch;
    let since = *c.identify_since.get_or_insert(o.frame_id);
    let reading = identify_opponent(data, o)
        .zip(battle.opponent_caught)
        .map(|(foe, caught)| (foe.species, foe.level, caught, battle.opponent_shiny));
    let confirmed = match (reading, c.identity.take()) {
        (Some(now), Some((frame, seen))) if now == seen && frame < o.frame_id => Some(now),
        (Some(now), Some((frame, seen))) if now == seen => {
            c.identity = Some((frame, seen));
            None
        }
        (Some(now), _) => {
            c.identity = Some((o.frame_id, now));
            None
        }
        (None, _) => None,
    };
    let Some((species, level, caught, shiny)) = confirmed else {
        if o.frame_id.saturating_sub(since) >= IDENTIFY_FRAMES {
            c.decided = true;
            events.push(log(format!(
                "wild opponent not identified ({:?} Lv{:?}): no catch",
                battle.opponent_name, battle.opponent_level
            )));
        }
        return;
    };
    c.decided = true;
    c.foe = Some((species.clone(), level));
    events.push(if caught {
        GameEvent::SpeciesCaught {
            species: species.clone(),
        }
    } else {
        GameEvent::SpeciesSeen {
            species: species.clone(),
        }
    });
    match shiny {
        Some(ShinyReading::Shiny) => events.push(GameEvent::ShinySeen {
            species: species.clone(),
        }),
        Some(ShinyReading::Unclear) => events.push(log(format!(
            "{species}: shiny reading unclear, treated as not shiny"
        ))),
        _ => {}
    }
    let Some(member) = party.lead() else {
        events.push(log(format!("{species}: no catch, the lead is unknown")));
        return;
    };
    let Some(hp) = battle
        .player_hp_numbers
        .filter(|hp| crate::party::plausible_hp_for(*hp, member.level, Some(&member.species)))
        .or_else(|| {
            member.hp.filter(|hp| {
                crate::party::plausible_hp_for(*hp, member.level, Some(&member.species))
            })
        })
    else {
        events.push(log(format!("{species}: no catch, our HP is unknown")));
        return;
    };
    let lead = Lead { member, hp };
    let foe = Foe {
        species: species.clone(),
        level,
        hp_per_mille: battle.opponent_hp.unwrap_or(1000),
        status: FoeStatus::None,
        shiny: shiny == Some(ShinyReading::Shiny),
        caught: Some(caught),
    };
    let wanted = c.wanted.as_deref() == Some(species.as_str());
    match plan_catch_wanted(data, state, &lead, &foe, false, wanted) {
        Ok(plan) => {
            events.push(log(format!(
                "catching {species} Lv{level} with {}: {}{}{}",
                plan.ball,
                plan.summary(),
                if plan.shiny { " (shiny)" } else { "" },
                if plan.wanted { " (hunted)" } else { "" }
            )));
            c.attempt = Some(Attempt {
                plan,
                opened: false,
                throws: 0,
                foe_status: FoeStatus::None,
                balls: ball_count(state),
                asleep_turns: 0,
            });
        }
        Err(reason) => events.push(log(format!("not catching {species} Lv{level}: {reason}"))),
    }
}

/// The battle input during a catch attempt. `None` hands the menu back to
/// the ordinary battle logic (the attempt was abandoned, or can't go on).
///
/// Every decision uses HUD numbers that two frames agreed on. Each turn
/// the odds are solved again from them (our HP, the foe's bar and status,
/// the balls left) and their choice is played: throw, the status move, an
/// attack, or RUN. A non-shiny attempt is abandoned for RUN when the odds
/// say RUN or put the lead's risk above the limit; a shiny one throws.
pub(crate) fn attempt_decision(
    o: &Observation,
    policy: &BattlePolicy,
    memory: &mut BattleMemory,
    party: &Party,
    data: &GameData,
    events: &mut Vec<GameEvent>,
) -> Option<Decision> {
    let battle = o.battle.as_ref()?;
    let menu = battle.menu?;
    memory.catch.attempt.as_ref()?;
    let (species, level) = memory.catch.foe.clone()?;
    let member = party.lead()?;
    let command = matches!(menu, BattleMenu::Command { .. });
    let reading = battle
        .player_hp_numbers
        .filter(|hp| crate::party::plausible_hp_for(*hp, member.level, Some(&member.species)))
        .zip(battle.opponent_hp)
        .map(|(us, foe)| (command, us, foe));
    let confirmed = match (reading, memory.catch.hud) {
        (Some(now), Some((frame, seen))) if seen == now && frame < o.frame_id => Some(now),
        (Some(now), Some((_, seen))) if seen == now => None,
        (Some(now), _) => {
            memory.catch.hud = Some((o.frame_id, now));
            None
        }
        (None, _) => None,
    };
    let Some((_, us_hp, foe_hp)) = confirmed else {
        let since = *memory.catch.hud_waiting_since.get_or_insert(o.frame_id);
        if o.frame_id.saturating_sub(since) > HUD_WAIT_FRAMES {
            memory.catch.attempt = None;
            memory.catch.hud_waiting_since = None;
            events.push(log(format!(
                "{species}: the HUD's HP didn't read the same on two frames: abandoning the catch"
            )));
            return None;
        }
        return Some(Decision::Wait("confirming the HUD on a later frame".into()));
    };
    memory.catch.hud_waiting_since = None;
    // A new turn: one more turn asleep.
    if command && !memory.catch.turn_counted {
        memory.catch.turn_counted = true;
        if let Some(a) = memory.catch.attempt.as_mut() {
            if a.foe_status == FoeStatus::Asleep {
                a.asleep_turns = a.asleep_turns.saturating_add(1);
            }
        }
    }
    let attempt = memory.catch.attempt.clone()?;
    let plan = &attempt.plan;
    let lead = Lead { member, hp: us_hp };
    let foe = Foe {
        species: species.clone(),
        level,
        hp_per_mille: foe_hp,
        status: attempt.foe_status,
        shiny: plan.shiny,
        caught: Some(false),
    };
    if plan.shiny {
        // A shiny never runs for risk; only the low-HP rule may RUN.
        let low = battle.player_hp.is_some_and(|hp| hp < policy.flee_below);
        if command && low && memory.run_attempts < policy.max_run_attempts {
            let BattleMenu::Command { column, row } = menu else {
                unreachable!()
            };
            return Some(step_toward(
                (column, row),
                (1, 1),
                |c, r| BattleMenu::Command { column: c, row: r },
                "RUN",
                Expectation::ScreenIsNot(ScreenState::BattleCommand),
            ));
        }
    } else if let Some(n) = attempt
        .balls
        .filter(|n| !plan.wanted && *n <= SHINY_RESERVE)
    {
        // Only a shiny may throw the reserve: RUN (in a wild battle, when
        // allowed), or fight on.
        memory.catch.attempt = None;
        memory.catch.flee = true;
        events.push(log(format!(
            "{species}: {n} balls, at the shiny reserve {SHINY_RESERVE}: abandoning the catch"
        )));
        return None;
    }
    let balls = ball_budget(
        attempt.balls.unwrap_or(SHINY_RESERVE),
        plan.shiny || plan.wanted,
    );
    let asleep_for = attempt.asleep_turns.saturating_sub(1);
    let key: OddsKey = (
        us_hp,
        foe_hp,
        attempt.foe_status,
        asleep_for,
        balls,
        !attempt.opened,
        memory.disabled.clone(),
    );
    let odds = match &memory.catch.odds {
        Some((k, odds)) if *k == key => odds.clone(),
        _ => {
            let means = means(
                &plan.ball,
                balls,
                !attempt.opened,
                memory.disabled.as_deref(),
            );
            let Some(odds) = odds(data, &lead, &foe, asleep_for, &means, plan.wanted) else {
                memory.catch.attempt = None;
                events.push(log(format!(
                    "{species}: no odds (unknown to the game data): abandoning the catch"
                )));
                return None;
            };
            let out = &odds.outlook;
            events.push(log(format!(
                "{species} at {foe_hp}‰, us {}/{}, {balls} balls: {} (P(catch) {:.2}, P(KO) {:.2}, risk {:.4})",
                us_hp.0, us_hp.1, odds.choice, out.catch, out.foe_faints, out.lead_faints
            )));
            memory.catch.odds = Some((key, odds.clone()));
            odds
        }
    };
    let choice = if plan.shiny {
        // A shiny is never run from: throw what there is.
        match odds.choice {
            Choice::Run if balls > 0 => Choice::Throw,
            Choice::Run => return None,
            c => c,
        }
    } else if odds.choice == Choice::Run || odds.outlook.lead_faints > RISK_LIMIT {
        memory.catch.attempt = None;
        memory.catch.flee = true;
        events.push(log(format!(
            "{species}: {} with risk {:.4} (limit {RISK_LIMIT}): abandoning the catch",
            odds.choice, odds.outlook.lead_faints
        )));
        return None;
    } else {
        odds.choice
    };
    let fight = match &choice {
        Choice::Status(slot, mv) => Some((*slot, mv.clone(), true)),
        Choice::Attack(slot, mv) => Some((*slot, mv.clone(), false)),
        Choice::Throw | Choice::Run => None,
    };
    Some(match menu {
        BattleMenu::Command { column, row } => {
            if fight.is_some() {
                step_toward(
                    (column, row),
                    (0, 0),
                    |c, r| BattleMenu::Command { column: c, row: r },
                    "FIGHT",
                    Expectation::ScreenIs(ScreenState::BattleMoveSelection),
                )
            } else {
                if (column, row) == (1, 0) {
                    memory.catch.thrower =
                        Some(Thrower::new().keeping_reserve(!(plan.shiny || plan.wanted)));
                }
                step_toward(
                    (column, row),
                    (1, 0),
                    |c, r| BattleMenu::Command { column: c, row: r },
                    "BAG",
                    Expectation::BagPocket(String::new()),
                )
            }
        }
        BattleMenu::Moves { column, row } => {
            let Some((slot, mv, status)) = fight else {
                return Some(Decision::Act(Action::new(
                    "back to the command menu to throw",
                    vec![ControllerCommand::Press(Button::B)],
                    Expectation::ScreenIs(ScreenState::BattleCommand),
                    45,
                )));
            };
            memory.last_move = Some(mv.clone());
            memory.last_slot = Some((member.slot, slot));
            let target = (slot % 2, slot / 2);
            if (column, row) == target {
                memory.catch.opener_chosen = status;
            }
            step_toward(
                (column, row),
                target,
                |c, r| BattleMenu::Moves { column: c, row: r },
                &format!("move {} ({})", slot + 1, mv.trim_start_matches("MOVE_")),
                Expectation::ScreenIsNot(ScreenState::BattleMoveSelection),
            )
        }
    })
}

/// Presses A on the Pokédex entry page shown after a first catch, once two
/// frames show it.
pub fn dismiss_pokedex(memory: &mut CatchMemory, o: &Observation) -> Option<Decision> {
    if !o.pokedex_page {
        memory.pokedex_seen = None;
        return None;
    }
    let since = *memory.pokedex_seen.get_or_insert(o.frame_id);
    if since == o.frame_id {
        return Some(Decision::Wait("confirming the Pokédex page".into()));
    }
    Some(Decision::Act(Action::new(
        "close the Pokédex page",
        vec![ControllerCommand::Press(Button::A)],
        Expectation::PokedexPageClosed,
        120,
    )))
}

/// The battle bag (or the frames around it) during a throw.
pub fn in_bag(
    o: &Observation,
    data: &GameData,
    memory: &mut CatchMemory,
    events: &mut Vec<GameEvent>,
) -> Decision {
    let ball = memory
        .attempt
        .as_ref()
        .map(|a| a.plan.ball.clone())
        .unwrap_or_default();
    // The attempt chose to throw, but the bag opened before the cursor was
    // seen on BAG (Switch, Route 1: the command menu read as battle text
    // after "cursor to BAG", its A opened the bag, and the bag was closed
    // as unexpected, over and over with 20 balls held): throw from it.
    if memory.thrower.is_none() && o.bag.is_some() {
        if let Some(plan) = memory
            .attempt
            .as_ref()
            .map(|a| &a.plan)
            .filter(|_| memory.choice() == Some(&Choice::Throw))
        {
            memory.thrower = Some(Thrower::new().keeping_reserve(!(plan.shiny || plan.wanted)));
        }
    }
    let Some(thrower) = &mut memory.thrower else {
        // A bag we didn't open: leave it.
        if o.bag.is_some() {
            return Decision::Act(Action::new(
                "close an unexpected battle bag",
                vec![ControllerCommand::Press(Button::B)],
                Expectation::BagClosed,
                60,
            ));
        }
        return Decision::Wait("battle".into());
    };
    let decision = thrower.next(o, data, &ball, events);
    if let (Some(n), Some(a)) = (thrower.counted(), &mut memory.attempt) {
        a.balls = Some(n);
    }
    match decision {
        Decision::Done(detail) => {
            if thrower.gave_up().is_some() {
                // At the reserve the attempt is over for good: RUN.
                memory.flee |= thrower.at_reserve();
                memory.attempt = None;
                events.push(log(format!("throw abandoned: {detail}")));
            }
            memory.thrower = None;
            Decision::Wait("back to the battle".into())
        }
        other => other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ThrowPhase {
    /// No bag on screen (opening, or the ball on its way after USE).
    #[default]
    Opening,
    /// Another pocket: step toward POKé BALLS.
    Pocket,
    /// The POKé BALLS list.
    List,
    /// The USE/CANCEL prompt.
    Prompt,
}

/// One throw through the battle bag: switch to POKé BALLS (reading the
/// title; the bag remembers pocket and row), read the rows on two agreeing
/// frames (`PocketObserved` when the whole pocket is on screen), re-pick the
/// best ball from them, move ▶ to it, A, then USE on the prompt. Retries are
/// capped; giving up closes the bag with B.
#[derive(Debug, Clone, Default)]
pub struct Thrower {
    phase: ThrowPhase,
    /// A on USE was pressed.
    use_pressed: bool,
    observed: bool,
    /// The ball A was pressed on (the prompt is for it).
    selected: Option<String>,
    /// The last unconfirmed list reading: (frame, ▶ row, rows by item).
    candidate: Option<(u64, u8, Rows)>,
    pending: Option<Expectation>,
    retries: u32,
    total_retries: u32,
    waiting_since: Option<u64>,
    gave_up: Option<String>,
    closes: u32,
    /// Give the throw up when the pocket holds [`SHINY_RESERVE`] balls or
    /// fewer (a non-shiny attempt).
    keep_reserve: bool,
    /// The balls the confirmed rows hold (Master Ball excluded).
    counted: Option<u16>,
    /// The throw was given up because only the reserve is left.
    at_reserve: bool,
}

impl Thrower {
    pub fn new() -> Self {
        Self::default()
    }

    /// Gives the throw up at [`SHINY_RESERVE`] balls or fewer (`keep`:
    /// the attempt is not for a shiny).
    pub fn keeping_reserve(mut self, keep: bool) -> Self {
        self.keep_reserve = keep;
        self
    }

    /// The balls the list read on two agreeing frames holds, once read.
    pub fn counted(&self) -> Option<u16> {
        self.counted
    }

    /// The throw was given up because only the shiny reserve is left.
    pub fn at_reserve(&self) -> bool {
        self.at_reserve
    }

    /// USE was pressed and the bag closed: the ball is on its way.
    pub fn used(&self) -> bool {
        self.use_pressed && self.phase == ThrowPhase::Opening
    }

    /// Why the throw was given up, if it was.
    pub fn gave_up(&self) -> Option<&str> {
        self.gave_up.as_deref()
    }

    /// The next input. `Done` once a given-up throw has closed the bag.
    pub fn next(
        &mut self,
        o: &Observation,
        data: &GameData,
        ball: &str,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        let phase = match &o.bag {
            None => ThrowPhase::Opening,
            Some(b) if b.prompt.is_some() => ThrowPhase::Prompt,
            Some(b) if pocket_from_title(&b.pocket) == Some(Pocket::PokeBalls) => ThrowPhase::List,
            Some(_) => ThrowPhase::Pocket,
        };
        if let Some(reason) = self.gave_up.clone() {
            return self.close(o, phase, &reason);
        }
        let pending = self.pending.take();
        if phase != self.phase {
            self.phase = phase;
            self.retries = 0;
            self.waiting_since = None;
        } else if let Some(expect) = pending {
            if !expect.met(o) {
                self.retry();
            }
        }
        if self.retries > BAG_RETRIES || self.total_retries > BAG_TOTAL_RETRIES {
            let reason = format!("no progress in {phase:?} of the battle bag");
            self.gave_up = Some(reason.clone());
            return self.close(o, phase, &reason);
        }
        match phase {
            ThrowPhase::Opening if self.use_pressed => Decision::Wait("the ball is thrown".into()),
            ThrowPhase::Opening => self.wait(o, "waiting for the battle bag"),
            ThrowPhase::Pocket => {
                let bag = o.bag.as_ref().expect("pocket phase");
                let (Some(at), Some(target)) = (
                    pocket_from_title(&bag.pocket).and_then(pocket_index),
                    pocket_index(Pocket::PokeBalls),
                ) else {
                    return self.wait(o, "reading the pocket title");
                };
                let (button, next) = if at < target {
                    (Button::Right, at + 1)
                } else {
                    (Button::Left, at.saturating_sub(1))
                };
                let title = POCKETS[next].1;
                self.act(
                    &format!("battle bag: {button:?} to {title}"),
                    button,
                    Expectation::BagPocket(title.to_owned()),
                    60,
                )
            }
            ThrowPhase::List => self.list(o, data, ball, events),
            ThrowPhase::Prompt => self.prompt(o),
        }
    }

    fn list(
        &mut self,
        o: &Observation,
        data: &GameData,
        ball: &str,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        let bag = o.bag.as_ref().expect("list phase");
        let Some(cursor) = bag.cursor else {
            return self.wait(o, "looking for the bag's ▶");
        };
        let Some(items) = read_rows(data, &bag.rows).filter(|_| !bag.rows.is_empty()) else {
            return self.wait(o, "reading the ball rows");
        };
        // A count or a row is a fact only once two frames read it the same.
        let view = by_item(data, &bag.rows);
        match self.candidate.take() {
            Some((frame, row, seen)) if frame < o.frame_id && row == cursor && seen == view => {}
            Some(same) if same.0 >= o.frame_id => {
                self.candidate = Some(same);
                return self.wait(o, "confirming the rows on a later frame");
            }
            Some(_) => {
                self.candidate = Some((o.frame_id, cursor, view));
                self.retry();
                return self.wait(o, "re-reading rows that read differently");
            }
            None => {
                self.candidate = Some((o.frame_id, cursor, view));
                return self.wait(o, "confirming the rows on a later frame");
            }
        }
        // With CANCEL on screen and a free row, the pocket can't scroll:
        // this is all of it.
        let whole = bag.rows.len() < 6 && bag.rows.iter().any(|(n, _)| is_cancel(n));
        if whole && !self.observed {
            self.observed = true;
            events.push(GameEvent::PocketObserved {
                pocket: Pocket::PokeBalls,
                items: items.clone(),
            });
        }
        // The count is a fact only for the whole pocket (a scrolled list
        // shows part of it).
        let count = whole.then(|| {
            items
                .iter()
                .filter(|(item, _)| ball_multiplier(item).is_some())
                .fold(0u16, |sum, (_, n)| sum.saturating_add(*n))
        });
        self.counted = count.or(self.counted);
        if let Some(count) = count.filter(|n| self.keep_reserve && *n <= SHINY_RESERVE) {
            let reason = format!("{count} balls: only the shiny reserve is left");
            self.at_reserve = true;
            self.gave_up = Some(reason.clone());
            return self.close(o, ThrowPhase::List, &reason);
        }
        let Some(best) = best_ball(data, &items) else {
            let reason = format!("no usable ball in {items:?}");
            self.gave_up = Some(reason.clone());
            return self.close(o, ThrowPhase::List, &reason);
        };
        let target = bag
            .rows
            .iter()
            .position(|(name, _)| item_key(data, name) == best)
            .expect("the best ball is one of the rows") as u8;
        let name = best.trim_start_matches("ITEM_");
        let note = if best != ball {
            format!(" (instead of {})", ball.trim_start_matches("ITEM_"))
        } else {
            String::new()
        };
        if cursor == target {
            self.selected = Some(best.clone());
            return self.act(
                &format!("throw {name}{note}: select it"),
                Button::A,
                Expectation::BagPrompt,
                60,
            );
        }
        let (button, next) = if target > cursor {
            (Button::Down, cursor + 1)
        } else {
            (Button::Up, cursor - 1)
        };
        self.act(
            &format!("battle bag: {button:?} toward {name}"),
            button,
            Expectation::BagCursorAt(next),
            45,
        )
    }

    fn prompt(&mut self, o: &Observation) -> Decision {
        let bag = o.bag.as_ref().expect("prompt phase");
        let (options, row) = bag.prompt.clone().expect("prompt phase");
        let Some(item) = self.selected.clone() else {
            // Not our prompt: back to the list.
            return self.act(
                "battle bag: leave a prompt we didn't open",
                Button::B,
                Expectation::ScreenIs(ScreenState::Bag),
                45,
            );
        };
        let Some(use_row) = options.iter().position(|l| crate::bag::fits("USE", l)) else {
            return self.wait(o, "reading USE/CANCEL");
        };
        let use_row = use_row as u8;
        if row == use_row {
            self.use_pressed = true;
            return self.act(
                &format!("throw {}: USE", item.trim_start_matches("ITEM_")),
                Button::A,
                Expectation::BagClosed,
                90,
            );
        }
        let (button, next) = if use_row > row {
            (Button::Down, row + 1)
        } else {
            (Button::Up, row - 1)
        };
        self.act(
            &format!("battle bag: {button:?} toward USE"),
            button,
            Expectation::BagPromptAt(next),
            45,
        )
    }

    /// After giving up: B until the bag is gone, then `Done`.
    fn close(&mut self, o: &Observation, phase: ThrowPhase, reason: &str) -> Decision {
        if o.bag.is_none() {
            return Decision::Done(reason.to_owned());
        }
        self.closes += 1;
        if self.closes > MAX_CLOSES {
            return Decision::Fail(format!("battle bag won't close ({reason})"));
        }
        let expect = if phase == ThrowPhase::Prompt {
            Expectation::ScreenIs(ScreenState::Bag)
        } else {
            Expectation::BagClosed
        };
        self.pending = None;
        Decision::Act(Action::new(
            "battle bag: B to give up the throw",
            vec![ControllerCommand::Press(Button::B)],
            expect,
            60,
        ))
    }

    fn act(&mut self, label: &str, button: Button, expect: Expectation, timeout: u64) -> Decision {
        self.waiting_since = None;
        self.candidate = None;
        self.pending = Some(expect.clone());
        Decision::Act(Action::new(
            label,
            vec![ControllerCommand::Press(button)],
            expect,
            timeout,
        ))
    }

    /// Waits; every [`BAG_WAIT_FRAMES`] of waiting counts as a retry.
    fn wait(&mut self, o: &Observation, reason: &str) -> Decision {
        let since = *self.waiting_since.get_or_insert(o.frame_id);
        if o.frame_id.saturating_sub(since) >= BAG_WAIT_FRAMES {
            self.retry();
            self.waiting_since = Some(o.frame_id);
        }
        Decision::Wait(reason.to_owned())
    }

    fn retry(&mut self) {
        self.retries += 1;
        self.total_retries += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pokebot_gamedata::GameData;
    use pokebot_state::{BattleMenu, DefaultReducer, EventRecord, GameEvent, Pocket, StateReducer};

    use super::*;
    use crate::party::Member;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    fn ivysaur(data: &GameData) -> Member {
        let mut member = Member::new(data, "SPECIES_IVYSAUR", 18);
        member.moves = [
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_LEECH_SEED",
            "MOVE_VINE_WHIP",
        ]
        .map(String::from)
        .to_vec();
        member
    }

    fn with_balls(items: &[(&str, u16)]) -> GameState {
        let event = GameEvent::PocketObserved {
            pocket: Pocket::PokeBalls,
            items: items.iter().map(|(i, n)| ((*i).to_owned(), *n)).collect(),
        };
        DefaultReducer.reduce(&GameState::default(), &[EventRecord { frame_id: 1, event }])
    }

    fn foe(species: &str, level: u8) -> Foe {
        Foe {
            species: species.into(),
            level,
            hp_per_mille: 1000,
            status: FoeStatus::None,
            shiny: false,
            caught: Some(false),
        }
    }

    #[test]
    fn uncaught_weak_foe_is_caught() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let plan = plan_catch(&data, &state, &lead, &foe("SPECIES_PIDGEY", 6), false).unwrap();
        assert_eq!(plan.ball, "ITEM_POKE_BALL");
        let o = plan.odds.outlook;
        assert!(o.lead_faints <= RISK_LIMIT, "{plan:?}");
        assert!(o.catch >= MIN_CATCH_CHANCE, "{plan:?}");
        assert!(o.balls >= 1.0, "{plan:?}");
        assert!(!plan.shiny);
        let plan = plan_catch(&data, &state, &lead, &foe("SPECIES_ZUBAT", 9), false).unwrap();
        assert!(plan.odds.outlook.lead_faints <= RISK_LIMIT, "{plan:?}");
    }

    /// Live (Switch, Route 24): a Lv27 IVYSAUR refused every WEEDLE and
    /// CATERPIE ("5 balls: not above the reserve 5 + 2 throws"), then
    /// fainted them with one hit. With balls above the reserve it catches,
    /// opening with a move unlikely to faint them (or a throw).
    #[test]
    fn a_strong_lead_catches_a_frail_foe_without_fainting_it() {
        let Some(data) = data() else { return };
        let mut member = Member::new(&data, "SPECIES_IVYSAUR", 27);
        member.moves = [
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_RAZOR_LEAF",
            "MOVE_VINE_WHIP",
        ]
        .map(String::from)
        .to_vec();
        let lead = Lead {
            member: &member,
            hp: (71, 71),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 8)]);
        for species in ["SPECIES_WEEDLE", "SPECIES_CATERPIE", "SPECIES_PIDGEY"] {
            let plan = plan_catch(&data, &state, &lead, &foe(species, 7), false).unwrap();
            let o = plan.odds.outlook;
            assert!(o.catch > 0.9, "{species}: {plan:?}");
            assert!(o.foe_faints < 0.05, "{species}: {plan:?}");
            assert!(
                !matches!(&plan.odds.choice, Choice::Attack(_, m) if m == "MOVE_RAZOR_LEAF"),
                "{species}: {plan:?}"
            );
        }
    }

    #[test]
    fn caught_species_is_not_caught_again() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut pidgey = foe("SPECIES_PIDGEY", 6);
        pidgey.caught = Some(true);
        assert!(plan_catch(&data, &state, &lead, &pidgey, false).is_err());
    }

    #[test]
    fn unknown_caught_flag_declines() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut pidgey = foe("SPECIES_PIDGEY", 6);
        pidgey.caught = None;
        assert!(plan_catch(&data, &state, &lead, &pidgey, false).is_err());
    }

    #[test]
    fn unknown_ball_count_declines_non_shiny() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let pidgey = foe("SPECIES_PIDGEY", 6);
        assert_eq!(balls_held(&GameState::default()), None);
        assert!(plan_catch(&data, &GameState::default(), &lead, &pidgey, false).is_err());
        // A tracked (stale) list, e.g. after a throw, is still a known count.
        let state = DefaultReducer.reduce(
            &with_balls(&[("ITEM_POKE_BALL", 10)]),
            &[EventRecord {
                frame_id: 2,
                event: GameEvent::ItemsChanged {
                    pocket: Pocket::PokeBalls,
                    item: "ITEM_POKE_BALL".into(),
                    delta: -1,
                    reason: "thrown".into(),
                },
            }],
        );
        assert_eq!(balls_held(&state), Some(vec![("ITEM_POKE_BALL".into(), 9)]));
        assert!(plan_catch(&data, &state, &lead, &pidgey, false).is_ok());
    }

    #[test]
    fn no_catch_at_or_below_the_reserve() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let pidgey = foe("SPECIES_PIDGEY", 6);
        // 5 balls: all of them the shiny reserve.
        let state = with_balls(&[("ITEM_POKE_BALL", 5)]);
        let err = plan_catch(&data, &state, &lead, &pidgey, false).unwrap_err();
        assert!(err.contains("reserve"), "{err}");
        // 6: one ball to throw, and the odds with it decide.
        let state = with_balls(&[("ITEM_POKE_BALL", 6)]);
        let plan = plan_catch(&data, &state, &lead, &pidgey, false).unwrap();
        assert!(plan.odds.outlook.balls <= 1.0 + 1e-9, "{plan:?}");
    }

    #[test]
    fn shiny_uses_the_reserve() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let mut pidgey = foe("SPECIES_PIDGEY", 6);
        pidgey.shiny = true;
        pidgey.caught = Some(true);
        let state = with_balls(&[("ITEM_POKE_BALL", 3)]);
        let plan = plan_catch(&data, &state, &lead, &pidgey, false).unwrap();
        assert_eq!(plan.ball, "ITEM_POKE_BALL");
        assert!(plan.shiny);
        // Unknown pocket: still Ok, planned with a Poké Ball.
        let plan = plan_catch(&data, &GameState::default(), &lead, &pidgey, false).unwrap();
        assert_eq!(plan.ball, "ITEM_POKE_BALL");
        assert!(plan.odds.outlook.balls >= 1.0);
        // No ball at all (only a Master Ball): Err even for a shiny.
        let state = with_balls(&[("ITEM_MASTER_BALL", 1)]);
        assert!(plan_catch(&data, &state, &lead, &pidgey, false).is_err());
    }

    #[test]
    fn risky_foe_is_not_caught() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (3, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 20)]);
        let geodude = foe("SPECIES_GEODUDE", 9);
        let err = plan_catch(&data, &state, &lead, &geodude, false).unwrap_err();
        // At 3/54 the lead is too weak to try at all.
        assert!(err.contains("below 75 %"), "{err}");
        // A shiny is still attempted, throwing at once.
        let shiny = Foe {
            shiny: true,
            ..geodude
        };
        let plan = plan_catch(&data, &state, &lead, &shiny, false).unwrap();
        assert!(plan.shiny);
        assert!(plan.odds.outlook.catch > 0.0, "{plan:?}");
    }

    /// The species a hunt is after is the goal: the reserve, a worn lead,
    /// an unread caught icon or pocket and a low chance don't let it go;
    /// only the lead's risk does (then the hunt runs, heals and comes
    /// back). Switch, Route 24: the hunt ran from every WEEDLE it met.
    #[test]
    fn the_hunted_species_is_declined_only_for_the_leads_risk() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let wanted = |state: &GameState, hp: u16, foe: &Foe| {
            let lead = Lead {
                member: &member,
                hp: (hp, 60),
            };
            plan_catch_wanted(&data, state, &lead, foe, false, true)
        };
        let pidgey = foe("SPECIES_PIDGEY", 6);
        // 5 balls, all the shiny reserve: thrown for the hunted species.
        let reserve = with_balls(&[("ITEM_POKE_BALL", 5)]);
        let plan = wanted(&reserve, 60, &pidgey).unwrap();
        assert!(plan.wanted && !plan.shiny);
        // A worn or paralyzed lead, still safe against it.
        assert!(wanted(&reserve, 30, &pidgey).is_ok());
        let paralyzed = DefaultReducer.reduce(
            &reserve,
            &[EventRecord {
                frame_id: 2,
                event: GameEvent::PartyObserved {
                    slot: 0,
                    species: None,
                    nickname: None,
                    level: None,
                    hp: None,
                    status: Some(Status::Paralyzed),
                    held_item: None,
                },
            }],
        );
        assert!(wanted(&paralyzed, 60, &pidgey).is_ok());
        // An unread caught icon, an unread pocket.
        let unread = Foe {
            caught: None,
            ..pidgey.clone()
        };
        assert!(wanted(&reserve, 60, &unread).is_ok());
        assert!(wanted(&GameState::default(), 60, &pidgey).is_ok());
        // One ball: a low chance still beats running from it.
        let one = with_balls(&[("ITEM_POKE_BALL", 1)]);
        let abra = foe("SPECIES_ABRA", 12);
        assert!(wanted(&one, 60, &abra).is_ok());
        // Declined: caught already, no ball, or the lead at risk.
        let caught = Foe {
            caught: Some(true),
            ..pidgey.clone()
        };
        assert!(wanted(&reserve, 60, &caught).is_err());
        let none = with_balls(&[("ITEM_POKE_BALL", 0)]);
        assert!(wanted(&none, 60, &pidgey).is_err());
        let geodude = foe("SPECIES_GEODUDE", 9);
        let err = wanted(&reserve, 3, &geodude).unwrap_err();
        assert!(err.contains("risk"), "{err}");
        // The same encounters as extras are still declined.
        let lead = Lead {
            member: &member,
            hp: (60, 60),
        };
        assert!(plan_catch(&data, &reserve, &lead, &pidgey, false).is_err());
    }

    /// Live: catches in Mt. Moon (weakening turns, throws) wore the lead
    /// down to 14/60 and PAR before Miguel. A non-shiny catch starts only
    /// with the lead at 75 % HP or more and no major status.
    #[test]
    fn a_weak_or_statused_lead_does_not_try_a_catch() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let pidgey = foe("SPECIES_PIDGEY", 6);
        let state = with_balls(&[("ITEM_POKE_BALL", 20)]);
        let at = |hp| Lead {
            member: &member,
            hp: (hp, 60),
        };
        assert!(plan_catch(&data, &state, &at(45), &pidgey, false).is_ok());
        let err = plan_catch(&data, &state, &at(44), &pidgey, false).unwrap_err();
        assert!(err.contains("below 75 %"), "{err}");
        // Paralyzed (read from battle text into the state).
        let paralyzed = DefaultReducer.reduce(
            &state,
            &[EventRecord {
                frame_id: 2,
                event: GameEvent::PartyObserved {
                    slot: 0,
                    species: None,
                    nickname: None,
                    level: None,
                    hp: None,
                    status: Some(Status::Paralyzed),
                    held_item: None,
                },
            }],
        );
        let err = plan_catch(&data, &paralyzed, &at(60), &pidgey, false).unwrap_err();
        assert!(err.contains("Paralyzed"), "{err}");
        // A shiny is still tried.
        let shiny = Foe {
            shiny: true,
            ..pidgey
        };
        assert!(plan_catch(&data, &paralyzed, &at(20), &shiny, false).is_ok());
    }

    #[test]
    fn trainer_mons_are_never_caught() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut pidgey = foe("SPECIES_PIDGEY", 6);
        assert!(plan_catch(&data, &state, &lead, &pidgey, true).is_err());
        pidgey.shiny = true;
        assert!(plan_catch(&data, &state, &lead, &pidgey, true).is_err());
    }

    #[test]
    fn asleep_foe_gets_no_status_move() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let asleep = Foe {
            status: FoeStatus::Asleep,
            ..foe("SPECIES_PIDGEY", 6)
        };
        let plan = plan_catch(&data, &state, &lead, &asleep, false).unwrap();
        assert!(!matches!(plan.odds.choice, Choice::Status(..)), "{plan:?}");
        // Without PP left, no status move either.
        let mut tired = member.clone();
        tired.pp_used.insert("MOVE_SLEEP_POWDER".into(), 15);
        let lead = Lead {
            member: &tired,
            hp: (54, 54),
        };
        let plan = plan_catch(&data, &state, &lead, &foe("SPECIES_PIDGEY", 6), false).unwrap();
        assert!(!matches!(plan.odds.choice, Choice::Status(..)), "{plan:?}");
    }

    #[test]
    fn best_ball_prefers_multiplier_then_price() {
        let Some(data) = data() else { return };
        let held = |v: &[(&str, u16)]| -> Vec<(String, u16)> {
            v.iter().map(|(i, n)| ((*i).to_owned(), *n)).collect()
        };
        assert_eq!(
            best_ball(
                &data,
                &held(&[("ITEM_POKE_BALL", 5), ("ITEM_GREAT_BALL", 1)])
            ),
            Some("ITEM_GREAT_BALL".into())
        );
        // Premier and Poké Ball both cost ¥200: the cheaper, then the name.
        let price = |i: &str| data.items[i].price;
        let expected = match price("ITEM_PREMIER_BALL").cmp(&price("ITEM_POKE_BALL")) {
            std::cmp::Ordering::Less => "ITEM_PREMIER_BALL",
            _ => "ITEM_POKE_BALL",
        };
        assert_eq!(
            best_ball(
                &data,
                &held(&[("ITEM_PREMIER_BALL", 1), ("ITEM_POKE_BALL", 1)])
            ),
            Some(expected.into())
        );
        assert_eq!(best_ball(&data, &held(&[("ITEM_MASTER_BALL", 1)])), None);
        assert_eq!(best_ball(&data, &held(&[("ITEM_POKE_BALL", 0)])), None);
    }

    #[test]
    fn every_broke_free_variant_means_throw_again() {
        for page in [
            "Oh, no! The POKéMON broke free!",
            "Aww! It appeared to be caught!",
            "Aargh! Almost had it!",
            "Shoot! It was so close, too!",
        ] {
            assert_eq!(throw_text(page), Some(ThrowOutcome::BrokeFree), "{page}");
        }
        assert_eq!(
            throw_text("Gotcha! PIDGEY was caught!"),
            Some(ThrowOutcome::Caught)
        );
        assert_eq!(
            throw_text("Gotcha! PID?EY was caught!"),
            Some(ThrowOutcome::Caught)
        );
        // Unmatched text (and half a page) never reads as caught.
        for page in [
            "PIDGEY used TACKLE!",
            "RED used POKé BALL!",
            "Gotcha!",
            "PIDGEY was caught!",
            "Aww!",
            "",
        ] {
            assert_eq!(throw_text(page), None, "{page}");
        }
    }

    #[test]
    fn foe_status_from_text() {
        assert_eq!(
            foe_status_text("Wild PIDGEY fell asleep!", "PIDGEY"),
            Some(FoeStatus::Asleep)
        );
        assert_eq!(
            foe_status_text(
                "Wild PIDGEY is paralyzed! It may be unable to move!",
                "PIDGEY"
            ),
            Some(FoeStatus::Paralyzed)
        );
        assert_eq!(
            foe_status_text("Wild PIDGEY woke up!", "PIDGEY"),
            Some(FoeStatus::None)
        );
        assert_eq!(
            foe_status_text("Wild PID?EY is fast asleep.", "PIDGEY"),
            Some(FoeStatus::Asleep)
        );
        // Our own lead, another species, or no status: nothing.
        for page in [
            "IVYSAUR fell asleep!",
            "Wild RATTATA fell asleep!",
            "Wild PIDGEY used TACKLE!",
        ] {
            assert_eq!(foe_status_text(page, "PIDGEY"), None, "{page}");
        }
    }

    /// A wild battle frame: IVYSAUR Lv18 54/54 against PIDGEY Lv6.
    fn wild(frame: u64, menu: BattleMenu, foe_hp: u16) -> Observation {
        wild_as(frame, menu, foe_hp, "PIDGEY", 6)
    }

    fn wild_as(frame: u64, menu: BattleMenu, foe_hp: u16, name: &str, level: u8) -> Observation {
        let mut o = Observation::bare(
            frame,
            pokebot_state::Observed {
                value: match menu {
                    BattleMenu::Command { .. } => pokebot_state::ScreenState::BattleCommand,
                    BattleMenu::Moves { .. } => pokebot_state::ScreenState::BattleMoveSelection,
                },
                detector: "test".into(),
            },
            Default::default(),
        );
        o.battle = Some(pokebot_state::BattleObservation {
            menu: Some(menu),
            player_name: Some("IVYSAUR".into()),
            player_level: Some(18),
            player_hp_numbers: Some((54, 54)),
            opponent_name: Some(name.into()),
            opponent_level: Some(level),
            player_hp: Some(1000),
            opponent_hp: Some(foe_hp),
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: Some(false),
            opponent_shiny: match menu {
                BattleMenu::Command { .. } => Some(pokebot_state::ShinyReading::Normal),
                BattleMenu::Moves { .. } => None,
            },
            level_up_stats: None,
        });
        o
    }

    /// The decision on the second of two agreeing frames.
    fn decide_on(
        data: &GameData,
        party: &Party,
        memory: &mut BattleMemory,
        o: &Observation,
    ) -> String {
        let policy = crate::battle::BattlePolicy::default();
        let mut events = Vec::new();
        let mut next = o.clone();
        next.frame_id += 1;
        crate::battle::decide(o, &policy, memory, party, data, &mut events);
        match crate::battle::decide(&next, &policy, memory, party, data, &mut events) {
            Some(crate::Decision::Act(a)) => a.label,
            Some(crate::Decision::Wait(r)) => format!("wait: {r}"),
            Some(crate::Decision::Done(r)) => format!("done: {r}"),
            Some(crate::Decision::Fail(r)) => format!("fail: {r}"),
            None => "none".into(),
        }
    }

    #[test]
    fn a_catch_sleeps_weakens_then_throws() {
        let Some(data) = data() else { return };
        let party = Party {
            members: vec![ivysaur(&data)],
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut memory = BattleMemory::default();
        let mut events = Vec::new();
        let command = BattleMenu::Command { column: 0, row: 0 };
        let moves = BattleMenu::Moves { column: 0, row: 0 };
        // Nothing is chosen before the opponent is identified.
        assert_eq!(
            decide_on(&data, &party, &mut memory, &wild(1, command, 1000)),
            "wait: identifying the wild opponent"
        );
        // Identified on two agreeing command-menu frames.
        identify(
            &wild(1, command, 1000),
            &data,
            &state,
            &party,
            &mut memory,
            &mut events,
        );
        assert!(!memory.catch.decided);
        identify(
            &wild(2, command, 1000),
            &data,
            &state,
            &party,
            &mut memory,
            &mut events,
        );
        assert!(memory.catch.decided);
        assert_eq!(
            events.first(),
            Some(&GameEvent::SpeciesSeen {
                species: "SPECIES_PIDGEY".into()
            })
        );
        assert!(memory.catch.attempt.is_some(), "{events:?}");
        // The first action is the plan's: a move through FIGHT, or BAG.
        let first = memory
            .catch
            .attempt
            .as_ref()
            .unwrap()
            .plan
            .odds
            .choice
            .clone();
        let label = decide_on(&data, &party, &mut memory, &wild(3, command, 1000));
        match &first {
            Choice::Status(..) | Choice::Attack(..) => {
                assert_eq!(label, "choose FIGHT");
                let label = decide_on(&data, &party, &mut memory, &wild(5, moves, 1000));
                assert!(label.contains(&first.to_string()), "{label} vs {first}");
            }
            Choice::Throw => assert!(label.contains("BAG"), "{label}"),
            Choice::Run => panic!("an attempt that runs"),
        }
        // Asleep at full HP: IVYSAUR Lv18's attacks would likely faint a
        // Lv6 PIDGEY (20 HP): throw.
        let attempt = memory.catch.attempt.as_mut().unwrap();
        attempt.opened = true;
        attempt.foe_status = FoeStatus::Asleep;
        let label = decide_on(&data, &party, &mut memory, &wild(7, command, 1000));
        assert!(label.contains("BAG"), "{label}");
        // Weakened (200‰): BAG.
        let label = decide_on(&data, &party, &mut memory, &wild(9, command, 200));
        assert!(label.contains("BAG"), "{label}");
        assert_eq!(label, "cursor to BAG: Right");
        // On BAG itself: A opens the bag, and a throw begins.
        let label = decide_on(
            &data,
            &party,
            &mut memory,
            &wild(11, BattleMenu::Command { column: 1, row: 0 }, 200),
        );
        assert_eq!(label, "choose BAG");
        assert!(memory.catch.thrower.is_some());
    }

    /// The moves menu goes to the odds' move; the status move counts as
    /// used once chosen; a disabled move is never chosen; a sleeping foe
    /// at a low bar is thrown at.
    #[test]
    fn the_odds_choice_is_played_on_the_moves_menu() {
        let Some(data) = data() else { return };
        // BULBASAUR Lv12 against a Lv6 PIDGEY (20 HP).
        let mut member = Member::new(&data, "SPECIES_BULBASAUR", 12);
        member.moves = [
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_LEECH_SEED",
            "MOVE_VINE_WHIP",
        ]
        .map(String::from)
        .to_vec();
        let party = Party {
            members: vec![member],
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut memory = BattleMemory::default();
        let mut events = Vec::new();
        let command = BattleMenu::Command { column: 0, row: 0 };
        let moves = BattleMenu::Moves { column: 0, row: 0 };
        for frame in [1, 2] {
            identify(
                &wild(frame, command, 1000),
                &data,
                &state,
                &party,
                &mut memory,
                &mut events,
            );
        }
        let plan = memory
            .catch
            .attempt
            .as_ref()
            .expect("an attempt")
            .plan
            .clone();
        let (Choice::Status(slot, mv) | Choice::Attack(slot, mv)) = plan.odds.choice.clone() else {
            panic!("expected a move first: {plan:?}");
        };
        let label = decide_on(&data, &party, &mut memory, &wild(3, moves, 1000));
        assert!(label.contains(mv.trim_start_matches("MOVE_")), "{label}");
        // Moving the cursor is not using it; on the move, A chooses it.
        memory.catch.on_move_confirmed();
        assert!(!memory.catch.attempt.as_ref().unwrap().opened);
        let on_it = BattleMenu::Moves {
            column: slot % 2,
            row: slot / 2,
        };
        let label = decide_on(&data, &party, &mut memory, &wild(3, on_it, 1000));
        assert_eq!(
            label,
            format!(
                "choose move {} ({})",
                slot + 1,
                mv.trim_start_matches("MOVE_")
            )
        );
        memory.catch.on_move_confirmed();
        let is_status = matches!(plan.odds.choice, Choice::Status(..));
        assert_eq!(memory.catch.attempt.as_ref().unwrap().opened, is_status);
        // "Wild PIDGEY fell asleep!" on two frames.
        let mut text = wild(5, command, 1000);
        text.battle.as_mut().unwrap().menu = None;
        text.dialogue = Some(battle_text(&["Wild PIDGEY", "fell asleep!"]));
        memory.catch.observe(&text);
        text.frame_id = 6;
        memory.catch.observe(&text);
        assert_eq!(
            memory.catch.attempt.as_ref().unwrap().foe_status,
            FoeStatus::Asleep
        );
        // Under DISABLE (live: Route 3, the bot chose a disabled move
        // forever) that move is never chosen.
        for disabled in ["MOVE_VINE_WHIP", "MOVE_TACKLE"] {
            memory.disabled = Some(disabled.into());
            let label = decide_on(&data, &party, &mut memory, &wild(10, moves, 1000));
            assert!(
                !label.contains(disabled.trim_start_matches("MOVE_")),
                "{disabled}: {label}"
            );
        }
        memory.disabled = None;
        // Asleep and weakened (200‰): BAG.
        let label = decide_on(&data, &party, &mut memory, &wild(11, command, 200));
        assert_eq!(label, "cursor to BAG: Right");
    }

    fn battle_text(lines: &[&str]) -> pokebot_state::DialogueObservation {
        pokebot_state::DialogueObservation {
            kind: pokebot_state::DialogueKind::BattleText,
            region: pokebot_state::Region::new(8, 119, 224, 34),
            waiting_for_input: false,
            arrow: None,
            stable_frames: 10,
            text_cells: vec![1; 4],
            lines: lines.iter().map(|l| (*l).to_owned()).collect(),
            help: false,
        }
    }

    fn bag_frame(
        frame: u64,
        pocket: &str,
        rows: &[(&str, Option<u16>)],
        cursor: Option<u8>,
        prompt: Option<u8>,
    ) -> Observation {
        let mut o = Observation::bare(
            frame,
            pokebot_state::Observed {
                value: if prompt.is_some() {
                    pokebot_state::ScreenState::BattleBag
                } else {
                    pokebot_state::ScreenState::Bag
                },
                detector: "test".into(),
            },
            Default::default(),
        );
        o.bag = Some(pokebot_state::BagObservation {
            pocket: pocket.into(),
            rows: rows.iter().map(|(n, c)| ((*n).to_owned(), *c)).collect(),
            cursor,
            prompt: prompt.map(|row| (vec!["USE".into(), "CANCEL".into()], row)),
        });
        o
    }

    fn step(
        t: &mut Thrower,
        data: &GameData,
        o: &Observation,
        events: &mut Vec<GameEvent>,
    ) -> String {
        match t.next(o, data, "ITEM_POKE_BALL", events) {
            crate::Decision::Act(a) => a.label,
            crate::Decision::Wait(r) => format!("wait: {r}"),
            crate::Decision::Done(r) => format!("done: {r}"),
            crate::Decision::Fail(r) => format!("fail: {r}"),
        }
    }

    #[test]
    fn a_throw_goes_through_the_remembered_pocket_and_row() {
        let Some(data) = data() else { return };
        let mut t = Thrower::new();
        let mut events = Vec::new();
        // The bag remembers the last pocket: ITEMS → Right → KEY ITEMS → Right.
        let items = [("POTION", Some(1)), ("CANCEL", None)];
        assert_eq!(
            step(
                &mut t,
                &data,
                &bag_frame(1, "ITEMS", &items, Some(0), None),
                &mut events
            ),
            "battle bag: Right to KEY ITEMS"
        );
        let keys = [("TEACHY TV", None), ("CANCEL", None)];
        assert_eq!(
            step(
                &mut t,
                &data,
                &bag_frame(2, "KEY ITEMS", &keys, Some(0), None),
                &mut events
            ),
            "battle bag: Right to POKé BALLS"
        );
        // ...and row: ▶ on CANCEL. Rows count once two frames agree.
        let balls = [("POKé BALL", Some(8)), ("CANCEL", None)];
        let on_cancel = |f| bag_frame(f, "POKé BALLS", &balls, Some(1), None);
        assert!(step(&mut t, &data, &on_cancel(3), &mut events).starts_with("wait"));
        assert!(events.is_empty());
        assert_eq!(
            step(&mut t, &data, &on_cancel(4), &mut events),
            "battle bag: Up toward POKE_BALL"
        );
        assert_eq!(
            events,
            vec![GameEvent::PocketObserved {
                pocket: Pocket::PokeBalls,
                items: vec![("ITEM_POKE_BALL".into(), 8)],
            }]
        );
        let on_ball = |f| bag_frame(f, "POKé BALLS", &balls, Some(0), None);
        assert!(step(&mut t, &data, &on_ball(5), &mut events).starts_with("wait"));
        assert_eq!(
            step(&mut t, &data, &on_ball(6), &mut events),
            "throw POKE_BALL: select it"
        );
        // Read once per throw.
        assert_eq!(events.len(), 1);
        // USE / CANCEL, ▶ on USE.
        let prompt = bag_frame(7, "POKé BALLS", &balls, None, Some(0));
        assert_eq!(
            step(&mut t, &data, &prompt, &mut events),
            "throw POKE_BALL: USE"
        );
        assert!(!t.used());
        // The bag closes: the ball is on its way.
        let gone = Observation::bare(8, prompt.screen.clone(), Default::default());
        assert_eq!(
            step(&mut t, &data, &gone, &mut events),
            "wait: the ball is thrown"
        );
        assert!(t.used());
    }

    #[test]
    fn the_best_ball_is_picked_from_the_rows() {
        let Some(data) = data() else { return };
        let mut t = Thrower::new();
        let mut events = Vec::new();
        let balls = [
            ("POKé BALL", Some(8)),
            ("GREAT BALL", Some(2)),
            ("CANCEL", None),
        ];
        let at = |f, row| bag_frame(f, "POKé BALLS", &balls, Some(row), None);
        step(&mut t, &data, &at(1, 0), &mut events);
        assert_eq!(
            step(&mut t, &data, &at(2, 0), &mut events),
            "battle bag: Down toward GREAT_BALL"
        );
        step(&mut t, &data, &at(3, 1), &mut events);
        assert_eq!(
            step(&mut t, &data, &at(4, 1), &mut events),
            "throw GREAT_BALL (instead of POKE_BALL): select it"
        );
    }

    #[test]
    fn a_misread_count_is_not_a_reading() {
        let Some(data) = data() else { return };
        let mut t = Thrower::new();
        let mut events = Vec::new();
        let read = |f, n| {
            bag_frame(
                f,
                "POKé BALLS",
                &[("POKé BALL", Some(n)), ("CANCEL", None)],
                Some(0),
                None,
            )
        };
        step(&mut t, &data, &read(1, 8), &mut events);
        assert!(step(&mut t, &data, &read(2, 3), &mut events).starts_with("wait"));
        assert!(events.is_empty());
        assert_eq!(
            step(&mut t, &data, &read(3, 3), &mut events),
            "throw POKE_BALL: select it"
        );
        assert_eq!(
            events,
            vec![GameEvent::PocketObserved {
                pocket: Pocket::PokeBalls,
                items: vec![("ITEM_POKE_BALL".into(), 3)],
            }]
        );
    }

    #[test]
    fn no_usable_ball_gives_the_throw_up() {
        let Some(data) = data() else { return };
        let mut memory = CatchMemory {
            thrower: Some(Thrower::new()),
            ..CatchMemory::default()
        };
        memory.attempt = Some(Attempt {
            plan: CatchPlan {
                ball: "ITEM_POKE_BALL".into(),
                odds: Odds {
                    choice: Choice::Throw,
                    outlook: Default::default(),
                },
                shiny: false,
                wanted: false,
            },
            opened: true,
            throws: 0,
            foe_status: FoeStatus::None,
            balls: Some(10),
            asleep_turns: 0,
        });
        let mut events = Vec::new();
        let empty = |f| bag_frame(f, "POKé BALLS", &[("CANCEL", None)], Some(0), None);
        assert!(matches!(
            in_bag(&empty(1), &data, &mut memory, &mut events),
            crate::Decision::Wait(_)
        ));
        let crate::Decision::Act(close) = in_bag(&empty(2), &data, &mut memory, &mut events) else {
            panic!("expected B");
        };
        assert_eq!(close.label, "battle bag: B to give up the throw");
        let gone = Observation::bare(3, empty(3).screen, Default::default());
        assert!(matches!(
            in_bag(&gone, &data, &mut memory, &mut events),
            crate::Decision::Wait(_)
        ));
        assert!(memory.thrower.is_none());
        assert!(memory.attempt.is_none());
        assert!(events.iter().any(|e| matches!(
            e,
            GameEvent::GoalProgress { phase, detail, .. } if phase == "Catch" && detail.contains("abandoned")
        )));
    }

    #[test]
    fn stuck_bag_is_given_up_after_the_retries() {
        let Some(data) = data() else { return };
        let mut t = Thrower::new();
        let mut events = Vec::new();
        // An unreadable pocket title, forever.
        let mut label = String::new();
        for f in 0..2000 {
            label = step(
                &mut t,
                &data,
                &bag_frame(f, "", &[], None, None),
                &mut events,
            );
            if !label.starts_with("wait") {
                break;
            }
        }
        assert_eq!(label, "battle bag: B to give up the throw");
        assert!(t.gave_up().is_some());
    }

    #[test]
    fn battle_text_is_read_on_two_frames_and_once() {
        let mut memory = CatchMemory {
            foe: Some(("SPECIES_PIDGEY".into(), 6)),
            attempt: Some(Attempt {
                plan: CatchPlan {
                    ball: "ITEM_POKE_BALL".into(),
                    odds: Odds {
                        choice: Choice::Throw,
                        outlook: Default::default(),
                    },
                    shiny: false,
                    wanted: false,
                },
                opened: true,
                throws: 0,
                foe_status: FoeStatus::None,
                balls: Some(10),
                asleep_turns: 0,
            }),
            ..CatchMemory::default()
        };
        let page = |f, lines: &[&str]| {
            let mut o = Observation::bare(
                f,
                pokebot_state::Observed {
                    value: pokebot_state::ScreenState::BattleText,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.dialogue = Some(battle_text(lines));
            o
        };
        // One frame of a misread "Gotcha!" is not a catch.
        memory.observe(&page(1, &["Gotcha!", "PIDGEY was caught!"]));
        memory.observe(&page(2, &["Shoot!", "It was so close, too!"]));
        assert_eq!(memory.caught, None);
        memory.observe(&page(3, &["Shoot!", "It was so close, too!"]));
        memory.observe(&page(4, &["Shoot!", "It was so close, too!"]));
        assert_eq!(memory.attempt.as_ref().unwrap().throws, 1);
        memory.observe(&page(5, &["Gotcha!", "PIDGEY was caught!"]));
        memory.observe(&page(6, &["Gotcha!", "PIDGEY was caught!"]));
        assert_eq!(memory.caught.as_deref(), Some("SPECIES_PIDGEY"));
        // Full party: the PC pages.
        for (f, lines) in [
            (7, ["PIDGEY was transferred to", "someone's PC."]),
            (9, ["It was placed in", "BOX “BOX2.”"]),
        ] {
            memory.observe(&page(f, &lines));
            memory.observe(&page(f + 1, &lines));
        }
        assert_eq!(memory.box_index, Some(1));
    }

    #[test]
    fn the_nickname_question_proves_a_catch() {
        let mut memory = CatchMemory {
            foe: Some(("SPECIES_PIDGEY".into(), 6)),
            ..CatchMemory::default()
        };
        for f in [1, 2] {
            let mut o = Observation::bare(
                f,
                pokebot_state::Observed {
                    value: pokebot_state::ScreenState::BattleText,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.dialogue = Some(battle_text(&["Give a nickname to the", "captured PIDGEY?"]));
            memory.observe(&o);
        }
        assert_eq!(memory.caught.as_deref(), Some("SPECIES_PIDGEY"));
    }

    #[test]
    fn box_index_from_the_pc_pages() {
        assert_eq!(box_from_text("It was placed in BOX “BOX1.”"), Some(0));
        assert_eq!(
            box_from_text("PIDGEY was transferred to BOX “BOX3.”"),
            Some(2)
        );
        assert_eq!(box_from_text("It was placed in BOX 12."), Some(11));
        // The full box is not where the catch went.
        assert_eq!(box_from_text("BOX “BOX1” on someone's PC was full."), None);
        assert_eq!(
            box_from_text("PIDGEY was transferred to someone's PC."),
            None
        );
        assert_eq!(box_from_text("RED used POKé BALL!"), None);
    }

    #[test]
    fn rising_risk_abandons_the_attempt_for_run() {
        let Some(data) = data() else { return };
        let party = Party {
            members: vec![ivysaur(&data)],
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut memory = BattleMemory::default();
        let mut events = Vec::new();
        let command = BattleMenu::Command { column: 0, row: 0 };
        for f in [1, 2] {
            identify(
                &wild(f, command, 1000),
                &data,
                &state,
                &party,
                &mut memory,
                &mut events,
            );
        }
        assert!(memory.catch.attempt.is_some());
        // Our HP fell to 2/54: the rest of the attempt is too risky.
        let mut low = wild(3, command, 1000);
        let b = low.battle.as_mut().unwrap();
        b.player_hp_numbers = Some((2, 54));
        b.player_hp = Some(37);
        assert_eq!(
            decide_on(&data, &party, &mut memory, &low),
            "cursor to RUN: Down"
        );
        assert!(memory.catch.attempt.is_none());
        assert!(memory.catch.flee);
        // Even above the low-HP line, the battle keeps running away.
        let mut mid = wild(5, command, 1000);
        let b = mid.battle.as_mut().unwrap();
        b.player_hp_numbers = Some((40, 54));
        b.player_hp = Some(740);
        assert_eq!(
            decide_on(&data, &party, &mut memory, &mid),
            "cursor to RUN: Down"
        );
    }

    /// Review: a non-shiny attempt kept throwing (`throws_left.max(1)`)
    /// into the shiny reserve. Once the tracked count is at the reserve
    /// the attempt is abandoned for RUN.
    #[test]
    fn a_non_shiny_attempt_stops_at_the_shiny_reserve() {
        let Some(data) = data() else { return };
        let party = Party {
            members: vec![ivysaur(&data)],
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut memory = BattleMemory::default();
        let mut events = Vec::new();
        let command = BattleMenu::Command { column: 0, row: 0 };
        for f in [1, 2] {
            identify(
                &wild(f, command, 1000),
                &data,
                &state,
                &party,
                &mut memory,
                &mut events,
            );
        }
        let attempt = memory.catch.attempt.as_mut().expect("an attempt");
        assert_eq!(attempt.balls, Some(10));
        attempt.balls = Some(SHINY_RESERVE + 1);
        attempt.opened = true;
        // One more ball broke free: only the reserve is left.
        let mut text = wild(3, command, 200);
        text.battle.as_mut().unwrap().menu = None;
        text.dialogue = Some(battle_text(&["Oh, no!", "The POKéMON broke free!"]));
        memory.catch.observe(&text);
        text.frame_id = 4;
        memory.catch.observe(&text);
        assert_eq!(
            memory.catch.attempt.as_ref().and_then(|a| a.balls),
            Some(SHINY_RESERVE)
        );
        assert_eq!(
            decide_on(&data, &party, &mut memory, &wild(5, command, 200)),
            "cursor to RUN: Down"
        );
        assert!(memory.catch.attempt.is_none());
        assert!(memory.catch.flee);
    }

    /// The battle bag's own count (read before each throw) also stops a
    /// non-shiny attempt at the reserve; a shiny throws it.
    #[test]
    fn the_bag_count_at_the_reserve_gives_a_non_shiny_throw_up() {
        let Some(data) = data() else { return };
        let plan = |shiny: bool| CatchPlan {
            ball: "ITEM_POKE_BALL".into(),
            odds: Odds {
                choice: Choice::Throw,
                outlook: Default::default(),
            },
            shiny,
            wanted: false,
        };
        let memory_for = |shiny: bool| CatchMemory {
            thrower: Some(Thrower::new().keeping_reserve(!shiny)),
            attempt: Some(Attempt {
                plan: plan(shiny),
                opened: true,
                throws: 0,
                foe_status: FoeStatus::None,
                // The tracked count said 8 (stale).
                balls: Some(8),
                asleep_turns: 0,
            }),
            ..CatchMemory::default()
        };
        let balls = [("POKé BALL", Some(SHINY_RESERVE)), ("CANCEL", None)];
        let list = |f| bag_frame(f, "POKé BALLS", &balls, Some(0), None);
        let mut events = Vec::new();
        let mut memory = memory_for(false);
        in_bag(&list(1), &data, &mut memory, &mut events);
        let crate::Decision::Act(close) = in_bag(&list(2), &data, &mut memory, &mut events) else {
            panic!("expected B");
        };
        assert_eq!(close.label, "battle bag: B to give up the throw");
        assert_eq!(
            memory.attempt.as_ref().and_then(|a| a.balls),
            Some(SHINY_RESERVE)
        );
        let gone = Observation::bare(3, list(3).screen, Default::default());
        in_bag(&gone, &data, &mut memory, &mut events);
        assert!(memory.attempt.is_none());
        assert!(memory.flee);
        // A shiny throws one of the reserve.
        let mut memory = memory_for(true);
        in_bag(&list(1), &data, &mut memory, &mut events);
        let crate::Decision::Act(select) = in_bag(&list(2), &data, &mut memory, &mut events) else {
            panic!("expected A");
        };
        assert_eq!(select.label, "throw POKE_BALL: select it");
        assert!(!memory.flee);
    }

    /// Switch, Route 1: hunting PIDGEY with 20 balls, the attempt chose to
    /// throw, but the command menu read as battle text after "cursor to
    /// BAG"; its A opened the bag before a thrower was armed, and the bag
    /// was closed as unexpected, again and again. A bag opened while the
    /// attempt throws is thrown from; any other is still closed.
    #[test]
    fn a_bag_opened_while_the_attempt_throws_is_thrown_from() {
        let Some(data) = data() else { return };
        let memory_for = |choice: Choice| CatchMemory {
            attempt: Some(Attempt {
                plan: CatchPlan {
                    ball: "ITEM_POKE_BALL".into(),
                    odds: Odds {
                        choice,
                        outlook: Default::default(),
                    },
                    shiny: false,
                    wanted: true,
                },
                opened: true,
                throws: 0,
                foe_status: FoeStatus::None,
                balls: Some(20),
                asleep_turns: 0,
            }),
            ..CatchMemory::default()
        };
        let balls = [("POKé BALL", Some(20)), ("CANCEL", None)];
        let list = |f| bag_frame(f, "POKé BALLS", &balls, Some(0), None);
        let mut events = Vec::new();
        let mut memory = memory_for(Choice::Throw);
        in_bag(&list(1), &data, &mut memory, &mut events);
        let crate::Decision::Act(select) = in_bag(&list(2), &data, &mut memory, &mut events) else {
            panic!("expected A");
        };
        assert_eq!(select.label, "throw POKE_BALL: select it");
        // The attempt's turn is a move, or no attempt: not ours to use.
        for mut memory in [memory_for(Choice::Run), CatchMemory::default()] {
            let crate::Decision::Act(close) = in_bag(&list(1), &data, &mut memory, &mut events)
            else {
                panic!("expected B");
            };
            assert_eq!(close.label, "close an unexpected battle bag");
        }
    }

    #[test]
    fn a_risky_shiny_is_thrown_at_not_run_from() {
        let Some(data) = data() else { return };
        let party = Party {
            members: vec![ivysaur(&data)],
        };
        // Below the reserve: a shiny is still caught.
        let state = with_balls(&[("ITEM_POKE_BALL", 3)]);
        let mut memory = BattleMemory::default();
        let mut events = Vec::new();
        let command = BattleMenu::Command { column: 0, row: 0 };
        let shiny = |f, hp: (u16, u16)| {
            let mut o = wild_as(f, command, 1000, "SPEAROW", 9);
            let b = o.battle.as_mut().unwrap();
            b.opponent_shiny = Some(pokebot_state::ShinyReading::Shiny);
            b.player_hp_numbers = Some(hp);
            b.player_hp = Some((u32::from(hp.0) * 1000 / u32::from(hp.1)) as u16);
            o
        };
        for f in [1, 2] {
            identify(
                &shiny(f, (54, 54)),
                &data,
                &state,
                &party,
                &mut memory,
                &mut events,
            );
        }
        assert!(events.contains(&GameEvent::ShinySeen {
            species: "SPECIES_SPEAROW".into()
        }));
        assert!(memory.catch.attempt.as_ref().is_some_and(|a| a.plan.shiny));
        // HP 40/54 (above the flee line): the rest of the attempt is over
        // the risk limit, yet a shiny is never run from: the odds' best
        // action (a throw or a move) instead.
        let lead = Lead {
            member: &party.members[0],
            hp: (40, 54),
        };
        let spearow = Foe {
            shiny: true,
            ..foe("SPECIES_SPEAROW", 9)
        };
        assert!(
            risk(&data, &lead, &spearow, 1) > RISK_LIMIT
                || risk(&data, &lead, &spearow, 3) > RISK_LIMIT
        );
        let label = decide_on(&data, &party, &mut memory, &shiny(3, (40, 54)));
        assert!(
            label == "cursor to BAG: Right" || label == "choose FIGHT",
            "{label}"
        );
        assert!(memory.catch.attempt.is_some());
        assert!(!memory.catch.flee);
        // Below the flee line, the low-HP rule may still RUN.
        assert_eq!(
            decide_on(&data, &party, &mut memory, &shiny(5, (5, 54))),
            "cursor to RUN: Down"
        );
        assert!(memory.catch.attempt.is_some());
    }

    #[test]
    fn an_unreadable_hud_abandons_the_attempt() {
        let Some(data) = data() else { return };
        let party = Party {
            members: vec![ivysaur(&data)],
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut memory = BattleMemory::default();
        let mut events = Vec::new();
        let command = BattleMenu::Command { column: 0, row: 0 };
        for f in [1, 2] {
            identify(
                &wild(f, command, 1000),
                &data,
                &state,
                &party,
                &mut memory,
                &mut events,
            );
        }
        let policy = crate::battle::BattlePolicy::default();
        let mut last = String::new();
        for f in 3..400 {
            // Our HP digits flicker between two readings every frame.
            let mut o = wild(f, command, 1000);
            o.battle.as_mut().unwrap().player_hp_numbers = Some((54 - (f % 2) as u16 * 10, 54));
            last = match crate::battle::decide(&o, &policy, &mut memory, &party, &data, &mut events)
            {
                Some(crate::Decision::Act(a)) => a.label,
                Some(crate::Decision::Wait(r)) => format!("wait: {r}"),
                other => panic!("{:?}", other.is_some()),
            };
            if memory.catch.attempt.is_none() {
                break;
            }
            assert_eq!(last, "wait: confirming the HUD on a later frame");
        }
        assert!(memory.catch.attempt.is_none());
        assert_eq!(last, "choose FIGHT");
    }

    #[test]
    fn box_index_from_the_pret_pages() {
        // data/text/pc_transfer.inc, joined across lines with " ".
        let pages = |list: &[&str]| -> Option<u8> {
            let mut memory = CatchMemory::default();
            for (i, page) in list.iter().enumerate() {
                let mut o = Observation::bare(
                    0,
                    pokebot_state::Observed {
                        value: pokebot_state::ScreenState::BattleText,
                        detector: "test".into(),
                    },
                    Default::default(),
                );
                o.dialogue = Some(battle_text(&page.split('\n').collect::<Vec<_>>()));
                for f in 0..2 {
                    o.frame_id = 10 * i as u64 + f;
                    memory.observe(&o);
                }
            }
            memory.box_index
        };
        assert_eq!(
            pages(&[
                "PIDGEY was transferred to\nSomeone's PC.",
                "It was placed in \nBOX “BOX 1.”"
            ]),
            Some(0)
        );
        assert_eq!(
            pages(&[
                "PIDGEY was transferred to\nBILL'S PC.",
                "It was placed in \nBOX “BOX 1.”"
            ]),
            Some(0)
        );
        assert_eq!(
            pages(&[
                "BOX “BOX 1” on\nSomeone's PC was full.",
                "PIDGEY was transferred to\nBOX “BOX 2.”"
            ]),
            Some(1)
        );
        assert_eq!(box_from_text("BOX “BOX 1” on Someone's PC was full."), None);
        assert_eq!(box_from_text("PIDGEY was transferred to BILL'S PC."), None);
        assert_eq!(box_from_text("It was placed in  BOX “BOX 12.”"), Some(11));
    }

    #[test]
    fn a_blocked_ball_means_a_trainer_battle() {
        let Some(data) = data() else { return };
        let party = Party {
            members: vec![ivysaur(&data)],
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut memory = BattleMemory::default();
        let mut events = Vec::new();
        let command = BattleMenu::Command { column: 0, row: 0 };
        for f in [1, 2] {
            identify(
                &wild(f, command, 1000),
                &data,
                &state,
                &party,
                &mut memory,
                &mut events,
            );
        }
        assert!(memory.catch.attempt.is_some());
        let mut text = wild(3, command, 1000);
        text.battle.as_mut().unwrap().menu = None;
        text.dialogue = Some(battle_text(&["The TRAINER blocked the BALL!"]));
        observe(&mut memory, &text);
        text.frame_id = 4;
        observe(&mut memory, &text);
        assert!(memory.trainer);
        assert!(memory.catch.attempt.is_none());
        // The battle goes on as a trainer battle: FIGHT, no RUN.
        assert_eq!(
            decide_on(&data, &party, &mut memory, &wild(5, command, 1000)),
            "choose FIGHT"
        );
    }

    #[test]
    fn a_new_turn_confirms_the_hud_afresh() {
        let Some(data) = data() else { return };
        let party = Party {
            members: vec![ivysaur(&data)],
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut memory = BattleMemory::default();
        let mut events = Vec::new();
        let command = BattleMenu::Command { column: 0, row: 0 };
        for f in [1, 2] {
            identify(
                &wild(f, command, 1000),
                &data,
                &state,
                &party,
                &mut memory,
                &mut events,
            );
        }
        assert_eq!(
            decide_on(&data, &party, &mut memory, &wild(3, command, 1000)),
            "choose FIGHT"
        );
        // The turn's text, then the next command menu with the same HUD.
        let mut text = wild(5, command, 1000);
        text.battle.as_mut().unwrap().menu = None;
        text.dialogue = Some(battle_text(&["Wild PIDGEY used", "TACKLE!"]));
        memory.catch.observe(&text);
        let policy = crate::battle::BattlePolicy::default();
        let first = crate::battle::decide(
            &wild(6, command, 1000),
            &policy,
            &mut memory,
            &party,
            &data,
            &mut events,
        );
        assert!(
            matches!(first, Some(crate::Decision::Wait(ref r)) if r.contains("confirming the HUD")),
            "a stale reading confirmed a new menu"
        );
        let second = crate::battle::decide(
            &wild(7, command, 1000),
            &policy,
            &mut memory,
            &party,
            &data,
            &mut events,
        );
        assert!(matches!(second, Some(crate::Decision::Act(_))));
    }
}
