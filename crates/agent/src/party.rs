//! What the bot knows about its party, kept up to date from what it sees
//! (battle HUD: name, level, HP) and from game data (moves learned by level).

use std::collections::BTreeMap;

use pokebot_gamedata::mechanics::Stats;
use pokebot_gamedata::GameData;
use pokebot_state::{BattleMenu, BattleObservation, GameEvent, GameState, MoveSlot, PartyMon};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Party slot (0 = lead).
    #[serde(default)]
    pub slot: u8,
    pub species: String,
    pub level: u8,
    /// Moves in menu order (the order they were learned).
    pub moves: Vec<String>,
    /// Current and maximum HP, when last seen.
    pub hp: Option<(u16, u16)>,
    /// PP spent per move since the last full heal.
    #[serde(default)]
    pub pp_used: BTreeMap<String, u8>,
    /// Attack, Defense, Speed, Sp. Atk, Sp. Def as read on the summary's
    /// Skills page (perhaps at an earlier level: see [`Member::stats`]).
    #[serde(default)]
    pub read_stats: Option<[u16; 5]>,
    /// Ability as the summary prints it (`OVERGROW`).
    #[serde(default)]
    pub ability: Option<String>,
}

impl Member {
    /// A freshly obtained Pokémon at `level` with its default moves.
    pub fn new(data: &GameData, species: &str, level: u8) -> Self {
        Self {
            slot: 0,
            species: species.to_owned(),
            level,
            moves: data.default_moves(species, level),
            hp: None,
            pp_used: BTreeMap::new(),
            read_stats: None,
            ability: None,
        }
    }

    /// Battle stats (HP, Atk, Def, Spe, SpA, SpD) from the summary's
    /// reading and the maximum HP, when all are known and plausible at the
    /// current level: each within what IVs 0–31, any EVs and a nature
    /// allow. A reading from before a level-up falls out of the range and
    /// counts as unknown.
    pub fn stats(&self, data: &GameData) -> Option<Stats> {
        let read = self.read_stats?;
        let (_, max_hp) = self.hp?;
        let base = data.species(&self.species)?.base;
        let level = u32::from(self.level);
        // Order of `read`: Atk, Def, Spe, SpA, SpD (base indices 1..=5).
        let plausible = read.iter().enumerate().all(|(i, v)| {
            let b = u32::from(base[i + 1]);
            let low = ((2 * b) * level / 100 + 5) * 9 / 10;
            let high = ((2 * b + 31 + 63) * level / 100 + 5) * 11 / 10;
            (low..=high).contains(&u32::from(*v))
        });
        plausible.then(|| {
            let [a, d, s, sa, sd] = read.map(u32::from);
            Stats([u32::from(max_hp), a, d, s, sa, sd])
        })
    }

    /// Name as the game prints it (`SPECIES_BULBASAUR` → `BULBASAUR`).
    pub fn display_name(&self) -> String {
        display_name(&self.species)
    }

    pub fn pp_left(&self, data: &GameData, mv: &str) -> u8 {
        let max = data.move_(mv).map_or(0, |m| m.pp);
        max.saturating_sub(*self.pp_used.get(mv).unwrap_or(&0))
    }

    /// PP that wild battles may spend on damaging moves: moves with 30+ PP
    /// in full, others above the reserve kept for trainers.
    pub fn wild_attack_pp(&self, data: &GameData, reserve: u8) -> u32 {
        self.moves
            .iter()
            .filter(|m| data.move_(m).is_some_and(|mv| mv.power > 0))
            .map(|m| {
                let left = self.pp_left(data, m);
                let max = data.move_(m).map_or(0, |mv| mv.pp);
                u32::from(if max >= 30 {
                    left
                } else {
                    left.saturating_sub(reserve)
                })
            })
            .sum()
    }

    pub fn heal(&mut self) {
        self.hp = self.hp.map(|(_, max)| (max, max));
        self.pp_used.clear();
    }
}

pub fn display_name(species: &str) -> String {
    let name = species.trim_start_matches("SPECIES_");
    name.trim_end_matches("_M")
        .trim_end_matches("_F")
        .replace('_', " ")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    pub members: Vec<Member>,
}

impl Party {
    pub fn lead(&self) -> Option<&Member> {
        self.members.first()
    }

    /// The party as the state knows it (members with a known species).
    pub fn from_state(state: &GameState) -> Party {
        let members = state
            .party
            .value
            .iter()
            .flatten()
            .enumerate()
            .filter_map(|(slot, m)| {
                let species = m.species.value.clone()?;
                let moves: Vec<&MoveSlot> = m.moves.iter().flatten().collect();
                Some(Member {
                    slot: slot as u8,
                    species,
                    level: m.level.value.unwrap_or(0),
                    moves: moves
                        .iter()
                        .map(|s| s.mv.value.clone().unwrap_or_else(|| "?".into()))
                        .collect(),
                    hp: m.hp.value,
                    pp_used: moves
                        .iter()
                        .filter_map(|s| Some((s.mv.value.clone()?, s.pp.value?)))
                        .map(|(mv, (cur, max))| (mv, max.saturating_sub(cur)))
                        .collect(),
                    read_stats: (|| {
                        let d = &m.details;
                        Some([
                            d.attack.value?,
                            d.defense.value?,
                            d.speed.value?,
                            d.sp_attack.value?,
                            d.sp_defense.value?,
                        ])
                    })(),
                    ability: m.details.ability.value.clone(),
                })
            })
            .collect();
        Party { members }
    }
}

