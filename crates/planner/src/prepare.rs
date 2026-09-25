//! Preparation planning: the cheapest (in estimated minutes) way to make the
//! party likely enough to beat the target trainers.
//!
//! Options considered, all derived from game data rather than hard-coded:
//! * train any party member to a higher level (new moves and evolutions come
//!   from its learnset/evolution data);
//! * catch one species available in a reachable area, then train it too;
//! * where to train/catch: any reachable area, priced by its encounter table.
//!
//! Search: every team composition (current party, or party + one catch) ×
//! every combination of level targets within a window, keeping the cheapest
//! that reaches the confidence target. Ties break on minutes, then on a
//! stable description order, so the result is deterministic.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use pokebot_gamedata::mechanics::{catch_probability, exp_for_level, exp_gain, Stats};
use pokebot_gamedata::{EncounterTable, GameData};
use serde::Serialize;

use crate::evaluate::{battle_vs_trainer, matchup, Combatant};

/// Seconds per walking step (16 frames at 59.73 Hz).
const STEP_SECONDS: f64 = 16.0 / 59.7275;
/// Fixed overhead per battle (intro, text, exp) and per turn.
const BATTLE_OVERHEAD_S: f64 = 18.0;
const TURN_S: f64 = 7.0;
/// IVs assumed for our own Pokémon (unknown; a modest value is conservative).
/// Everything that estimates our side's P(win) uses this one value.
pub const OUR_IV: u32 = 10;
/// Wild Pokémon average IV.
const WILD_IV: u32 = 15;
/// Levels above the current one considered per member.
const LEVEL_WINDOW: u8 = 14;
/// Level combinations evaluated per party composition before the search
/// settles for what it found (a four-member party has 15⁴ of them; the
/// cheapest that meets the target usually comes within the first few).
const MAX_LEVEL_COMBOS: usize = 96;
const POKE_BALL: &str = "ITEM_POKE_BALL";

#[derive(Debug, Clone, Serialize)]
pub struct PartyMember {
    pub species: String,
    pub level: u8,
    /// Total experience, if known (else the minimum for the level).
    pub exp: Option<u64>,
    /// Known moves (empty = assume the level-up default).
    pub moves: Vec<String>,
}

/// A place to train or catch.
#[derive(Debug, Clone, Serialize)]
pub struct Area {
    pub map: String,
    /// Minutes to get there from the current position.
    pub travel_minutes: f64,
    /// Minutes for a round trip to the nearest Pokémon Center.
    pub heal_minutes: f64,
}

#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub party: Vec<PartyMember>,
    pub targets: Vec<String>,
    pub areas: Vec<Area>,
    /// Required probability of winning every target battle.
    pub confidence: f64,
    pub money: u32,
    pub data: &'a GameData,
}

#[derive(Debug, Clone, Serialize)]
pub enum PlanStep {
    Catch {
        species: String,
        map: String,
        level: u8,
        minutes: f64,
        balls: u32,
    },
    Train {
        species: String,
        from: u8,
        to: u8,
        map: String,
        minutes: f64,
        battles: u32,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparationPlan {
    pub steps: Vec<PlanStep>,
    pub minutes: f64,
    /// Probability of winning each target battle with the prepared party.
    pub confidence: Vec<(String, f64)>,
    /// Final party (species, level, moves).
    pub party: Vec<(String, u8, Vec<String>)>,
}

impl PreparationPlan {
    pub fn min_confidence(&self) -> f64 {
        self.confidence.iter().map(|(_, p)| *p).fold(1.0, f64::min)
    }
}

/// Moves a member has at `level`: the known moves plus any learned since,
/// keeping the last four (as when every new move is accepted).
fn moves_at(data: &GameData, member: &PartyMember, species: &str, level: u8) -> Vec<String> {
    if member.moves.is_empty() {
        return data.default_moves(species, level);
    }
    let mut moves = member.moves.clone();
    for (lvl, mv) in data
        .species(species)
        .map(|s| s.learnset.as_slice())
        .unwrap_or(&[])
    {
        if *lvl > member.level && *lvl <= level && !moves.contains(mv) {
            if moves.len() == 4 {
                moves.remove(0);
            }
            moves.push(mv.clone());
        }
    }
    moves
}

fn combatant(data: &GameData, member: &PartyMember, level: u8) -> Option<Combatant> {
    let species = data.evolved_at(&member.species, level);
    let moves = moves_at(data, member, &species, level);
    Combatant::new(data, &species, level, moves, OUR_IV)
}

/// Land encounter table of an area, if it has one.
fn land<'d>(data: &'d GameData, area: &Area) -> Option<&'d EncounterTable> {
    data.wild.get(&area.map)?.get("land")
}

