//! FireRed game mechanics data (extracted offline from pret/pokefirered by
//! `tools/gamedata/extract_gamedata.py`) and the Generation III formulas that
//! use it. Names are the decompilation's constants (`SPECIES_BULBASAUR`,
//! `MOVE_VINE_WHIP`, `TYPE_GRASS`, ...).

pub mod mechanics;

use std::collections::HashMap;
use std::path::Path;

use pokebot_core::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Species {
    /// HP, Attack, Defense, Speed, Sp. Atk, Sp. Def.
    pub base: [u16; 6],
    pub types: Vec<String>,
    pub catch_rate: u16,
    pub exp_yield: u16,
    pub ev_yield: [u8; 6],
    pub growth_rate: String,
    pub abilities: Vec<String>,
    /// (level, move) in learning order.
    pub learnset: Vec<(u8, String)>,
    /// (method, parameter, target species).
    pub evolutions: Vec<(String, serde_json::Value, String)>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Move {
    pub effect: Option<String>,
    pub power: u16,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub accuracy: u16,
    pub pp: u8,
    pub priority: i8,
    pub secondary_chance: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrainerMon {
    pub species: String,
    pub level: u8,
    /// 0–255 scale (the game maps it onto 0–31 IVs).
    pub iv: u16,
    pub item: Option<String>,
    /// Custom moves, or `None` for the species' last four level-up moves.
    pub moves: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Trainer {
    pub class: Option<String>,
    pub name: String,
    pub double: bool,
    pub items: Vec<String>,
    pub party: Vec<TrainerMon>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MapTrainer {
    pub local_id: u32,
    pub trainer: String,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub sight: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EncounterSlot {
    pub species: String,
    pub min_level: u8,
    pub max_level: u8,
    /// Percent chance of this slot.
    pub chance: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EncounterTable {
    /// Per-step encounter rate (the game rolls rate×16 out of 2880).
    pub rate: u16,
    pub slots: Vec<EncounterSlot>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    pub price: u32,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct GameData {
    pub species: HashMap<String, Species>,
    pub moves: HashMap<String, Move>,
    /// (attacking type, defending type, multiplier ×10).
    pub type_chart: Vec<(String, String, u8)>,
    pub trainers: HashMap<String, Trainer>,
    /// Map name → trainer NPCs on it.
    pub map_trainers: HashMap<String, Vec<MapTrainer>>,
    /// Map name → encounter tables by kind (`land`, `water`, `fishing`, ...).
    pub wild: HashMap<String, HashMap<String, EncounterTable>>,
    /// Map name → items sold there.
    pub marts: HashMap<String, Vec<String>>,
    pub items: HashMap<String, Item>,
}

impl GameData {
    pub fn load(path: impl AsRef<Path>) -> Result<GameData> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        serde_json::from_str(&text)
            .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))
    }

    pub fn species(&self, name: &str) -> Option<&Species> {
        self.species.get(name)
    }

    pub fn move_(&self, name: &str) -> Option<&Move> {
        self.moves.get(name)
    }

    /// Damage multiplier ×10 for `attack` against a defender of `defender`
    /// types (10 = neutral, 0 = immune, 40 = double super effective).
    pub fn effectiveness(&self, attack: &str, defender: &[String]) -> u32 {
        defender.iter().fold(10, |acc, d| {
            let m = self
                .type_chart
                .iter()
                .find(|(a, t, _)| a == attack && t == d)
                .map_or(10, |(_, _, m)| u32::from(*m));
            acc * m / 10
        })
    }

    /// Moves a species knows at `level` when nothing else taught it: the
    /// last four learned by level-up (how the game fills wild and default
    /// trainer movesets).
    pub fn default_moves(&self, species: &str, level: u8) -> Vec<String> {
        let mut moves: Vec<String> = Vec::new();
        for (lvl, m) in self
            .species(species)
            .map(|s| s.learnset.as_slice())
            .unwrap_or(&[])
        {
            if *lvl > level {
                break;
            }
            if !moves.contains(m) {
                if moves.len() == 4 {
                    moves.remove(0);
                }
                moves.push(m.clone());
            }
        }
        moves
    }

    /// The species `species` becomes by `level` through level evolutions.
    pub fn evolved_at(&self, species: &str, level: u8) -> String {
        let mut current = species.to_owned();
        while let Some((_, _, target)) = self.species(&current).and_then(|s| {
            s.evolutions.iter().find(|(method, param, _)| {
                method == "EVO_LEVEL" && param.as_u64().is_some_and(|p| p <= u64::from(level))
            })
        }) {
            current = target.clone();
        }
        current
    }
}