/// A newly obtained Pokémon, known from game data (default moves, full PP).
pub fn starter_mon(data: &GameData, species: &str, level: u8) -> PartyMon {
    pokebot_sense::obtained_mon(data, species, level)
}

/// Reject impossible HUD totals before they can overwrite the party belief.
/// Ordinary Pokémon have at least level + 10 HP; use `plausible_hp_for`
/// when species is known so Shedinja's one HP is handled correctly.
/// The Switch OCR has read `22` as `2`, including on a nearly fainted lead.
pub fn plausible_hp(read: (u16, u16), level: u8) -> bool {
    let (current, maximum) = read;
    current <= maximum && maximum >= u16::from(level) + 10
}

/// Species-aware validation, including Shedinja's fixed one HP.
pub fn plausible_hp_for(read: (u16, u16), level: u8, species: Option<&str>) -> bool {
    plausible_hp(read, level) || (species == Some("SPECIES_SHEDINJA") && read.1 == 1 && read.0 <= 1)
}

/// Events for what the battle HUD and move menu show that the party view
/// doesn't know yet. Nothing is emitted for unchanged or ambiguous readings.
pub fn battle_events(data: &GameData, party: &Party, battle: &BattleObservation) -> Vec<GameEvent> {
    let mut events = Vec::new();
    let Some(name) = &battle.player_name else {
        return events;
    };
    let Some((member, species)) = party
        .members
        .iter()
        .find_map(|m| seen_as(data, &m.species, name).map(|s| (m, s)))
    else {
        // A species the knowledge can't explain (e.g. restored from another
        // save): with a single member it can only be that one, so the HUD
        // corrects it.
        if let ([member], Some(species)) = (party.members.as_slice(), data.species_named(name)) {
            events.push(GameEvent::PartyObserved {
                slot: member.slot,
                species: Some(species.to_owned()),
                nickname: None,
                level: battle.player_level,
                hp: battle.player_hp_numbers.filter(|hp| {
                    plausible_hp_for(
                        *hp,
                        battle.player_level.unwrap_or(member.level),
                        Some(&member.species),
                    )
                }),
                status: None,
                held_item: None,
            });
        }
        return events;
    };
    let slot = member.slot;
    if species != member.species {
        events.push(GameEvent::Evolved { slot, species });
    }
    let level = battle.player_level.filter(|l| *l != member.level);
    let hp = battle.player_hp_numbers.filter(|hp| {
        plausible_hp_for(
            *hp,
            battle.player_level.unwrap_or(member.level),
            Some(&member.species),
        ) && Some(*hp) != member.hp
    });
    if level.is_some() || hp.is_some() {
        events.push(GameEvent::PartyObserved {
            slot,
            species: None,
            nickname: None,
            level,
            hp,
            status: None,
            held_item: None,
        });
    }
    if let Some(BattleMenu::Moves { column, row }) = battle.menu {
        let names: Option<Vec<String>> = battle
            .move_names
            .iter()
            .filter(|n| !n.is_empty() && n.as_str() != "-")
            .map(|n| data.move_named(n).map(str::to_owned))
            .collect();
        let mut moves = member.moves.clone();
        if let Some(names) = names.filter(|n| !n.is_empty() && *n != member.moves) {
            events.push(GameEvent::MovesObserved {
                slot,
                moves: names.clone(),
            });
            moves = names;
        }
        let at = usize::from(row * 2 + column);
        if let (Some((cur, max)), Some(mv)) = (battle.move_pp, moves.get(at)) {
            let known = member.moves.get(at) == Some(mv)
                && data.move_(mv).is_some_and(|m| m.pp == max)
                && member.pp_left(data, mv) == cur;
            if !known {
                events.push(GameEvent::MovePpObserved {
                    slot,
                    move_slot: at as u8,
                    cur,
                    max,
                });
            }
        }
    }
    events
}

/// `species` itself or the evolution of it whose name matches `read`.
fn seen_as(data: &GameData, species: &str, read: &str) -> Option<String> {
    let mut frontier = vec![species.to_owned()];
    while !frontier.is_empty() {
        let current = frontier.remove(0);
        if names_match(&display_name(&current), read) {
            return Some(current);
        }
        frontier.extend(data.evolutions_of(&current));
    }
    None
}

