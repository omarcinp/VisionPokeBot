//! The odds of a catch: which action now (throw, a status move, an attack
//! or RUN) gives the best chance to catch the wild foe with the balls we
//! may spend, counting the chance each attack faints the foe and the
//! chance the foe faints our lead.
//!
//! Each turn is one of our actions and then one foe attack. A throw
//! catches with the game's formula at the foe's HP and status; an attack
//! lands one of its 32 rolls (crits 1 in 16) or misses, and a roll at or
//! above the foe's HP faints it, ending the attempt; a status move lands
//! with its accuracy. Sleep lasts 1–4 of our actions (the game's 2–5
//! counter), paralysis for good. The foe's attack faints our lead with
//! the hazard of its most damaging move from our current HP (foe iv 31,
//! us iv 0), each turn, asleep or not.
//!
//! The foe's HP is only known as a bar and its IVs not at all; our stats
//! are the summary's reading when it fits the lead's level (else our IVs
//! are unknown too), and OVERGROW, BLAZE and TORRENT raise the matching
//! type's damage ×1.5 at a third of our HP or less. The belief is a set of
//! scenarios (three foe IVs with our real stats, or three (our iv, foe iv)
//! pairs from the weakest to the strongest hits, × every HP the bar
//! allows). Each scenario is solved exactly (dynamic programming over
//! turn, balls left, foe HP and status); the action taken now is the one
//! best on average over the scenarios, recomputed every turn from the
//! fresh HUD. An action's value is P(catch) − [`Weights::lead_faint`] ×
//! P(lead faints); RUN is worth 0.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use pokebot_gamedata::mechanics::{catch_probability_status, damage, DamageRolls};
use pokebot_gamedata::GameData;
use pokebot_planner::evaluate::faint_probability;
use pokebot_planner::Combatant;

use crate::catch::{FoeStatus, Lead};

/// Turns looked ahead; past it the attempt counts as given up.
const MAX_TURNS: u8 = 20;
/// Balls looked at (P(catch) is saturated long before).
pub const MAX_BALLS: u16 = 15;
/// Foe HP is solved in at most this many units (exact up to this max HP).
const HP_UNITS: u32 = 96;
/// (our iv, the foe's iv) sampled when our stats are unknown: the weakest
/// hits on the sturdiest foe, the middle, the strongest hits on the
/// frailest foe. With our stats known only the foe's iv varies.
const IVS: [(u32, u32); 3] = [(0, 31), (15, 15), (31, 0)];
/// A foe put to sleep stays asleep for one of these many of our following
/// actions, all equally likely (the game's 2–5 counter, less the foe's
/// turn right after).
const SLEEP_ACTIONS: [u8; 4] = [1, 2, 3, 4];

/// What to do this turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Throw,
    /// A status move (slot, move).
    Status(u8, String),
    /// A damaging move (slot, move).
    Attack(u8, String),
    Run,
}

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Choice::Throw => write!(f, "throw"),
            Choice::Status(_, m) | Choice::Attack(_, m) => {
                write!(f, "{}", m.trim_start_matches("MOVE_"))
            }
            Choice::Run => write!(f, "run"),
        }
    }
}

/// What the rest of the attempt looks like under the chosen actions.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Outlook {
    /// P(catch) − lead-faint weight × P(lead faints).
    pub value: f64,
    pub catch: f64,
    /// P(an attack of ours faints the foe).
    pub foe_faints: f64,
    pub lead_faints: f64,
    /// Expected balls thrown.
    pub balls: f64,
}

impl Outlook {
    fn scaled(self, p: f64) -> Self {
        Self {
            value: self.value * p,
            catch: self.catch * p,
            foe_faints: self.foe_faints * p,
            lead_faints: self.lead_faints * p,
            balls: self.balls * p,
        }
    }

    fn plus(self, o: Self) -> Self {
        Self {
            value: self.value + o.value,
            catch: self.catch + o.catch,
            foe_faints: self.foe_faints + o.foe_faints,
            lead_faints: self.lead_faints + o.lead_faints,
            balls: self.balls + o.balls,
        }
    }
}

