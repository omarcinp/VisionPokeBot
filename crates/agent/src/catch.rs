//! Whether and how to catch a wild Pokémon (the pure decision).
//!
//! A shiny is always caught, with any ball including the reserve. Otherwise
//! a species not caught yet is caught only when both hold: the probability
//! that our lead faints during the whole attempt (weakening turns plus the
//! expected throws, the foe using its most damaging move every turn) is at
//! most [`RISK_LIMIT`], and the known ball stock exceeds the shiny reserve
//! plus the expected throws. Weakening opens with a status move (sleep before
//! paralysis), then uses only moves whose critical maximum roll cannot faint
//! the foe at the lowest HP its bar allows, until the bar is below
//! [`WEAKENED_PER_MILLE`]. Trainers' Pokémon are never caught, and unknown
//! facts (caught flag, ball stock) decline the catch rather than guess.
//!
//! The flow in battle: [`identify`] reads the wild opponent once (two
//! agreeing command-menu frames), emits what the Pokédex learns and plans
//! the catch; the battle decision then weakens and throws
//! (`attempt_decision`, re-checking the risk each turn); [`Thrower`] goes
//! through the battle bag; [`CatchMemory::observe`] reads the result, the
//! foe's status and the PC box from battle text; [`caught_events`] records
//! the catch when the battle ends.

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::mechanics::{ball_multiplier, catch_probability_status, damage, DamageRolls};
use pokebot_gamedata::{printed_name, GameData};
use pokebot_planner::evaluate::faint_probability;
use pokebot_planner::Combatant;
use pokebot_state::{
    BattleMenu, BoxMon, GameEvent, GameState, Knowledge, Observation, Pocket, ScreenState,
    ShinyReading,
};

use crate::bag::{
    by_item, is_cancel, item_key, pocket_from_title, pocket_index, read_rows, Rows, POCKETS,
};
use crate::battle::{choose_move, identify_opponent, step_toward, BattleMemory, BattlePolicy};
use crate::party::{starter_mon, Member, Party};
use crate::stock::{ball_count, SHINY_RESERVE};
use crate::{Action, Decision, Expectation};

/// Largest accepted P(our lead faints during the attempt).
pub const RISK_LIMIT: f64 = 0.02;
/// Weakening stops once the foe's HP bar is below this (‰).
pub const WEAKENED_PER_MILLE: u16 = 250;
/// Caps on the estimates.
const MAX_THROWS: u32 = 20;
const MAX_WEAKENING_TURNS: u32 = 10;

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
    /// Opening status move (slot, move); `None` to weaken without it or to
    /// throw at once.
    pub status_move: Option<(u8, String)>,
    pub expected_throws: u32,
    /// P(our lead faints during the attempt).
    pub risk: f64,
    pub shiny: bool,
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

/// The status move to open with (sleep before paralysis, then slot order),
/// only with PP left and only on a foe without a status.
pub fn status_move(data: &GameData, lead: &Member, foe: FoeStatus) -> Option<(u8, String)> {
    if foe != FoeStatus::None {
        return None;
    }
    lead.moves
        .iter()
        .enumerate()
        .filter(|(_, m)| lead.pp_left(data, m) > 0)
        .filter_map(|(slot, m)| {
            let rank = match data.move_(m)?.effect.as_deref()? {
                "EFFECT_SLEEP" => 0,
                "EFFECT_PARALYZE" => 1,
                _ => return None,
            };
            Some((rank, slot, m))
        })
        .min()
        .map(|(_, slot, m)| (slot as u8, m.clone()))
}

