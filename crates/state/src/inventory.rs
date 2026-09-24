//! Bag, money, PC storage and the Pokédex's caught/seen flags.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Knowledge, PartyMon};

/// PC boxes in FireRed.
pub const BOXES: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Pocket {
    Items,
    KeyItems,
    PokeBalls,
    TmCase,
    BerryPouch,
}

impl Pocket {
    pub const ALL: [Pocket; 5] = [
        Pocket::Items,
        Pocket::KeyItems,
        Pocket::PokeBalls,
        Pocket::TmCase,
        Pocket::BerryPouch,
    ];

    /// From the decompilation's `POCKET_*` name.
    pub fn from_decomp(name: &str) -> Option<Pocket> {
        Some(match name {
            "POCKET_ITEMS" => Pocket::Items,
            "POCKET_KEY_ITEMS" => Pocket::KeyItems,
            "POCKET_POKE_BALLS" => Pocket::PokeBalls,
            "POCKET_TM_CASE" => Pocket::TmCase,
            "POCKET_BERRY_POUCH" => Pocket::BerryPouch,
            _ => return None,
        })
    }
}

/// (item constant, count), in the pocket's display order.
pub type ItemList = Vec<(String, u16)>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bag {
    pub pockets: BTreeMap<Pocket, Knowledge<ItemList>>,
}

impl Default for Bag {
    fn default() -> Self {
        Self {
            pockets: Pocket::ALL
                .iter()
                .map(|p| (*p, Knowledge::unknown()))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxMon {
    /// 0..30 within the box.
    pub slot: u8,
    pub species: Knowledge<String>,
    pub level: Knowledge<u8>,
    pub nickname: Knowledge<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcStorage {
    pub boxes: Vec<Knowledge<Vec<BoxMon>>>,
    pub items: Knowledge<ItemList>,
}

impl Default for PcStorage {
    fn default() -> Self {
        Self {
            boxes: vec![Knowledge::unknown(); BOXES],
            items: Knowledge::unknown(),
        }
    }
}

/// The totals the Pokédex's own header shows; the Trainer Card shows the
/// caught total only (`seen` stays `None` until the Pokédex is read).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PokedexCounts {
    pub seen: Option<u16>,
    pub caught: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Pokedex {
    pub caught: BTreeMap<String, Knowledge<bool>>,
    pub seen: BTreeMap<String, Knowledge<bool>>,
    /// The totals as last read; the per-species maps are a lower bound
    /// when this is unknown. Missing in files written before it existed.
    #[serde(default)]
    pub counts: Knowledge<PokedexCounts>,
}

/// The knowledge that belongs to a save file: stored beside it after every
/// in-game save and restored when that save is loaded.
/// Missing fields load as unknown, so older files keep loading.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedKnowledge {
    pub party: Knowledge<Vec<PartyMon>>,
    pub bag: Bag,
    pub money: Knowledge<u32>,
    pub pc: PcStorage,
    pub pokedex: Pokedex,
    pub world: crate::WorldBelief,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GameState, Knowledge, KnowledgeSource};

    #[test]
    fn new_state_knows_nothing_about_inventory() {
        let s = GameState::default();
        assert_eq!(s.party, Knowledge::unknown());
        assert_eq!(s.money, Knowledge::unknown());
        assert_eq!(s.pc.boxes.len(), BOXES);
        assert!(s.pc.boxes.iter().all(|b| b.value.is_none()));
        assert!(s.bag.pockets.values().all(|p| p.value.is_none()));
        assert_eq!(s.bag.pockets.len(), 5);
    }

    #[test]
    fn staleness_comes_from_provenance() {
        assert!(Knowledge::<u8>::unknown().needs_audit());
        assert!(!Knowledge::observed(3u8, 1).needs_audit());
        assert!(Knowledge::tracked(3u8, Some(1)).is_stale());
        assert!(!Knowledge::derived(3u8, 1).is_stale());
        assert_eq!(
            Knowledge::tracked(3u8, Some(1)).source,
            KnowledgeSource::Tracked
        );
    }

    #[test]
    fn saved_knowledge_round_trips() {
        let s = GameState {
            money: Knowledge::observed(4600, 10),
            ..Default::default()
        };
        let saved = s.saved_knowledge();
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(
            serde_json::from_str::<SavedKnowledge>(&json).unwrap(),
            saved
        );
    }

    #[test]
    fn saved_knowledge_with_missing_fields_loads() {
        let saved: SavedKnowledge = serde_json::from_str(
            r#"{"money":{"value":5,"source":"Observed","last_verified_frame":1}}"#,
        )
        .unwrap();
        assert_eq!(saved.money.value, Some(5));
        assert_eq!(saved.party, Knowledge::unknown());
        assert_eq!(saved.pc, PcStorage::default());
    }
}
