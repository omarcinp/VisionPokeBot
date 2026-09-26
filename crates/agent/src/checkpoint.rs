//! The knowledge that goes with a save file (`state.json`, beside
//! `progress.json`), written after every in-game save and restored whenever
//! that save is loaded. It carries the identity of the `progress.json` it was
//! written with, so a `state.json` left over from another save is never
//! trusted.

use std::path::{Path, PathBuf};

use pokebot_core::{Error, Result};
use pokebot_gamedata::GameData;
use pokebot_state::{Knowledge, MoveSlot, PartyMon, PlayerPose, SavedKnowledge};
use serde::{Deserialize, Serialize};

use crate::party::Party;
use crate::Progress;

pub fn path_for(progress: &Path) -> PathBuf {
    progress.with_file_name("state.json")
}

/// What ties a `state.json` to its `progress.json`: the milestones done and
/// where the game was saved, both written by the same checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub milestones: Vec<String>,
    pub saved_at: Option<PlayerPose>,
}

impl Identity {
    pub fn of(progress: &Progress) -> Identity {
        Identity {
            milestones: progress.milestones.clone(),
            saved_at: progress.saved_at.clone(),
        }
    }
}

/// The contents of `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Absent in files written before identities existed.
    #[serde(default)]
    pub identity: Option<Identity>,
    #[serde(flatten)]
    pub knowledge: SavedKnowledge,
}

/// Writes `path` atomically (a temporary file renamed over it), so a crash
/// never leaves half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).map_err(|e| Error::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
}

pub fn store(path: &Path, identity: &Identity, knowledge: &SavedKnowledge) -> Result<()> {
    let checkpoint = Checkpoint {
        identity: Some(identity.clone()),
        knowledge: knowledge.clone(),
    };
    let json =
        serde_json::to_vec_pretty(&checkpoint).map_err(|e| Error::InvalidData(e.to_string()))?;
    write_atomic(path, &json)
}

pub fn load(path: &Path) -> Result<Option<Checkpoint>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let mut checkpoint: Checkpoint = serde_json::from_str(&text)
        .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
    // Old Switch captures could OCR `22` as `2`. A checkpoint must not
    // preserve that impossible value as an observed fact on every restart.
    if let Some(party) = checkpoint.knowledge.party.value.as_mut() {
        for mon in party {
            if let (Some(level), Some(hp)) = (mon.level.value, mon.hp.value) {
                if !crate::party::plausible_hp_for(hp, level, mon.species.value.as_deref()) {
                    mon.hp = Knowledge::unknown();
                }
            }
        }
    }
    Ok(Some(checkpoint))
}

/// Knowledge from an older `progress.json` that only had a party: kept, but
/// as tracked (stale) knowledge. No party there means the party is unknown.
pub fn legacy_knowledge(data: &GameData, party: &Party) -> SavedKnowledge {
    if party.members.is_empty() {
        return SavedKnowledge::default();
    }
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
                    .filter(|hp| crate::party::plausible_hp_for(*hp, m.level, Some(&m.species)))
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
        party: Knowledge::tracked(mons, None),
        ..SavedKnowledge::default()
    }
}

/// The knowledge restored for a loaded save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub knowledge: SavedKnowledge,
    /// Where it came from, for the log.
    pub source: String,
    /// Why `state.json` was not used, when it exists but can't be trusted.
    pub warning: Option<String>,
}

