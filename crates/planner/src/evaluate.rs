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
    /// The abilities it may have (a species with two: either).
    pub abilities: Vec<String>,
    pub hp: u32,
    /// Accuracy and evasion stages (−6..=6) in the battle; 0 when fresh.
    pub acc_stage: i8,
    pub evasion_stage: i8,
    /// PP left of the moves whose count is known short of their maximum
    /// (the rest are full): a team estimate spends from there.
    #[serde(default)]
    pub pp_left: std::collections::BTreeMap<String, u32>,
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
            abilities: s.abilities.clone(),
            hp: stats.hp(),
            acc_stage: 0,
            evasion_stage: 0,
            pp_left: Default::default(),
        })
    }

    pub fn max_hp(&self) -> u32 {
        self.stats.hp()
    }
}

/// Damaging moves not chosen for their power: they fail unless the foe or
/// the user is asleep (DREAM EATER, SNORE), faint the user (EXPLOSION),
/// need a setup first (SPIT UP), only return damage taken (BIDE, COUNTER,
/// MIRROR COAT), land two turns later (FUTURE SIGHT), or fail against a
/// higher level (the OHKO moves, whose power is nominal), or never faint
/// the foe (FALSE SWIPE leaves it 1 HP: fleet continue-3's MAROWAK used it
/// on a TANGELA four turns running, poisoned, and fled at 16/120). Neither
/// the battle tool nor a matchup counts on them (fleet continue-1:
/// HAUNTER's DREAM EATER made Route 18's DODUO and SPEAROW, immune to its
/// GHOST moves, a win on paper; in battle it had no attack and fled them
/// all).
pub const UNRELIABLE_EFFECTS: [&str; 10] = [
    "EFFECT_FALSE_SWIPE",
    "EFFECT_DREAM_EATER",
    "EFFECT_SNORE",
    "EFFECT_EXPLOSION",
    "EFFECT_SPIT_UP",
    "EFFECT_BIDE",
    "EFFECT_COUNTER",
    "EFFECT_MIRROR_COAT",
    "EFFECT_FUTURE_SIGHT",
    "EFFECT_OHKO",
];

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
        if mv
            .effect
            .as_deref()
            .is_some_and(|e| UNRELIABLE_EFFECTS.contains(&e))
        {
            continue;
        }
        let Some(mut rolls) = damage(
            data,
            mv,
            &attacker.types,
            attacker.level,
            &attacker.stats,
            &defender.types,
            &defender.abilities,
            &defender.stats,
        ) else {
            continue;
        };
        if mv.accuracy > 0 {
            rolls.hit_chance = staged_accuracy(
                rolls.hit_chance,
                attacker.acc_stage - defender.evasion_stage,
            );
        }
        let dist = ko_distribution(&rolls, defender.hp);
        let expected = expected_turns(&dist);
        if best.as_ref().is_none_or(|(e, _, _)| expected < *e) {
            best = Some((expected, name, rolls));
        }
    }
    best.map(|(_, n, r)| (n, r))
}

