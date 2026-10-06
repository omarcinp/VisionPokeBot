//! Battle control: menu navigation verified cursor by cursor, and move
//! choice from the battle evaluator (best move against the identified
//! opponent, keeping PP of strong moves for trainer battles).

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_planner::evaluate::best_move;
use pokebot_planner::Combatant;
use pokebot_state::{BattleMenu, BattleObservation, GameEvent, Observation, ScreenState, Status};

use crate::catch::{self, CatchMemory};
use crate::party::{display_name, plausible_hp_for, Party};
use crate::{Action, Decision, Expectation};

#[derive(Debug, Clone, Copy)]
pub struct BattlePolicy {
    /// Try to RUN from wild battles when our HP bar is below this (per mille).
    pub flee_below: u16,
    /// RUN attempts per battle before fighting instead.
    pub max_run_attempts: u32,
    /// In wild battles, moves with at most this many PP left are saved for
    /// trainers (moves with 30+ PP are always usable).
    pub wild_pp_reserve: u8,
    /// The battle is to be lost: [`losing_move`] every turn.
    pub lose: bool,
}

impl Default for BattlePolicy {
    fn default() -> Self {
        Self {
            flee_below: 350,
            max_run_attempts: 3,
            wild_pp_reserve: 5,
            lose: false,
        }
    }
}

/// Per-battle memory.
#[derive(Debug, Default, Clone)]
pub struct BattleMemory {
    pub run_attempts: u32,
    /// A trainer battle (can't flee; spend PP freely).
    pub trainer: bool,
    /// The trainer as the battle names them ("YOUNGSTER BEN would like to
    /// battle!").
    pub trainer_name: Option<String>,
    /// Last move chosen (for PP accounting).
    pub last_move: Option<String>,
    /// (party slot, move slot) of the last move chosen.
    pub last_slot: Option<(u8, u8)>,
    /// Identifying the wild opponent and catching it.
    pub catch: CatchMemory,
    /// Our lead's move under the foe's DISABLE, read from battle text; never
    /// chosen until "… is disabled no more!".
    pub disabled: Option<String>,
    /// Our battler's stat stages (HP, Atk, Def, Spe, SpA, SpD; −6..=6) as
    /// the battle text tells them ("MANKEY's DEFENSE fell!").
    pub our_stages: [i8; 6],
    /// The trainer's Pokémon out is asleep ("Foe X fell asleep!" until
    /// "Foe X woke up!"), and the sleep moves chosen against it.
    pub foe_asleep: bool,
    pub sleep_tries: u8,
    /// Why the battle is being run from, once told.
    pub fled_for: Option<String>,
    /// Our battler's accuracy stage and the foe's evasion stage (−6..=6)
    /// from the battle text ("VENUSAUR's accuracy fell!", "Foe X's
    /// evasiveness rose!"): the evaluator scales hit chances by them.
    pub our_accuracy: i8,
    pub foe_evasion: i8,
    /// What the foe's moves and abilities forbid (a trap, a refused move).
    pub limits: crate::limits::Limits,
    /// Our battler's status, as the party and the battle text tell it.
    pub lead_status: Option<pokebot_state::Status>,
    /// Move types the foe out took nothing from ("It doesn't affect …"),
    /// until another foe comes out.
    pub no_effect: Vec<String>,
    /// The one out has no move the game takes and the foe feels: a wild
    /// battle is run from.
    pub nothing_to_use: bool,
}

impl BattleMemory {
    /// Whether the game accepts our move `mv` now: not DISABLEd, nor
    /// refused by TAUNT, TORMENT, IMPRISON or a CHOICE BAND.
    pub fn allows(&self, data: &GameData, mv: &str) -> bool {
        self.disabled.as_deref() != Some(mv)
            && self.limits.allows(data, mv, self.last_move.as_deref())
            && !data
                .move_(mv)
                .and_then(|m| m.kind.as_deref())
                .is_some_and(|k| self.no_effect.iter().any(|t| t == k))
    }
}

/// The foe on the HUD, if it can be read.
pub fn hud_foe(data: &GameData, battle: &BattleObservation) -> Option<catch::Foe> {
    Some(catch::Foe {
        species: data
            .species_named(battle.opponent_name.as_deref()?)?
            .to_owned(),
        level: battle.opponent_level?,
        hp_per_mille: battle.opponent_hp.unwrap_or(1000),
        status: catch::FoeStatus::None,
        shiny: false,
        caught: None,
    })
}

/// P(the one out, `party`'s first, faints within two turns) against the
/// foe on the HUD, with our stat stages; `None` when either can't be read.
pub fn lead_risk(
    data: &GameData,
    party: &Party,
    battle: &BattleObservation,
    stages: [i8; 6],
) -> Option<f64> {
    let active = party.lead()?;
    let foe = hud_foe(data, battle)?;
    let hp = battle.player_hp_numbers.or(active.hp)?;
    Some(catch::risk_staged(
        data,
        &catch::Lead { member: active, hp },
        &foe,
        2,
        stages,
    ))
}

/// The species a trainer's page says comes next ("LEADER ERIKA is about
/// to use VICTREEBEL."), as its constant.
pub fn next_foe(data: &GameData, page: &str) -> Option<String> {
    let (_, rest) = page.split_once(" is about to use ")?;
    let name = rest.split(['.', '!']).next()?.trim();
    data.species_named(name).map(str::to_owned)
}

/// Before a trainer's next Pokémon (`foe`) comes out, the member to send
/// against it: the one that beats it best, when that isn't the one out
/// (`active`) and beats it clearly better, or as surely in half the turns
/// or fewer (the HP saved is for the rest of the battle; the SHIFT before
/// the foe comes out costs no turn). The team is planned this way
/// (`pokebot_planner::team_vs_trainer`): a VENUSAUR whose grass ERIKA's
/// VICTREEBEL resists makes way for a flier. With that member's chance
/// and the one out's.
pub fn member_against(
    data: &GameData,
    party: &Party,
    active: u8,
    foe: &catch::Foe,
) -> Option<(u8, f64, f64)> {
    let odds = |m: &crate::party::Member| -> Option<(f64, f64)> {
        let hp = m.hp.filter(|(hp, _)| *hp > 0)?;
        let m = catch::matchup_against(data, &catch::Lead { member: m, hp }, foe)?;
        Some((m.p_win, m.turns))
    };
    let out = party
        .members
        .iter()
        .find(|m| m.slot == active)
        .and_then(odds)
        .unwrap_or((0.0, f64::INFINITY));
    // Winners (even odds or better) by turns, else by chance.
    let key = |(p, turns): (f64, f64)| (p >= 0.5, if p >= 0.5 { -turns } else { p });
    let (slot, best) = party
        .members
        .iter()
        .filter(|m| m.slot != active)
        .filter_map(|m| Some((m.slot, odds(m)?)))
        .max_by(|a, b| {
            let (ka, kb) = (key(a.1), key(b.1));
            ka.0.cmp(&kb.0)
                .then(ka.1.total_cmp(&kb.1))
                .then(b.0.cmp(&a.0))
        })?;
    let margin = crate::tools::party_order::LEAD_MARGIN;
    let clearer = best.0 > out.0 + margin;
    let faster = best.0 >= out.0 - margin && out.0 >= 0.5 && best.1 * 2.0 <= out.1;
    (clearer || faster).then_some((slot, best.0, out.0))
}

/// Whether the one out (a switch-trained trainee) should fight the foe on
/// the HUD itself rather than hand it to `carrier`: it wins, and not so
/// slowly that the carrier's quick win (half the experience) pays better
/// (fleet worker 4: a Lv10 PARAS scratched at Lv13–16 ODDISH alone while
/// VENUSAUR waited).
pub fn alone_pays(data: &GameData, party: &Party, battle: &BattleObservation, carrier: u8) -> bool {
    let (Some(active), Some(foe)) = (party.lead(), hud_foe(data, battle)) else {
        return false;
    };
    let Some(hp) = battle.player_hp_numbers.or(active.hp) else {
        return false;
    };
    let Some(alone) = catch::turns_to_win(data, &catch::Lead { member: active, hp }, &foe) else {
        return false;
    };
    let carried = party
        .members
        .iter()
        .find(|m| m.slot == carrier)
        .and_then(|m| {
            catch::turns_to_win(
                data,
                &catch::Lead {
                    member: m,
                    hp: m.hp?,
                },
                &foe,
            )
        });
    carried.is_none_or(|c| pokebot_planner::prepare::alone_pays(alone, c))
}