/// The decision this turn and its outlook.
#[derive(Debug, Clone, PartialEq)]
pub struct Odds {
    pub choice: Choice,
    pub outlook: Outlook,
}

/// How much a fainted lead costs, in catches.
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub lead_faint: f64,
}

/// What we know of the foe for the odds.
#[derive(Debug, Clone)]
pub struct FoeView<'a> {
    pub species: &'a str,
    pub level: u8,
    /// HP bar fill, 0–1000.
    pub hp_per_mille: u16,
    pub status: FoeStatus,
    /// Our actions since it was seen asleep (narrows how long it sleeps on).
    pub asleep_for: u8,
}

/// What we may do.
#[derive(Debug, Clone)]
pub struct Means<'a> {
    /// The ball's multiplier (×10).
    pub ball: u32,
    /// Balls we may throw.
    pub balls: u16,
    /// A status move may still be used.
    pub status_allowed: bool,
    /// A move the game refuses (DISABLE).
    pub disabled: Option<&'a str>,
}

/// The foe's status in a scenario: asleep for this many more actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum State {
    None,
    Paralyzed,
    Asleep(u8),
}

impl State {
    fn catch_x10(self) -> u32 {
        match self {
            State::None => 10,
            State::Paralyzed => 15,
            State::Asleep(_) => 20,
        }
    }

    /// After one of our actions and the foe's turn.
    fn after_turn(self) -> Self {
        match self {
            State::Asleep(n) if n > 1 => State::Asleep(n - 1),
            State::Asleep(_) => State::None,
            s => s,
        }
    }
}

/// The memo: a packed (turn, balls, HP, status) key, hashed by a multiply.
type Memo = HashMap<u64, Outlook, BuildHasherDefault<PackedHasher>>;

#[derive(Default)]
struct PackedHasher(u64);

impl Hasher for PackedHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 << 8 | u64::from(*b)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

fn key(turn: u8, balls: u16, hp: u32, state: State) -> u64 {
    let state = match state {
        State::None => 0,
        State::Paralyzed => 1,
        State::Asleep(n) => 1 + u64::from(n),
    };
    u64::from(turn) | u64::from(balls) << 8 | state << 24 | u64::from(hp) << 32
}

/// One of our actions, as the scenario plays it.
enum Effect {
    Throw,
    /// Lands with this chance, putting the foe to sleep (or paralysing it).
    Status {
        p: f64,
        sleep: bool,
    },
    /// Damage outcomes with their chances (misses as 0 damage).
    Attack(Vec<(u32, f64)>),
    Run,
}

/// One scenario: fixed IVs, solved over (turn, balls, foe HP, status).
struct Scenario {
    catch_rate: u16,
    max_hp: u32,
    /// HP per unit of the solved HP (1 up to [`HP_UNITS`] max HP).
    unit: u32,
    ball: u32,
    /// Per choice (same order as the choices).
    effects: Rc<[Effect]>,
    /// P(the lead faints on turn t | alive before it).
    hazard: Vec<f64>,
    lead_faint: f64,
    memo: Memo,
}

impl Scenario {
    /// Best outlook from a state, over every choice (RUN included).
    fn best(&mut self, turn: u8, balls: u16, hp: u32, state: State) -> Outlook {
        if balls == 0 || turn >= MAX_TURNS {
            return Outlook::default();
        }
        let key = key(turn, balls, hp, state);
        if let Some(o) = self.memo.get(&key) {
            return *o;
        }
        let best = (0..self.effects.len())
            .filter_map(|i| self.q(i, turn, balls, hp, state))
            .fold(
                Outlook::default(),
                |a, b| if b.value > a.value { b } else { a },
            );
        self.memo.insert(key, best);
        best
    }