/// Expected minutes and battles to train `member` from its level to `to` in `area`.
fn training_cost(data: &GameData, member: &PartyMember, to: u8, area: &Area) -> Option<(f64, u32)> {
    if to <= member.level {
        return Some((0.0, 0));
    }
    let table = land(data, area)?;
    let species_now = data.evolved_at(&member.species, member.level);
    let growth = &data.species(&species_now)?.growth_rate;
    let start = member
        .exp
        .unwrap_or_else(|| exp_for_level(growth, member.level));
    let needed = exp_for_level(growth, to).saturating_sub(start) as f64;
    // Average over the encounter table; battles are fought at the midpoint
    // level of the training window.
    let mid = (member.level + to).div_ceil(2);
    let us = combatant(data, member, mid)?;
    let (mut exp, mut seconds, mut damage, mut weight) = (0.0, 0.0, 0.0, 0.0);
    for slot in &table.slots {
        let level = (slot.min_level + slot.max_level) / 2;
        let moves = data.default_moves(&slot.species, level);
        let Some(foe) = Combatant::new(data, &slot.species, level, moves, WILD_IV) else {
            continue;
        };
        let m = matchup(data, &us, &foe);
        if m.p_win < 0.5 {
            return None; // too dangerous to train here
        }
        let w = f64::from(slot.chance);
        exp += w * exp_gain(data, &slot.species, level, false) as f64;
        seconds += w * (BATTLE_OVERHEAD_S + TURN_S * m.turns);
        damage += w * f64::from(us.hp.saturating_sub(m.our_hp_after));
        weight += w;
    }
    if weight == 0.0 || exp == 0.0 {
        return None;
    }
    let (exp, seconds, damage) = (exp / weight, seconds / weight, damage / weight);
    let battles = (needed / exp).ceil();
    let steps_per_encounter = 2880.0 / (16.0 * f64::from(table.rate.max(1)));
    let per_battle = seconds + steps_per_encounter * STEP_SECONDS;
    // Heal when HP would drop below a third.
    let battles_per_heal = ((f64::from(us.max_hp()) * 2.0 / 3.0) / damage.max(0.5)).max(1.0);
    let heals = (battles / battles_per_heal).floor();
    let minutes = battles * per_battle / 60.0 + heals * area.heal_minutes + area.travel_minutes;
    Some((minutes, battles as u32))
}

/// Expected minutes and Poké Balls to catch `species` in `area`.
fn catch_cost(data: &GameData, species: &str, area: &Area) -> Option<(f64, u8, u32)> {
    let table = land(data, area)?;
    let (share, levels): (f64, Vec<u8>) = table.slots.iter().filter(|s| s.species == species).fold(
        (0.0, Vec::new()),
        |(p, mut l), s| {
            l.push((s.min_level + s.max_level) / 2);
            (p + f64::from(s.chance) / 100.0, l)
        },
    );
    if share == 0.0 {
        return None;
    }
    let level = *levels.iter().min()?;
    let sp = data.species(species)?;
    let max_hp = Stats::compute(&sp.base, level, WILD_IV).hp();
    // Weakened to about half HP before throwing.
    let p = catch_probability(sp.catch_rate, max_hp, max_hp / 2, 10).max(0.01);
    let balls = (1.0 / p).ceil();
    let encounters = 1.0 / share;
    let steps = 2880.0 / (16.0 * f64::from(table.rate.max(1)));
    let seconds = encounters * (steps * STEP_SECONDS + BATTLE_OVERHEAD_S) + balls * 12.0;
    Some((seconds / 60.0 + area.travel_minutes, level, balls as u32))
}

/// Levels the plan's training adds up to.
fn levels_gained(plan: &PreparationPlan) -> u32 {
    plan.steps
        .iter()
        .map(|s| match s {
            PlanStep::Train { from, to, .. } => u32::from(to.saturating_sub(*from)),
            PlanStep::Catch { .. } => 0,
        })
        .sum()
}

