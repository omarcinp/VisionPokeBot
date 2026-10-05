//! Generation III formulas (integer arithmetic as in the game).

use crate::{GameData, Move};

/// Physical types in Generation III (the move's type decides the category).
pub fn is_physical(kind: &str) -> bool {
    matches!(
        kind,
        "TYPE_NORMAL"
            | "TYPE_FIGHTING"
            | "TYPE_FLYING"
            | "TYPE_POISON"
            | "TYPE_GROUND"
            | "TYPE_ROCK"
            | "TYPE_BUG"
            | "TYPE_GHOST"
            | "TYPE_STEEL"
    )
}

/// Battle stats of one Pokémon: HP, Atk, Def, Spe, SpA, SpD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats(pub [u32; 6]);

impl Stats {
    /// Neutral nature, `iv` in every stat, no EVs.
    pub fn compute(base: &[u16; 6], level: u8, iv: u32) -> Stats {
        let l = u32::from(level);
        let mut s = [0; 6];
        for (i, b) in base.iter().enumerate() {
            let core = (2 * u32::from(*b) + iv) * l / 100;
            s[i] = if i == 0 { core + l + 10 } else { core + 5 };
        }
        Stats(s)
    }

    /// One individual's stats: its IVs, EVs and nature per stat (HP, Atk,
    /// Def, Spe, SpA, SpD), as the game computes them
    /// ([`crate::training::stat`]).
    pub fn of_individual(
        base: &[u16; 6],
        level: u8,
        ivs: [u8; 6],
        evs: [u16; 6],
        nature: u8,
    ) -> Stats {
        Stats(std::array::from_fn(|i| {
            u32::from(crate::training::stat(
                base[i], i, level, ivs[i], evs[i], nature,
            ))
        }))
    }

    pub fn hp(&self) -> u32 {
        self.0[0]
    }
    pub fn attack(&self) -> u32 {
        self.0[1]
    }
    pub fn defense(&self) -> u32 {
        self.0[2]
    }
    pub fn speed(&self) -> u32 {
        self.0[3]
    }
    pub fn sp_attack(&self) -> u32 {
        self.0[4]
    }
    pub fn sp_defense(&self) -> u32 {
        self.0[5]
    }
}

/// Total experience needed to reach `level`.
pub fn exp_for_level(growth: &str, level: u8) -> u64 {
    let n = i64::from(level);
    let n3 = n * n * n;
    let v = match growth {
        "GROWTH_FAST" => 4 * n3 / 5,
        "GROWTH_MEDIUM_SLOW" => 6 * n3 / 5 - 15 * n * n + 100 * n - 140,
        "GROWTH_SLOW" => 5 * n3 / 4,
        "GROWTH_ERRATIC" => match level {
            0..=50 => n3 * (100 - n) / 50,
            51..=68 => n3 * (150 - n) / 100,
            69..=98 => n3 * ((1911 - 10 * n) / 3) / 500,
            _ => n3 * (160 - n) / 100,
        },
        "GROWTH_FLUCTUATING" => match level {
            0..=15 => n3 * ((n + 1) / 3 + 24) / 50,
            16..=36 => n3 * (n + 14) / 50,
            _ => n3 * (n / 2 + 32) / 50,
        },
        _ => n3, // GROWTH_MEDIUM_FAST
    };
    v.max(0) as u64
}

/// Experience one participant gains for defeating `species` at `level`.
pub fn exp_gain(data: &GameData, species: &str, level: u8, trainer: bool) -> u64 {
    let base = u64::from(data.species(species).map_or(0, |s| s.exp_yield));
    let exp = base * u64::from(level) / 7;
    if trainer {
        exp * 3 / 2
    } else {
        exp
    }
}

/// One attack's possible damage values (the 16 random rolls, 85–100 %),
/// without and with a critical hit. Empty for status moves or immunity.
pub struct DamageRolls {
    pub normal: [u32; 16],
    pub critical: [u32; 16],
    pub hit_chance: f64,
}

/// Effects whose move takes two turns for one hit: charged first (DIG,
/// FLY, SOLAR BEAM, …) or followed by a turn of recharge (HYPER BEAM).
const TWO_TURN_EFFECTS: [&str; 6] = [
    "EFFECT_SEMI_INVULNERABLE",
    "EFFECT_SOLAR_BEAM",
    "EFFECT_RAZOR_WIND",
    "EFFECT_SKY_ATTACK",
    "EFFECT_SKULL_BASH",
    "EFFECT_RECHARGE",
];