    /// The outlook of choice `i` from a state; `None` when not possible.
    fn q(&mut self, i: usize, turn: u8, balls: u16, hp: u32, state: State) -> Option<Outlook> {
        let effects = Rc::clone(&self.effects);
        Some(match &effects[i] {
            Effect::Run => Outlook::default(),
            Effect::Throw => {
                if balls == 0 {
                    return None;
                }
                let p = catch_probability_status(
                    self.catch_rate,
                    self.max_hp,
                    (hp * self.unit).min(self.max_hp),
                    self.ball,
                    state.catch_x10(),
                );
                let caught = Outlook {
                    value: 1.0,
                    catch: 1.0,
                    balls: 1.0,
                    ..Outlook::default()
                };
                let missed = self.foe_turn(turn, balls - 1, hp, state);
                let missed = Outlook {
                    balls: missed.balls + 1.0,
                    ..missed
                };
                caught.scaled(p).plus(missed.scaled(1.0 - p))
            }
            Effect::Status { p, sleep } => {
                if state != State::None {
                    return None;
                }
                let (p, sleep) = (*p, *sleep);
                let missed = self.foe_turn(turn, balls, hp, state).scaled(1.0 - p);
                let landed = if sleep {
                    let each = p / SLEEP_ACTIONS.len() as f64;
                    SLEEP_ACTIONS
                        .iter()
                        // The foe's turn right after takes one off the counter.
                        .map(|n| {
                            self.foe_turn(turn, balls, hp, State::Asleep(n + 1))
                                .scaled(each)
                        })
                        .fold(Outlook::default(), Outlook::plus)
                } else {
                    self.foe_turn(turn, balls, hp, State::Paralyzed).scaled(p)
                };
                landed.plus(missed)
            }
            Effect::Attack(outcomes) => {
                let mut total = Outlook::default();
                for &(dmg, p) in outcomes {
                    let o = if dmg >= hp {
                        Outlook {
                            foe_faints: 1.0,
                            ..Outlook::default()
                        }
                    } else {
                        self.foe_turn(turn, balls, hp - dmg, state)
                    };
                    total = total.plus(o.scaled(p));
                }
                total
            }
        })
    }

    /// The foe's attack after our action, then the next turn.
    fn foe_turn(&mut self, turn: u8, balls: u16, hp: u32, state: State) -> Outlook {
        let h = self.hazard.get(usize::from(turn)).copied().unwrap_or(1.0);
        let fainted = Outlook {
            value: -self.lead_faint,
            lead_faints: 1.0,
            ..Outlook::default()
        };
        let on = self.best(turn + 1, balls, hp, state.after_turn());
        fainted.scaled(h).plus(on.scaled(1.0 - h))
    }
}

/// The best action now and the outlook under it; `None` when either side
/// is unknown to the game data.
pub fn best(
    data: &GameData,
    lead: &Lead,
    foe: &FoeView,
    means: &Means,
    weights: Weights,
) -> Option<Odds> {
    let catch_rate = data.species(foe.species)?.catch_rate;
    let choices = choices(data, lead, foe, means);
    let hazard = hazard(data, lead, foe)?;
    // (weight, scenario, foe HP, status).
    let mut belief: Vec<(f64, usize, u32, State)> = Vec::new();
    let mut scenarios = Vec::new();
    let states = states(foe);
    let known = lead.member.stats(data).is_some();
    for (our_iv, foe_iv) in IVS {
        // Our real stats: the pair's foe iv is the foe's (0, 15, 31) and
        // ours doesn't matter.
        let foe_iv = if known { our_iv } else { foe_iv };
        {
            let us = combatant(data, lead, our_iv)?;
            let pinch = pinch_type(lead);
            let them = foe_combatant(data, foe, foe_iv)?;
            let max_hp = them.max_hp();
            let unit = max_hp.div_ceil(HP_UNITS).max(1);
            let effects = choices
                .iter()
                .map(|c| effect(data, c, &us, &them, unit, pinch.as_deref()))
                .collect();
            let index = scenarios.len();
            scenarios.push(Scenario {
                catch_rate,
                max_hp,
                unit,
                ball: means.ball,
                effects,
                hazard: hazard.clone(),
                lead_faint: weights.lead_faint,
                memo: Memo::default(),
            });
            let hps = bar_hps(max_hp, foe.hp_per_mille);
            let w = 1.0 / (IVS.len() * hps.len() * states.len()) as f64;
            for hp in &hps {
                for state in &states {
                    belief.push((w, index, hp.div_ceil(unit), *state));
                }
            }
        }
    }
    let balls = means.balls.min(MAX_BALLS);
    // RUN (worth 0) unless an action is worth more; earlier choices win
    // ties: throw, status, attacks.
    let mut best = Odds {
        choice: Choice::Run,
        outlook: Outlook::default(),
    };
    for (i, choice) in choices.iter().enumerate() {
        if *choice == Choice::Run {
            continue;
        }
        let mut total = Outlook::default();
        let mut possible = true;
        for (w, s, hp, state) in &belief {
            match scenarios[*s].q(i, 0, balls, *hp, *state) {
                Some(o) => total = total.plus(o.scaled(*w)),
                None => possible = false,
            }
        }
        if possible && total.value > best.outlook.value + 1e-9 {
            best = Odds {
                choice: choice.clone(),
                outlook: total,
            };
        }
    }
    Some(best)
}

