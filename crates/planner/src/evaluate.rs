//! Exact battle outcome estimates.
//!
//! One-on-one: each side uses its most damaging move every turn. For each
//! side the distribution of "attacks needed to faint the other" is computed
//! exactly over hit/miss, crit and the 16 damage rolls; combined with the
//! speed order this gives the probability of winning the matchup.
//!
//! Team battles chain matchups in party order with HP carried over as
//! expected values (an approximation, documented as such).

use pokebot_gamedata::mechanics::{damage, DamageRolls, Stats};
use pokebot_gamedata::GameData;
use serde::Serialize;

/// Turns considered before calling a fight a stalemate.
const MAX_TURNS: usize = 40;

#[derive(Debug, Clone, Serialize)]
pub struct Combatant {
    pub species: String,
    pub level: u8,
    pub moves: Vec<String>,
    #[serde(skip)]
    pub stats: Stats,
    pub types: Vec<String>,
    pub hp: u32,
}

impl Combatant {
    /// `iv` applies to every stat (0–31).
    pub fn new(
        data: &GameData,
        species: &str,
        level: u8,
        moves: Vec<String>,
        iv: u32,
    ) -> Option<Combatant> {
        let s = data.species(species)?;
        let stats = Stats::compute(&s.base, level, iv);
        Some(Combatant {
            species: species.to_owned(),
            level,
            moves,
            stats,
            types: s.types.clone(),
            hp: stats.hp(),
        })
    }

    pub fn max_hp(&self) -> u32 {
        self.stats.hp()
    }
}

/// The attacker's best move against `defender`: fewest expected attacks to
/// faint it (ties: move name order). `None` if nothing does damage.
pub fn best_move(
    data: &GameData,
    attacker: &Combatant,
    defender: &Combatant,
) -> Option<(String, DamageRolls)> {
    let mut best: Option<(f64, String, DamageRolls)> = None;
    let mut names = attacker.moves.clone();
    names.sort();
    for name in names {
        let Some(mv) = data.move_(&name) else {
            continue;
        };
        let Some(rolls) = damage(
            data,
            mv,
            &attacker.types,
            attacker.level,
            &attacker.stats,
            &defender.types,
            &defender.stats,
        ) else {
            continue;
        };
        let dist = ko_distribution(&rolls, defender.hp);
        let expected = expected_turns(&dist);
        if best.as_ref().is_none_or(|(e, _, _)| expected < *e) {
            best = Some((expected, name, rolls));
        }
    }
    best.map(|(_, n, r)| (n, r))
}

/// Probability that `defender` faints within `turns` attacks of `attacker`'s
/// most damaging move (every attack is that move), from `defender.hp`.
pub fn faint_probability(
    data: &GameData,
    attacker: &Combatant,
    defender: &Combatant,
    turns: usize,
) -> f64 {
    let Some((_, rolls)) = best_move(data, attacker, defender) else {
        return 0.0;
    };
    ko_distribution(&rolls, defender.hp)
        .iter()
        .take(turns.min(MAX_TURNS))
        .sum::<f64>()
        .min(1.0)
}

/// `result[t]` = probability the defender faints exactly on attack `t+1`.
fn ko_distribution(rolls: &DamageRolls, hp: u32) -> Vec<f64> {
    let hp = hp as usize;
    let mut alive = vec![0.0; hp + 1]; // index = remaining HP
    alive[hp] = 1.0;
    let mut out = Vec::with_capacity(MAX_TURNS);
    let crit = 1.0 / 16.0;
    let outcomes: Vec<(u32, f64)> = rolls
        .normal
        .iter()
        .map(|d| (*d, rolls.hit_chance * (1.0 - crit) / 16.0))
        .chain(
            rolls
                .critical
                .iter()
                .map(|d| (*d, rolls.hit_chance * crit / 16.0)),
        )
        .collect();
    let miss = 1.0 - rolls.hit_chance;
    for _ in 0..MAX_TURNS {
        let mut next = vec![0.0; hp + 1];
        let mut fainted = 0.0;
        for (remaining, p) in alive.iter().enumerate() {
            if *p == 0.0 {
                continue;
            }
            next[remaining] += p * miss;
            for (dmg, q) in &outcomes {
                let r = remaining as i64 - i64::from(*dmg);
                if r <= 0 {
                    fainted += p * q;
                } else {
                    next[r as usize] += p * q;
                }
            }
        }
        out.push(fainted);
        alive = next;
    }
    out
}

fn expected_turns(dist: &[f64]) -> f64 {
    let reached: f64 = dist.iter().sum();
    let e: f64 = dist
        .iter()
        .enumerate()
        .map(|(t, p)| (t as f64 + 1.0) * p)
        .sum();
    e + (1.0 - reached) * (MAX_TURNS as f64 * 2.0)
}

