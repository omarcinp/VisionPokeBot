//! What the bot knows about each party Pokémon. Every field has its own
//! provenance; names are decompilation constants.

use serde::{Deserialize, Serialize};

use crate::Knowledge;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Healthy,
    Poisoned,
    BadlyPoisoned,
    Burned,
    Paralyzed,
    Asleep,
    Frozen,
    Fainted,
}

/// One move slot: the move and its (current, maximum) PP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveSlot {
    pub mv: Knowledge<String>,
    pub pp: Knowledge<(u8, u8)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PartyMon {
    pub species: Knowledge<String>,
    pub nickname: Knowledge<String>,
    pub level: Knowledge<u8>,
    /// (current, maximum)
    pub hp: Knowledge<(u16, u16)>,
    pub status: Knowledge<Status>,
    /// Menu order; `None` = no move in that slot (or not known to exist).
    pub moves: [Option<MoveSlot>; 4],
    pub held_item: Knowledge<Option<String>>,
    pub shiny: Knowledge<bool>,
}