/// Throw, the status moves, the damaging moves, RUN (a move with PP left
/// and not disabled).
fn choices(data: &GameData, lead: &Lead, foe: &FoeView, means: &Means) -> Vec<Choice> {
    let mut out = Vec::new();
    if means.balls > 0 {
        out.push(Choice::Throw);
    }
    let usable = lead
        .member
        .moves
        .iter()
        .enumerate()
        .filter(|(_, m)| lead.member.pp_left(data, m) > 0)
        .filter(|(_, m)| means.disabled != Some(m.as_str()));
    let mut attacks = Vec::new();
    for (slot, m) in usable {
        let Some(mv) = data.move_(m) else { continue };
        match mv.effect.as_deref() {
            Some("EFFECT_SLEEP" | "EFFECT_PARALYZE") => {
                if means.status_allowed && foe.status == FoeStatus::None {
                    out.push(Choice::Status(slot as u8, m.clone()));
                }
            }
            _ if mv.power > 0 => attacks.push(Choice::Attack(slot as u8, m.clone())),
            _ => {}
        }
    }
    out.extend(attacks);
    out.push(Choice::Run);
    out
}

/// How `choice` plays out, damage in HP units of `unit`; moves of the
/// `pinch` type hit ×1.5.
fn effect(
    data: &GameData,
    choice: &Choice,
    us: &Combatant,
    them: &Combatant,
    unit: u32,
    pinch: Option<&str>,
) -> Effect {
    match choice {
        Choice::Throw => Effect::Throw,
        Choice::Run => Effect::Run,
        Choice::Status(_, m) => {
            let mv = data.move_(m);
            let accuracy = mv.map_or(0, |m| m.accuracy);
            let p = if accuracy == 0 {
                1.0
            } else {
                f64::from(accuracy.min(100)) / 100.0
            };
            let sleep = mv.and_then(|m| m.effect.as_deref()) == Some("EFFECT_SLEEP");
            Effect::Status { p, sleep }
        }
        Choice::Attack(_, m) => match rolls(data, m, us, them) {
            Some(mut r) => {
                let kind = data.move_(m).and_then(|mv| mv.kind.as_deref());
                if pinch.is_some() && kind == pinch {
                    for d in r.normal.iter_mut().chain(r.critical.iter_mut()) {
                        *d = *d * 3 / 2;
                    }
                }
                Effect::Attack(outcomes(&r, unit))
            }
            // Unknown damage: the attack does nothing we can count on.
            None => Effect::Attack(vec![(0, 1.0)]),
        },
    }
}

/// Damage outcomes of one attack in HP units (rounded): 16 normal and 16
/// critical rolls (crits 1 in 16) when it hits, 0 when it misses.
fn outcomes(r: &DamageRolls, unit: u32) -> Vec<(u32, f64)> {
    let crit = 1.0 / 16.0;
    let mut out: Vec<(u32, f64)> = Vec::with_capacity(33);
    let mut add = |d: u32, p: f64| match out.iter_mut().find(|(x, _)| *x == d) {
        Some((_, q)) => *q += p,
        None => out.push((d, p)),
    };
    let units = |d: u32| (d + unit / 2) / unit;
    for d in r.normal {
        add(units(d), r.hit_chance * (1.0 - crit) / 16.0);
    }
    for d in r.critical {
        add(units(d), r.hit_chance * crit / 16.0);
    }
    add(0, 1.0 - r.hit_chance);
    out
}

