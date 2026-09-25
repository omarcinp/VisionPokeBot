//! What the bot believes about the world beyond the party and the bag: the
//! game's event flags and variables, the maps it has visited, where a
//! blackout lands, and where the NPCs it has looked at were. Absent entries
//! are unknown; every present entry carries provenance.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Direction, Knowledge};

/// How many script paths the belief remembers running (oldest dropped).
pub const PATHS_RUN_KEPT: usize = 256;

/// Where a blackout or Teleport lands the player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealSpot {
    pub map: String,
    pub x: i32,
    pub y: i32,
}

/// An NPC (object event) of a map, by its `local_id` in the map data.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NpcBelief {
    /// Tile last seen at.
    pub pos: Knowledge<(i32, i32)>,
    pub facing: Knowledge<Direction>,
    /// Whether it was there when its tile was looked at (`false` means a
    /// `FLAG_HIDE_*`-style flag is set, or it walked away).
    pub present: Knowledge<bool>,
}

/// A fact the planner can ask the belief about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Fact {
    Flag { flag: String },
    Var { var: String },
    Visited { visited: String },
}

impl Fact {
    pub fn flag(name: impl Into<String>) -> Fact {
        Fact::Flag { flag: name.into() }
    }

    pub fn var(name: impl Into<String>) -> Fact {
        Fact::Var { var: name.into() }
    }

    pub fn visited(map: impl Into<String>) -> Fact {
        Fact::Visited {
            visited: map.into(),
        }
    }
}

impl std::fmt::Display for Fact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fact::Flag { flag } => write!(f, "flag {flag}"),
            Fact::Var { var } => write!(f, "var {var}"),
            Fact::Visited { visited } => write!(f, "visited {visited}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldBelief {
    /// Decomp flag names (`FLAG_BADGE01_GET`); absent = unknown.
    pub flags: BTreeMap<String, Knowledge<bool>>,
    /// Decomp var names (`VAR_MAP_SCENE_PALLET_TOWN_SIGN_LADY`).
    pub vars: BTreeMap<String, Knowledge<u16>>,
    /// Maps by name as in `data/world/index.json`. Kept apart from the
    /// `FLAG_WORLD_MAP_*` flags because Fly is decided on it.
    pub visited: BTreeMap<String, Knowledge<bool>>,
    pub respawn: Knowledge<HealSpot>,
    /// map → local_id → NPC (nested rather than keyed by a pair so that the
    /// JSON stays a plain object).
    pub npcs: BTreeMap<String, BTreeMap<u32, NpcBelief>>,
    /// Intents that failed this session and should not be planned again
    /// until the belief is reset by a checkpoint. Left out of `state.json`
    /// (see `GameState::saved_knowledge`).
    pub infeasible: BTreeSet<String>,
    /// (script label, path index) of the compiled paths run to completion,
    /// oldest first, bounded to [`PATHS_RUN_KEPT`].
    pub paths_run: Vec<(String, usize)>,
}

impl WorldBelief {
    /// `unknown()` when the flag was never observed or tracked.
    pub fn flag(&self, name: &str) -> Knowledge<bool> {
        self.flags.get(name).cloned().unwrap_or_default()
    }

    pub fn var(&self, name: &str) -> Knowledge<u16> {
        self.vars.get(name).cloned().unwrap_or_default()
    }

    pub fn visited(&self, map: &str) -> Knowledge<bool> {
        self.visited.get(map).cloned().unwrap_or_default()
    }

    pub fn npc(&self, map: &str, local_id: u32) -> Option<&NpcBelief> {
        self.npcs.get(map)?.get(&local_id)
    }

    pub fn npc_mut(&mut self, map: &str, local_id: u32) -> &mut NpcBelief {
        self.npcs
            .entry(map.to_owned())
            .or_default()
            .entry(local_id)
            .or_default()
    }

    /// Whether `fact` has any value at all (of any provenance).
    pub fn knows(&self, fact: &Fact) -> bool {
        match fact {
            Fact::Flag { flag } => self.flags.get(flag).is_some_and(|k| k.value.is_some()),
            Fact::Var { var } => self.vars.get(var).is_some_and(|k| k.value.is_some()),
            Fact::Visited { visited } => {
                self.visited.get(visited).is_some_and(|k| k.value.is_some())
            }
        }
    }

    /// The facts among `facts` that are unknown, in the given order: what
    /// the planner turns into probes.
    pub fn needs(&self, facts: &[Fact]) -> Vec<Fact> {
        facts.iter().filter(|f| !self.knows(f)).cloned().collect()
    }

    /// Records a compiled path run to completion, forgetting the oldest
    /// beyond [`PATHS_RUN_KEPT`].
    pub fn record_path(&mut self, script: &str, path: usize) {
        self.paths_run.push((script.to_owned(), path));
        if self.paths_run.len() > PATHS_RUN_KEPT {
            let extra = self.paths_run.len() - PATHS_RUN_KEPT;
            self.paths_run.drain(..extra);
        }
    }
}