/// Hits a move lands per turn used: a two-turn move half as often.
fn per_turn(mv: &Move) -> f64 {
    if mv
        .effect
        .as_deref()
        .is_some_and(|e| TWO_TURN_EFFECTS.contains(&e))
    {
        0.5
    } else {
        1.0
    }
}

/// Whether the defender's `ability` stops a `kind` move of `effectiveness`
/// (×10) from doing damage (`battle_util.c`, `AbilityBattleEffects`
/// ABILITYEFFECT_ABSORBING and `CalculateBaseDamage`): LEVITATE and
/// Ground, VOLT ABSORB and Electric, WATER ABSORB and Water, FLASH FIRE
/// and Fire; WONDER GUARD lets through only a super-effective hit.
pub fn ability_blocks(ability: &str, kind: &str, effectiveness: u32) -> bool {
    match ability {
        "ABILITY_LEVITATE" => kind == "TYPE_GROUND",
        "ABILITY_VOLT_ABSORB" => kind == "TYPE_ELECTRIC",
        "ABILITY_WATER_ABSORB" => kind == "TYPE_WATER",
        "ABILITY_FLASH_FIRE" => kind == "TYPE_FIRE",
        "ABILITY_WONDER_GUARD" => effectiveness <= 10,
        _ => false,
    }
}

/// The share of the defender's possible abilities that let a `kind` move
/// through: a species with two may have either (Switch: DIG chosen against
/// GASTLY, Ground on Poison "super effective", but LEVITATE takes none).
fn ability_passes(abilities: &[String], kind: &str, effectiveness: u32) -> f64 {
    let known: Vec<&String> = abilities
        .iter()
        .filter(|a| a.as_str() != "ABILITY_NONE")
        .collect();
    if known.is_empty() {
        return 1.0;
    }
    let passes = known
        .iter()
        .filter(|a| !ability_blocks(a, kind, effectiveness))
        .count();
    passes as f64 / known.len() as f64
}

/// Damage of `mv` from an attacker (types, level, stats) to a defender
/// (types, stats, the abilities it may have). An ability that may stop
/// the move lowers its hit chance by the share that would.
#[allow(clippy::too_many_arguments)]
pub fn damage(
    data: &GameData,
    mv: &Move,
    attacker_types: &[String],
    level: u8,
    atk: &Stats,
    defender_types: &[String],
    defender_abilities: &[String],
    def: &Stats,
) -> Option<DamageRolls> {
    let kind = mv.kind.as_deref()?;
    if mv.power == 0 {
        return None;
    }
    let effectiveness = data.effectiveness(kind, defender_types);
    if effectiveness == 0 {
        return None;
    }
    let passes = ability_passes(defender_abilities, kind, effectiveness);
    if passes == 0.0 {
        return None;
    }
    let (a, d) = if is_physical(kind) {
        (atk.attack(), def.defense())
    } else {
        (atk.sp_attack(), def.sp_defense())
    };
    let base = |crit: u32| {
        let core = ((2 * u32::from(level) / 5 + 2) * u32::from(mv.power) * a / d.max(1)) / 50;
        (core + 2) * crit
    };
    let stab = attacker_types.iter().any(|t| t == kind);
    let roll = |base: u32, r: u32| {
        let mut dmg = base * r / 100;
        if stab {
            dmg = dmg * 3 / 2;
        }
        (dmg * effectiveness / 10).max(1)
    };
    let (b1, b2) = (base(1), base(2));
    let mut normal = [0; 16];
    let mut critical = [0; 16];
    for i in 0..16 {
        normal[i] = roll(b1, 85 + i as u32);
        critical[i] = roll(b2, 85 + i as u32);
    }
    let hit_chance = passes
        * per_turn(mv)
        * if mv.accuracy == 0 {
            1.0
        } else {
            f64::from(mv.accuracy.min(100)) / 100.0
        };
    Some(DamageRolls {
        normal,
        critical,
        hit_chance,
    })
}

/// Catch multiplier ×10 for the balls the bot throws on its own. The Master
/// Ball is never auto-used; balls with conditional bonuses (Net, Nest, …)
/// count as a Poké Ball.
pub fn ball_multiplier(item: &str) -> Option<u32> {
    match item {
        "ITEM_ULTRA_BALL" => Some(20),
        "ITEM_GREAT_BALL" | "ITEM_SAFARI_BALL" => Some(15),
        "ITEM_MASTER_BALL" => None,
        i if i.ends_with("_BALL") => Some(10),
        _ => None,
    }
}