/// Every HP the bar allows: the bar has 48 pixels, `hp × 48 / max` of them
/// filled (at least one while alive).
fn bar_hps(max: u32, per_mille: u16) -> Vec<u32> {
    let pixel = |hp: u32| (hp * 48 / max).max(1);
    let seen = ((u32::from(per_mille) * 48 + 500) / 1000).clamp(1, 48);
    let hps: Vec<u32> = (1..=max).filter(|hp| pixel(*hp) == seen).collect();
    if hps.is_empty() {
        vec![(max * u32::from(per_mille) / 1000).clamp(1, max)]
    } else {
        hps
    }
}

/// The foe's status in the belief: how long a sleeping foe may sleep on.
fn states(foe: &FoeView) -> Vec<State> {
    match foe.status {
        FoeStatus::None => vec![State::None],
        FoeStatus::Paralyzed => vec![State::Paralyzed],
        FoeStatus::Asleep => {
            let left: Vec<State> = SLEEP_ACTIONS
                .iter()
                .filter(|n| **n > foe.asleep_for)
                .map(|n| State::Asleep(n - foe.asleep_for))
                .collect();
            if left.is_empty() {
                vec![State::Asleep(1)]
            } else {
                left
            }
        }
    }
}

/// P(the lead faints on turn t | alive before it), from our current HP.
fn hazard(data: &GameData, lead: &Lead, foe: &FoeView) -> Option<Vec<f64>> {
    let them = foe_combatant(data, foe, 31)?;
    let us = combatant(data, lead, 0)?;
    let cumulative: Vec<f64> = (0..=usize::from(MAX_TURNS))
        .map(|t| faint_probability(data, &them, &us, t))
        .collect();
    Some(
        cumulative
            .windows(2)
            .map(|w| {
                let alive = 1.0 - w[0];
                if alive <= 1e-12 {
                    1.0
                } else {
                    ((w[1] - w[0]) / alive).clamp(0.0, 1.0)
                }
            })
            .collect(),
    )
}

/// Our side: the real stats when known, else all at `iv`.
fn combatant(data: &GameData, lead: &Lead, iv: u32) -> Option<Combatant> {
    let m = lead.member;
    let mut us = Combatant::new(data, &m.species, m.level, m.moves.clone(), iv)?;
    if let Some(stats) = m.stats(data) {
        us.stats = stats;
    }
    us.hp = u32::from(lead.hp.0);
    Some(us)
}

/// The type our ability powers up now: OVERGROW (grass), BLAZE (fire),
/// TORRENT (water) at a third of our HP or less.
fn pinch_type(lead: &Lead) -> Option<String> {
    let kind = match lead.member.ability.as_deref()? {
        "OVERGROW" => "TYPE_GRASS",
        "BLAZE" => "TYPE_FIRE",
        "TORRENT" => "TYPE_WATER",
        _ => return None,
    };
    let (hp, max) = lead.hp;
    (u32::from(hp) * 3 <= u32::from(max)).then(|| kind.to_owned())
}

