//! What the screen shows now, as last confirmed by perception: the page of
//! text, the open menu, the opponent in battle and the sprites on the
//! field. Unlike the rest of the state this is not memory of the game but
//! of the screen; it is replaced whole by `ViewObserved` whenever a stable
//! reading differs, and `diff` turns that into specific changes.

use serde::{Deserialize, Serialize};

use crate::{Direction, ShinyReading};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewState {
    /// The page in the message box or battle text box.
    pub text: Option<Vec<String>>,
    pub menu: Option<MenuView>,
    pub opponent: Option<OpponentView>,
    /// Object sprites on the field, by map tile.
    pub npcs: Vec<VisibleNpc>,
    /// The name in the map-name popup ("PEWTER CITY"): the game's own word
    /// on which named place the player just entered.
    pub map_popup: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MenuView {
    pub rows: Vec<String>,
    pub cursor: u8,
}

/// The opposing Pokémon on screen.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpponentView {
    /// Species constant, when the HUD name resolved to one.
    pub species: Option<String>,
    /// The HUD name as read.
    pub name: String,
    pub level: Option<u8>,
    /// HP bar fill, per mille.
    pub hp: Option<u16>,
    /// The caught-ball icon beside its name.
    pub caught: Option<bool>,
    pub shiny: Option<ShinyReading>,
}

/// A sprite standing on a map tile, and the NPC it was matched to (the
/// object's `local_id` in the map data) when one could be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleNpc {
    pub map: String,
    pub x: i32,
    pub y: i32,
    pub local_id: Option<u32>,
    pub facing: Option<Direction>,
}

impl VisibleNpc {
    /// The same identified NPC.
    pub fn same_as(&self, other: &VisibleNpc) -> bool {
        self.local_id.is_some() && self.local_id == other.local_id && self.map == other.map
    }

    /// On the same tile or one step away, on the same map.
    pub fn near(&self, other: &VisibleNpc) -> bool {
        self.map == other.map && (self.x - other.x).abs() + (self.y - other.y).abs() <= 1
    }
}