/// Probability that one Poké Ball throw catches a Pokémon at full HP with
/// no status (Generation III shake-check formula). `ball` is ×10 (Poké Ball 10,
/// Great 15, Ultra 20).
pub fn catch_probability(catch_rate: u16, max_hp: u32, current_hp: u32, ball: u32) -> f64 {
    catch_probability_status(catch_rate, max_hp, current_hp, ball, 10)
}

/// `catch_probability` with the status bonus ×10 (20 asleep or frozen, 15
/// paralysed, poisoned or burned, 10 none).
pub fn catch_probability_status(
    catch_rate: u16,
    max_hp: u32,
    current_hp: u32,
    ball: u32,
    status_x10: u32,
) -> f64 {
    let hp_term = (3 * max_hp).saturating_sub(2 * current_hp).max(1);
    let a = (f64::from(hp_term) * f64::from(catch_rate) * f64::from(ball) / 10.0)
        / f64::from(3 * max_hp)
        * f64::from(status_x10)
        / 10.0;
    if a >= 255.0 {
        return 1.0;
    }
    let b = 1_048_560.0 / (16_711_680.0 / a.max(1.0)).sqrt().sqrt();
    (b / 65536.0).powi(4).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_match_known_values() {
        // Bulbasaur (45/49/49/45/65/65), level 5, IV 0: HP 19, Atk 9.
        let s = Stats::compute(&[45, 49, 49, 45, 65, 65], 5, 0);
        assert_eq!(s.hp(), 19);
        assert_eq!(s.attack(), 9);
    }

    #[test]
    fn exp_curves() {
        assert_eq!(exp_for_level("GROWTH_MEDIUM_FAST", 10), 1000);
        // Medium slow at 5: 150 - 375 + 500 - 140 = 135.
        assert_eq!(exp_for_level("GROWTH_MEDIUM_SLOW", 5), 135);
        assert_eq!(exp_for_level("GROWTH_FAST", 10), 800);
    }

    #[test]
    fn catching_weakened_is_easier() {
        // Catch rate 255 at full HP: a = 85, about a one-in-three throw.
        let full = catch_probability(255, 20, 20, 10);
        assert!((full - 0.33).abs() < 0.02, "{full}");
        let low = catch_probability(45, 20, 1, 10);
        let hard_full = catch_probability(45, 20, 20, 10);
        assert!(low > 2.0 * hard_full);
    }

    #[test]
    fn balls_and_status_raise_the_catch_chance() {
        assert_eq!(ball_multiplier("ITEM_POKE_BALL"), Some(10));
        assert_eq!(ball_multiplier("ITEM_GREAT_BALL"), Some(15));
        assert_eq!(ball_multiplier("ITEM_ULTRA_BALL"), Some(20));
        assert_eq!(ball_multiplier("ITEM_PREMIER_BALL"), Some(10));
        assert_eq!(ball_multiplier("ITEM_MASTER_BALL"), None);
        assert_eq!(ball_multiplier("ITEM_POTION"), None);
        let plain = catch_probability_status(45, 30, 7, 10, 10);
        assert_eq!(plain, catch_probability(45, 30, 7, 10));
        assert!(catch_probability_status(45, 30, 7, 10, 20) > plain);
        assert!(catch_probability_status(45, 30, 7, 15, 10) > plain);
    }
}

#[cfg(test)]
mod ability_tests {
    use super::*;

    fn abilities(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn abilities_that_stop_a_type_of_move() {
        assert!(ability_blocks("ABILITY_LEVITATE", "TYPE_GROUND", 20));
        assert!(!ability_blocks("ABILITY_LEVITATE", "TYPE_ROCK", 10));
        assert!(ability_blocks("ABILITY_VOLT_ABSORB", "TYPE_ELECTRIC", 10));
        assert!(ability_blocks("ABILITY_WATER_ABSORB", "TYPE_WATER", 10));
        assert!(ability_blocks("ABILITY_FLASH_FIRE", "TYPE_FIRE", 10));
        assert!(ability_blocks("ABILITY_WONDER_GUARD", "TYPE_NORMAL", 10));
        assert!(!ability_blocks("ABILITY_WONDER_GUARD", "TYPE_FIRE", 20));
        // One of two possible abilities stops it: half the hits land.
        let either = abilities(&["ABILITY_VOLT_ABSORB", "ABILITY_STATIC"]);
        assert_eq!(ability_passes(&either, "TYPE_ELECTRIC", 10), 0.5);
        let only = abilities(&["ABILITY_LEVITATE", "ABILITY_NONE"]);
        assert_eq!(ability_passes(&only, "TYPE_GROUND", 20), 0.0);
        assert_eq!(ability_passes(&[], "TYPE_GROUND", 20), 1.0);
    }
}