/// `read` may contain `?` for unrecognised letters.
pub fn names_match(name: &str, read: &str) -> bool {
    name.len() == read.len()
        && name
            .chars()
            .zip(read.chars())
            .all(|(a, b)| b == '?' || a == b)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pokebot_state::{
        BattleMenu, BattleObservation, DefaultReducer, EventRecord, GameState, StateReducer,
    };

    use super::*;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    fn state_with(data: &GameData, species: &str, level: u8) -> GameState {
        let event = GameEvent::PartyMonDerived {
            slot: 0,
            mon: Box::new(starter_mon(data, species, level)),
        };
        DefaultReducer.reduce(&GameState::default(), &[EventRecord { frame_id: 1, event }])
    }

    fn hud(name: &str, level: u8) -> BattleObservation {
        BattleObservation {
            menu: Some(BattleMenu::Moves { column: 0, row: 0 }),
            player_name: Some(name.into()),
            player_level: Some(level),
            player_hp_numbers: Some((28, 49)),
            opponent_name: None,
            opponent_level: None,
            player_hp: Some(562),
            opponent_hp: None,
            move_pp: Some((12, 35)),
            move_names: vec![
                "TACKLE".into(),
                "GROWL".into(),
                "LEECH SEED".into(),
                "VINE WHIP".into(),
            ],
            opponent_caught: None,
            opponent_shiny: None,
        }
    }

    #[test]
    fn the_view_follows_the_state() {
        let Some(d) = data() else { return };
        let party = Party::from_state(&state_with(&d, "SPECIES_BULBASAUR", 10));
        let lead = party.lead().unwrap();
        assert_eq!(lead.species, "SPECIES_BULBASAUR");
        assert_eq!(
            lead.moves,
            vec![
                "MOVE_TACKLE",
                "MOVE_GROWL",
                "MOVE_LEECH_SEED",
                "MOVE_VINE_WHIP"
            ]
        );
        assert_eq!(lead.pp_left(&d, "MOVE_TACKLE"), 35);
    }

    #[test]
    fn hud_and_move_menu_become_events() {
        let Some(d) = data() else { return };
        let state = state_with(&d, "SPECIES_BULBASAUR", 15);
        let party = Party::from_state(&state);
        let events = battle_events(&d, &party, &hud("I?YSAUR", 16));
        assert!(events.contains(&GameEvent::Evolved {
            slot: 0,
            species: "SPECIES_IVYSAUR".into()
        }));
        assert!(events.contains(&GameEvent::MovePpObserved {
            slot: 0,
            move_slot: 0,
            cur: 12,
            max: 35
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            GameEvent::PartyObserved {
                level: Some(16),
                hp: Some((28, 49)),
                ..
            }
        )));
    }

    #[test]
    fn wrong_species_of_a_single_member_self_corrects() {
        let Some(d) = data() else { return };
        // Knowledge from another save: IVYSAUR Lv18, but the game has BULBASAUR.
        let state = state_with(&d, "SPECIES_IVYSAUR", 18);
        let events = battle_events(&d, &Party::from_state(&state), &hud("BULBASAUR", 14));
        assert_eq!(
            events,
            vec![GameEvent::PartyObserved {
                slot: 0,
                species: Some("SPECIES_BULBASAUR".into()),
                nickname: None,
                level: Some(14),
                hp: Some((28, 49)),
                status: None,
                held_item: None,
            }]
        );
        let state = DefaultReducer.reduce(
            &state,
            &events
                .into_iter()
                .map(|event| EventRecord { frame_id: 2, event })
                .collect::<Vec<_>>(),
        );
        let lead = Party::from_state(&state).lead().cloned().unwrap();
        assert_eq!(
            (lead.species.as_str(), lead.level),
            ("SPECIES_BULBASAUR", 14)
        );
    }

    #[test]
    fn unknown_name_with_several_members_emits_nothing() {
        let Some(d) = data() else { return };
        let mut state = state_with(&d, "SPECIES_IVYSAUR", 18);
        state = DefaultReducer.reduce(
            &state,
            &[EventRecord {
                frame_id: 2,
                event: GameEvent::PartyMonDerived {
                    slot: 1,
                    mon: Box::new(starter_mon(&d, "SPECIES_PIDGEY", 5)),
                },
            }],
        );
        assert!(battle_events(&d, &Party::from_state(&state), &hud("BULBASAUR", 14)).is_empty());
    }

    #[test]
    fn unchanged_hud_emits_nothing() {
        let Some(d) = data() else { return };
        let state = state_with(&d, "SPECIES_BULBASAUR", 10);
        let events = battle_events(&d, &Party::from_state(&state), &hud("BULBASAUR", 10));
        let state = DefaultReducer.reduce(
            &state,
            &events
                .into_iter()
                .map(|event| EventRecord { frame_id: 2, event })
                .collect::<Vec<_>>(),
        );
        assert!(battle_events(&d, &Party::from_state(&state), &hud("BULBASAUR", 10)).is_empty());
    }

    #[test]
    fn ambiguous_move_name_emits_nothing() {
        let Some(d) = data() else { return };
        let state = state_with(&d, "SPECIES_BULBASAUR", 10);
        let mut obs = hud("BULBASAUR", 10);
        obs.move_names = vec![
            "?????".into(),
            "GROWL".into(),
            "LEECH SEED".into(),
            "VINE WHIP".into(),
        ];
        let events = battle_events(&d, &Party::from_state(&state), &obs);
        assert!(!events
            .iter()
            .any(|e| matches!(e, GameEvent::MovesObserved { .. })));
    }
}
