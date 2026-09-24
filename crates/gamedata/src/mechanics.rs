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

/// Damage of `mv` from an attacker (types, level, stats) to a defender.
pub fn damage(
    data: &GameData,
    mv: &Move,
    attacker_types: &[String],
    level: u8,
    atk: &Stats,
    defender_types: &[String],
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
    let hit_chance = if mv.accuracy == 0 {
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

/// Probability that one Poké Ball throw catches a Pokémon at full HP with
/// no status (Generation III shake-check formula). `ball` is ×10 (Poké Ball 10,
/// Great 15, Ultra 20).
pub fn catch_probability(catch_rate: u16, max_hp: u32, current_hp: u32, ball: u32) -> f64 {
    let hp_term = (3 * max_hp).saturating_sub(2 * current_hp).max(1);
    let a = (f64::from(hp_term) * f64::from(catch_rate) * f64::from(ball) / 10.0)
        / f64::from(3 * max_hp);
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
}