fn confidence(data: &GameData, party: &[Combatant], targets: &[String]) -> Vec<(String, f64)> {
    targets
        .iter()
        .map(|t| {
            (
                t.clone(),
                battle_vs_trainer(data, party, t).map_or(0.0, |e| e.p_win),
            )
        })
        .collect()
}

/// Returns up to `alternatives` plans, cheapest first; the first meets the
/// confidence target if any plan does.
pub fn plan_preparation(request: &Request<'_>, alternatives: usize) -> Vec<PreparationPlan> {
    plan(request, alternatives, true)
}

/// Like [`plan_preparation`], but only training the party as it is (no
/// catches).
pub fn plan_training(request: &Request<'_>, alternatives: usize) -> Vec<PreparationPlan> {
    plan(request, alternatives, false)
}

fn plan(request: &Request<'_>, alternatives: usize, catching: bool) -> Vec<PreparationPlan> {
    let data = request.data;
    // Team compositions: as is, or plus one catchable species.
    let mut catchable: BTreeSet<String> = BTreeSet::new();
    for area in &request.areas {
        if let Some(t) = land(data, area) {
            catchable.extend(t.slots.iter().map(|s| s.species.clone()));
        }
    }
    let ball_price = data.items.get(POKE_BALL).map_or(200, |i| i.price);
    let mut compositions: Vec<(Vec<PartyMember>, Vec<PlanStep>, f64)> =
        vec![(request.party.clone(), Vec::new(), 0.0)];
    if catching && request.party.len() < 6 {
        for species in &catchable {
            let best = request
                .areas
                .iter()
                .filter_map(|a| catch_cost(data, species, a).map(|c| (c, a)))
                .min_by(|x, y| {
                    x.0 .0
                        .total_cmp(&y.0 .0)
                        .then_with(|| x.1.map.cmp(&y.1.map))
                });
            let Some(((minutes, level, balls), area)) = best else {
                continue;
            };
            if balls * ball_price > request.money {
                continue;
            }
            let mut party = request.party.clone();
            party.push(PartyMember {
                species: species.clone(),
                level,
                exp: None,
                moves: Vec::new(),
            });
            let step = PlanStep::Catch {
                species: species.clone(),
                map: area.map.clone(),
                level,
                minutes,
                balls,
            };
            compositions.push((party, vec![step], minutes));
        }
    }

    let mut plans: Vec<PreparationPlan> = Vec::new();
    let mut options: LevelOptions = BTreeMap::new();
    for (party, steps, base_minutes) in compositions {
        search_levels(
            request,
            &party,
            &steps,
            base_minutes,
            alternatives.max(1),
            &mut options,
            &mut plans,
        );
    }
    let ok = |p: &PreparationPlan| p.min_confidence() >= request.confidence;
    // If anything reaches the target, only those count; otherwise the most
    // confident ones (a best effort the caller can report).
    if plans.iter().any(ok) {
        plans.retain(ok);
    }
    plans.sort_by(|a, b| {
        if ok(a) {
            a.minutes
                .total_cmp(&b.minutes)
                .then_with(|| b.min_confidence().total_cmp(&a.min_confidence()))
        } else {
            // Nothing reaches the target: the most confident best effort,
            // and at equal confidence the one that trains furthest (the
            // party is judged again after it), not the one that does least.
            b.min_confidence()
                .total_cmp(&a.min_confidence())
                .then_with(|| levels_gained(b).cmp(&levels_gained(a)))
                .then_with(|| a.minutes.total_cmp(&b.minutes))
        }
        .then_with(|| format!("{:?}", a.steps).cmp(&format!("{:?}", b.steps)))
    });
    // Keep only Pareto-optimal plans: drop any plan that an earlier one beats
    // or matches on both time and confidence (e.g. catching something useless).
    let mut kept: Vec<PreparationPlan> = Vec::new();
    for plan in plans {
        let dominated = kept
            .iter()
            .any(|k| k.minutes <= plan.minutes + 1e-9 && k.min_confidence() >= plan.min_confidence() - 1e-9)
            // Among plans that meet the target, extra confidence is only worth
            // listing if it is meaningfully higher.
            || kept.iter().any(|k| ok(k) && ok(&plan) && k.min_confidence() + 0.02 >= plan.min_confidence() && k.minutes <= plan.minutes);
        if !dominated {
            kept.push(plan);
        }
        if kept.len() == alternatives {
            break;
        }
    }
    kept
}