/// In a battle that can't be run from (a trainer's), the member to SHIFT
/// to when the one out is at risk against the foe on the HUD (above
/// [`catch::risk_limit`]: with others to send out, a faint is not the
/// end): the healthy member with the least risk (over three turns: the
/// switch gives the foe a free attack), when that is safe or half the risk
/// at most. `party` has the battler out first.
pub fn defensive_switch(
    data: &GameData,
    party: &Party,
    battle: &BattleObservation,
    stages: [i8; 6],
) -> Option<u8> {
    let foe = hud_foe(data, battle)?;
    let at_risk = lead_risk(data, party, battle, stages)?;
    if at_risk <= catch::risk_limit(party.backed()) {
        return None;
    }
    let (slot, risk) = party
        .members
        .iter()
        .skip(1)
        // One that can hit back: a member with no attack is "safe" only
        // in that it takes the hits (fleet emu3 at LASS MIRIAM: WARTORTLE
        // at risk made way for KAKUNA Lv4, HARDEN only, which ODDISH's
        // ABSORB drained to a faint).
        .filter(|m| {
            choose_move(
                data,
                &Party {
                    members: vec![(*m).clone()],
                },
                None,
                &BattleMemory::default(),
                &BattlePolicy::default(),
            )
            .is_some()
        })
        .filter_map(|m| {
            let hp = m.hp.filter(|(hp, _)| *hp > 0)?;
            let risk = catch::risk(data, &catch::Lead { member: m, hp }, &foe, 3);
            Some((m.slot, risk))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))?;
    (risk <= catch::RISK_LIMIT || risk * 2.0 <= at_risk).then_some(slot)
}

/// The stage change a page tells of our battler named `lead`, per stat.
pub fn stage_text(page: &str, lead: &str) -> Option<(usize, i8)> {
    let rest = [format!("{lead}’s "), format!("{lead}'s ")]
        .iter()
        .find_map(|p| page.strip_prefix(p.as_str()))?;
    let index = [
        ("ATTACK", 1),
        ("DEFENSE", 2),
        ("SPEED", 3),
        ("SP. ATK", 4),
        ("SP. DEF", 5),
    ]
    .into_iter()
    .find(|(s, _)| rest.starts_with(s))
    .map(|(_, i)| i)?;
    let delta = if rest.contains("harshly fell") {
        -2
    } else if rest.contains("fell") {
        -1
    } else if rest.contains("sharply rose") {
        2
    } else if rest.contains("rose") {
        1
    } else {
        return None;
    };
    Some((index, delta))
}

/// Whose stage an accuracy/evasion page is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ours,
    Foe,
}

/// "VENUSAUR's accuracy fell!" (our lead, `lead` its printed name) or
/// "Foe CHARMELEON's evasiveness rose!": whose, and by how many stages.
/// Our evasion and the foe's accuracy aren't tracked.
pub fn accuracy_text(page: &str, lead: &str) -> Option<(Side, i8)> {
    let (side, rest, stat) = if let Some(rest) = page.strip_prefix("Foe ") {
        let rest = rest.split_once("’s ").or_else(|| rest.split_once("'s "))?.1;
        (Side::Foe, rest, "evasiveness")
    } else {
        let rest = [format!("{lead}’s "), format!("{lead}'s ")]
            .iter()
            .find_map(|p| page.strip_prefix(p.as_str()))?;
        (Side::Ours, rest, "accuracy")
    };
    let rest = rest.strip_prefix(stat)?;
    let delta = if rest.contains("harshly fell") {
        -2
    } else if rest.contains("fell") {
        -1
    } else if rest.contains("sharply rose") {
        2
    } else if rest.contains("rose") {
        1
    } else {
        return None;
    };
    Some((side, delta))
}

/// DISABLE in battle text about our lead (`lead`, its printed name):
/// `Some(Some(move))` for "IVYSAUR's VINE WHIP was disabled!" (the foe used
/// DISABLE) and "… is disabled!" (the move was chosen anyway),
/// `Some(None)` for "IVYSAUR is disabled no more!". Pages about the foe
/// ("Foe …", "Wild …") never match. The font's apostrophe reads as `’`.
pub fn disable_text(page: &str, lead: &str, data: &GameData) -> Option<Option<String>> {
    let page = page.replace('’', "'");
    if page == format!("{lead} is disabled no more!") {
        return Some(None);
    }
    let rest = page.strip_prefix(&format!("{lead}'s "))?;
    let name = rest
        .strip_suffix(" was disabled!")
        .or_else(|| rest.strip_suffix(" is disabled!"))?;
    data.move_named(name).map(|m| Some(m.to_owned()))
}

/// Our lead's major status from battle text: "IVYSAUR is paralyzed!",
/// "…was poisoned!", "…fell asleep!", "…was burned!", "…was frozen
/// solid!", and its end ("…woke up!", "…was defrosted!", "…was cured…").
/// Pages about "Foe …"/"Wild …" never match (they start with the prefix,
/// not our lead's name). The HUD's status badge isn't read, so this text is
/// the only source of the lead's status.
pub fn lead_status_text(page: &str, lead: &str) -> Option<Status> {
    let rest = page.strip_prefix(lead)?.strip_prefix(' ')?;
    const CHANGES: [(&str, Status); 12] = [
        ("woke up", Status::Healthy),
        ("was defrosted", Status::Healthy),
        ("thawed out", Status::Healthy),
        ("was cured", Status::Healthy),
        ("is paralyzed", Status::Paralyzed),
        ("was paralyzed", Status::Paralyzed),
        ("is badly poisoned", Status::BadlyPoisoned),
        ("was badly poisoned", Status::BadlyPoisoned),
        ("was poisoned", Status::Poisoned),
        ("fell asleep", Status::Asleep),
        ("was burned", Status::Burned),
        ("was frozen", Status::Frozen),
    ];
    CHANGES
        .iter()
        .find(|(text, _)| rest.starts_with(text))
        .map(|(_, status)| *status)
}

/// Battle text read on two frames ([`crate::catch::observe`]): tracks
/// DISABLE on our lead.
pub fn observe_page(memory: &mut BattleMemory, page: &str, party: &Party, data: &GameData) {
    if page.starts_with("Wild ") && page.contains("appeared") {
        memory.trainer = false;
        memory.no_effect.clear();
    } else if page.contains("sent out") || page.contains("would like to battle") {
        memory.trainer = true;
        if let Some((name, _)) = page.split_once(" would like to battle") {
            memory.trainer_name = Some(name.trim().to_owned());
        }
    }
    // Our move did nothing to the foe (Switch, Pokémon Tower: VENUSAUR
    // used TACKLE on a GASTLY twice, "It doesn't affect Wild GASTLY…",
    // the foe not yet read off the HUD): its type is out for this foe.
    if page.starts_with("It doesn") && page.contains("t affect") {
        if let Some(kind) = memory
            .last_move
            .as_deref()
            .and_then(|m| data.move_(m))
            .and_then(|m| m.kind.clone())
        {
            if !memory.no_effect.contains(&kind) {
                memory.no_effect.push(kind);
            }
        }
    }
    if page.contains("sent out") {
        memory.no_effect.clear();
        memory.foe_asleep = false;
        memory.sleep_tries = 0;
        memory.foe_evasion = 0;
    }
    let lead_name = party.lead().map(|l| l.display_name()).unwrap_or_default();
    if let Some((whose, delta)) = accuracy_text(page, &lead_name) {
        match whose {
            Side::Ours => memory.our_accuracy = (memory.our_accuracy + delta).clamp(-6, 6),
            Side::Foe => memory.foe_evasion = (memory.foe_evasion + delta).clamp(-6, 6),
        }
    }
    if let Some(rest) = page.strip_prefix("Foe ") {
        if ["fell asleep!", "is fast asleep", "is already asleep"]
            .iter()
            .any(|p| rest.contains(p))
        {
            memory.foe_asleep = true;
        } else if rest.contains("woke up!") {
            memory.foe_asleep = false;
        }
    }
    memory.limits.observe(page, &lead_name, data);
    let Some(lead) = party.lead() else { return };
    if let Some(disabled) = disable_text(page, &lead.display_name(), data) {
        memory.disabled = disabled;
    }
    if let Some((i, d)) = stage_text(page, &lead.display_name()) {
        memory.our_stages[i] = (memory.our_stages[i] + d).clamp(-6, 6);
    }
}

/// The opponent as a combatant, if its name and level can be read.
pub fn identify_opponent(data: &GameData, observation: &Observation) -> Option<Combatant> {
    let battle = observation.battle.as_ref()?;
    let read = battle.opponent_name.as_deref()?;
    let level = battle.opponent_level?;
    let names: Vec<(String, &String)> = data.species.keys().map(|k| (display_name(k), k)).collect();
    let species = pokebot_vision::detect::hud::resolve(read, names.iter().map(|(n, _)| n.as_str()))
        .and_then(|n| names.iter().find(|(d, _)| d == n))
        .map(|(_, k)| (*k).clone())?;
    Combatant::new(
        data,
        &species,
        level,
        data.default_moves(&species, level),
        15,
    )
}