/// The attack that weakens the foe fastest without any risk of fainting it:
/// its critical maximum roll (our iv 31 vs foe iv 0) stays below the lowest
/// HP the bar allows. Ties: highest normal maximum, then move name.
pub fn weakening_move(data: &GameData, lead: &Lead, foe: &Foe) -> Option<(u8, String)> {
    let us = our_combatant(data, lead, 31)?;
    let them = foe_combatant(data, foe, 0)?;
    let floor = hp_floor(them.max_hp(), foe.hp_per_mille);
    lead.member
        .moves
        .iter()
        .enumerate()
        .filter(|(_, m)| lead.member.pp_left(data, m) > 0)
        .filter_map(|(slot, m)| {
            let rolls = rolls(data, m, &us, &them)?;
            let crit = rolls.critical.iter().max().copied()?;
            let normal = rolls.normal.iter().max().copied()?;
            (crit < floor).then_some((normal, slot, m))
        })
        .max_by(|(na, _, a), (nb, _, b)| na.cmp(nb).then(b.cmp(a)))
        .map(|(_, slot, m)| (slot as u8, m.clone()))
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

/// Decide whether to catch `foe`, and how. `Err` carries the reason not to.
pub fn plan_catch(
    data: &GameData,
    state: &GameState,
    lead: &Lead,
    foe: &Foe,
    trainer: bool,
) -> Result<CatchPlan, String> {
    if trainer {
        return Err("a trainer's Pokémon can't be caught".into());
    }
    if !foe.shiny && foe.caught != Some(false) {
        return Err(format!("{}: caught flag {:?}", foe.species, foe.caught));
    }
    let ball = match balls_held(state) {
        Some(balls) => best_ball(data, &balls).ok_or("no usable ball held")?,
        None if foe.shiny => "ITEM_POKE_BALL".to_owned(),
        None => return Err("ball count unknown (pocket not audited)".into()),
    };
    let multiplier = ball_multiplier(&ball).unwrap_or(10);
    let mut attempt = estimate(data, lead, foe, multiplier, true)?;
    let mut risk_now = risk(data, lead, foe, attempt.turns + attempt.throws);
    if foe.shiny {
        if risk_now > RISK_LIMIT {
            attempt = estimate(data, lead, foe, multiplier, false)?;
            risk_now = risk(data, lead, foe, attempt.throws);
        }
    } else {
        let count = ball_count(state).unwrap_or(0);
        if u32::from(count) <= u32::from(SHINY_RESERVE) + attempt.throws {
            return Err(format!(
                "{count} balls: not above the reserve {SHINY_RESERVE} + {} throws",
                attempt.throws
            ));
        }
        if risk_now > RISK_LIMIT {
            return Err(format!("risk {risk_now:.4} above {RISK_LIMIT}"));
        }
    }
    Ok(CatchPlan {
        ball,
        status_move: attempt.status_move,
        expected_throws: attempt.throws,
        risk: risk_now,
        shiny: foe.shiny,
    })
}

/// The attempt's shape: opener, expected throws and weakening turns.
struct Estimate {
    status_move: Option<(u8, String)>,
    throws: u32,
    turns: u32,
}

/// Estimate an attempt with `ball` (×10); `weaken` false throws at once.
fn estimate(
    data: &GameData,
    lead: &Lead,
    foe: &Foe,
    ball: u32,
    weaken: bool,
) -> Result<Estimate, String> {
    let unknown = || format!("{} unknown to the game data", foe.species);
    let species = data.species(&foe.species).ok_or_else(unknown)?;
    let max = foe_combatant(data, foe, 0).ok_or_else(unknown)?.max_hp();
    let hp_now = (max * u32::from(foe.hp_per_mille) / 1000).max(1);
    let opener = weaken
        .then(|| status_move(data, lead.member, foe.status))
        .flatten();
    let attack = weaken.then(|| weakening_move(data, lead, foe)).flatten();
    let target = if attack.is_some() {
        throw_hp(data, lead, foe, max).min(hp_now).max(1)
    } else {
        hp_now
    };
    let status_x10 = match opener.as_ref().map(|(_, m)| data.move_(m)) {
        Some(Some(m)) if m.effect.as_deref() == Some("EFFECT_SLEEP") => 20,
        Some(_) => 15,
        None => match foe.status {
            FoeStatus::Asleep => 20,
            FoeStatus::Paralyzed => 15,
            FoeStatus::None => 10,
        },
    };
    let p = catch_probability_status(species.catch_rate, max, target, ball, status_x10);
    let throws = if p > 0.0 {
        ((1.0 / p).ceil() as u32).clamp(1, MAX_THROWS)
    } else {
        MAX_THROWS
    };
    let attack_turns = attack.map_or(0, |(_, m)| {
        weakening_turns(data, lead, foe, &m, hp_now, target)
    });
    Ok(Estimate {
        turns: u32::from(opener.is_some()) + attack_turns,
        status_move: opener,
        throws,
    })
}

/// HP at which weakening stops: [`WEAKENED_PER_MILLE`] of `max`, or higher
/// where even the least damaging attack with PP could faint the foe on a
/// crit (its critical maximum + 1 + one bar pixel).
fn throw_hp(data: &GameData, lead: &Lead, foe: &Foe, max: u32) -> u32 {
    let weakened = max * u32::from(WEAKENED_PER_MILLE) / 1000;
    let (Some(us), Some(them)) = (our_combatant(data, lead, 31), foe_combatant(data, foe, 0))
    else {
        return weakened;
    };
    let least_crit = lead
        .member
        .moves
        .iter()
        .filter(|m| lead.member.pp_left(data, m) > 0)
        .filter_map(|m| rolls(data, m, &us, &them)?.critical.iter().max().copied())
        .min();
    least_crit.map_or(weakened, |c| weakened.max(c + 1 + max / 48))
}

/// Attacks of `mv` to bring the foe from `hp_now` down to `target`, with
/// pessimistic mean damage (our iv 0 vs foe iv 31), capped.
fn weakening_turns(
    data: &GameData,
    lead: &Lead,
    foe: &Foe,
    mv: &str,
    hp_now: u32,
    target: u32,
) -> u32 {
    let excess = hp_now.saturating_sub(target);
    if excess == 0 {
        return 0;
    }
    let mean = our_combatant(data, lead, 0)
        .zip(foe_combatant(data, foe, 31))
        .and_then(|(us, them)| rolls(data, mv, &us, &them))
        .map_or(0.0, |r| {
            r.hit_chance * r.normal.iter().map(|d| f64::from(*d)).sum::<f64>() / 16.0
        });
    if mean <= 0.0 {
        return MAX_WEAKENING_TURNS;
    }
    ((f64::from(excess) / mean).ceil() as u32).min(MAX_WEAKENING_TURNS)
}

/// Lowest HP the bar allows: its fill minus one pixel (1/48), at least 1.
fn hp_floor(max: u32, per_mille: u16) -> u32 {
    (max * u32::from(per_mille) / 1000)
        .saturating_sub(max / 48)
        .max(1)
}

fn our_combatant(data: &GameData, lead: &Lead, iv: u32) -> Option<Combatant> {
    let m = lead.member;
    let mut us = Combatant::new(data, &m.species, m.level, m.moves.clone(), iv)?;
    us.hp = u32::from(lead.hp.0);
    Some(us)
}

fn foe_combatant(data: &GameData, foe: &Foe, iv: u32) -> Option<Combatant> {
    let moves = data.default_moves(&foe.species, foe.level);
    Combatant::new(data, &foe.species, foe.level, moves, iv)
}

fn rolls(data: &GameData, mv: &str, from: &Combatant, to: &Combatant) -> Option<DamageRolls> {
    damage(
        data,
        data.move_(mv)?,
        &from.types,
        from.level,
        &from.stats,
        &to.types,
        &to.stats,
    )
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
}

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
    /// The last page read once (awaiting a second frame) and the last page
    /// applied.
    page: Option<(u64, String)>,
    applied: String,
    pokedex_seen: Option<u64>,
}