/// A move's hit chance with the attacker's accuracy stage net of the
/// defender's evasion (clamped to ±6), as the game scales it
/// (`sAccuracyStageRatios`: −1 → 75/100, −2 → 60/100, …, +6 → 3/1).
pub fn staged_accuracy(hit_chance: f64, stage: i8) -> f64 {
    const RATIOS: [(f64, f64); 13] = [
        (33.0, 100.0),
        (36.0, 100.0),
        (43.0, 100.0),
        (50.0, 100.0),
        (60.0, 100.0),
        (75.0, 100.0),
        (1.0, 1.0),
        (133.0, 100.0),
        (166.0, 100.0),
        (2.0, 1.0),
        (233.0, 100.0),
        (133.0, 50.0),
        (3.0, 1.0),
    ];
    let (num, den) = RATIOS[(i32::from(stage.clamp(-6, 6)) + 6) as usize];
    (hit_chance * num / den).min(1.0)
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

/// [`matchup`] remembered: a team is judged against the same opponents
/// for every training level and roster the preparation search tries.
pub fn matchup_cached(data: &GameData, us: &Combatant, them: &Combatant) -> Matchup {
    type Key = (
        String,
        u8,
        Vec<String>,
        u32,
        [u32; 6],
        String,
        u8,
        Vec<String>,
        u32,
        [u32; 6],
    );
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<Key, Matchup>> =
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
        them.moves.clone(),
        them.hp,
        them.stats.0,
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

/// The whole party against a trainer's team, as the battle is played.
#[derive(Debug, Clone, Serialize)]
pub struct TeamEstimate {
    pub trainer: String,
    pub p_win: f64,
    /// The member to lead with (the one met by the first opponent).
    pub lead: usize,
    /// Per opponent: (species, level, probability it is defeated, the
    /// party index of the member sent against it first).
    pub opponents: Vec<(String, u8, f64, usize)>,
}

/// HP a trainer's healing item gives the opponent now out, taking the item
/// (`battle_ai_switch_items.c` `ShouldUseItem`): item `i` past the first
/// is considered only while at most `items − i + 1` opponents are left;
/// a heal item is used once the opponent is under ¼ HP or down by more
/// than it heals, so a fight that gets that far meets the heal (the turn
/// spent on it isn't counted: the estimate errs against us). Fleet
/// worker 6: MISTY's SUPER POTION stretched STARYU to six turns, and
/// WARTORTLE met STARMIE at 26/59.
fn trainer_heal(items: &mut [Option<&str>], left: usize, max_hp: u32) -> Option<u32> {
    let n = items.len();
    for (i, slot) in items.iter_mut().enumerate() {
        if i > 0 && left > n - i + 1 {
            continue;
        }
        let heal = match (*slot)? {
            "ITEM_POTION" => 20,
            "ITEM_SUPER_POTION" => 50,
            "ITEM_HYPER_POTION" => 200,
            "ITEM_MAX_POTION" | "ITEM_FULL_RESTORE" => max_hp,
            _ => continue,
        };
        *slot = None;
        return Some(heal.min(max_hp));
    }
    None
}

/// The party against `trainer` as a team: each opponent is met by the
/// member that beats it best, which is how the battle is played (the SHIFT
/// offered before the trainer's next Pokémon is taken for the member that
/// matches it; after a faint the one sent out is chosen the same way). A
/// member that loses wears the opponent down for the next; the battle is
/// lost only when all have fainted (the user's rule: one Pokémon fainting
/// is not the end). So a team is worth its coverage: a VENUSAUR whose
/// GRASS moves ERIKA's team resists needs a member that hits it.
/// PP our member spends winning `m`: a use of its move each turn. A move
/// spent out is no longer chosen against the trainer's next ones (fleet
/// continue-4, Route 22's rival: HYPNO's PSYCHIC, 10 PP, beat five of six
/// at 100% each in the estimate; in the battle the PP ran out before
/// ALAKAZAM, HYPNO used FLASH and fainted, and the party whited out).
fn spend_pp(
    data: &GameData,
    pp: &mut std::collections::HashMap<String, u32>,
    us: &mut Combatant,
    m: &Matchup,
) {
    let Some(mv) = &m.our_move else { return };
    let left = pp.entry(mv.clone()).or_insert_with(|| {
        us.pp_left
            .get(mv)
            .copied()
            .unwrap_or_else(|| data.move_(mv).map_or(0, |x| u32::from(x.pp)))
    });
    *left = left.saturating_sub(m.turns.ceil().max(1.0) as u32);
    if *left == 0 {
        us.moves.retain(|x| x != mv);
    }
}

pub fn team_vs_trainer(
    data: &GameData,
    party: &[Combatant],
    trainer: &str,
) -> Option<TeamEstimate> {
    let t = data.trainers.get(trainer)?;
    // The trainer's healing items, each used once (`ShouldUseItem`).
    let mut items: Vec<Option<&str>> = t.items.iter().map(|i| Some(i.as_str())).collect();
    let mut ours: Vec<Combatant> = party.to_vec();
    let mut pp = vec![std::collections::HashMap::new(); ours.len()];
    let mut p_total = 1.0;
    let mut lead = None;
    let mut opponents = Vec::new();
    for mon in &t.party {
        let moves = mon
            .moves
            .clone()
            .unwrap_or_else(|| data.default_moves(&mon.species, mon.level));
        let iv = u32::from(mon.iv) * 31 / 255;
        let Some(mut enemy) = Combatant::new(data, &mon.species, mon.level, moves, iv) else {
            continue;
        };
        // A healing item the AI would use on this one counts as its HP.
        let left = t.party.len() - opponents.len();
        if let Some(heal) = trainer_heal(&mut items, left, enemy.max_hp()) {
            enemy.hp += heal;
        }
        let mut p_lose_all = 1.0;
        let mut by = None;
        loop {
            let best = ours
                .iter()
                .enumerate()
                .filter(|(_, c)| c.hp > 0)
                .map(|(i, c)| (i, matchup_cached(data, c, &enemy)))
                .max_by(|(i, a), (j, b)| {
                    a.p_win
                        .total_cmp(&b.p_win)
                        .then(b.turns.total_cmp(&a.turns))
                        .then(j.cmp(i))
                });
            let Some((i, m)) = best else {
                break;
            };
            lead.get_or_insert(i);
            by.get_or_insert(i);
            p_lose_all *= 1.0 - m.p_win;
            if m.p_win >= 0.5 {
                ours[i].hp = m.our_hp_after;
                spend_pp(data, &mut pp[i], &mut ours[i], &m);
                break;
            }
            // Expected path: our member faints; the opponent is worn down.
            enemy.hp = m.their_hp_after.max(1);
            ours[i].hp = 0;
        }
        let p = 1.0 - p_lose_all;
        opponents.push((mon.species.clone(), mon.level, p, by.unwrap_or(0)));
        p_total *= p;
        if ours.iter().all(|c| c.hp == 0) {
            break;
        }
    }
    if opponents.len() < t.party.len() {
        p_total = 0.0;
    }
    Some(TeamEstimate {
        trainer: format!("{} ({trainer})", t.name),
        p_win: p_total,
        lead: lead?,
        opponents,
    })
}

/// The party (in order, lead first) against a trainer's team.
/// The member who beats `trainer` alone with the best chance, and that
/// chance: the bot doesn't plan on a Pokémon fainting (the battle sends
/// out the next when one does, but a relay where one member faints and the
/// next finishes is not a plan); the one who can win it all leads instead
/// (fleet workers: CHARMANDER
/// led against Brock and fainted to ONIX while the MANKEY trained to
/// finish him waited in the party).
pub fn best_fighter(data: &GameData, party: &[Combatant], trainer: &str) -> Option<(usize, f64)> {
    party
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            battle_vs_trainer(data, std::slice::from_ref(c), trainer).map(|e| (i, e.p_win))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
}

pub fn battle_vs_trainer(
    data: &GameData,
    party: &[Combatant],
    trainer: &str,
) -> Option<BattleEstimate> {
    let t = data.trainers.get(trainer)?;
    // The trainer's healing items, each used once (`ShouldUseItem`).
    let mut items: Vec<Option<&str>> = t.items.iter().map(|i| Some(i.as_str())).collect();
    let mut ours: Vec<Combatant> = party.to_vec();
    let mut pp = vec![std::collections::HashMap::new(); ours.len()];
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
        // A healing item the AI would use on this one counts as its HP.
        let left = t.party.len() - opponents.len();
        if let Some(heal) = trainer_heal(&mut items, left, enemy.max_hp()) {
            enemy.hp += heal;
        }
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
                spend_pp(data, &mut pp[active], &mut ours[active], &m);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fleet continue-4, Route 22's rival: a Lv70 HYPNO whose one strong
    /// attack, PSYCHIC, has 10 PP, and a weak rest. Counted turn by turn it
    /// runs out before the sixth; the estimate was 94% and the party
    /// whited out twice.
    #[test]
    fn a_team_estimate_spends_pp() {
        let Ok(data) = GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let mon = |species: &str, level: u8, moves: &[&str]| {
            Combatant::new(
                &data,
                species,
                level,
                moves.iter().map(|m| m.to_string()).collect(),
                15,
            )
            .unwrap()
        };
        let party = [
            mon(
                "SPECIES_HYPNO",
                70,
                &[
                    "MOVE_FLASH",
                    "MOVE_PSYCHIC",
                    "MOVE_FUTURE_SIGHT",
                    "MOVE_CONFUSION",
                ],
            ),
            mon("SPECIES_RATTATA", 8, &["MOVE_TACKLE", "MOVE_QUICK_ATTACK"]),
            mon("SPECIES_SPEAROW", 13, &["MOVE_PECK", "MOVE_FLY"]),
            mon("SPECIES_LAPRAS", 25, &["MOVE_SURF", "MOVE_BODY_SLAM"]),
            mon("SPECIES_GOLDUCK", 34, &["MOVE_DISABLE", "MOVE_CONFUSION"]),
        ];
        let rival = "TRAINER_RIVAL_ROUTE22_LATE_CHARMANDER";
        let estimate = team_vs_trainer(&data, &party, rival).unwrap();
        assert!(estimate.p_win < 0.5, "{:.2}", estimate.p_win);
    }

    /// Fleet continue-1: HAUNTER (LICK, DREAM EATER, CURSE, NIGHT SHADE)
    /// against Route 18's DODUO: its GHOST moves don't touch a NORMAL
    /// type, and DREAM EATER fails on a foe awake. No move does damage.
    #[test]
    fn dream_eater_is_no_attack_on_a_foe_awake() {
        let Ok(data) = GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let moves: Vec<String> = [
            "MOVE_LICK",
            "MOVE_DREAM_EATER",
            "MOVE_CURSE",
            "MOVE_NIGHT_SHADE",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let haunter = Combatant::new(&data, "SPECIES_HAUNTER", 40, moves, 15).unwrap();
        let doduo = Combatant::new(
            &data,
            "SPECIES_DODUO",
            26,
            data.default_moves("SPECIES_DODUO", 26),
            15,
        )
        .unwrap();
        assert_eq!(best_move(&data, &haunter, &doduo).map(|(m, _)| m), None);
    }

    /// Fleet continue-3: MAROWAK trained on Route 21 with FALSE SWIPE as
    /// its best move against TANGELA; it never faints the foe, and the
    /// battle went on until MAROWAK, poisoned, fled. Another move wins it.
    #[test]
    fn false_swipe_is_no_way_to_faint_a_foe() {
        let Ok(data) = GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let moves = ["MOVE_FALSE_SWIPE", "MOVE_BONE_CLUB"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let marowak = Combatant::new(&data, "SPECIES_MAROWAK", 44, moves, 15).unwrap();
        let tangela = Combatant::new(
            &data,
            "SPECIES_TANGELA",
            30,
            data.default_moves("SPECIES_TANGELA", 30),
            15,
        )
        .unwrap();
        assert_eq!(
            best_move(&data, &marowak, &tangela)
                .map(|(m, _)| m)
                .as_deref(),
            Some("MOVE_BONE_CLUB")
        );
    }
}