use pokebot_planner::evaluate::UNRELIABLE_EFFECTS;

/// The move slot to use: the evaluator's best damaging move among those
/// with PP to spare; in wild battles, the trainer reserve is spent only when
/// nothing else is left. A disabled move is never chosen (the game refuses
/// it and returns to the move menu). `None` when no damaging move has PP.
pub fn choose_move(
    data: &GameData,
    party: &Party,
    opponent: Option<&Combatant>,
    memory: &BattleMemory,
    policy: &BattlePolicy,
) -> Option<(u8, String)> {
    let lead = party.lead()?;
    let damaging = |m: &String| {
        data.move_(m).is_some_and(|mv| {
            mv.power > 0
                && !mv
                    .effect
                    .as_deref()
                    .is_some_and(|e| UNRELIABLE_EFFECTS.contains(&e))
        }) && memory.allows(data, m)
    };
    let spare = |m: &String| {
        let left = lead.pp_left(data, m);
        let max = data.move_(m).map_or(0, |mv| mv.pp);
        left > 0 && (memory.trainer || max >= 30 || left > policy.wild_pp_reserve)
    };
    let with_pp = |m: &String| lead.pp_left(data, m) > 0;
    let pick = |usable: Vec<String>| -> Option<String> {
        let best = opponent.and_then(|foe| {
            let mut us = Combatant::new(data, &lead.species, lead.level, usable.clone(), 10)?;
            us.acc_stage = memory.our_accuracy;
            let mut foe = foe.clone();
            foe.evasion_stage = memory.foe_evasion;
            best_move(data, &us, &foe).map(|(m, _)| m)
        });
        // Unknown opponent: strongest usable move by power.
        best.or_else(|| {
            usable
                .iter()
                .max_by_key(|m| data.move_(m).map_or(0, |mv| mv.power))
                .cloned()
        })
    };
    let tier = |keep: &dyn Fn(&String) -> bool| -> Vec<String> {
        lead.moves
            .iter()
            .filter(|m| damaging(m) && keep(m))
            .cloned()
            .collect()
    };
    let chosen = pick(tier(&spare)).or_else(|| pick(tier(&with_pp)))?;
    let slot = lead.moves.iter().position(|x| *x == chosen)? as u8;
    Some((slot, chosen))
}

/// Move effects that hurt the foe without power: poison, a seed, confusion
/// (it hits itself), a fixed-damage or OHKO move.
const HARMFUL_EFFECTS: [&str; 9] = [
    "EFFECT_POISON",
    "EFFECT_TOXIC",
    "EFFECT_LEECH_SEED",
    "EFFECT_CONFUSE",
    "EFFECT_SWAGGER",
    "EFFECT_FLATTER",
    "EFFECT_LEVEL_DAMAGE",
    "EFFECT_DRAGON_RAGE",
    "EFFECT_SONICBOOM",
];

/// A move that does nothing to the foe (GROWL, TAIL WHIP): no power and
/// none of [`HARMFUL_EFFECTS`]. A move not known (unread) may hurt.
pub fn harmless(data: &GameData, mv: &str) -> bool {
    data.move_(mv).is_some_and(|m| {
        m.power == 0
            && m.effect
                .as_deref()
                .is_none_or(|e| !HARMFUL_EFFECTS.contains(&e))
    })
}

/// The move least likely to win a battle that is to be lost: a
/// [`harmless`] one first, then the weakest; slot order breaks ties. Only
/// moves with PP the game accepts; `None` when none has PP (the game uses
/// STRUGGLE by itself).
pub fn losing_move(data: &GameData, party: &Party, memory: &BattleMemory) -> Option<(u8, String)> {
    let lead = party.lead()?;
    lead.moves
        .iter()
        .enumerate()
        .filter(|(_, m)| lead.pp_left(data, m) > 0 && memory.allows(data, m))
        .min_by_key(|(slot, m)| {
            // A harmful move without power (SUPERSONIC's confusion) is
            // priced like a weak attack.
            let power = data
                .move_(m)
                .map_or(u16::MAX, |mv| if mv.power > 0 { mv.power } else { 40 });
            (!harmless(data, m), power, *slot)
        })
        .map(|(slot, m)| (slot as u8, m.clone()))
}

/// Any move the game will accept when no damaging move can be chosen (the
/// only one is disabled or out of PP): the first non-disabled move with PP,
/// status moves included, in slot order. `None` when no move has PP (the
/// game then uses STRUGGLE by itself).
pub fn fallback_move(
    data: &GameData,
    party: &Party,
    memory: &BattleMemory,
) -> Option<(u8, String)> {
    let lead = party.lead()?;
    lead.moves
        .iter()
        .enumerate()
        .find(|(_, m)| lead.pp_left(data, m) > 0 && memory.allows(data, m))
        .map(|(slot, m)| (slot as u8, m.clone()))
}

/// A move with PP the game takes, whether or not the foe out feels it
/// (its type had no effect): a turn spent when nothing else is left.
fn ignored_move(data: &GameData, party: &Party, memory: &BattleMemory) -> Option<(u8, String)> {
    let lead = party.lead()?;
    lead.moves
        .iter()
        .enumerate()
        .find(|(_, m)| {
            lead.pp_left(data, m) > 0
                && memory.disabled.as_deref() != Some(m.as_str())
                && memory.limits.allows(data, m, memory.last_move.as_deref())
        })
        .map(|(slot, m)| (slot as u8, m.clone()))
}

