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
    #[serde(default)]
    pub details: PokemonDetails,
    /// Last 512 changes read on the summary pages; saved with the member.
    #[serde(default)]
    pub history: Vec<PartyDetailChange>,
}

/// Fields visible on the Info and Skills pages. Missing OCR is not a zero.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SummaryDetails {
    pub trainer_id: Option<String>,
    pub original_trainer: Option<String>,
    pub attack: Option<u16>,
    pub defense: Option<u16>,
    pub sp_attack: Option<u16>,
    pub sp_defense: Option<u16>,
    pub speed: Option<u16>,
    pub exp_points: Option<u32>,
    pub next_level: Option<u32>,
    pub ability: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PokemonDetails {
    pub trainer_id: Knowledge<String>,
    pub original_trainer: Knowledge<String>,
    pub attack: Knowledge<u16>,
    pub defense: Knowledge<u16>,
    pub sp_attack: Knowledge<u16>,
    pub sp_defense: Knowledge<u16>,
    pub speed: Knowledge<u16>,
    pub exp_points: Knowledge<u32>,
    pub next_level: Knowledge<u32>,
    pub ability: Knowledge<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyDetailChange {
    pub frame_id: u64,
    pub level: Option<u8>,
    pub field: String,
    pub from: Option<String>,
    pub to: String,
}

impl PokemonDetails {
    pub fn reading(&self) -> SummaryDetails {
        SummaryDetails {
            trainer_id: self.trainer_id.value.clone(),
            original_trainer: self.original_trainer.value.clone(),
            attack: self.attack.value,
            defense: self.defense.value,
            sp_attack: self.sp_attack.value,
            sp_defense: self.sp_defense.value,
            speed: self.speed.value,
            exp_points: self.exp_points.value,
            next_level: self.next_level.value,
            ability: self.ability.value.clone(),
        }
    }
}

impl SummaryDetails {
    pub fn values(&self) -> Vec<(&'static str, Option<String>)> {
        vec![
            ("trainer_id", self.trainer_id.clone()),
            ("original_trainer", self.original_trainer.clone()),
            ("attack", self.attack.map(|v| v.to_string())),
            ("defense", self.defense.map(|v| v.to_string())),
            ("sp_attack", self.sp_attack.map(|v| v.to_string())),
            ("sp_defense", self.sp_defense.map(|v| v.to_string())),
            ("speed", self.speed.map(|v| v.to_string())),
            ("exp_points", self.exp_points.map(|v| v.to_string())),
            ("next_level", self.next_level.map(|v| v.to_string())),
            ("ability", self.ability.clone()),
        ]
    }
}

impl PartyMon {
    pub fn observe_details(&mut self, reading: &SummaryDetails, frame: u64) {
        macro_rules! observe {
            ($field:ident) => {
                if let Some(value) = &reading.$field {
                    if self.details.$field.value.as_ref() != Some(value) {
                        self.history.push(PartyDetailChange {
                            frame_id: frame,
                            level: self.level.value,
                            field: stringify!($field).into(),
                            from: self.details.$field.value.as_ref().map(ToString::to_string),
                            to: value.to_string(),
                        });
                    }
                    self.details.$field = Knowledge::observed(value.clone(), frame);
                }
            };
        }
        observe!(trainer_id);
        observe!(original_trainer);
        observe!(attack);
        observe!(defense);
        observe!(sp_attack);
        observe!(sp_defense);
        observe!(speed);
        observe!(exp_points);
        observe!(next_level);
        observe!(ability);
        if self.history.len() > 512 {
            self.history.drain(..self.history.len() - 512);
        }
    }

    /// A conservative match across an audit/reorder. An OT's ID is shared
    /// by all their Pokémon, so it alone never identifies a member.
    pub(crate) fn matches_member(&self, other: &Self) -> bool {
        self.nickname.value.is_some()
            && self.nickname.value == other.nickname.value
            && self.species.value == other.species.value
            && [
                (&self.details.trainer_id, &other.details.trainer_id),
                (
                    &self.details.original_trainer,
                    &other.details.original_trainer,
                ),
            ]
            .iter()
            .all(|(a, b)| a.value.is_none() || b.value.is_none() || a.value == b.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefaultReducer, EventRecord, GameEvent, GameState, StateReducer};
    fn mon(name: &str) -> PartyMon {
        PartyMon {
            nickname: Knowledge::observed(name.into(), 1),
            species: Knowledge::observed("SPECIES_PIDGEY".into(), 1),
            level: Knowledge::observed(10, 1),
            ..Default::default()
        }
    }
    fn apply(s: &GameState, event: GameEvent) -> GameState {
        DefaultReducer.reduce(
            s,
            &[EventRecord {
                frame_id: 50,
                event,
            }],
        )
    }
    #[test]
    fn partial_details_keep_known_values_and_record_changes_once() {
        let mut m = mon("BIRD");
        let reading = SummaryDetails {
            trainer_id: Some("00042".into()),
            original_trainer: Some("RED".into()),
            attack: Some(20),
            exp_points: Some(0),
            ..Default::default()
        };
        m.observe_details(&reading, 10);
        m.observe_details(&reading, 20);
        assert_eq!(m.history.len(), 4);
        m.observe_details(
            &SummaryDetails {
                attack: Some(23),
                next_level: Some(0),
                ..Default::default()
            },
            30,
        );
        assert_eq!(m.details.trainer_id.value.as_deref(), Some("00042"));
        assert_eq!(m.details.exp_points.value, Some(0));
        assert_eq!(m.history[4].from.as_deref(), Some("20"));
        assert_eq!(m.history[4].to, "23");
        assert_eq!(m.history[4].frame_id, 30);
        let state = GameState {
            party: Knowledge::observed(vec![m], 30),
            ..Default::default()
        };
        let checkpoint = serde_json::to_string(&state.saved_knowledge()).unwrap();
        let restored = apply(
            &GameState::default(),
            GameEvent::CheckpointRestored {
                knowledge: Box::new(serde_json::from_str(&checkpoint).unwrap()),
            },
        );
        assert_eq!(state.party, restored.party);
    }
    #[test]
    fn audit_reorders_history_with_the_member_and_keeps_unread_details() {
        let mut a = mon("BIRD");
        a.observe_details(
            &SummaryDetails {
                attack: Some(20),
                trainer_id: Some("00042".into()),
                ..Default::default()
            },
            10,
        );
        let mut b = mon("OTHER");
        b.observe_details(
            &SummaryDetails {
                defense: Some(30),
                ..Default::default()
            },
            11,
        );
        let state = GameState {
            party: Knowledge::observed(vec![a.clone(), b.clone()], 11),
            ..Default::default()
        };
        let mut new_a = mon("BIRD");
        new_a.observe_details(
            &SummaryDetails {
                attack: Some(25),
                ..Default::default()
            },
            40,
        );
        let updated = apply(
            &state,
            GameEvent::PartyAudited {
                members: vec![mon("OTHER"), new_a],
            },
        );
        let party = updated.party.value.unwrap();
        assert_eq!(party[0].history, b.history);
        assert_eq!(party[1].details.trainer_id, a.details.trainer_id);
        assert_eq!(party[1].history.len(), 3);
        assert_eq!(party[1].history[2].from.as_deref(), Some("20"));
        assert_eq!(party[1].history[2].to, "25");
        let mut different_ot = mon("BIRD");
        different_ot.observe_details(
            &SummaryDetails {
                trainer_id: Some("12345".into()),
                ..Default::default()
            },
            40,
        );
        let updated = apply(
            &state,
            GameEvent::PartyAudited {
                members: vec![different_ot],
            },
        );
        assert_eq!(updated.party.value.unwrap()[0].details.attack.value, None);
    }
    #[test]
    fn older_checkpoints_default_new_fields_to_unknown() {
        let mut value = serde_json::to_value(mon("BIRD")).unwrap();
        value.as_object_mut().unwrap().remove("details");
        value.as_object_mut().unwrap().remove("history");
        let old: PartyMon = serde_json::from_value(value).unwrap();
        assert!(old.details.attack.value.is_none());
        assert!(old.history.is_empty());
    }
}