/// A target level for one member: (level, minutes, where and how many battles).
type LevelOption = (u8, f64, Option<(String, u32)>);
/// Level options per member, keyed by species, level and experience: the
/// party members recur in every composition, so they are priced once.
type LevelOptions = BTreeMap<(String, u8, Option<u64>), Vec<LevelOption>>;

/// Level targets per member (up to LEVEL_WINDOW above its level, only
/// where it can train), searched cheapest combination first: training time
/// adds up and confidence only grows with levels, so the first combination
/// that meets the target is the cheapest one. Stops after `wanted` meet it
/// or [`MAX_LEVEL_COMBOS`] were tried.
fn search_levels(
    request: &Request<'_>,
    party: &[PartyMember],
    base_steps: &[PlanStep],
    base_minutes: f64,
    wanted: usize,
    memo: &mut LevelOptions,
    plans: &mut Vec<PreparationPlan>,
) {
    let data = request.data;
    // Cheapest training option per member and target level.
    let options: Vec<Vec<LevelOption>> = party
        .iter()
        .map(|m| {
            memo.entry((m.species.clone(), m.level, m.exp))
                .or_insert_with(|| {
                    (m.level..=m.level.saturating_add(LEVEL_WINDOW).min(100))
                        .filter_map(|to| {
                            if to == m.level {
                                return Some((to, 0.0, None));
                            }
                            request
                                .areas
                                .iter()
                                .filter_map(|a| {
                                    training_cost(data, m, to, a)
                                        .map(|(min, b)| (min, b, a.map.clone()))
                                })
                                .min_by(|x, y| x.0.total_cmp(&y.0).then_with(|| x.2.cmp(&y.2)))
                                .map(|(min, battles, map)| (to, min, Some((map, battles))))
                        })
                        .collect()
                })
                .clone()
        })
        .collect();
    let minutes_of = |index: &[usize]| -> f64 {
        base_minutes
            + index
                .iter()
                .zip(&options)
                .map(|(i, o)| o[*i].1)
                .sum::<f64>()
    };
    let start = vec![0usize; party.len()];
    let mut heap: BinaryHeap<Reverse<(Minutes, Vec<usize>)>> = BinaryHeap::new();
    let mut seen: BTreeSet<Vec<usize>> = BTreeSet::new();
    heap.push(Reverse((Minutes(minutes_of(&start)), start.clone())));
    seen.insert(start);
    let mut found = 0;
    let mut tried = 0;
    while let Some(Reverse((Minutes(minutes), index))) = heap.pop() {
        if tried >= MAX_LEVEL_COMBOS {
            break;
        }
        tried += 1;
        let prepared: Vec<Combatant> = party
            .iter()
            .zip(&index)
            .zip(&options)
            .filter_map(|((m, i), o)| combatant(data, m, o[*i].0))
            .collect();
        let conf = confidence(data, &prepared, &request.targets);
        let mut steps = base_steps.to_vec();
        for ((m, i), o) in party.iter().zip(&index).zip(&options) {
            if let (to, min, Some((map, battles))) = &o[*i] {
                steps.push(PlanStep::Train {
                    species: m.species.clone(),
                    from: m.level,
                    to: *to,
                    map: map.clone(),
                    minutes: *min,
                    battles: *battles,
                });
            }
        }
        let plan = PreparationPlan {
            steps,
            minutes,
            confidence: conf,
            party: prepared
                .iter()
                .map(|c| (c.species.clone(), c.level, c.moves.clone()))
                .collect(),
        };
        if plan.min_confidence() >= request.confidence {
            found += 1;
        }
        plans.push(plan);
        if found >= wanted {
            break;
        }
        // One member a level target further, each way.
        for k in 0..index.len() {
            let mut next = index.clone();
            next[k] += 1;
            if next[k] < options[k].len() && seen.insert(next.clone()) {
                heap.push(Reverse((Minutes(minutes_of(&next)), next)));
            }
        }
    }
}

/// Minutes with a total order, for the open set.
#[derive(Clone, Copy, PartialEq)]
struct Minutes(f64);

impl Eq for Minutes {}

impl PartialOrd for Minutes {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Minutes {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}