/// The next battle input, if a battle menu is open.
pub fn decide(
    observation: &Observation,
    policy: &BattlePolicy,
    memory: &mut BattleMemory,
    party: &Party,
    data: &GameData,
    events: &mut Vec<GameEvent>,
) -> Option<Decision> {
    let battle = observation.battle.as_ref()?;
    let menu = battle.menu?;
    if matches!(menu, BattleMenu::Command { .. }) {
        memory.limits.at_command_menu();
    }
    // A catch attempt drives the menus (its risk check replaces fleeing);
    // `None` means it was abandoned and the battle goes on as usual.
    if memory.catch.attempt.is_some() {
        if let Some(decision) =
            catch::attempt_decision(observation, policy, memory, party, data, events)
        {
            return Some(decision);
        }
    }
    // Wild battles: the opponent is identified (and the catch decided) on
    // the command menu before the first choice.
    if !memory.trainer && !memory.catch.decided && matches!(menu, BattleMenu::Command { .. }) {
        return Some(Decision::Wait("identifying the wild opponent".into()));
    }
    // Low HP: plausible HUD numbers first (the bar isn't always read on
    // the Switch), then the bar. Flee wild encounters while the lead is
    // at risk; a failed RUN is retried on the next turn.
    let lead = party.lead();
    let level = battle
        .player_level
        .or_else(|| lead.map(|m| m.level))
        .unwrap_or(0);
    let usable = party
        .members
        .iter()
        .filter(|m| {
            m.hp.is_some_and(|hp| plausible_hp_for(hp, m.level, Some(&m.species)) && hp.0 > 0)
        })
        .count();
    let flee_below = if usable <= 1 {
        policy.flee_below.max(500)
    } else {
        policy.flee_below
    };
    let numbers = battle
        .player_hp_numbers
        .filter(|hp| plausible_hp_for(*hp, level, lead.map(|m| m.species.as_str())))
        .or_else(|| {
            lead.and_then(|m| m.hp)
                .filter(|hp| plausible_hp_for(*hp, level, lead.map(|m| m.species.as_str())))
        });
    let low = numbers
        .map(|(hp, max)| u32::from(hp) * 1000 < u32::from(max) * u32::from(flee_below))
        .or_else(|| battle.player_hp.map(|hp| hp < flee_below))
        // If no trustworthy HP is available, a wild encounter must not
        // gamble the only known battler on a fight.
        .unwrap_or(usable <= 1);
    let no_attacks = choose_move(data, party, None, memory, policy).is_none();
    let opponent = identify_opponent(data, observation);
    // Allow for a failed escape followed by another attack. A full HP bar
    // can still be unsafe against a much stronger wild opponent.
    let mut risk = None;
    let high_risk = match (
        lead,
        numbers,
        battle.opponent_name.as_deref(),
        battle.opponent_level,
    ) {
        (Some(member), Some(hp), Some(name), Some(level)) => {
            data.species_named(name).is_none_or(|species| {
                let foe = catch::Foe {
                    species: species.to_owned(),
                    level,
                    hp_per_mille: battle.opponent_hp.unwrap_or(1000),
                    status: catch::FoeStatus::None,
                    shiny: false,
                    caught: battle.opponent_caught,
                };
                let r = catch::risk_staged(
                    data,
                    &catch::Lead { member, hp },
                    &foe,
                    2,
                    memory.our_stages,
                );
                risk = Some(r);
                r > catch::risk_limit(usable > 1)
            })
        }
        _ => true,
    };
    // With another member to send out, the risk decides alone when it
    // could be priced: a low HP bar is no reason to run from a battle the
    // one out still wins (the user's rule: Pokémon may faint, as long as
    // not all of them do).
    let low = low && !(usable > 1 && risk.is_some());
    let wants_out = (low || high_risk || no_attacks || memory.catch.flee || memory.nothing_to_use)
        && !memory.trainer;
    // Trapped (ARENA TRAP, MEAN LOOK, WRAP…): the game refuses RUN and
    // puts the menu back; fight on.
    let flee = wants_out && memory.limits.can_run();
    if wants_out && !flee && memory.fled_for.as_deref() != Some("trapped") {
        events.push(GameEvent::GoalProgress {
            goal: "Story".into(),
            phase: "Battle".into(),
            detail: format!(
                "can't run ({}): fighting on",
                memory.limits.why().unwrap_or_default()
            ),
        });
        memory.fled_for = Some("trapped".into());
    }
    if flee {
        // Once a battle, with every reason: a run costs the training
        // battle, and a hunt heals after one.
        let why: Vec<String> = [
            low.then(|| format!("HP {numbers:?} under {flee_below}‰")),
            high_risk.then(|| match risk {
                Some(r) => format!(
                    "faint risk {:.1}% in 2 turns (stages {:?})",
                    r * 100.0,
                    memory.our_stages
                ),
                None => format!(
                    "risk unknown (HP {numbers:?}, foe {:?} Lv{:?})",
                    battle.opponent_name, battle.opponent_level
                ),
            }),
            no_attacks.then(|| "no attacking move".to_owned()),
            memory
                .nothing_to_use
                .then(|| "no move the foe feels".to_owned()),
            memory.catch.flee.then(|| "not catching it".to_owned()),
        ]
        .into_iter()
        .flatten()
        .collect();
        let why = why.join(", ");
        if memory.fled_for.as_deref() != Some(why.as_str()) {
            events.push(GameEvent::GoalProgress {
                goal: "Story".into(),
                phase: "Battle".into(),
                detail: format!("running: {why}"),
            });
            memory.fled_for = Some(why);
        }
    }
    Some(match menu {
        BattleMenu::Command { column, row } if flee => step_toward(
            (column, row),
            (1, 1),
            |c, r| BattleMenu::Command { column: c, row: r },
            "RUN",
            Expectation::ScreenIsNot(ScreenState::BattleCommand),
        ),
        BattleMenu::Command { column, row } => step_toward(
            (column, row),
            (0, 0),
            |c, r| BattleMenu::Command { column: c, row: r },
            "FIGHT",
            // ENCORE and STRUGGLE use their move without the move menu.
            Expectation::ScreenIsNot(ScreenState::BattleCommand),
        ),
        BattleMenu::Moves { .. } if flee => Decision::Act(Action::new(
            "back to the command menu to RUN",
            vec![ControllerCommand::Press(Button::B)],
            Expectation::ScreenIs(ScreenState::BattleCommand),
            45,
        )),
        BattleMenu::Moves { column, row } => {
            let menu_party = with_menu_moves(data, party, battle);
            let party = menu_party.as_ref().unwrap_or(party);
            let chosen = if policy.lose {
                losing_move(data, party, memory)
            } else {
                let opener = sleep_opener(data, party, opponent.as_ref(), battle, memory);
                if opener.is_some() {
                    memory.sleep_tries += 1;
                }
                opener.or_else(|| choose_move(data, party, opponent.as_ref(), memory, policy))
            };
            let chosen = chosen.or_else(|| fallback_move(data, party, memory));
            // Nothing the foe out feels (fleet continue-5, Pokémon Tower: a
            // wild GASTLY, a RATTATA with only NORMAL moves out; the party
            // the run-or-fight check read had another lead, and the turn
            // failed "no move has PP left" again and again). A wild battle
            // is run from; a trainer's turn is spent on a move it ignores.
            let chosen = match chosen {
                Some(c) => Some(c),
                None if !memory.trainer && memory.limits.can_run() => {
                    memory.nothing_to_use = true;
                    return Some(Decision::Act(Action::new(
                        "back to the command menu to RUN",
                        vec![ControllerCommand::Press(Button::B)],
                        Expectation::ScreenIs(ScreenState::BattleCommand),
                        45,
                    )));
                }
                None => ignored_move(data, party, memory),
            };
            let Some((slot, name)) = chosen else {
                return Some(Decision::Fail("no move has PP left".into()));
            };
            memory.last_move = Some(name.clone());
            memory.last_slot = party.lead().map(|lead| (lead.slot, slot));
            let label = format!("move {} ({})", slot + 1, name.trim_start_matches("MOVE_"));
            step_toward(
                (column, row),
                (slot % 2, slot / 2),
                |c, r| BattleMenu::Moves { column: c, row: r },
                &label,
                Expectation::ScreenIsNot(ScreenState::BattleMoveSelection),
            )
        }
    })
}

/// The party with the moves the move menu shows for the one out, when
/// they differ from what the knowledge holds: the menu is what the cursor
/// moves over (fleet worker 2 at BROCK: CHARMANDER's METAL CLAW, move 4,
/// aimed at on WEEDLE's two-move menu after the faint, the one out taken
/// for the fainted one). `None` when they agree or the menu isn't read.
fn with_menu_moves(data: &GameData, party: &Party, battle: &BattleObservation) -> Option<Party> {
    let lead = party.lead()?;
    let shown: Vec<String> = battle
        .move_names
        .iter()
        .filter(|n| !n.is_empty() && n.as_str() != "-")
        .map(|n| data.move_named(n).map(str::to_owned))
        .collect::<Option<_>>()?;
    if shown.is_empty() || shown == lead.moves {
        return None;
    }
    let mut party = party.clone();
    let lead = &mut party.members[0];
    lead.pp_used.retain(|m, _| shown.contains(m));
    lead.moves = shown;
    Some(party)
}

/// Sleep attempts per foe before attacking regardless.
const MAX_SLEEP_TRIES: u8 = 2;
/// A fight attacking alone wins with at least this: no opener needed.
const SURE_WIN: f64 = 0.99;
/// The turn spent putting the foe to sleep must be affordable: our risk of
/// fainting to its next attack stays under this.
const OPENER_RISK: f64 = 0.05;

/// In a trainer's battle (it must be won), the sleep move to open with,
/// as a player would: when attacking alone isn't a sure win from here,
/// our best move doesn't knock the foe out at once, and one of its
/// attacks can't faint us. A sleeping foe loses turns while it is beaten
/// (Switch: IVYSAUR, SAND-ATTACKed by the rival's PIDGEOTTO, missed
/// three TACKLEs against his CHARMANDER and fainted, SLEEP POWDER unused).
pub fn sleep_opener(
    data: &GameData,
    party: &Party,
    opponent: Option<&Combatant>,
    battle: &pokebot_state::BattleObservation,
    memory: &BattleMemory,
) -> Option<(u8, String)> {
    if !memory.trainer || memory.foe_asleep || memory.sleep_tries >= MAX_SLEEP_TRIES {
        return None;
    }
    let lead = party.lead()?;
    let foe = opponent?;
    let (slot, sleep) = lead.moves.iter().enumerate().find(|(_, m)| {
        data.move_(m)
            .is_some_and(|mv| mv.effect.as_deref() == Some("EFFECT_SLEEP"))
            && lead.pp_left(data, m) > 0
            && memory.allows(data, m)
    })?;
    let hp = battle
        .player_hp_numbers
        .filter(|h| plausible_hp_for(*h, lead.level, Some(&lead.species)))
        .or(lead.hp)?;
    let mut us = Combatant::new(data, &lead.species, lead.level, lead.moves.clone(), 10)?;
    if let Some(stats) = lead.stats(data) {
        us.stats = stats;
    }
    us.hp = u32::from(hp.0);
    us.acc_stage = memory.our_accuracy;
    let mut them = foe.clone();
    them.evasion_stage = memory.foe_evasion;
    them.hp = (foe.hp * u32::from(battle.opponent_hp.unwrap_or(1000)))
        .div_ceil(1000)
        .max(1);
    if pokebot_planner::evaluate::matchup(data, &us, &them).p_win >= SURE_WIN {
        return None;
    }
    if pokebot_planner::evaluate::faint_probability(data, &us, &them, 1) >= SAFE_KO {
        return None;
    }
    let as_foe = catch::Foe {
        species: foe.species.clone(),
        level: foe.level,
        hp_per_mille: battle.opponent_hp.unwrap_or(1000),
        status: catch::FoeStatus::None,
        shiny: false,
        caught: None,
    };
    let risk = catch::risk_staged(
        data,
        &catch::Lead { member: lead, hp },
        &as_foe,
        1,
        memory.our_stages,
    );
    (risk < OPENER_RISK).then(|| (slot as u8, sleep.clone()))
}