fn foe_combatant(data: &GameData, foe: &FoeView, iv: u32) -> Option<Combatant> {
    let moves = data.default_moves(foe.species, foe.level);
    Combatant::new(data, foe.species, foe.level, moves, iv)
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::party::Member;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    fn member(data: &GameData, species: &str, level: u8, moves: &[&str]) -> Member {
        let mut m = Member::new(data, species, level);
        m.moves = moves.iter().map(|m| (*m).to_owned()).collect();
        m
    }

    fn view(species: &str, level: u8, hp: u16) -> FoeView<'_> {
        FoeView {
            species,
            level,
            hp_per_mille: hp,
            status: FoeStatus::None,
            asleep_for: 0,
        }
    }

    const WEIGHTS: Weights = Weights { lead_faint: 20.0 };

    fn means(balls: u16) -> Means<'static> {
        Means {
            ball: 10,
            balls,
            status_allowed: true,
            disabled: None,
        }
    }

    /// The live case: IVYSAUR Lv24 against a Lv7 WEEDLE. Every attack is
    /// likely to faint it, so the best is to throw (or sleep it first),
    /// never to attack.
    #[test]
    fn a_likely_ko_is_not_attacked_but_thrown_at() {
        let Some(data) = data() else { return };
        let ivy = member(
            &data,
            "SPECIES_IVYSAUR",
            24,
            &[
                "MOVE_TACKLE",
                "MOVE_SLEEP_POWDER",
                "MOVE_RAZOR_LEAF",
                "MOVE_VINE_WHIP",
            ],
        );
        let lead = Lead {
            member: &ivy,
            hp: (74, 74),
        };
        let odds = best(
            &data,
            &lead,
            &view("SPECIES_WEEDLE", 7, 1000),
            &means(2),
            WEIGHTS,
        )
        .unwrap();
        assert!(
            matches!(odds.choice, Choice::Throw | Choice::Status(..)),
            "{odds:?}"
        );
        assert!(odds.outlook.foe_faints < 0.05, "{odds:?}");
        assert!(odds.outlook.catch > 0.5, "{odds:?}");
        // Tackle only (it faints WEEDLE on most rolls): a throw at once.
        let mut plain = ivy.clone();
        plain.moves = vec!["MOVE_TACKLE".into()];
        let lead = Lead {
            member: &plain,
            hp: (74, 74),
        };
        let odds = best(
            &data,
            &lead,
            &view("SPECIES_WEEDLE", 7, 1000),
            &means(2),
            WEIGHTS,
        )
        .unwrap();
        assert_eq!(odds.choice, Choice::Throw, "{odds:?}");
    }

    /// A tough foe at full HP with a weak attack: weaken it first.
    #[test]
    fn a_safe_attack_weakens_first() {
        let Some(data) = data() else { return };
        // BULBASAUR Lv12 with only Tackle against a Lv6 ZUBAT, one ball:
        // chipping it down first beats throwing at full HP.
        let bulba = member(&data, "SPECIES_BULBASAUR", 12, &["MOVE_TACKLE"]);
        let lead = Lead {
            member: &bulba,
            hp: (36, 36),
        };
        let odds = best(
            &data,
            &lead,
            &view("SPECIES_ZUBAT", 6, 1000),
            &means(1),
            WEIGHTS,
        )
        .unwrap();
        // One ball: the chance to catch is better after some damage.
        let throw_now = {
            let mut m = means(1);
            m.status_allowed = false;
            let mut only = bulba.clone();
            only.moves.clear();
            let lead = Lead {
                member: &only,
                hp: (36, 36),
            };
            best(&data, &lead, &view("SPECIES_ZUBAT", 6, 1000), &m, WEIGHTS).unwrap()
        };
        assert!(
            odds.outlook.catch >= throw_now.outlook.catch,
            "{odds:?} vs {throw_now:?}"
        );
        assert!(odds.outlook.foe_faints < 0.5, "{odds:?}");
    }

    #[test]
    fn no_balls_means_run() {
        let Some(data) = data() else { return };
        let ivy = member(&data, "SPECIES_IVYSAUR", 18, &["MOVE_TACKLE"]);
        let lead = Lead {
            member: &ivy,
            hp: (54, 54),
        };
        let odds = best(
            &data,
            &lead,
            &view("SPECIES_PIDGEY", 6, 1000),
            &means(0),
            WEIGHTS,
        )
        .unwrap();
        assert_eq!(odds.choice, Choice::Run);
        assert_eq!(odds.outlook.catch, 0.0);
    }

    #[test]
    fn a_weak_lead_against_a_strong_foe_runs() {
        let Some(data) = data() else { return };
        let ivy = member(&data, "SPECIES_IVYSAUR", 18, &["MOVE_TACKLE"]);
        let lead = Lead {
            member: &ivy,
            hp: (2, 54),
        };
        let odds = best(
            &data,
            &lead,
            &view("SPECIES_GEODUDE", 12, 1000),
            &means(10),
            WEIGHTS,
        )
        .unwrap();
        // A throw first may still be worth it; the outlook keeps the risk
        // in check either way.
        assert!(
            odds.choice == Choice::Run || odds.outlook.value >= 0.0,
            "{odds:?}"
        );
    }

    /// PR 15's summary stats: a real (weak) Attack lowers the chance an
    /// attack faints the foe compared with our IVs unknown, and OVERGROW
    /// at a third of our HP raises it.
    #[test]
    fn read_stats_and_the_pinch_ability_move_the_odds() {
        let Some(data) = data() else { return };
        let mut ivy = member(&data, "SPECIES_IVYSAUR", 27, &["MOVE_RAZOR_LEAF"]);
        ivy.hp = Some((71, 71));
        let lead = Lead {
            member: &ivy,
            hp: (71, 71),
        };
        let no_status = Means {
            status_allowed: false,
            ..means(1)
        };
        // Weakened ODDISH at 400‰ with one ball: attack first or throw.
        let foe = view("SPECIES_ODDISH", 13, 400);
        let unknown = best(&data, &lead, &foe, &no_status, WEIGHTS).unwrap();
        // The lowest Attack the level allows: attacks faint it less often.
        let base = data.species("SPECIES_IVYSAUR").unwrap().base;
        let low = |b: u16| ((2 * u32::from(b)) * 27 / 100 + 5) as u16;
        ivy.read_stats = Some([
            low(base[1]),
            low(base[2]),
            low(base[3]),
            low(base[4]),
            low(base[5]),
        ]);
        assert!(ivy.stats(&data).is_some());
        let lead = Lead {
            member: &ivy,
            hp: (71, 71),
        };
        let weak = best(&data, &lead, &foe, &no_status, WEIGHTS).unwrap();
        assert!(
            weak.outlook.foe_faints <= unknown.outlook.foe_faints + 1e-9,
            "{weak:?} vs {unknown:?}"
        );
        // A reading from before a level-up (Lv5 stats at Lv27) is ignored.
        let mut stale = ivy.clone();
        stale.read_stats = Some([9, 9, 9, 9, 9]);
        assert_eq!(stale.stats(&data), None);
        // OVERGROW at 20/71: Razor Leaf ×1.5, more likely to faint it.
        let mut pinch = ivy.clone();
        pinch.ability = Some("OVERGROW".into());
        let outcomes_at = |m: &Member, hp: u16| {
            let lead = Lead {
                member: m,
                hp: (hp, 71),
            };
            let us = combatant(&data, &lead, 15).unwrap();
            let them = foe_combatant(&data, &foe, 15).unwrap();
            let choice = Choice::Attack(0, "MOVE_RAZOR_LEAF".into());
            let Effect::Attack(o) =
                effect(&data, &choice, &us, &them, 1, pinch_type(&lead).as_deref())
            else {
                panic!("an attack")
            };
            o.iter().map(|(d, p)| f64::from(*d) * p).sum::<f64>()
        };
        assert!(outcomes_at(&pinch, 20) > outcomes_at(&pinch, 71) * 1.4);
        assert!((outcomes_at(&ivy, 20) - outcomes_at(&ivy, 71)).abs() < 1e-9);
    }

    #[test]
    fn the_bar_allows_a_range_of_hp() {
        assert_eq!(bar_hps(20, 1000), vec![20]);
        let half = bar_hps(40, 500);
        assert!(half.contains(&20), "{half:?}");
        assert!(half.iter().all(|hp| (hp * 48 / 40) == 24), "{half:?}");
        // The last pixel: any HP down to 1.
        assert!(bar_hps(100, 10).contains(&1));
    }

    #[test]
    fn sleep_left_narrows_with_time() {
        let mut foe = view("SPECIES_PIDGEY", 6, 1000);
        foe.status = FoeStatus::Asleep;
        assert_eq!(states(&foe).len(), 4);
        foe.asleep_for = 2;
        assert_eq!(states(&foe), vec![State::Asleep(1), State::Asleep(2)]);
        foe.asleep_for = 9;
        assert_eq!(states(&foe), vec![State::Asleep(1)]);
    }
}