fn mean_damage(rolls: &DamageRolls) -> f64 {
    let crit = 1.0 / 16.0;
    let n: f64 = rolls.normal.iter().map(|d| f64::from(*d)).sum::<f64>() / 16.0;
    let c: f64 = rolls.critical.iter().map(|d| f64::from(*d)).sum::<f64>() / 16.0;
    rolls.hit_chance * ((1.0 - crit) * n + crit * c)
}

#[derive(Debug, Clone, Serialize)]
pub struct Matchup {
    pub p_win: f64,
    pub our_move: Option<String>,
    pub their_move: Option<String>,
    /// Our expected HP afterwards if we win; their expected HP if we lose.
    pub our_hp_after: u32,
    pub their_hp_after: u32,
    /// Expected number of turns.
    pub turns: f64,
}

/// One-on-one from the current HP of both sides.
pub fn matchup(data: &GameData, us: &Combatant, them: &Combatant) -> Matchup {
    let ours = best_move(data, us, them);
    let theirs = best_move(data, them, us);
    let our_dist = ours.as_ref().map_or_else(
        || vec![0.0; MAX_TURNS],
        |(_, r)| ko_distribution(r, them.hp),
    );
    let their_dist = theirs
        .as_ref()
        .map_or_else(|| vec![0.0; MAX_TURNS], |(_, r)| ko_distribution(r, us.hp));
    // P(they need at least k attacks).
    let their_tail = |k: usize| 1.0 - their_dist.iter().take(k.saturating_sub(1)).sum::<f64>();
    let faster = match us.stats.speed().cmp(&them.stats.speed()) {
        std::cmp::Ordering::Greater => 1.0,
        std::cmp::Ordering::Equal => 0.5,
        std::cmp::Ordering::Less => 0.0,
    };
    let mut p_win = 0.0;
    let mut turns_if_win = 0.0;
    for (t, p) in our_dist.iter().enumerate() {
        let k = t + 1;
        // Moving first we win if they need ≥ k attacks; moving second, > k.
        let survive = faster * their_tail(k) + (1.0 - faster) * their_tail(k + 1);
        p_win += p * survive;
        turns_if_win += p * survive * k as f64;
    }
    let p_win = p_win.clamp(0.0, 1.0);
    let our_mean = ours.as_ref().map_or(0.0, |(_, r)| mean_damage(r));
    let their_mean = theirs.as_ref().map_or(0.0, |(_, r)| mean_damage(r));
    let turns = if p_win > 0.0 {
        turns_if_win / p_win
    } else {
        expected_turns(&their_dist)
    };
    let taken = (their_mean * (turns - faster).max(0.0)).round() as u32;
    let dealt = (our_mean * expected_turns(&their_dist).min(MAX_TURNS as f64)).round() as u32;
    Matchup {
        p_win,
        our_move: ours.map(|(m, _)| m),
        their_move: theirs.map(|(m, _)| m),
        our_hp_after: us.hp.saturating_sub(taken).max(1),
        their_hp_after: them.hp.saturating_sub(dealt),
        turns,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BattleEstimate {
    pub trainer: String,
    pub p_win: f64,
    /// Per opponent: (species, level, probability it is defeated).
    pub opponents: Vec<(String, u8, f64)>,
    pub matchups: Vec<(String, String, Matchup)>,
}

/// The party (in order, lead first) against a trainer's team.
pub fn battle_vs_trainer(
    data: &GameData,
    party: &[Combatant],
    trainer: &str,
) -> Option<BattleEstimate> {
    let t = data.trainers.get(trainer)?;
    let mut ours: Vec<Combatant> = party.to_vec();
    let mut active = 0;
    let mut p_total = 1.0;
    let mut opponents = Vec::new();
    let mut matchups = Vec::new();
    for mon in &t.party {
        let moves = mon
            .moves
            .clone()
            .unwrap_or_else(|| data.default_moves(&mon.species, mon.level));
        let iv = u32::from(mon.iv) * 31 / 255;
        let Some(mut enemy) = Combatant::new(data, &mon.species, mon.level, moves, iv) else {
            continue;
        };
        // Chance this opponent is beaten by one of our remaining members.
        let mut p_lose_all = 1.0;
        while active < ours.len() {
            let m = matchup(data, &ours[active], &enemy);
            matchups.push((
                ours[active].species.clone(),
                enemy.species.clone(),
                m.clone(),
            ));
            p_lose_all *= 1.0 - m.p_win;
            if m.p_win >= 0.5 {
                ours[active].hp = m.our_hp_after;
                break;
            }
            // Expected path: our member faints; the opponent is worn down.
            enemy.hp = m.their_hp_after.max(1);
            ours[active].hp = 0;
            active += 1;
        }
        let p = 1.0 - p_lose_all;
        opponents.push((mon.species.clone(), mon.level, p));
        p_total *= p;
        if active >= ours.len() {
            break;
        }
    }
    if opponents.len() < t.party.len() {
        p_total = 0.0;
    }
    Some(BattleEstimate {
        trainer: format!("{} ({trainer})", t.name),
        p_win: p_total,
        opponents,
        matchups,
    })
}
