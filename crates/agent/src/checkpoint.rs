//! The knowledge that goes with a save file (`saves/state.json`), written
//! after every in-game save and restored whenever that save is loaded.

use std::path::{Path, PathBuf};

use pokebot_core::{Error, Result};
use pokebot_gamedata::GameData;
use pokebot_state::{Knowledge, KnowledgeSource, MoveSlot, PartyMon, SavedKnowledge};

use crate::party::Party;
use crate::Progress;

pub fn path_for(progress: &Path) -> PathBuf {
    progress.with_file_name("state.json")
}

pub fn store(path: &Path, knowledge: &SavedKnowledge) -> Result<()> {
    let json =
        serde_json::to_vec_pretty(knowledge).map_err(|e| Error::InvalidData(e.to_string()))?;
    std::fs::write(path, json).map_err(|e| Error::io(path, e))
}

pub fn load(path: &Path) -> Result<Option<SavedKnowledge>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))
}

/// Knowledge from an older `progress.json` that only had a party: kept, but
/// as tracked (stale) knowledge.
pub fn legacy_knowledge(data: &GameData, party: &Party) -> SavedKnowledge {
    let t = |v| Knowledge::tracked(v, None);
    let mons = party
        .members
        .iter()
        .map(|m| {
            let mut mon = PartyMon {
                species: t(m.species.clone()),
                level: Knowledge::tracked(m.level, None),
                hp: m
                    .hp
                    .map_or_else(Knowledge::unknown, |hp| Knowledge::tracked(hp, None)),
                ..PartyMon::default()
            };
            for (i, mv) in m.moves.iter().enumerate().take(4) {
                let max = data.move_(mv).map_or(0, |x| x.pp);
                let used = *m.pp_used.get(mv).unwrap_or(&0);
                mon.moves[i] = Some(MoveSlot {
                    mv: t(mv.clone()),
                    pp: Knowledge::tracked((max.saturating_sub(used), max), None),
                });
            }
            mon
        })
        .collect();
    SavedKnowledge {
        party: Knowledge {
            value: Some(mons),
            source: KnowledgeSource::Tracked,
            last_verified_frame: None,
        },
        ..SavedKnowledge::default()
    }
}

/// The knowledge for a loaded save: `state.json`, or migrated from the
/// legacy party in `progress.json`.
pub fn restore(state_path: &Path, progress: &Progress, data: &GameData) -> Result<SavedKnowledge> {
    Ok(match load(state_path)? {
        Some(k) => k,
        None => legacy_knowledge(data, &progress.party),
    })
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::party::Member;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    #[test]
    fn legacy_party_migrates() {
        let Some(d) = data() else { return };
        let party = Party {
            members: vec![Member {
                slot: 0,
                species: "SPECIES_IVYSAUR".into(),
                level: 18,
                moves: [
                    "MOVE_TACKLE",
                    "MOVE_SLEEP_POWDER",
                    "MOVE_LEECH_SEED",
                    "MOVE_VINE_WHIP",
                ]
                .map(String::from)
                .to_vec(),
                hp: Some((54, 54)),
                pp_used: [("MOVE_VINE_WHIP".to_owned(), 5)].into_iter().collect(),
            }],
        };
        let k = legacy_knowledge(&d, &party);
        let mon = &k.party.value.as_ref().unwrap()[0];
        assert_eq!(mon.species.value.as_deref(), Some("SPECIES_IVYSAUR"));
        assert!(
            mon.species.is_stale(),
            "legacy knowledge is tracked, not observed"
        );
        assert_eq!(mon.moves[3].as_ref().unwrap().pp.value, Some((5, 10)));
        assert_eq!(
            Party::from_state(&{
                let mut s = pokebot_state::GameState::default();
                s.party = k.party.clone();
                s
            })
            .lead()
            .unwrap()
            .moves,
            party.members[0].moves
        );
    }

    #[test]
    fn store_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("pokebot-ckpt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = path_for(&dir.join("progress.json"));
        assert_eq!(path, dir.join("state.json"));
        assert_eq!(load(&path).unwrap(), None);
        let mut k = SavedKnowledge::default();
        k.money = pokebot_state::Knowledge::observed(42, 7);
        store(&path, &k).unwrap();
        assert_eq!(load(&path).unwrap(), Some(k));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