/// `tracked(value)` over `current`, unless `current` is an observation of
/// the same value (an observation outranks a tracked change to what it
/// already shows).
pub(crate) fn track<T: PartialEq>(
    current: Option<&Knowledge<T>>,
    value: T,
) -> Option<Knowledge<T>> {
    match current {
        Some(k)
            if k.source == crate::KnowledgeSource::Observed && k.value.as_ref() == Some(&value) =>
        {
            None
        }
        Some(k) => Some(Knowledge::tracked(value, k.last_verified_frame)),
        None => Some(Knowledge::tracked(value, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GameState, KnowledgeSource, SavedKnowledge};

    #[test]
    fn new_belief_knows_nothing() {
        let b = WorldBelief::default();
        assert_eq!(b.flag("FLAG_BADGE01_GET"), Knowledge::unknown());
        assert_eq!(b.var("VAR_X"), Knowledge::unknown());
        assert_eq!(b.visited("PalletTown"), Knowledge::unknown());
        assert_eq!(b.respawn, Knowledge::unknown());
        assert!(b.npc("PalletTown", 1).is_none());
        assert_eq!(GameState::default().world, b);
    }

    #[test]
    fn needs_lists_the_unknown_facts_in_order() {
        let mut b = WorldBelief::default();
        b.flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::tracked(true, None));
        b.visited
            .insert("PalletTown".into(), Knowledge::observed(true, 3));
        let facts = vec![
            Fact::visited("CeruleanCity"),
            Fact::flag("FLAG_BADGE01_GET"),
            Fact::var("VAR_X"),
            Fact::visited("PalletTown"),
        ];
        assert_eq!(
            b.needs(&facts),
            vec![Fact::visited("CeruleanCity"), Fact::var("VAR_X")]
        );
    }

    #[test]
    fn tracking_keeps_an_observation_of_the_same_value() {
        let observed = Knowledge::observed(true, 5);
        assert_eq!(track(Some(&observed), true), None);
        assert_eq!(
            track(Some(&observed), false),
            Some(Knowledge::tracked(false, Some(5)))
        );
        let derived = Knowledge::derived(true, 5);
        assert_eq!(
            track(Some(&derived), true),
            Some(Knowledge::tracked(true, Some(5)))
        );
        assert_eq!(track(None, true), Some(Knowledge::tracked(true, None)));
    }

    #[test]
    fn paths_run_is_bounded() {
        let mut b = WorldBelief::default();
        for i in 0..(PATHS_RUN_KEPT + 10) {
            b.record_path("S", i);
        }
        assert_eq!(b.paths_run.len(), PATHS_RUN_KEPT);
        assert_eq!(b.paths_run[0], ("S".to_owned(), 10));
    }

    #[test]
    fn facts_serialize_by_kind() {
        let f: Vec<Fact> =
            serde_json::from_str(r#"[{"flag":"FLAG_A"},{"var":"VAR_B"},{"visited":"PalletTown"}]"#)
                .unwrap();
        assert_eq!(
            f,
            vec![
                Fact::flag("FLAG_A"),
                Fact::var("VAR_B"),
                Fact::visited("PalletTown")
            ]
        );
        assert_eq!(f[0].to_string(), "flag FLAG_A");
    }

    #[test]
    fn belief_round_trips_through_saved_knowledge_without_session_state() {
        let mut s = GameState::default();
        s.world
            .flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::observed(true, 9));
        s.world.respawn = Knowledge::observed(
            HealSpot {
                map: "PewterCity_PokemonCenter_1F".into(),
                x: 7,
                y: 4,
            },
            9,
        );
        let npc = s.world.npc_mut("PalletTown", 1);
        npc.pos = Knowledge::observed((4, 8), 9);
        npc.facing = Knowledge::observed(Direction::Down, 9);
        npc.present = Knowledge::observed(true, 9);
        s.world.infeasible.insert("Fly".into());
        s.world.record_path("PalletTown_EventScript_SignLady", 1);
        let json = serde_json::to_string(&s.saved_knowledge()).unwrap();
        assert!(!json.contains("Fly"), "session-scoped: {json}");
        let back: SavedKnowledge = serde_json::from_str(&json).unwrap();
        assert!(back.world.infeasible.is_empty(), "session-scoped");
        let mut expected = s.world.clone();
        expected.infeasible.clear();
        assert_eq!(back.world, expected);
        assert_eq!(
            back.world.flag("FLAG_BADGE01_GET").source,
            KnowledgeSource::Observed
        );
        // Files written before the belief existed still load.
        let old: SavedKnowledge = serde_json::from_str("{}").unwrap();
        assert_eq!(old.world, WorldBelief::default());
    }
}
