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

use crate::evaluate::{best_fighter, matchup, Combatant};

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
const MAX_LEVEL_COMBOS: usize = 200;
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
    /// Opponents of the targets the prepared party's best fighter is
    /// expected to beat, added up: how far a best effort gets when no plan
    /// wins.
    pub progress: f64,
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

/// [`matchup`] remembered: training is priced for every member, target
/// level and carrier level against the same wild slots.
fn matchup_cached(data: &GameData, us: &Combatant, them: &Combatant) -> crate::evaluate::Matchup {
    type Key = (String, u8, Vec<String>, u32, [u32; 6], String, u8, u32);
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<Key, crate::evaluate::Matchup>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    let key: Key = (
        us.species.clone(),
        us.level,
        us.moves.clone(),
        us.hp,
        us.stats.0,
        them.species.clone(),
        them.level,
        them.hp,
    );
    if let Some(m) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return m;
    }
    let m = matchup(data, us, them);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > 200_000 {
            c.clear();
        }
        c.insert(key, m.clone());
    });
    m
}

/// Whether `member` wins its first battles on `map` at the level it has
/// (it trains there by fighting; else it is switch-trained); `None`
/// without land encounters there.
pub fn trains_alone(data: &GameData, member: &PartyMember, map: &str) -> Option<bool> {
    let table = data.wild.get(map)?.get("land")?;
    Some(safe_against(
        data,
        &combatant(data, member, member.level)?,
        table,
    ))
}

/// Whether `us` wins (at least even odds) against every slot of `table`
/// at its middle level.
fn safe_against(data: &GameData, us: &Combatant, table: &EncounterTable) -> bool {
    table.slots.iter().all(|slot| {
        let level = (slot.min_level + slot.max_level) / 2;
        let moves = data.default_moves(&slot.species, level);
        Combatant::new(data, &slot.species, level, moves, WILD_IV)
            .is_none_or(|foe| matchup_cached(data, us, &foe).p_win >= 0.5)
    })
}

/// Expected minutes and battles to train `member` from its level to `to` in
/// `area`. One that would lose its first battles there (fleet worker 4: a
/// Lv2 MANKEY on Route 1, judged at its window's middle, fainted to a
/// PIDGEY) is switch-trained when `carrier` can fight there: it starts
/// each battle and `carrier` is switched in to win it, the experience
/// shared between the two.
fn training_cost(
    data: &GameData,
    member: &PartyMember,
    to: u8,
    area: &Area,
    carrier: Option<&PartyMember>,
) -> Option<(f64, u32)> {
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
    // level of the training window, by the member itself when it can win
    // its first ones.
    let mid = (member.level + to).div_ceil(2);
    let alone = safe_against(data, &combatant(data, member, member.level)?, table)
        .then(|| combatant(data, member, mid))
        .flatten()
        .and_then(|us| battles_cost(data, &us, 1.0, 0.0, needed, table, area));
    let carried = carrier
        .filter(|_| !traps(data, member, &species_now, table))
        .and_then(|c| combatant(data, c, c.level))
        .and_then(|us| battles_cost(data, &us, 0.5, TURN_S, needed, table, area));
    // Whichever is faster: a trainee that wins, but slowly (fleet worker 4:
    // a Lv10 PARAS scratching at Lv13–16 ODDISH), is switch-trained when
    // its carrier wins at once, half the experience for a fraction of the
    // turns.
    match (alone, carried) {
        (Some(a), Some(c)) => Some(if c.0 < a.0 { c } else { a }),
        (a, c) => a.or(c),
    }
}

/// Whether a wild one on `table` may trap the trainee (ARENA TRAP, SHADOW
/// TAG, MAGNET PULL: the SHIFT and RUN are refused) and beat it: then the
/// trainee can't be switch-trained there (the Switch: a Lv5 CATERPIE
/// against DIGLETT, Diglett's Cave, fought alone and had to win).
fn traps(data: &GameData, member: &PartyMember, species_now: &str, table: &EncounterTable) -> bool {
    let Some(alone) = combatant(data, member, member.level) else {
        return true;
    };
    table.slots.iter().any(|slot| {
        if !data.may_trap(&slot.species, species_now) {
            return false;
        }
        let level = (slot.min_level + slot.max_level) / 2;
        let moves = data.default_moves(&slot.species, level);
        Combatant::new(data, &slot.species, level, moves, WILD_IV)
            .is_some_and(|foe| matchup_cached(data, &alone, &foe).p_win < 0.5)
    })
}

