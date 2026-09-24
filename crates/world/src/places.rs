//! Named places (`data/world/places.json`): heal spots, Fly destinations,
//! marts, field-move gates and water per map.

use std::collections::BTreeMap;
use std::path::Path;

use pokebot_core::Result;
use serde::Deserialize;

use crate::events::load_json;

#[derive(Debug, Clone, Deserialize)]
pub struct HealSpot {
    /// `HEAL_LOCATION_*`.
    pub id: String,
    pub map: String,
    pub x: i32,
    pub y: i32,
    /// The Pokémon Center (or home) the player wakes up in after a white-out.
    pub respawn_map: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FlySpot {
    /// `FLAG_WORLD_MAP_*`: set once the place has been visited.
    pub flag: String,
    pub map: String,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Requirement {
    /// `MOVE_CUT`, `MOVE_STRENGTH`, `MOVE_ROCK_SMASH`.
    pub r#move: String,
    /// The badge flag that allows the move outside battle.
    pub badge: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Gate {
    pub map: String,
    pub x: i32,
    pub y: i32,
    pub local_id: u32,
    /// `cut_tree`, `rock_smash` or `boulder`.
    pub kind: String,
    pub requires: Requirement,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Places {
    pub rom: String,
    #[serde(default)]
    pub sha1: String,
    pub heal_spots: Vec<HealSpot>,
    pub fly_spots: Vec<FlySpot>,
    /// Map → items sold.
    pub marts: BTreeMap<String, Vec<String>>,
    pub gates: Vec<Gate>,
    /// Map → number of water tiles (Surf regions are read from the map).
    pub water: BTreeMap<String, u32>,
}

impl Places {
    /// Loads `dir/places.json`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Places> {
        load_json(&dir.as_ref().join("places.json"))
    }

    pub fn fly_spot(&self, flag: &str) -> Option<&FlySpot> {
        self.fly_spots.iter().find(|f| f.flag == flag)
    }

    pub fn heal_spot(&self, id: &str) -> Option<&HealSpot> {
        self.heal_spots.iter().find(|h| h.id == id)
    }
}