/// Probability of a KO in one hit to count as sure (a 95 %-accurate move
/// such as Tackle against a nearly fainted foe counts).
const SAFE_KO: f64 = 0.9;

/// Whether our lead surely faints the (identified) opponent before it
/// moves: it is faster, and its best move KOs the opponent's remaining HP
/// (from its bar) with probability [`SAFE_KO`] at least.
pub fn safe_ko(
    data: &GameData,
    party: &Party,
    opponent: Option<&Combatant>,
    battle: &pokebot_state::BattleObservation,
) -> bool {
    let (Some(lead), Some(foe), Some(bar)) = (party.lead(), opponent, battle.opponent_hp) else {
        return false;
    };
    let Some(us) = Combatant::new(data, &lead.species, lead.level, lead.moves.clone(), 10) else {
        return false;
    };
    if us.stats.speed() <= foe.stats.speed() {
        return false;
    }
    let mut now = foe.clone();
    now.hp = (foe.hp * u32::from(bar)).div_ceil(1000).max(1);
    pokebot_planner::evaluate::faint_probability(data, &us, &now, 1) >= SAFE_KO
}

pub(crate) fn step_toward(
    at: (u8, u8),
    target: (u8, u8),
    cell: impl Fn(u8, u8) -> BattleMenu,
    name: &str,
    confirmed: Expectation,
) -> Decision {
    if at == target {
        return Decision::Act(Action::new(
            format!("choose {name}"),
            vec![ControllerCommand::Press(Button::A)],
            confirmed,
            90,
        ));
    }
    let (button, next) = if at.1 != target.1 {
        (
            if target.1 > at.1 {
                Button::Down
            } else {
                Button::Up
            },
            (at.0, target.1),
        )
    } else {
        (
            if target.0 > at.0 {
                Button::Right
            } else {
                Button::Left
            },
            (target.0, at.1),
        )
    };
    Decision::Act(Action::new(
        format!("cursor to {name}: {button:?}"),
        vec![ControllerCommand::Press(button)],
        Expectation::BattleMenuAt(cell(next.0, next.1)),
        45,
    ))
}

/// "Will RED change POKéMON?": asked before a trainer sends the next
/// Pokémon, when the party has more than one.
/// "Use next POKéMON?": our battler fainted in a wild battle.
pub fn is_use_next_question(page: &str) -> bool {
    page.starts_with("Use next POK")
}

pub fn is_switch_question(page: &str) -> bool {
    page.starts_with("Will ") && page.contains(" change") && page.contains("POK")
}

#[cfg(test)]
mod tests {
    #[test]
    fn our_leads_status_is_read_from_battle_text() {
        use pokebot_state::Status;
        let lead = "IVYSAUR";
        let read = |p| lead_status_text(p, lead);
        // Live: PARAS's STUN SPORE in Mt. Moon.
        assert_eq!(
            read("IVYSAUR is paralyzed! It may be unable to move!"),
            Some(Status::Paralyzed)
        );
        assert_eq!(read("IVYSAUR was poisoned!"), Some(Status::Poisoned));
        assert_eq!(read("IVYSAUR fell asleep!"), Some(Status::Asleep));
        assert_eq!(read("IVYSAUR was burned!"), Some(Status::Burned));
        assert_eq!(read("IVYSAUR was frozen solid!"), Some(Status::Frozen));
        assert_eq!(read("IVYSAUR woke up!"), Some(Status::Healthy));
        // The foe's status is not ours.
        assert_eq!(
            read("Foe PARAS is paralyzed! It may be unable to move!"),
            None
        );
        assert_eq!(read("Wild ZUBAT fell asleep!"), None);
        assert_eq!(read("IVYSAUR used TACKLE!"), None);
    }

    use std::path::Path;

    use super::*;
    use crate::party::Member;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    fn ivysaur(data: &GameData, used: &[(&str, u8)]) -> Party {
        let mut member = Member::new(data, "SPECIES_IVYSAUR", 16);
        member.moves = [
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_LEECH_SEED",
            "MOVE_VINE_WHIP",
        ]
        .map(String::from)
        .to_vec();
        for (m, n) in used {
            member.pp_used.insert((*m).to_owned(), *n);
        }
        Party {
            members: vec![member],
        }
    }

    /// Fleet emu3 at LASS MIRIAM: WARTORTLE at risk against her ODDISH made
    /// way for KAKUNA Lv4, HARDEN only, the safest only in taking hits; it
    /// was drained to a faint. A member with no attack isn't switched to;
    /// one that can fight is.
    #[test]
    fn a_defensive_switch_goes_to_one_that_can_hit_back() {
        use pokebot_state::{BattleMenu, BattleObservation};
        let Some(data) = data() else { return };
        let mut wartortle = Member::new(&data, "SPECIES_WARTORTLE", 20);
        wartortle.hp = Some((4, 56));
        wartortle.moves = vec!["MOVE_TACKLE".into(), "MOVE_BUBBLE".into()];
        let mut kakuna = Member::new(&data, "SPECIES_KAKUNA", 4);
        kakuna.slot = 1;
        kakuna.hp = Some((17, 17));
        kakuna.moves = vec!["MOVE_HARDEN".into()];
        let battle = BattleObservation {
            menu: Some(BattleMenu::Command { column: 0, row: 0 }),
            player_name: Some("WARTORTLE".into()),
            player_level: Some(20),
            player_hp_numbers: Some((4, 56)),
            opponent_name: Some("ODDISH".into()),
            opponent_level: Some(11),
            player_hp: None,
            opponent_hp: Some(1000),
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: Some(true),
            opponent_shiny: None,
            level_up_stats: None,
        };
        let party = Party {
            members: vec![wartortle.clone(), kakuna],
        };
        assert_eq!(defensive_switch(&data, &party, &battle, [0; 6]), None);
        let mut pidgeotto = Member::new(&data, "SPECIES_PIDGEOTTO", 22);
        pidgeotto.slot = 1;
        pidgeotto.hp = Some((60, 60));
        pidgeotto.moves = vec!["MOVE_GUST".into(), "MOVE_WING_ATTACK".into()];
        let party = Party {
            members: vec![wartortle, pidgeotto],
        };
        assert_eq!(defensive_switch(&data, &party, &battle, [0; 6]), Some(1));
    }