/// Expected minutes and battles for `us` (the trainee, or its carrier) to
/// win `needed` experience on `table`, `share` of it going to the trainee;
/// `extra_s`: seconds each battle adds (the SHIFT). `None` when a slot
/// beats `us` or gives nothing.
fn battles_cost(
    data: &GameData,
    us: &Combatant,
    share: f64,
    extra_s: f64,
    needed: f64,
    table: &EncounterTable,
    area: &Area,
) -> Option<(f64, u32)> {
    let (mut exp, mut seconds, mut damage, mut weight) = (0.0, 0.0, 0.0, 0.0);
    for slot in &table.slots {
        let level = (slot.min_level + slot.max_level) / 2;
        let moves = data.default_moves(&slot.species, level);
        let Some(foe) = Combatant::new(data, &slot.species, level, moves, WILD_IV) else {
            continue;
        };
        let m = matchup_cached(data, us, &foe);
        if m.p_win < 0.5 {
            return None; // too dangerous to train here
        }
        let w = f64::from(slot.chance);
        exp += w * exp_gain(data, &slot.species, level, false) as f64 * share;
        seconds += w * (BATTLE_OVERHEAD_S + extra_s + TURN_S * m.turns);
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

/// Whether a trainee that beats a wild one in `alone` turns should fight
/// it itself rather than hand it to a carrier that takes `carried`: alone
/// it earns all the experience, switched half, a turn later (the SHIFT).
pub fn alone_pays(alone: f64, carried: f64) -> bool {
    BATTLE_OVERHEAD_S + TURN_S * alone <= 2.0 * (BATTLE_OVERHEAD_S + TURN_S * (carried + 1.0))
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

/// Opponents of `targets` expected beaten, added up over the targets:
/// by each one's best fighter alone (as [`best_fighter`] judges a win).
fn progress(data: &GameData, party: &[Combatant], targets: &[String]) -> f64 {
    targets
        .iter()
        .map(|t| {
            party
                .iter()
                .filter_map(|c| {
                    crate::evaluate::battle_vs_trainer(data, std::slice::from_ref(c), t)
                })
                .map(|e| e.opponents.iter().map(|(_, _, p)| p).sum::<f64>())
                .fold(0.0, f64::max)
        })
        .sum()
}

fn confidence(data: &GameData, party: &[Combatant], targets: &[String]) -> Vec<(String, f64)> {
    targets
        .iter()
        .map(|t| {
            (
                t.clone(),
                best_fighter(data, party, t).map_or(0.0, |(_, p)| p),
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
            // and at equal confidence the one that gets furthest against
            // the targets (the party is judged again after it), cheapest
            // first. Not the one that adds the most levels: levels on a
            // member who beats nothing more are time lost (fleet worker 4
            // trained PARAS, ZUBAT and CLEFAIRY for ERIKA and the Elite
            // Four, all at 0%).
            b.min_confidence()
                .total_cmp(&a.min_confidence())
                .then_with(|| b.progress.total_cmp(&a.progress))
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

/// One member's part of a combination: (target level, minutes, where and
/// how many battles).
type MemberCost = (u8, f64, Option<(String, u32)>);
/// The cheapest way to train one member to a level: (minutes, where and
/// how many battles; `None` for no training).
type LevelCost = Option<(f64, Option<(String, u32)>)>;
/// Costs keyed by the member (species, level, experience), the target
/// level and the carrier it may be switch-trained with (species and level
/// in the combination): the party members recur in every composition, so
/// each is priced once.
type LevelOptions = BTreeMap<(String, u8, Option<u64>, u8, Option<(String, u8)>), LevelCost>;

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
    memo_out: &mut LevelOptions,
    plans: &mut Vec<PreparationPlan>,
) {
    let data = request.data;
    // The member a switch-trained one hands its battles to: the highest
    // level other one.
    let carrier_of = |i: usize| {
        party
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .max_by_key(|(_, c)| c.level)
            .map(|(j, _)| j)
    };
    let window = |m: &PartyMember| m.level.saturating_add(LEVEL_WINDOW).min(100) - m.level;
    let memo = std::cell::RefCell::new(std::mem::take(memo_out));
    // Cheapest training of member `i` to `to`, with `carrier` (party index,
    // level in the combination) to switch-train with.
    let cost = |i: usize, to: u8, carrier: Option<(usize, u8)>| -> LevelCost {
        let m = &party[i];
        if to == m.level {
            return Some((0.0, None));
        }
        let carrier = carrier.map(|(c, level)| PartyMember {
            level,
            exp: None,
            ..party[c].clone()
        });
        let key = (
            m.species.clone(),
            m.level,
            m.exp,
            to,
            carrier.as_ref().map(|c| (c.species.clone(), c.level)),
        );
        if let Some(c) = memo.borrow().get(&key) {
            return c.clone();
        }
        let best = request
            .areas
            .iter()
            .filter_map(|a| {
                training_cost(data, m, to, a, carrier.as_ref())
                    .map(|(min, b)| (min, b, a.map.clone()))
            })
            .min_by(|x, y| x.0.total_cmp(&y.0).then_with(|| x.2.cmp(&y.2)))
            .map(|(min, battles, map)| (min, Some((map, battles))));
        memo.borrow_mut().insert(key, best.clone());
        best
    };
    // A combination: each member's level offset. Its members' costs, the
    // carrier at the level the combination gives it; `None` when one of
    // them can't be trained that far.
    let costs_of = |index: &[usize]| -> Option<Vec<MemberCost>> {
        (0..party.len())
            .map(|i| {
                let to = party[i].level + index[i] as u8;
                let carrier = carrier_of(i).map(|c| (c, party[c].level + index[c] as u8));
                let (min, place) = cost(i, to, carrier)?;
                Some((to, min, place))
            })
            .collect()
    };
    let minutes_of = |index: &[usize]| -> Option<f64> {
        costs_of(index).map(|c| base_minutes + c.iter().map(|(_, m, _)| m).sum::<f64>())
    };
    let start = vec![0usize; party.len()];
    let mut heap: BinaryHeap<Reverse<(Minutes, Vec<usize>)>> = BinaryHeap::new();
    let mut seen: BTreeSet<Vec<usize>> = BTreeSet::new();
    if let Some(m) = minutes_of(&start) {
        heap.push(Reverse((Minutes(m), start.clone())));
    }
    seen.insert(start);
    let mut found = 0;
    let mut tried = 0;
    while let Some(Reverse((Minutes(minutes), index))) = heap.pop() {
        if tried >= MAX_LEVEL_COMBOS {
            break;
        }
        tried += 1;
        let Some(costs) = costs_of(&index) else {
            continue;
        };
        let prepared: Vec<Combatant> = party
            .iter()
            .zip(&costs)
            .filter_map(|(m, (to, _, _))| combatant(data, m, *to))
            .collect();
        let conf = confidence(data, &prepared, &request.targets);
        // Worth knowing only when nothing wins.
        let progress = if conf.iter().all(|(_, p)| *p < request.confidence) {
            progress(data, &prepared, &request.targets)
        } else {
            0.0
        };
        let mut steps = base_steps.to_vec();
        for (m, c) in party.iter().zip(&costs) {
            if let (to, min, Some((map, battles))) = c {
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
            progress,
        };
        if plan.min_confidence() >= request.confidence {
            found += 1;
        }
        plans.push(plan);
        if found >= wanted {
            break;
        }
        // One member a level target further, each way. A combination with
        // a member that can't be trained that far isn't searched: it is
        // reached once its carrier is further on (from that combination).
        for k in 0..index.len() {
            let mut next = index.clone();
            next[k] += 1;
            if next[k] <= usize::from(window(&party[k])) && seen.insert(next.clone()) {
                if let Some(m) = minutes_of(&next) {
                    heap.push(Reverse((Minutes(m), next)));
                }
            }
        }
    }
    *memo_out = memo.into_inner();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Option<GameData> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        GameData::load(root.join("data/world/gamedata.json")).ok()
    }

    fn member(species: &str, level: u8) -> PartyMember {
        PartyMember {
            species: species.into(),
            level,
            exp: None,
            moves: Vec::new(),
        }
    }

    fn area(map: &str) -> Area {
        Area {
            map: map.into(),
            travel_minutes: 1.0,
            heal_minutes: 2.0,
        }
    }

    /// The Switch switch-trained a Lv5 CATERPIE for a Lv33 VENUSAUR in
    /// Diglett's Cave: DIGLETT's ARENA TRAP refused the SHIFT, and CATERPIE
    /// had to fight it alone. A trainee that can't beat a wild one that may
    /// trap it isn't switch-trained there; where nothing traps, it is.
    #[test]
    fn no_switch_training_where_a_wild_one_may_trap_the_trainee() {
        let Some(data) = data() else { return };
        let caterpie = member("SPECIES_CATERPIE", 5);
        let venusaur = member("SPECIES_VENUSAUR", 33);
        let cave = training_cost(
            &data,
            &caterpie,
            11,
            &area("DiglettsCave_B1F"),
            Some(&venusaur),
        );
        assert_eq!(cave, None);
        let route = training_cost(&data, &caterpie, 11, &area("Route6"), Some(&venusaur));
        assert!(route.is_some());
        // A flier isn't held by ARENA TRAP.
        let pidgey = member("SPECIES_PIDGEY", 5);
        let cave = training_cost(
            &data,
            &pidgey,
            11,
            &area("DiglettsCave_B1F"),
            Some(&venusaur),
        );
        assert!(cave.is_some());
    }

    /// Fleet worker 4 trained PARAS, ZUBAT and CLEFAIRY for ERIKA and the
    /// Elite Four, all at 0% before and after: a best effort took the plan
    /// adding the most levels, wherever they went. It now trains the member
    /// that gets further against the trainer (VENUSAUR against ERIKA), and
    /// no one where no one does (LORELEI, for now).
    #[test]
    fn a_best_effort_trains_only_who_gets_further() {
        let Some(data) = data() else { return };
        let party = vec![
            member("SPECIES_VENUSAUR", 36),
            member("SPECIES_PARAS", 10),
            member("SPECIES_ZUBAT", 9),
            member("SPECIES_GEODUDE", 17),
            member("SPECIES_CLEFAIRY", 9),
            member("SPECIES_DIGLETT", 19),
        ];
        let trained = |target: &str| -> Vec<String> {
            let request = Request {
                party: party.clone(),
                targets: vec![target.into()],
                areas: ["Route6", "Route11", "DiglettsCave_B1F", "Route9", "Route5"]
                    .iter()
                    .map(|m| area(m))
                    .collect(),
                confidence: 0.9,
                money: 0,
                data: &data,
            };
            let plan = plan_training(&request, 1).remove(0);
            plan.steps
                .iter()
                .filter_map(|s| match s {
                    PlanStep::Train { species, .. } => Some(species.clone()),
                    PlanStep::Catch { .. } => None,
                })
                .collect()
        };
        assert_eq!(trained("TRAINER_LEADER_ERIKA"), vec!["SPECIES_VENUSAUR"]);
        assert!(trained("TRAINER_ELITE_FOUR_LORELEI").is_empty());
    }

    /// Fighting alone earns all the experience; a carrier's win half, a
    /// turn later. A slow win alone doesn't pay.
    #[test]
    fn a_slow_win_alone_is_handed_to_the_carrier() {
        assert!(alone_pays(2.0, 1.0));
        assert!(!alone_pays(8.0, 1.0));
        let Some(data) = data() else { return };
        // PARAS can't face Route 6 alone; VENUSAUR carries it there.
        let paras = member("SPECIES_PARAS", 10);
        let venusaur = member("SPECIES_VENUSAUR", 36);
        assert_eq!(
            training_cost(&data, &paras, 12, &area("Route6"), None),
            None
        );
        assert!(training_cost(&data, &paras, 12, &area("Route6"), Some(&venusaur)).is_some());
    }
}
