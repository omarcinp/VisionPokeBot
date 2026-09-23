//! What the bot knows about its party, kept up to date from what it sees
//! (battle HUD: name, level, HP) and from game data (moves learned by level).

use std::collections::BTreeMap;

use pokebot_gamedata::GameData;
use pokebot_state::BattleObservation;
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

    /// Applies a level seen on screen: learn moves gained since (appended in
    /// learning order; the oldest is replaced once four are known, as when
    /// every new move is accepted) and evolve if due.
    pub fn observe_level(&mut self, data: &GameData, level: u8) -> Vec<String> {
        let mut learned = Vec::new();
        if level <= self.level {
            return learned;
        }
        let evolved = data.evolved_at(&self.species, level);
        for species in [self.species.clone(), evolved.clone()] {
            for (lvl, mv) in data
                .species(&species)
                .map(|s| s.learnset.clone())
                .unwrap_or_default()
            {
                if lvl > self.level && lvl <= level && !self.moves.contains(&mv) {
                    if self.moves.len() == 4 {
                        self.moves.remove(0);
                    }
                    self.moves.push(mv.clone());
                    learned.push(mv);
                }
            }
        }
        self.species = evolved;
        self.level = level;
        learned
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
        let Some(member) = self.members.iter_mut().find(|m| {
            crate::party::names_match(&m.display_name(), name)
                || crate::party::names_match(&display_name(&data.evolved_at(&m.species, 100)), name)
        }) else {
            return Vec::new();
        };
        if let Some(hp) = battle.player_hp_numbers {
            member.hp = Some(hp);
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

/// `read` may contain `?` for unrecognised letters.
pub fn names_match(name: &str, read: &str) -> bool {
    name.len() == read.len()
        && name
            .chars()
            .zip(read.chars())
            .all(|(a, b)| b == '?' || a == b)
}