impl CatchMemory {
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
                }
            }
            None => {}
        }
        if let (Some(a), Some(species)) = (&mut self.attempt, &species) {
            if let Some(status) = foe_status_text(&page, &printed_name(species)) {
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

    /// Events for the end of the battle: the catch, if there was one.
    pub fn after_battle(&self, data: &GameData, state: &GameState) -> Vec<GameEvent> {
        match (&self.caught, &self.foe) {
            (Some(species), Some((_, level))) => {
                caught_events(data, state, species, *level, self.box_index)
            }
            _ => Vec::new(),
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

/// "It was placed in BOX “BOX2.”" / "X was transferred to BOX “BOX2.”" →
/// 1. The "BOX … was full." page names the full box, not the new one.
pub fn box_from_text(page: &str) -> Option<u8> {
    if page.contains("was full") || !(page.contains("placed in") || page.contains("transferred to"))
    {
        return None;
    }
    // The last "BOX" followed (within two characters) by a number.
    page.match_indices("BOX")
        .filter_map(|(at, _)| {
            let digits: String = page[at + 3..]
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect();
            let gap = page[at + 3..]
                .chars()
                .take_while(|c| !c.is_ascii_digit())
                .count();
            (gap <= 2).then(|| digits.parse::<u8>().ok()).flatten()
        })
        .last()
        .filter(|n| *n >= 1)
        .map(|n| n - 1)
}

/// Events after a catch: `SpeciesCaught`, then `PartyMonDerived` into the
/// next free party slot, or `SentToPc` when the party is full (or the text
/// named a box). With an unknown party and no box, only `SpeciesCaught`.
pub fn caught_events(
    data: &GameData,
    state: &GameState,
    species: &str,
    level: u8,
    box_index: Option<u8>,
) -> Vec<GameEvent> {
    let mut events = vec![GameEvent::SpeciesCaught {
        species: species.to_owned(),
    }];
    let party = state.party.value.as_ref().map(Vec::len);
    if box_index.is_some() || party.is_some_and(|n| n >= 6) {
        let slot = box_index
            .and_then(|i| state.pc.boxes.get(usize::from(i)))
            .and_then(|b| b.value.as_ref())
            .and_then(|list| (0..30u8).find(|s| list.iter().all(|m| m.slot != *s)))
            .unwrap_or(0);
        events.push(GameEvent::SentToPc {
            box_index,
            mon: BoxMon {
                slot,
                species: Knowledge::derived(species.to_owned(), 0),
                level: Knowledge::derived(level, 0),
                nickname: Knowledge::unknown(),
            },
        });
    } else if let Some(n) = party {
        events.push(GameEvent::PartyMonDerived {
            slot: n as u8,
            mon: Box::new(starter_mon(data, species, level)),
        });
    }
    events
}

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
    let Some(hp) = battle.player_hp_numbers.or(member.hp) else {
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
    match plan_catch(data, state, &lead, &foe, false) {
        Ok(plan) => {
            events.push(log(format!(
                "catching {species} Lv{level}: {} ×{} expected, opener {:?}, risk {:.4}{}",
                plan.ball,
                plan.expected_throws,
                plan.status_move.as_ref().map(|(_, m)| m),
                plan.risk,
                if plan.shiny { " (shiny)" } else { "" }
            )));
            c.attempt = Some(Attempt {
                plan,
                opened: false,
                throws: 0,
                foe_status: FoeStatus::None,
            });
        }
        Err(reason) => events.push(log(format!("not catching {species} Lv{level}: {reason}"))),
    }
}

/// Attacks of `mv` left to bring the foe from its bar down to where
/// throwing starts (at least 1 while an attack is still wanted).
fn weakening_left(data: &GameData, lead: &Lead, foe: &Foe, mv: &str) -> u32 {
    let Some(max) = foe_combatant(data, foe, 0).map(|c| c.max_hp()) else {
        return MAX_WEAKENING_TURNS;
    };
    let hp_now = (max * u32::from(foe.hp_per_mille) / 1000).max(1);
    let target = throw_hp(data, lead, foe, max).min(hp_now).max(1);
    weakening_turns(data, lead, foe, mv, hp_now, target).max(1)
}

/// The battle input during a catch attempt. `None` hands the menu back to
/// the ordinary battle logic (the attempt was abandoned, or can't go on).
///
/// Every decision uses HUD numbers that two frames agreed on. Each time the
/// command menu shows, the risk of the rest of the attempt (opener, the
/// weakening left and the throws left) is recomputed: above the limit a
/// non-shiny attempt is abandoned for RUN, a shiny one throws at once.
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
    let attempt = memory.catch.attempt.clone()?;
    let (species, level) = memory.catch.foe.clone()?;
    let member = party.lead()?;
    let command = matches!(menu, BattleMenu::Command { .. });
    let reading = battle
        .player_hp_numbers
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
    } else if choose_move(data, party, None, memory, policy).is_none() {
        memory.catch.attempt = None;
        events.push(log(format!(
            "{species}: no damaging move has PP: abandoning the catch"
        )));
        return None;
    }
    // The game refuses a disabled move: never choose it.
    let usable = |(_, m): &(u8, String)| memory.disabled.as_ref() != Some(m);
    let status = (!attempt.opened && plan.status_move.is_some())
        .then(|| status_move(data, member, attempt.foe_status))
        .flatten()
        .filter(usable);
    let attack = (foe_hp >= WEAKENED_PER_MILLE)
        .then(|| weakening_move(data, &lead, &foe))
        .flatten()
        .filter(usable);
    let throws_left = plan.expected_throws.saturating_sub(attempt.throws).max(1);
    let turns = u32::from(status.is_some())
        + attack
            .as_ref()
            .map_or(0, |(_, m)| weakening_left(data, &lead, &foe, m))
        + throws_left;
    let risk_now = risk(data, &lead, &foe, turns);
    let (status, attack) = if risk_now <= RISK_LIMIT {
        (status, attack)
    } else if plan.shiny {
        // Throw at once rather than risk the weakening.
        (None, None)
    } else {
        memory.catch.attempt = None;
        memory.catch.flee = true;
        events.push(log(format!(
            "{species}: risk {risk_now:.4} above {RISK_LIMIT} over {turns} turns: abandoning the catch"
        )));
        return None;
    };
    Some(match menu {
        BattleMenu::Command { column, row } => {
            if status.is_some() || attack.is_some() {
                step_toward(
                    (column, row),
                    (0, 0),
                    |c, r| BattleMenu::Command { column: c, row: r },
                    "FIGHT",
                    Expectation::ScreenIs(ScreenState::BattleMoveSelection),
                )
            } else {
                if (column, row) == (1, 0) {
                    memory.catch.thrower = Some(Thrower::new());
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
            let Some((slot, mv)) = status.clone().or(attack) else {
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
                memory.catch.opener_chosen = status.is_some();
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
    match thrower.next(o, data, &ball, events) {
        Decision::Done(detail) => {
            if thrower.gave_up().is_some() {
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
}

impl Thrower {
    pub fn new() -> Self {
        Self::default()
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

    use pokebot_gamedata::mechanics::{catch_probability_status, damage};
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
        assert_eq!(plan.status_move, Some((1, "MOVE_SLEEP_POWDER".into())));
        assert!(plan.risk <= RISK_LIMIT, "{plan:?}");
        assert!(plan.expected_throws >= 1);
        assert!(!plan.shiny);
        // Zubat has a safe weakening move: its turns count in the risk too.
        let plan = plan_catch(&data, &state, &lead, &foe("SPECIES_ZUBAT", 9), false).unwrap();
        assert_eq!(plan.status_move, Some((1, "MOVE_SLEEP_POWDER".into())));
        assert!(plan.risk <= RISK_LIMIT, "{plan:?}");
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
        // 6 balls ≤ 5 reserve + at least one throw.
        let state = with_balls(&[("ITEM_POKE_BALL", 6)]);
        let err = plan_catch(&data, &state, &lead, &pidgey, false).unwrap_err();
        assert!(err.contains("reserve"), "{err}");
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
        assert!(plan.expected_throws >= 1);
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
        assert!(err.contains("risk"), "{err}");
        // A shiny is still attempted, throwing at once.
        let shiny = Foe {
            shiny: true,
            ..geodude
        };
        let plan = plan_catch(&data, &state, &lead, &shiny, false).unwrap();
        assert_eq!(plan.status_move, None);
        assert!(plan.risk > RISK_LIMIT);
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

    /// Crit maximum (our iv 31 vs foe iv 0) of `mv` and the foe's HP floor.
    fn crit_max_and_floor(data: &GameData, mv: &str, foe: &Foe) -> (u32, u32) {
        let us = Combatant::new(data, "SPECIES_IVYSAUR", 18, vec![], 31).unwrap();
        let them = Combatant::new(data, &foe.species, foe.level, vec![], 0).unwrap();
        let rolls = damage(
            data,
            data.move_(mv).unwrap(),
            &us.types,
            us.level,
            &us.stats,
            &them.types,
            &them.stats,
        )
        .unwrap();
        let max = them.max_hp();
        let floor = (max * u32::from(foe.hp_per_mille) / 1000)
            .saturating_sub(max / 48)
            .max(1);
        (rolls.critical[15], floor)
    }

    #[test]
    fn weakening_move_is_safe_even_on_a_crit() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        // Zubat Lv9 at full HP: Vine Whip (resisted) can't KO even on a crit;
        // Tackle could.
        let zubat = foe("SPECIES_ZUBAT", 9);
        let (slot, mv) = weakening_move(&data, &lead, &zubat).unwrap();
        assert_eq!((slot, mv.as_str()), (3, "MOVE_VINE_WHIP"));
        let (crit, floor) = crit_max_and_floor(&data, &mv, &zubat);
        assert!(crit < floor, "{crit} vs {floor}");
        let (tackle, _) = crit_max_and_floor(&data, "MOVE_TACKLE", &zubat);
        assert!(tackle >= floor);
        // Mankey Lv7 at 400‰: every move's crit could KO it — none is safe.
        let mankey = Foe {
            hp_per_mille: 400,
            ..foe("SPECIES_MANKEY", 7)
        };
        for mv in ["MOVE_TACKLE", "MOVE_VINE_WHIP"] {
            let (crit, floor) = crit_max_and_floor(&data, mv, &mankey);
            assert!(crit >= floor, "{mv}: {crit} vs {floor}");
        }
        assert_eq!(weakening_move(&data, &lead, &mankey), None);
        // Low HP: nothing is safe.
        for f in [&zubat, &mankey] {
            let low = Foe {
                hp_per_mille: 60,
                ..f.clone()
            };
            assert_eq!(weakening_move(&data, &lead, &low), None);
        }
    }

    #[test]
    fn throws_are_estimated_where_weakening_must_stop() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let zubat = foe("SPECIES_ZUBAT", 9);
        let plan = plan_catch(&data, &state, &lead, &zubat, false).unwrap();
        // Oracle: the least damaging move stays crit-safe down to its crit
        // max + 1 + one bar pixel; weakening can't go below that (nor 25 %).
        let max = Combatant::new(&data, "SPECIES_ZUBAT", 9, vec![], 0)
            .unwrap()
            .max_hp();
        let least_crit = ["MOVE_TACKLE", "MOVE_VINE_WHIP"]
            .iter()
            .map(|m| crit_max_and_floor(&data, m, &zubat).0)
            .min()
            .unwrap();
        let throw_hp = (max * 250 / 1000).max(least_crit + 1 + max / 48);
        assert!(throw_hp > max / 4, "{throw_hp} vs {max}");
        let rate = data.species("SPECIES_ZUBAT").unwrap().catch_rate;
        // Sleep Powder opener: status ×2.
        let p = catch_probability_status(rate, max, throw_hp, 10, 20);
        let expected = ((1.0 / p).ceil() as u32).clamp(1, 20);
        assert_eq!(plan.expected_throws, expected, "p {p} at {throw_hp}/{max}");
        // Already paralysed (×1.5, no opener): at 25 % one throw would do,
        // at the real stopping HP it takes more.
        let paralyzed = Foe {
            status: FoeStatus::Paralyzed,
            ..zubat
        };
        let plan = plan_catch(&data, &state, &lead, &paralyzed, false).unwrap();
        let p = catch_probability_status(rate, max, throw_hp, 10, 15);
        let expected = ((1.0 / p).ceil() as u32).clamp(1, 20);
        assert_eq!(catch_probability_status(rate, max, max / 4, 10, 15), 1.0);
        assert!(expected > 1, "p {p}");
        assert_eq!(plan.expected_throws, expected, "p {p} at {throw_hp}/{max}");
    }

    #[test]
    fn asleep_foe_gets_no_status_move() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        assert_eq!(
            status_move(&data, &member, FoeStatus::None),
            Some((1, "MOVE_SLEEP_POWDER".into()))
        );
        assert_eq!(status_move(&data, &member, FoeStatus::Asleep), None);
        assert_eq!(status_move(&data, &member, FoeStatus::Paralyzed), None);
        // Without PP left, no status move.
        let mut tired = member.clone();
        tired.pp_used.insert("MOVE_SLEEP_POWDER".into(), 15);
        assert_eq!(status_move(&data, &tired, FoeStatus::None), None);
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
        assert_eq!(plan.status_move, None);
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

    fn party_of(data: &GameData, n: u8) -> GameState {
        let records: Vec<EventRecord> = (0..n)
            .map(|slot| EventRecord {
                frame_id: 1,
                event: GameEvent::PartyMonDerived {
                    slot,
                    mon: Box::new(crate::party::starter_mon(data, "SPECIES_RATTATA", 5)),
                },
            })
            .collect();
        DefaultReducer.reduce(&GameState::default(), &records)
    }

    #[test]
    fn full_party_sends_catch_to_pc() {
        let Some(data) = data() else { return };
        let state = party_of(&data, 6);
        let events = caught_events(&data, &state, "SPECIES_PIDGEY", 6, Some(0));
        assert_eq!(
            events,
            vec![
                GameEvent::SpeciesCaught {
                    species: "SPECIES_PIDGEY".into()
                },
                GameEvent::SentToPc {
                    box_index: Some(0),
                    mon: pokebot_state::BoxMon {
                        slot: 0,
                        species: pokebot_state::Knowledge::derived("SPECIES_PIDGEY".into(), 0),
                        level: pokebot_state::Knowledge::derived(6, 0),
                        nickname: pokebot_state::Knowledge::unknown(),
                    }
                }
            ]
        );
        // Full, but the box went unread: still the PC, never a 7th slot.
        let events = caught_events(&data, &state, "SPECIES_PIDGEY", 6, None);
        assert!(matches!(
            events.as_slice(),
            [
                GameEvent::SpeciesCaught { .. },
                GameEvent::SentToPc {
                    box_index: None,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn catch_joins_the_party() {
        let Some(data) = data() else { return };
        let state = party_of(&data, 1);
        let events = caught_events(&data, &state, "SPECIES_PIDGEY", 6, None);
        let [GameEvent::SpeciesCaught { species }, GameEvent::PartyMonDerived { slot, mon }] =
            events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!((species.as_str(), *slot), ("SPECIES_PIDGEY", 1));
        assert_eq!(mon.species.value.as_deref(), Some("SPECIES_PIDGEY"));
        assert_eq!(mon.species.source, pokebot_state::KnowledgeSource::Derived);
        assert_eq!(mon.level.value, Some(6));
        assert_eq!(mon.level.source, pokebot_state::KnowledgeSource::Derived);
        let moves: Vec<(String, (u8, u8))> = mon
            .moves
            .iter()
            .flatten()
            .map(|m| (m.mv.value.clone().unwrap(), m.pp.value.unwrap()))
            .collect();
        let expected: Vec<(String, (u8, u8))> = data
            .default_moves("SPECIES_PIDGEY", 6)
            .into_iter()
            .map(|m| {
                let pp = data.move_(&m).unwrap().pp;
                (m, (pp, pp))
            })
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(moves, expected);
        let state = DefaultReducer.reduce(
            &state,
            &events
                .into_iter()
                .map(|event| EventRecord { frame_id: 2, event })
                .collect::<Vec<_>>(),
        );
        assert_eq!(state.party.value.as_ref().map(Vec::len), Some(2));
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
        // Sleep Powder first.
        assert_eq!(
            decide_on(&data, &party, &mut memory, &wild(3, command, 1000)),
            "choose FIGHT"
        );
        let label = decide_on(&data, &party, &mut memory, &wild(5, moves, 1000));
        assert!(label.contains("SLEEP_POWDER"), "{label}");
        // Asleep at full HP: IVYSAUR Lv18 has no move whose crit can't KO a
        // Lv6 PIDGEY (Vine Whip's crit max is 43 against 20 HP): throw.
        let attempt = memory.catch.attempt.as_mut().unwrap();
        attempt.opened = true;
        attempt.foe_status = FoeStatus::Asleep;
        let lead = Lead {
            member: &party.members[0],
            hp: (54, 54),
        };
        let asleep = Foe {
            status: FoeStatus::Asleep,
            ..foe("SPECIES_PIDGEY", 6)
        };
        assert_eq!(weakening_move(&data, &lead, &asleep), None);
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

    #[test]
    fn a_safe_attack_weakens_a_sleeping_foe() {
        let Some(data) = data() else { return };
        // BULBASAUR Lv12: Vine Whip's crit max (19) is below PIDGEY Lv6's
        // 20 HP; Tackle's (22) is not.
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
        assert!(memory.catch.attempt.is_some(), "{events:?}");
        let label = decide_on(&data, &party, &mut memory, &wild(3, moves, 1000));
        assert_eq!(label, "cursor to move 2 (SLEEP_POWDER): Right");
        // Moving the cursor is not using it.
        memory.catch.on_move_confirmed();
        assert!(!memory.catch.attempt.as_ref().unwrap().opened);
        let on_it = BattleMenu::Moves { column: 1, row: 0 };
        let label = decide_on(&data, &party, &mut memory, &wild(3, on_it, 1000));
        assert_eq!(label, "choose move 2 (SLEEP_POWDER)");
        // The opener is used once its choice is confirmed.
        memory.catch.on_move_confirmed();
        assert!(memory.catch.attempt.as_ref().unwrap().opened);
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
        assert_eq!(
            decide_on(&data, &party, &mut memory, &wild(7, command, 1000)),
            "choose FIGHT"
        );
        let label = decide_on(&data, &party, &mut memory, &wild(9, moves, 1000));
        assert_eq!(label, "cursor to move 4 (VINE_WHIP): Down");
        assert_eq!(memory.last_slot, Some((0, 3)));
        // VINE WHIP under DISABLE (live: Route 3, the bot chose a disabled
        // move forever): no safe attack is left, so throw instead.
        memory.disabled = Some("MOVE_VINE_WHIP".into());
        let label = decide_on(&data, &party, &mut memory, &wild(10, moves, 1000));
        assert_eq!(label, "back to the command menu to throw");
        memory.disabled = None;
        // Weakened (200‰): BAG.
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
                status_move: None,
                expected_throws: 1,
                risk: 0.0,
                shiny: false,
            },
            opened: true,
            throws: 0,
            foe_status: FoeStatus::None,
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
                    status_move: None,
                    expected_throws: 2,
                    risk: 0.0,
                    shiny: false,
                },
                opened: true,
                throws: 0,
                foe_status: FoeStatus::None,
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
        // the risk limit, so throw at once rather than RUN.
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
        assert_eq!(
            decide_on(&data, &party, &mut memory, &shiny(3, (40, 54))),
            "cursor to BAG: Right"
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
