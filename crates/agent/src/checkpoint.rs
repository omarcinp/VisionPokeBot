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

/// Corrects the checkpoint at `path` with what the screen showed wrong in
/// it: each `ScriptPathRetracted` of a path the checkpoint itself records
/// (so recorded before the save), with the facts observed on the same
/// frame (the var a trigger fired on, its trainer not beaten). The save is
/// what every reload restores; a belief the game contradicted would be
/// restored again after the faint it caused (Switch: Cerulean's rival held
/// beaten, met unprepared on every cycle). `true` when it was rewritten.
pub fn correct(path: &Path, events: &[pokebot_state::EventRecord]) -> Result<bool> {
    use pokebot_state::{DefaultReducer, EventRecord, GameEvent, GameState, StateReducer};
    let Some(Checkpoint {
        identity: Some(identity),
        knowledge,
    }) = load(path)?
    else {
        return Ok(false);
    };
    let recorded = |script: &str, path: usize| {
        knowledge
            .world
            .paths_run
            .iter()
            .any(|(s, p)| s == script && *p == path)
    };
    let frames: Vec<u64> = events
        .iter()
        .filter(|r| {
            matches!(&r.event, GameEvent::ScriptPathRetracted { script, path, .. }
                if recorded(script, *path))
        })
        .map(|r| r.frame_id)
        .collect();
    let apply: Vec<EventRecord> = events
        .iter()
        .filter(|r| frames.contains(&r.frame_id))
        .filter(|r| match &r.event {
            GameEvent::ScriptPathRetracted { script, path, .. } => recorded(script, *path),
            GameEvent::VarObserved { .. } | GameEvent::FlagObserved { .. } => true,
            _ => false,
        })
        .cloned()
        .collect();
    if apply.is_empty() {
        return Ok(false);
    }
    let restored = DefaultReducer.reduce(
        &GameState::default(),
        &[EventRecord {
            frame_id: 0,
            event: GameEvent::CheckpointRestored {
                knowledge: Box::new(knowledge),
            },
        }],
    );
    let corrected = DefaultReducer.reduce(&restored, &apply);
    store(path, &identity, &corrected.saved_knowledge())?;
    Ok(true)
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

    /// Where Dig and the Escape Rope lead is part of the save: kept by the
    /// checkpoint, and unknown in one written before it was tracked.
    #[test]
    fn the_escape_warp_round_trips_through_state_json() {
        let dir = temp_dir("escape");
        let path = dir.join("state.json");
        let mut k = SavedKnowledge::default();
        k.world.escape = Knowledge::observed(
            pokebot_state::EscapeWarp {
                map: "Route4".into(),
                x: 19,
                y: 6,
                entered: "MtMoon_1F".into(),
            },
            7,
        );
        let id = Identity::of(&progress(&["A"], 7));
        store(&path, &id, &k).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert_eq!(back.knowledge.world.escape, k.world.escape);
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        json["world"].as_object_mut().unwrap().remove("escape");
        std::fs::write(&path, json.to_string()).unwrap();
        let old = load(&path).unwrap().unwrap();
        assert_eq!(old.knowledge.world.escape, Knowledge::unknown());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Switch: a wild battle was taken for Cerulean's rival battle and
    /// saved. His trigger firing (the sensor's retraction, the var armed)
    /// corrects the checkpoint; a path the save doesn't record, or facts of
    /// other frames, don't touch it.
    #[test]
    fn a_contradicted_path_is_corrected_in_the_checkpoint() {
        use pokebot_state::{EventRecord, GameEvent};
        let dir = temp_dir("correct");
        let path = dir.join("state.json");
        let script = "CeruleanCity_EventScript_RivalTriggerLeft";
        let var = "VAR_MAP_SCENE_CERULEAN_CITY_RIVAL";
        let rival = "TRAINER_RIVAL_CERULEAN_CHARMANDER";
        let mut k = SavedKnowledge::default();
        k.world.record_path(script, 4);
        k.world
            .vars
            .insert(var.into(), Knowledge::tracked(1, Some(5)));
        k.world
            .flags
            .insert(rival.into(), Knowledge::tracked(true, Some(5)));
        let id = Identity::of(&progress(&["A"], 7));
        store(&path, &id, &k).unwrap();
        let at = |frame_id, event| EventRecord { frame_id, event };
        let unrelated = at(
            9,
            GameEvent::FlagObserved {
                flag: "FLAG_OTHER".into(),
                value: true,
            },
        );
        let not_saved = at(
            10,
            GameEvent::ScriptPathRetracted {
                script: "Other_EventScript_X".into(),
                path: 0,
                flags: Vec::new(),
                vars: Vec::new(),
            },
        );
        assert!(!correct(&path, &[unrelated.clone(), not_saved.clone()]).unwrap());
        let fired = [
            unrelated,
            not_saved,
            at(
                20,
                GameEvent::ScriptPathRetracted {
                    script: script.into(),
                    path: 4,
                    flags: vec![rival.into()],
                    vars: vec![var.into()],
                },
            ),
            at(
                20,
                GameEvent::FlagObserved {
                    flag: rival.into(),
                    value: false,
                },
            ),
            at(
                20,
                GameEvent::VarObserved {
                    var: var.into(),
                    value: 0,
                },
            ),
        ];
        assert!(correct(&path, &fired).unwrap());
        let c = load(&path).unwrap().unwrap();
        assert_eq!(c.identity, Some(id));
        assert!(c.knowledge.world.paths_run.is_empty());
        assert_eq!(c.knowledge.world.vars[var].value, Some(0));
        assert_eq!(c.knowledge.world.flags[rival].value, Some(false));
        assert!(!c.knowledge.world.flags.contains_key("FLAG_OTHER"));
        // Corrected once: the path is gone, nothing more to do.
        assert!(!correct(&path, &fired).unwrap());
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
