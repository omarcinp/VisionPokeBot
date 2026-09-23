//! What the bot knows about its party, kept up to date from what it sees
//! (battle HUD: name, level, HP) and from game data (moves learned by level).

use std::collections::BTreeMap;

use pokebot_gamedata::GameData;
use pokebot_state::{BattleMenu, BattleObservation};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub species: String,
    pub level: u8,
    /// Moves in menu order (the order they were learned).
    pub moves: Vec<String>,
    /// Current and maximum HP, when last seen.
    pub hp: Option<(u16, u16)>,
    /// PP spent per move since the last full heal.
    #[serde(default)]
    pub pp_used: BTreeMap<String, u8>,
}

impl Member {
    /// A freshly obtained Pokémon at `level` with its default moves.
    pub fn new(data: &GameData, species: &str, level: u8) -> Self {
        Self {
            species: species.to_owned(),
            level,
            moves: data.default_moves(species, level),
            hp: None,
            pp_used: BTreeMap::new(),
        }
    }

    /// Name as the game prints it (`SPECIES_BULBASAUR` → `BULBASAUR`).
    pub fn display_name(&self) -> String {
        display_name(&self.species)
    }

    pub fn pp_left(&self, data: &GameData, mv: &str) -> u8 {
        let max = data.move_(mv).map_or(0, |m| m.pp);
        max.saturating_sub(*self.pp_used.get(mv).unwrap_or(&0))
    }

    /// Applies a level seen on screen: learn moves gained since while a slot
    /// is free (appended in learning order). With four moves known the game
    /// asks which to forget, and evolution can be cancelled: both are
    /// recorded from the screen (`learn`, the HUD name), not assumed here.
    pub fn observe_level(&mut self, data: &GameData, level: u8) -> Vec<String> {
        let mut learned = Vec::new();
        if level <= self.level {
            return learned;
        }
        {
            let species = self.species.clone();
            for (lvl, mv) in data
                .species(&species)
                .map(|s| s.learnset.clone())
                .unwrap_or_default()
            {
                if lvl > self.level
                    && lvl <= level
                    && !self.moves.contains(&mv)
                    && self.moves.len() < 4
                {
                    self.moves.push(mv.clone());
                    learned.push(mv);
                }
            }
        }
        self.level = level;
        learned
    }

    /// PP that wild battles may spend on damaging moves: moves with 30+ PP
    /// in full, others above the reserve kept for trainers.
    pub fn wild_attack_pp(&self, data: &GameData, reserve: u8) -> u32 {
        self.moves
            .iter()
            .filter(|m| data.move_(m).is_some_and(|mv| mv.power > 0))
            .map(|m| {
                let left = self.pp_left(data, m);
                let max = data.move_(m).map_or(0, |mv| mv.pp);
                u32::from(if max >= 30 {
                    left
                } else {
                    left.saturating_sub(reserve)
                })
            })
            .sum()
    }

    pub fn heal(&mut self) {
        self.hp = self.hp.map(|(_, max)| (max, max));
        self.pp_used.clear();
    }
}

pub fn display_name(species: &str) -> String {
    let name = species.trim_start_matches("SPECIES_");
    name.trim_end_matches("_M")
        .trim_end_matches("_F")
        .replace('_', " ")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    pub members: Vec<Member>,
}

impl Party {
    pub fn lead(&self) -> Option<&Member> {
        self.members.first()
    }

    /// Updates the member shown in a battle HUD. Returns newly learned moves.
    pub fn observe_battle(&mut self, data: &GameData, battle: &BattleObservation) -> Vec<String> {
        let Some(name) = &battle.player_name else {
            return Vec::new();
        };
        // The HUD shows an evolved name once the member has evolved.
        let Some((member, species)) = self
            .members
            .iter_mut()
            .find_map(|m| seen_as(data, &m.species, name).map(|s| (m, s)))
        else {
            return Vec::new();
        };
        member.species = species;
        if let Some(hp) = battle.player_hp_numbers {
            member.hp = Some(hp);
        }
        // The move menu shows the PP of the move under the ▶.
        if let (Some(BattleMenu::Moves { column, row }), Some((left, max))) =
            (battle.menu, battle.move_pp)
        {
            if let Some(mv) = member.moves.get(usize::from(row * 2 + column)).cloned() {
                member.pp_used.insert(mv, max.saturating_sub(left));
            }
        }
        match battle.player_level {
            Some(level) => member.observe_level(data, level),
            None => Vec::new(),
        }
    }

    pub fn heal_all(&mut self) {
        self.members.iter_mut().for_each(Member::heal);
    }
}

/// `species` itself or the evolution of it whose name matches `read`.
fn seen_as(data: &GameData, species: &str, read: &str) -> Option<String> {
    let mut frontier = vec![species.to_owned()];
    while !frontier.is_empty() {
        let current = frontier.remove(0);
        if names_match(&display_name(&current), read) {
            return Some(current);
        }
        frontier.extend(data.evolutions_of(&current));
    }
    None
}

/// `read` may contain `?` for unrecognised letters.
pub fn names_match(name: &str, read: &str) -> bool {
    name.len() == read.len()
        && name
            .chars()
            .zip(read.chars())
            .all(|(a, b)| b == '?' || a == b)
}