/// The knowledge for a loaded save: `state.json` when it was written with
/// this `progress.json`; otherwise migrated from the legacy party in
/// `progress.json` (unknown when there is none).
pub fn restore(state_path: &Path, progress: &Progress, data: &GameData) -> Result<Restored> {
    let legacy = |warning: Option<String>| Restored {
        knowledge: legacy_knowledge(data, &progress.party),
        source: if progress.party.members.is_empty() {
            "nothing: no state.json for this save and no legacy party (party unknown)".into()
        } else {
            "the legacy party in progress.json (migrated as Tracked)".into()
        },
        warning,
    };
    let checkpoint = match load(state_path) {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(legacy(None)),
        Err(e) => return Ok(legacy(Some(format!("{e}: ignored")))),
    };
    let expected = Identity::of(progress);
    match &checkpoint.identity {
        Some(id) if *id == expected => Ok(Restored {
            knowledge: checkpoint.knowledge,
            source: format!("{}", state_path.display()),
            warning: None,
        }),
        Some(_) => Ok(legacy(Some(format!(
            "{} was written for another save (its milestones or save position differ from progress.json): ignored",
            state_path.display()
        )))),
        None => Ok(legacy(Some(format!(
            "{} has no identity (written before checkpoints were tied to progress.json): ignored",
            state_path.display()
        )))),
    }
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
                read_stats: None,
                ability: None,
                ivs: None,
                stats_evs: [0; 6],
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
        let dir = temp_dir("roundtrip");
        let path = path_for(&dir.join("progress.json"));
        assert_eq!(path, dir.join("state.json"));
        assert_eq!(load(&path).unwrap(), None);
        let mut k = SavedKnowledge::default();
        k.money = pokebot_state::Knowledge::observed(42, 7);
        let id = Identity::of(&progress(&["A"], 7));
        store(&path, &id, &k).unwrap();
        assert_eq!(
            load(&path).unwrap(),
            Some(Checkpoint {
                identity: Some(id),
                knowledge: k
            })
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn impossible_switch_hp_is_unknown_after_checkpoint_load() {
        let dir = temp_dir("bad-hp");
        let path = dir.join("state.json");
        let mut knowledge = SavedKnowledge::default();
        knowledge.party = Knowledge::observed(
            vec![PartyMon {
                level: Knowledge::observed(6, 10),
                hp: Knowledge::observed((3, 2), 10),
                ..PartyMon::default()
            }],
            10,
        );
        store(&path, &Identity::of(&progress(&["A"], 7)), &knowledge).unwrap();
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(
            loaded.knowledge.party.value.unwrap()[0].hp,
            Knowledge::unknown()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn progress(milestones: &[&str], x: i32) -> Progress {
        Progress {
            player_name: "RED".into(),
            rival_name: "GREEN".into(),
            gender: pokebot_state::Gender::Boy,
            starter: crate::Starter::Bulbasaur,
            milestones: milestones.iter().map(|m| m.to_string()).collect(),
            saved_at: Some(pokebot_state::PlayerPose {
                map: "PewterCity_PokemonCenter_1F".into(),
                x,
                y: 4,
            }),
            party: Party::default(),
        }
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pokebot-ckpt-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn checkpoint_for_another_save_falls_back_to_legacy() {
        let Some(d) = data() else { return };
        let dir = temp_dir("mismatch");
        let path = dir.join("state.json");
        let mut k = SavedKnowledge::default();
        k.money = Knowledge::observed(42, 7);
        store(&path, &Identity::of(&progress(&["A", "B"], 7)), &k).unwrap();
        // Same save: restored as stored.
        let same = restore(&path, &progress(&["A", "B"], 7), &d).unwrap();
        assert_eq!(same.knowledge, k);
        assert!(same.warning.is_none());
        // Another save (fewer milestones): the legacy migration applies.
        let other = restore(&path, &progress(&["A"], 7), &d).unwrap();
        assert_eq!(other.knowledge.money, Knowledge::unknown());
        assert!(other.warning.is_some());
        // Same milestones, saved elsewhere.
        assert!(restore(&path, &progress(&["A", "B"], 8), &d)
            .unwrap()
            .warning
            .is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checkpoint_without_identity_is_not_trusted() {
        let Some(d) = data() else { return };
        let dir = temp_dir("noid");
        let path = dir.join("state.json");
        let mut k = SavedKnowledge::default();
        k.money = Knowledge::observed(42, 7);
        // The format before identities: the knowledge alone.
        std::fs::write(&path, serde_json::to_vec(&k).unwrap()).unwrap();
        assert_eq!(load(&path).unwrap().unwrap().identity, None);
        let r = restore(&path, &progress(&["A"], 7), &d).unwrap();
        assert_eq!(r.knowledge.money, Knowledge::unknown());
        assert!(r.warning.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_legacy_party_is_unknown() {
        let Some(d) = data() else { return };
        let k = legacy_knowledge(&d, &Party::default());
        assert_eq!(k.party, Knowledge::unknown());
    }

    #[test]
    fn world_belief_round_trips_through_state_json() {
        let dir = temp_dir("belief");
        let path = dir.join("state.json");
        let mut k = SavedKnowledge::default();
        k.world
            .flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::observed(true, 7));
        k.world
            .visited
            .insert("PewterCity".into(), Knowledge::observed(true, 7));
        let id = Identity::of(&progress(&["A"], 7));
        store(&path, &id, &k).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert_eq!(back.knowledge.world, k.world);
        assert_eq!(
            back.knowledge.world.flag("FLAG_BADGE01_GET"),
            Knowledge::observed(true, 7)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_leaves_no_temp_file() {
        let dir = temp_dir("atomic");
        let path = dir.join("state.json");
        store(
            &path,
            &Identity::of(&progress(&["A"], 7)),
            &SavedKnowledge::default(),
        )
        .unwrap();
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("state.json")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