    /// Switch goal run: a Lv6 Bulbasaur fought two wild Pidgeys on Route
    /// 1 at 7/22 then 3/22 HP (the bar unread) and whited out.
    /// The user's rule: Pokémon may faint as long as not all of them do.
    /// Fleet worker 5's SQUIRTLE (27/30, Defense −4 after two TAIL WHIPs)
    /// ran from a RATTATA at a fifth of its HP: a 9.7 % faint risk. With
    /// another member to send out it fights on; alone it still runs.
    #[test]
    fn a_member_with_others_behind_it_accepts_more_risk() {
        use pokebot_state::{BattleMenu, BattleObservation, Observation, Observed};
        let Some(data) = data() else { return };
        let squirtle = || {
            let mut m = Member::new(&data, "SPECIES_SQUIRTLE", 10);
            m.hp = Some((27, 30));
            m.moves = vec!["MOVE_TACKLE".into(), "MOVE_BUBBLE".into()];
            m
        };
        let mut o = Observation::bare(
            1,
            Observed {
                value: ScreenState::BattleCommand,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.battle = Some(BattleObservation {
            menu: Some(BattleMenu::Command { column: 0, row: 0 }),
            player_name: Some("SQUIRTLE".into()),
            player_level: Some(10),
            player_hp_numbers: Some((27, 30)),
            opponent_name: Some("RATTATA".into()),
            opponent_level: Some(5),
            player_hp: None,
            opponent_hp: Some(208),
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: Some(true),
            opponent_shiny: None,
            level_up_stats: None,
        });
        let decide_with = |party: Party| {
            let mut memory = BattleMemory::default();
            memory.catch.decided = true;
            memory.our_stages[2] = -4;
            match decide(
                &o,
                &BattlePolicy::default(),
                &mut memory,
                &party,
                &data,
                &mut Vec::new(),
            ) {
                Some(Decision::Act(a)) => a.label,
                _ => "none".into(),
            }
        };
        let alone = decide_with(Party {
            members: vec![squirtle()],
        });
        assert!(alone.contains("RUN"), "{alone}");
        let mut charmander = Member::new(&data, "SPECIES_CHARMANDER", 14);
        charmander.slot = 1;
        charmander.hp = Some((38, 38));
        let backed = decide_with(Party {
            members: vec![squirtle(), charmander],
        });
        assert!(backed.contains("FIGHT"), "{backed}");
    }

    #[test]
    fn a_wild_battle_at_low_or_unreadable_hp_is_run_from() {
        use pokebot_state::{BattleMenu, BattleObservation, Observation, Observed};
        let Some(data) = data() else { return };
        let policy = BattlePolicy::default();
        let party = {
            let mut m = Member::new(&data, "SPECIES_BULBASAUR", 6);
            m.hp = Some((3, 22));
            Party { members: vec![m] }
        };
        let hud = |hp: (u16, u16), foe: &str, level: u8, foe_bar: u16| {
            let mut o = Observation::bare(
                1,
                Observed {
                    value: ScreenState::BattleCommand,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.battle = Some(BattleObservation {
                menu: Some(BattleMenu::Command { column: 0, row: 0 }),
                player_name: Some("BULBASAUR".into()),
                player_level: Some(6),
                player_hp_numbers: Some(hp),
                opponent_name: Some(foe.into()),
                opponent_level: Some(level),
                player_hp: None,
                opponent_hp: Some(foe_bar),
                move_pp: None,
                move_names: Vec::new(),
                opponent_caught: Some(true),
                opponent_shiny: None,
                level_up_stats: None,
            });
            o
        };
        let mut wild = BattleMemory::default();
        wild.catch.decided = true;
        let mut events = Vec::new();
        let label = |d: Option<Decision>| match d {
            Some(Decision::Act(a)) => a.label,
            Some(Decision::Wait(w)) => format!("wait: {w}"),
            Some(Decision::Done(x)) | Some(Decision::Fail(x)) => x,
            None => "none".into(),
        };
        // 3/22 against a healthy Pidgey: RUN.
        let d = label(decide(
            &hud((3, 22), "PIDGEY", 4, 1000),
            &policy,
            &mut wild,
            &party,
            &data,
            &mut events,
        ));
        assert!(d.contains("RUN"), "{d}");
        // Why is told once.
        let told = |events: &[GameEvent]| {
            events
                .iter()
                .filter_map(|e| match e {
                    GameEvent::GoalProgress { detail, .. } => Some(detail.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(told(&events).len(), 1);
        assert!(
            told(&events)[0].starts_with("running: HP Some((3, 22)) under 500‰"),
            "{:?}",
            told(&events)
        );
        // Even a nearly defeated foe is not worth a possible faint.
        let d = label(decide(
            &hud((3, 22), "PIDGEY", 4, 10),
            &policy,
            &mut wild,
            &party,
            &data,
            &mut events,
        ));
        assert!(d.contains("RUN"), "{d}");
        // The Switch misread 22 as 2; the impossible number must not make
        // this single-member party appear healthy. Retry RUN after failures.
        wild.run_attempts = policy.max_run_attempts;
        let d = label(decide(
            &hud((3, 2), "PIDGEY", 4, 1000),
            &policy,
            &mut wild,
            &party,
            &data,
            &mut events,
        ));
        assert!(d.contains("RUN"), "{d}");
        // Healthy: FIGHT.
        let d = label(decide(
            &hud((20, 22), "PIDGEY", 4, 1000),
            &policy,
            &mut wild,
            &party,
            &data,
            &mut events,
        ));
        assert!(d.contains("FIGHT"), "{d}");
        // A trainer's battle can't be run from.
        let mut trainer = BattleMemory {
            trainer: true,
            ..BattleMemory::default()
        };
        let d = label(decide(
            &hud((3, 22), "PIDGEY", 4, 1000),
            &policy,
            &mut trainer,
            &party,
            &data,
            &mut events,
        ));
        assert!(d.contains("FIGHT"), "{d}");
    }

    /// Switch, Pokémon Tower: VENUSAUR used TACKLE twice on a GASTLY it
    /// hadn't identified ("It doesn't affect Wild GASTLY…"). Once told, a
    /// Normal move is out for that foe; a new foe clears it.
    #[test]
    fn a_move_that_did_nothing_is_not_used_again_on_that_foe() {
        let Some(data) = data() else { return };
        let party = ivysaur(&data, &[]);
        let policy = BattlePolicy::default();
        let mut memory = BattleMemory {
            last_move: Some("MOVE_TACKLE".into()),
            ..BattleMemory::default()
        };
        observe_page(&mut memory, "It doesn’t affect Wild GASTLY…", &party, &data);
        assert_eq!(memory.no_effect, vec!["TYPE_NORMAL".to_string()]);
        assert_eq!(
            choose_move(&data, &party, None, &memory, &policy).map(|(_, m)| m),
            Some("MOVE_VINE_WHIP".into())
        );
        observe_page(&mut memory, "Wild RATTATA appeared!", &party, &data);
        assert!(memory.no_effect.is_empty());
    }

    /// A move that takes two turns lands half as often per turn: SLASH
    /// before DIG against a foe both hit; SELFDESTRUCT, despite its power,
    /// is never chosen.
    #[test]
    fn two_turn_and_self_fainting_moves_are_weighed_by_their_rules() {
        let Some(data) = data() else { return };
        let foe = Combatant::new(&data, "SPECIES_RATTATA", 20, vec![], 15).unwrap();
        let policy = BattlePolicy::default();
        let memory = BattleMemory::default();
        let lead = |moves: &[&str]| {
            let mut member = Member::new(&data, "SPECIES_DUGTRIO", 40);
            member.moves = moves.iter().map(|m| (*m).to_owned()).collect();
            Party {
                members: vec![member],
            }
        };
        let party = lead(&["MOVE_DIG", "MOVE_SLASH"]);
        assert_eq!(
            choose_move(&data, &party, Some(&foe), &memory, &policy).map(|(_, m)| m),
            Some("MOVE_SLASH".into())
        );
        let party = lead(&["MOVE_SELF_DESTRUCT", "MOVE_SCRATCH"]);
        assert_eq!(
            choose_move(&data, &party, Some(&foe), &memory, &policy).map(|(_, m)| m),
            Some("MOVE_SCRATCH".into())
        );
    }

    /// Switch, Pokémon Tower: DUGTRIO used DIG on a GASTLY. Ground on
    /// Poison reads super effective, but GASTLY's LEVITATE takes nothing
    /// from it: the other move is chosen. Against a foe without the
    /// ability, DIG is still the best.
    #[test]
    fn a_move_the_foes_ability_stops_is_not_chosen() {
        let Some(data) = data() else { return };
        let mut member = Member::new(&data, "SPECIES_DUGTRIO", 40);
        member.moves = vec!["MOVE_DIG".into(), "MOVE_SLASH".into()];
        let party = Party {
            members: vec![member],
        };
        let foe = |species: &str| {
            Combatant::new(&data, species, 30, data.default_moves(species, 30), 15).unwrap()
        };
        let policy = BattlePolicy::default();
        let memory = BattleMemory::default();
        let gastly = foe("SPECIES_GASTLY");
        assert_eq!(
            choose_move(&data, &party, Some(&gastly), &memory, &policy).map(|(_, m)| m),
            Some("MOVE_SLASH".into())
        );
        let grimer = foe("SPECIES_GRIMER");
        assert_eq!(
            choose_move(&data, &party, Some(&grimer), &memory, &policy).map(|(_, m)| m),
            Some("MOVE_DIG".into())
        );
    }

    #[test]
    fn wild_battles_spend_the_reserve_before_running_dry() {
        let Some(data) = data() else { return };
        let policy = BattlePolicy::default();
        let wild = BattleMemory::default();
        // Tackle empty, Vine Whip at the reserve: Vine Whip rather than nothing.
        let party = ivysaur(&data, &[("MOVE_TACKLE", 35), ("MOVE_VINE_WHIP", 5)]);
        assert_eq!(
            choose_move(&data, &party, None, &wild, &policy),
            Some((3, "MOVE_VINE_WHIP".into()))
        );
        // No damaging PP at all: nothing to choose (the battle policy runs).
        let party = ivysaur(&data, &[("MOVE_TACKLE", 35), ("MOVE_VINE_WHIP", 10)]);
        assert_eq!(choose_move(&data, &party, None, &wild, &policy), None);
    }

    /// Fleet continue-5, Pokémon Tower: a wild GASTLY against a RATTATA
    /// whose moves are all NORMAL, which it took nothing from. The move
    /// menu failed "no move has PP left" turn after turn. From a wild
    /// battle it backs out to RUN; a trainer's turn is spent on a move.
    #[test]
    fn nothing_the_foe_feels_runs_from_a_wild_battle() {
        let Some(data) = data() else { return };
        let policy = BattlePolicy::default();
        // The party read has another lead (IVYSAUR, with GRASS moves); the
        // move menu shows the RATTATA out.
        let party = ivysaur(&data, &[]);
        let normal = data
            .move_("MOVE_TACKLE")
            .and_then(|m| m.kind.clone())
            .unwrap();
        let mut o = pokebot_state::Observation::bare(
            1,
            pokebot_state::Observed {
                value: ScreenState::BattleMoveSelection,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.battle = Some(pokebot_state::BattleObservation {
            menu: Some(BattleMenu::Moves { column: 0, row: 1 }),
            player_name: Some("RATTATA".into()),
            player_level: Some(11),
            player_hp_numbers: Some((29, 29)),
            opponent_name: Some("GASTLY".into()),
            opponent_level: Some(18),
            player_hp: Some(1000),
            opponent_hp: Some(1000),
            move_pp: None,
            move_names: ["TACKLE", "TAIL WHIP", "CUT", "QUICK ATTACK"]
                .map(String::from)
                .to_vec(),
            opponent_caught: None,
            opponent_shiny: None,
            level_up_stats: None,
        });
        let mut wild = BattleMemory {
            no_effect: vec![normal.clone()],
            ..BattleMemory::default()
        };
        wild.catch.decided = true;
        let mut events = Vec::new();
        match decide(&o, &policy, &mut wild, &party, &data, &mut events) {
            Some(Decision::Act(a)) => assert_eq!(a.label, "back to the command menu to RUN"),
            _ => panic!("expected a way back to RUN"),
        }
        assert!(wild.nothing_to_use);
        let mut trainer = BattleMemory {
            trainer: true,
            no_effect: vec![normal],
            ..BattleMemory::default()
        };
        match decide(&o, &policy, &mut trainer, &party, &data, &mut events) {
            Some(Decision::Act(a)) => assert!(a.label.contains("TACKLE"), "{}", a.label),
            _ => panic!("expected a move"),
        }
    }

    /// Review: in a trainer battle with the only damaging move disabled,
    /// the move menu failed the story. Any accepted move is chosen instead;
    /// only a lead with no PP at all fails.
    #[test]
    fn a_trainer_battle_with_the_only_attack_disabled_uses_another_move() {
        let Some(data) = data() else { return };
        let policy = BattlePolicy::default();
        // Tackle out of PP, Vine Whip disabled: Sleep Powder (slot 2).
        let party = ivysaur(&data, &[("MOVE_TACKLE", 35)]);
        let mut memory = BattleMemory {
            trainer: true,
            disabled: Some("MOVE_VINE_WHIP".into()),
            ..BattleMemory::default()
        };
        assert_eq!(choose_move(&data, &party, None, &memory, &policy), None);
        assert_eq!(
            fallback_move(&data, &party, &memory),
            Some((1, "MOVE_SLEEP_POWDER".into()))
        );
        let mut o = pokebot_state::Observation::bare(
            1,
            pokebot_state::Observed {
                value: ScreenState::BattleMoveSelection,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.battle = Some(pokebot_state::BattleObservation {
            menu: Some(BattleMenu::Moves { column: 1, row: 0 }),
            player_name: Some("IVYSAUR".into()),
            player_level: Some(16),
            player_hp_numbers: Some((50, 50)),
            opponent_name: None,
            opponent_level: None,
            player_hp: Some(1000),
            opponent_hp: Some(1000),
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: None,
            opponent_shiny: None,
            level_up_stats: None,
        });
        let mut events = Vec::new();
        match decide(&o, &policy, &mut memory, &party, &data, &mut events) {
            Some(Decision::Act(a)) => assert_eq!(a.label, "choose move 2 (SLEEP_POWDER)"),
            _ => panic!("unexpected decision"),
        }
        // No PP anywhere (the disabled move aside): fail.
        let party = ivysaur(
            &data,
            &[
                ("MOVE_TACKLE", 35),
                ("MOVE_SLEEP_POWDER", 15),
                ("MOVE_LEECH_SEED", 10),
            ],
        );
        assert_eq!(fallback_move(&data, &party, &memory), None);
        match decide(&o, &policy, &mut memory, &party, &data, &mut events) {
            Some(Decision::Fail(r)) => assert_eq!(r, "no move has PP left"),
            _ => panic!("unexpected decision"),
        }
    }

    /// Fleet worker 2 at BROCK: CHARMANDER fainted and a WEEDLE came out,
    /// but the one out was still taken for CHARMANDER, and its METAL CLAW
    /// (move 4) was aimed at on WEEDLE's two-move menu, Down pressed until
    /// "no progress". The menu's moves are what is chosen from.
    #[test]
    fn the_move_is_chosen_from_the_moves_the_menu_shows() {
        let Some(data) = data() else { return };
        let policy = BattlePolicy::default();
        let mut charmander = Member::new(&data, "SPECIES_CHARMANDER", 14);
        charmander.moves = [
            "MOVE_SCRATCH",
            "MOVE_GROWL",
            "MOVE_EMBER",
            "MOVE_METAL_CLAW",
        ]
        .map(String::from)
        .to_vec();
        let party = Party {
            members: vec![charmander],
        };
        let mut o = pokebot_state::Observation::bare(
            1,
            pokebot_state::Observed {
                value: ScreenState::BattleMoveSelection,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.battle = Some(pokebot_state::BattleObservation {
            menu: Some(BattleMenu::Moves { column: 0, row: 0 }),
            player_name: Some("WEEDLE".into()),
            player_level: Some(5),
            player_hp_numbers: Some((19, 19)),
            opponent_name: Some("ONIX".into()),
            opponent_level: Some(14),
            player_hp: Some(1000),
            opponent_hp: Some(354),
            move_pp: Some((35, 35)),
            move_names: vec![
                "POISON STING".into(),
                "STRING SHOT".into(),
                "".into(),
                "".into(),
            ],
            opponent_caught: None,
            opponent_shiny: None,
            level_up_stats: None,
        });
        let mut memory = BattleMemory {
            trainer: true,
            ..BattleMemory::default()
        };
        let mut events = Vec::new();
        match decide(&o, &policy, &mut memory, &party, &data, &mut events) {
            Some(Decision::Act(a)) => assert_eq!(a.label, "choose move 1 (POISON_STING)"),
            _ => panic!("unexpected decision"),
        }
    }

    /// Switch, Cerulean: the rival's CHARMANDER against IVYSAUR Lv28 at
    /// 52/75 (after PIDGEOTTO) — attacking alone lost; SLEEP POWDER first.
    /// Not against a foe beaten outright, a sleeping one, nor in the wild.
    #[test]
    fn a_risky_trainer_fight_opens_with_sleep() {
        let Some(data) = data() else { return };
        let mut ivysaur = Member::new(&data, "SPECIES_IVYSAUR", 28);
        ivysaur.moves = [
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_RAZOR_LEAF",
            "MOVE_VINE_WHIP",
        ]
        .map(String::from)
        .to_vec();
        ivysaur.hp = Some((52, 75));
        let party = Party {
            members: vec![ivysaur],
        };
        let foe = |species: &str, level| {
            Combatant::new(
                &data,
                species,
                level,
                data.default_moves(species, level),
                15,
            )
            .unwrap()
        };
        let battle = pokebot_state::BattleObservation {
            menu: Some(BattleMenu::Moves { column: 0, row: 0 }),
            player_name: Some("IVYSAUR".into()),
            player_level: Some(28),
            player_hp_numbers: Some((52, 75)),
            opponent_name: None,
            opponent_level: None,
            player_hp: None,
            opponent_hp: Some(1000),
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: None,
            opponent_shiny: None,
            level_up_stats: None,
        };
        let mut memory = BattleMemory::default();
        observe_page(
            &mut memory,
            "RIVAL GREEN sent out CHARMANDER!",
            &party,
            &data,
        );
        assert!(memory.trainer);
        let charmander = foe("SPECIES_CHARMANDER", 18);
        assert_eq!(
            sleep_opener(&data, &party, Some(&charmander), &battle, &memory),
            Some((1, "MOVE_SLEEP_POWDER".into()))
        );
        // Asleep: attack it.
        observe_page(&mut memory, "Foe CHARMANDER fell asleep!", &party, &data);
        assert!(memory.foe_asleep);
        assert_eq!(
            sleep_opener(&data, &party, Some(&charmander), &battle, &memory),
            None
        );
        observe_page(&mut memory, "Foe CHARMANDER woke up!", &party, &data);
        assert!(!memory.foe_asleep);
        // Tried twice on this foe: attack regardless; the next foe resets.
        memory.sleep_tries = MAX_SLEEP_TRIES;
        assert_eq!(
            sleep_opener(&data, &party, Some(&charmander), &battle, &memory),
            None
        );
        observe_page(&mut memory, "RIVAL GREEN sent out ABRA!", &party, &data);
        assert_eq!(memory.sleep_tries, 0);
        // A foe beaten outright needs no opener.
        let rattata = foe("SPECIES_RATTATA", 15);
        assert_eq!(
            sleep_opener(&data, &party, Some(&rattata), &battle, &memory),
            None
        );
        // In the wild the catch policy owns sleep.
        let wild = BattleMemory::default();
        assert_eq!(
            sleep_opener(&data, &party, Some(&charmander), &battle, &wild),
            None
        );
    }

    /// Switch, S.S. Anne: SAND-ATTACK and SMOKESCREEN cut VENUSAUR's
    /// accuracy twice; its TACKLEs missed and CHARMELEON's EMBER fainted
    /// it. The pages are counted, and at −2 the fight is no sure win:
    /// SLEEP POWDER first.
    #[test]
    fn accuracy_drops_make_a_trainer_fight_open_with_sleep() {
        assert_eq!(
            accuracy_text("VENUSAUR’s accuracy fell!", "VENUSAUR"),
            Some((Side::Ours, -1))
        );
        assert_eq!(
            accuracy_text("Foe PIDGEY’s evasiveness sharply rose!", "VENUSAUR"),
            Some((Side::Foe, 2))
        );
        assert_eq!(
            accuracy_text("Foe PIDGEY’s accuracy fell!", "VENUSAUR"),
            None
        );
        assert_eq!(accuracy_text("VENUSAUR’s DEFENSE fell!", "VENUSAUR"), None);
        let Some(data) = data() else { return };
        let mut venusaur = Member::new(&data, "SPECIES_VENUSAUR", 32);
        venusaur.moves = [
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_RAZOR_LEAF",
            "MOVE_VINE_WHIP",
        ]
        .map(String::from)
        .to_vec();
        venusaur.hp = Some((78, 98));
        let party = Party {
            members: vec![venusaur],
        };
        let charmeleon = Combatant::new(
            &data,
            "SPECIES_CHARMELEON",
            20,
            data.default_moves("SPECIES_CHARMELEON", 20),
            15,
        )
        .unwrap();
        let battle = pokebot_state::BattleObservation {
            menu: Some(BattleMenu::Moves { column: 0, row: 0 }),
            player_name: Some("VENUSAUR".into()),
            player_level: Some(32),
            player_hp_numbers: Some((78, 98)),
            opponent_name: None,
            opponent_level: None,
            player_hp: None,
            opponent_hp: Some(1000),
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: None,
            opponent_shiny: None,
            level_up_stats: None,
        };
        let mut memory = BattleMemory::default();
        observe_page(
            &mut memory,
            "RIVAL GREEN sent out CHARMELEON!",
            &party,
            &data,
        );
        let fresh = sleep_opener(&data, &party, Some(&charmeleon), &battle, &memory);
        for page in ["VENUSAUR’s accuracy fell!", "VENUSAUR’s accuracy fell!"] {
            observe_page(&mut memory, page, &party, &data);
        }
        assert_eq!(memory.our_accuracy, -2);
        assert_eq!(
            sleep_opener(&data, &party, Some(&charmeleon), &battle, &memory),
            Some((1, "MOVE_SLEEP_POWDER".into())),
            "fresh accuracy: {fresh:?}"
        );
    }

    /// Fleet worker 4 (Route 1): two TAIL WHIPs halved MANKEY's Defense and
    /// a critical TACKLE took all 16 HP; the risk had counted its Defense
    /// whole. The pages are counted, and the risk rises with them.
    #[test]
    fn lowered_defense_raises_the_risk() {
        let Some(data) = data() else { return };
        assert_eq!(
            stage_text("MANKEY’s DEFENSE fell!", "MANKEY"),
            Some((2, -1))
        );
        assert_eq!(
            stage_text("MANKEY's ATTACK sharply rose!", "MANKEY"),
            Some((1, 2))
        );
        assert_eq!(stage_text("Foe RATTATA’s DEFENSE fell!", "MANKEY"), None);
        let mut memory = BattleMemory::default();
        let party = Party {
            members: vec![Member::new(&data, "SPECIES_MANKEY", 3)],
        };
        for _ in 0..2 {
            observe_page(&mut memory, "MANKEY’s DEFENSE fell!", &party, &data);
        }
        assert_eq!(memory.our_stages[2], -2);
        let mankey = &party.members[0];
        let lead = catch::Lead {
            member: mankey,
            hp: (16, 16),
        };
        let rattata = catch::Foe {
            species: "SPECIES_RATTATA".into(),
            level: 3,
            hp_per_mille: 187,
            status: catch::FoeStatus::None,
            shiny: false,
            caught: None,
        };
        let plain = catch::risk_staged(&data, &lead, &rattata, 2, [0; 6]);
        let whipped = catch::risk_staged(&data, &lead, &rattata, 2, memory.our_stages);
        // Fought on at 1.5 %; run from at −2 Defense.
        assert!(
            plain < catch::RISK_LIMIT && whipped > catch::RISK_LIMIT,
            "{whipped} vs {plain}"
        );
        assert_eq!(plain, catch::risk(&data, &lead, &rattata, 2));
    }

    /// Fleet worker 2 (Viridian Forest): the lead SQUIRTLE fainted and
    /// RATTATA came out; the move picker still read SQUIRTLE's moves and
    /// sent the cursor after BUBBLE (slot 2) on a two-move menu, forever.
    /// The member the HUD names is the one the moves are chosen for.
    #[test]
    fn moves_are_chosen_for_the_member_in_battle() {
        let Some(data) = data() else { return };
        let mut squirtle = Member::new(&data, "SPECIES_SQUIRTLE", 9);
        squirtle.moves = vec![
            "MOVE_TACKLE".into(),
            "MOVE_TAIL_WHIP".into(),
            "MOVE_BUBBLE".into(),
        ];
        squirtle.hp = Some((0, 28));
        let mut rattata = Member::new(&data, "SPECIES_RATTATA", 2);
        rattata.slot = 1;
        rattata.moves = vec!["MOVE_TACKLE".into(), "MOVE_TAIL_WHIP".into()];
        rattata.hp = Some((13, 13));
        let party = Party {
            members: vec![squirtle, rattata],
        };
        let slot = party.battler_named(&data, "RATTATA");
        assert_eq!(slot, Some(1));
        let party = party.with_first(1);
        assert_eq!(party.lead().map(|m| m.slot), Some(1));
        let memory = BattleMemory::default();
        let chosen = choose_move(&data, &party, None, &memory, &BattlePolicy::default());
        assert!(
            chosen.as_ref().is_some_and(|(i, _)| *i < 2),
            "{chosen:?}: RATTATA knows two moves"
        );
    }

    /// Live (Route 3, Lass Robin's JIGGLYPUFF): DISABLE on VINE WHIP, and the
    /// bot chose VINE WHIP again every turn: "IVYSAUR's VINE WHIP is
    /// disabled!" sent it back to the move menu forever.
    #[test]
    fn a_disabled_move_is_not_chosen_until_disable_ends() {
        let Some(data) = data() else { return };
        let policy = BattlePolicy::default();
        let party = ivysaur(&data, &[("MOVE_VINE_WHIP", 3)]);
        let mut memory = BattleMemory {
            trainer: true,
            ..BattleMemory::default()
        };
        assert_eq!(
            choose_move(&data, &party, None, &memory, &policy),
            Some((3, "MOVE_VINE_WHIP".into()))
        );
        // The foe's own moves and unrelated pages change nothing.
        for page in [
            "Foe JIGGLYPUFF's POUND was disabled!",
            "Foe JIGGLYPUFF used DISABLE!",
        ] {
            observe_page(&mut memory, page, &party, &data);
            assert_eq!(memory.disabled, None, "{page}");
        }
        observe_page(
            &mut memory,
            "IVYSAUR's VINE WHIP was disabled!",
            &party,
            &data,
        );
        assert_eq!(memory.disabled.as_deref(), Some("MOVE_VINE_WHIP"));
        assert_eq!(
            choose_move(&data, &party, None, &memory, &policy),
            Some((0, "MOVE_TACKLE".into()))
        );
        // Chosen anyway (e.g. the first page was missed): the refusal page
        // also marks it.
        memory.disabled = None;
        observe_page(
            &mut memory,
            // As read live: the font's apostrophe is `’`.
            "IVYSAUR’s VINE WHIP is disabled!",
            &party,
            &data,
        );
        assert_eq!(memory.disabled.as_deref(), Some("MOVE_VINE_WHIP"));
        observe_page(&mut memory, "IVYSAUR is disabled no more!", &party, &data);
        assert_eq!(memory.disabled, None);
        assert_eq!(
            choose_move(&data, &party, None, &memory, &policy),
            Some((3, "MOVE_VINE_WHIP".into()))
        );
    }
}
