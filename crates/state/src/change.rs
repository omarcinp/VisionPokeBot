//! What changed between two states, as specific change notices.
//!
//! Events are the input to the state; changes are its output. Whatever
//! produced an event (perception, the agent, a tracked delta), every value
//! it modified comes out here once, as a typed change with its old and new
//! value: `PartyHpChanged`, `ItemCountChanged`, `NpcMoved`, … Subscribers
//! react to changes instead of polling the state. `diff` is pure; only
//! values count (a re-observation that refreshes `last_verified_frame` or
//! upgrades provenance without changing the value is not a change).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    Direction, GameState, Gender, HealSpot, Knowledge, PartyMon, PlayerPose, Pocket, PokedexCounts,
    ScreenState, Status, ViewState, VisibleNpc,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateChange {
    ScreenChanged {
        from: Option<ScreenState>,
        to: Option<ScreenState>,
    },
    /// First fix of the player's position (or after it was lost).
    PlayerLocated {
        pose: PlayerPose,
    },
    PlayerMoved {
        from: PlayerPose,
        to: PlayerPose,
    },
    MapChanged {
        from: PlayerPose,
        to: PlayerPose,
    },
    BattleStarted,
    BattleEnded,

    GenderKnown {
        gender: Gender,
    },
    PlayerNameChanged {
        from: Option<String>,
        to: Option<String>,
    },
    RivalNameChanged {
        from: Option<String>,
        to: Option<String>,
    },
    BadgeAdded {
        badge: String,
    },
    BadgeRemoved {
        badge: String,
    },

    /// The party list became known, or its length changed.
    PartySizeChanged {
        from: Option<usize>,
        to: Option<usize>,
    },
    PartyMonAdded {
        slot: u8,
        species: Option<String>,
    },
    PartyMonRemoved {
        slot: u8,
        species: Option<String>,
    },
    PartySpeciesChanged {
        slot: u8,
        from: Option<String>,
        to: Option<String>,
    },
    PartyNicknameChanged {
        slot: u8,
        from: Option<String>,
        to: Option<String>,
    },
    PartyLevelChanged {
        slot: u8,
        from: Option<u8>,
        to: Option<u8>,
    },
    PartyHpChanged {
        slot: u8,
        from: Option<(u16, u16)>,
        to: Option<(u16, u16)>,
    },
    PartyStatusChanged {
        slot: u8,
        from: Option<Status>,
        to: Option<Status>,
    },
    PartyHeldItemChanged {
        slot: u8,
        from: Option<Option<String>>,
        to: Option<Option<String>>,
    },
    PartyShinyChanged {
        slot: u8,
        from: Option<bool>,
        to: Option<bool>,
    },
    /// A move slot's move changed (learned, replaced, forgotten, read).
    PartyMoveChanged {
        slot: u8,
        move_slot: u8,
        from: Option<String>,
        to: Option<String>,
    },
    PartyPpChanged {
        slot: u8,
        move_slot: u8,
        mv: Option<String>,
        from: Option<(u8, u8)>,
        to: Option<(u8, u8)>,
    },

    /// A pocket's contents became known (or unknown).
    PocketKnown {
        pocket: Pocket,
        known: bool,
    },
    /// An item's count in a known pocket; `None` = not in the pocket.
    ItemCountChanged {
        pocket: Pocket,
        item: String,
        from: Option<u16>,
        to: Option<u16>,
    },
    MoneyChanged {
        from: Option<u32>,
        to: Option<u32>,
    },

    PcBoxChanged {
        box_index: u8,
        /// (box slot, species) now there that weren't.
        added: Vec<(u8, Option<String>)>,
        removed: Vec<(u8, Option<String>)>,
    },
    PcItemsChanged,

    PokedexSeen {
        species: String,
    },
    PokedexCaught {
        species: String,
    },
    PokedexCountsChanged {
        from: Option<PokedexCounts>,
        to: Option<PokedexCounts>,
    },

    FlagChanged {
        flag: String,
        from: Option<bool>,
        to: Option<bool>,
    },
    VarChanged {
        var: String,
        from: Option<u16>,
        to: Option<u16>,
    },
    MapVisited {
        map: String,
    },
    RespawnChanged {
        from: Option<HealSpot>,
        to: Option<HealSpot>,
    },
    /// An NPC was seen at a tile for the first time.
    NpcAppeared {
        map: String,
        local_id: u32,
        at: (i32, i32),
    },
    NpcMoved {
        map: String,
        local_id: u32,
        from: (i32, i32),
        to: (i32, i32),
    },
    NpcTurned {
        map: String,
        local_id: u32,
        from: Option<Direction>,
        to: Option<Direction>,
    },
    /// Its tile was seen empty.
    NpcGone {
        map: String,
        local_id: u32,
    },

    /// The page of text on screen (message box or battle text).
    TextShown {
        lines: Vec<String>,
    },
    TextCleared,
    /// The rows of the open list menu, or its cursor, changed.
    MenuShown {
        rows: Vec<String>,
        cursor: u8,
    },
    MenuClosed,
    /// The opponent in battle (species constant, level) came into view.
    OpponentAppeared {
        species: Option<String>,
        level: Option<u8>,
    },
    OpponentHpChanged {
        from: Option<u16>,
        to: Option<u16>,
    },
    OpponentGone,
    /// A sprite came into view on the field (an object, by tile).
    SpriteAppeared {
        npc: VisibleNpc,
    },
    SpriteMoved {
        from: VisibleNpc,
        to: VisibleNpc,
    },
    SpriteLeft {
        npc: VisibleNpc,
    },
    /// The map-name popup came up with this name (entering a named place).
    MapNameShown {
        name: String,
    },
}

/// A change and the frame it was made at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub frame_id: u64,
    pub change: StateChange,
}

/// Every value that differs between `old` and `new`, in a fixed order.
pub fn diff(old: &GameState, new: &GameState) -> Vec<StateChange> {
    let mut out = Vec::new();
    if old == new {
        return out;
    }
    if old.screen.value != new.screen.value {
        out.push(StateChange::ScreenChanged {
            from: old.screen.value,
            to: new.screen.value,
        });
    }
    pose(&mut out, &old.player.pose, &new.player.pose);
    match (old.in_battle, new.in_battle) {
        (false, true) => out.push(StateChange::BattleStarted),
        (true, false) => out.push(StateChange::BattleEnded),
        _ => {}
    }
    progression(&mut out, old, new);
    party(&mut out, &old.party, &new.party);
    bag(&mut out, old, new);
    if old.money.value != new.money.value {
        out.push(StateChange::MoneyChanged {
            from: old.money.value,
            to: new.money.value,
        });
    }
    pc(&mut out, old, new);
    pokedex(&mut out, old, new);
    world(&mut out, old, new);
    view(&mut out, &old.view, &new.view);
    out
}

fn pose(out: &mut Vec<StateChange>, old: &Knowledge<PlayerPose>, new: &Knowledge<PlayerPose>) {
    match (&old.value, &new.value) {
        (None, Some(pose)) => out.push(StateChange::PlayerLocated { pose: pose.clone() }),
        (Some(from), Some(to)) if from.map != to.map => out.push(StateChange::MapChanged {
            from: from.clone(),
            to: to.clone(),
        }),
        (Some(from), Some(to)) if from != to => out.push(StateChange::PlayerMoved {
            from: from.clone(),
            to: to.clone(),
        }),
        _ => {}
    }
}

fn progression(out: &mut Vec<StateChange>, old: &GameState, new: &GameState) {
    let (o, n) = (&old.progression, &new.progression);
    if let (None, Some(gender)) = (o.gender.value, n.gender.value) {
        out.push(StateChange::GenderKnown { gender });
    }
    if o.player_name.value != n.player_name.value {
        out.push(StateChange::PlayerNameChanged {
            from: o.player_name.value.clone(),
            to: n.player_name.value.clone(),
        });
    }
    if o.rival_name.value != n.rival_name.value {
        out.push(StateChange::RivalNameChanged {
            from: o.rival_name.value.clone(),
            to: n.rival_name.value.clone(),
        });
    }
    let set = |k: &Knowledge<Vec<String>>| -> BTreeSet<String> {
        k.value.iter().flatten().cloned().collect()
    };
    let (before, after) = (set(&o.badges), set(&n.badges));
    for badge in after.difference(&before) {
        out.push(StateChange::BadgeAdded {
            badge: badge.clone(),
        });
    }
    for badge in before.difference(&after) {
        out.push(StateChange::BadgeRemoved {
            badge: badge.clone(),
        });
    }
}

fn party(
    out: &mut Vec<StateChange>,
    old: &Knowledge<Vec<PartyMon>>,
    new: &Knowledge<Vec<PartyMon>>,
) {
    let (a, b) = (
        old.value.as_deref().unwrap_or_default(),
        new.value.as_deref().unwrap_or_default(),
    );
    let (la, lb) = (
        old.value.as_ref().map(Vec::len),
        new.value.as_ref().map(Vec::len),
    );
    if la != lb {
        out.push(StateChange::PartySizeChanged { from: la, to: lb });
    }
    for slot in 0..a.len().max(b.len()) {
        let s = slot as u8;
        match (a.get(slot), b.get(slot)) {
            (None, Some(m)) => {
                out.push(StateChange::PartyMonAdded {
                    slot: s,
                    species: m.species.value.clone(),
                });
                member(out, s, &PartyMon::default(), m);
            }
            (Some(m), None) => out.push(StateChange::PartyMonRemoved {
                slot: s,
                species: m.species.value.clone(),
            }),
            (Some(x), Some(y)) => member(out, s, x, y),
            (None, None) => {}
        }
    }
}

fn member(out: &mut Vec<StateChange>, slot: u8, a: &PartyMon, b: &PartyMon) {
    macro_rules! field {
        ($f:ident, $variant:ident) => {
            if a.$f.value != b.$f.value {
                out.push(StateChange::$variant {
                    slot,
                    from: a.$f.value.clone(),
                    to: b.$f.value.clone(),
                });
            }
        };
    }
    field!(species, PartySpeciesChanged);
    field!(nickname, PartyNicknameChanged);
    field!(level, PartyLevelChanged);
    field!(hp, PartyHpChanged);
    field!(status, PartyStatusChanged);
    field!(held_item, PartyHeldItemChanged);
    field!(shiny, PartyShinyChanged);
    for (i, (x, y)) in a.moves.iter().zip(&b.moves).enumerate() {
        let mv = |s: &Option<crate::MoveSlot>| s.as_ref().and_then(|s| s.mv.value.clone());
        let pp = |s: &Option<crate::MoveSlot>| s.as_ref().and_then(|s| s.pp.value);
        let move_slot = i as u8;
        if mv(x) != mv(y) {
            out.push(StateChange::PartyMoveChanged {
                slot,
                move_slot,
                from: mv(x),
                to: mv(y),
            });
        }
        if pp(x) != pp(y) {
            out.push(StateChange::PartyPpChanged {
                slot,
                move_slot,
                mv: mv(y),
                from: pp(x),
                to: pp(y),
            });
        }
    }
}

fn bag(out: &mut Vec<StateChange>, old: &GameState, new: &GameState) {
    for pocket in Pocket::ALL {
        let get = |s: &GameState| s.bag.pockets.get(&pocket).and_then(|k| k.value.clone());
        let (a, b) = (get(old), get(new));
        if a.is_some() != b.is_some() {
            out.push(StateChange::PocketKnown {
                pocket,
                known: b.is_some(),
            });
        }
        let counts = |list: &Option<crate::ItemList>| -> BTreeMap<String, u16> {
            list.iter().flatten().cloned().collect()
        };
        let (ca, cb) = (counts(&a), counts(&b));
        let items: BTreeSet<&String> = ca.keys().chain(cb.keys()).collect();
        for item in items {
            let (from, to) = (ca.get(item).copied(), cb.get(item).copied());
            if from != to {
                out.push(StateChange::ItemCountChanged {
                    pocket,
                    item: item.clone(),
                    from,
                    to,
                });
            }
        }
    }
}

fn pc(out: &mut Vec<StateChange>, old: &GameState, new: &GameState) {
    for (i, (a, b)) in old.pc.boxes.iter().zip(&new.pc.boxes).enumerate() {
        if a.value == b.value {
            continue;
        }
        let slots = |k: &Knowledge<Vec<crate::BoxMon>>| -> BTreeSet<(u8, Option<String>)> {
            k.value
                .iter()
                .flatten()
                .map(|m| (m.slot, m.species.value.clone()))
                .collect()
        };
        let (sa, sb) = (slots(a), slots(b));
        out.push(StateChange::PcBoxChanged {
            box_index: i as u8,
            added: sb.difference(&sa).cloned().collect(),
            removed: sa.difference(&sb).cloned().collect(),
        });
    }
    if old.pc.items.value != new.pc.items.value {
        out.push(StateChange::PcItemsChanged);
    }
}

fn pokedex(out: &mut Vec<StateChange>, old: &GameState, new: &GameState) {
    let yes = |m: &BTreeMap<String, Knowledge<bool>>, s: &str| {
        m.get(s).and_then(|k| k.value).unwrap_or(false)
    };
    for (species, k) in &new.pokedex.seen {
        if k.value == Some(true) && !yes(&old.pokedex.seen, species) {
            out.push(StateChange::PokedexSeen {
                species: species.clone(),
            });
        }
    }
    for (species, k) in &new.pokedex.caught {
        if k.value == Some(true) && !yes(&old.pokedex.caught, species) {
            out.push(StateChange::PokedexCaught {
                species: species.clone(),
            });
        }
    }
    if old.pokedex.counts.value != new.pokedex.counts.value {
        out.push(StateChange::PokedexCountsChanged {
            from: old.pokedex.counts.value,
            to: new.pokedex.counts.value,
        });
    }
}

fn world(out: &mut Vec<StateChange>, old: &GameState, new: &GameState) {
    let (a, b) = (&old.world, &new.world);
    for (flag, from, to) in changed(&a.flags, &b.flags) {
        out.push(StateChange::FlagChanged { flag, from, to });
    }
    for (var, from, to) in changed(&a.vars, &b.vars) {
        out.push(StateChange::VarChanged { var, from, to });
    }
    for (map, _, to) in changed(&a.visited, &b.visited) {
        if to == Some(true) {
            out.push(StateChange::MapVisited { map });
        }
    }
    if a.respawn.value != b.respawn.value {
        out.push(StateChange::RespawnChanged {
            from: a.respawn.value.clone(),
            to: b.respawn.value.clone(),
        });
    }
    for (map, npcs) in &b.npcs {
        for (id, n) in npcs {
            let before = a.npc(map, *id);
            let pos = |k: Option<&crate::NpcBelief>| k.and_then(|n| n.pos.value);
            match (pos(before), n.pos.value) {
                (None, Some(at)) => out.push(StateChange::NpcAppeared {
                    map: map.clone(),
                    local_id: *id,
                    at,
                }),
                (Some(from), Some(to)) if from != to => out.push(StateChange::NpcMoved {
                    map: map.clone(),
                    local_id: *id,
                    from,
                    to,
                }),
                _ => {}
            }
            let facing = before.and_then(|n| n.facing.value);
            if facing != n.facing.value && n.facing.value.is_some() {
                out.push(StateChange::NpcTurned {
                    map: map.clone(),
                    local_id: *id,
                    from: facing,
                    to: n.facing.value,
                });
            }
            let present = before.and_then(|n| n.present.value);
            if n.present.value == Some(false) && present != Some(false) {
                out.push(StateChange::NpcGone {
                    map: map.clone(),
                    local_id: *id,
                });
            }
        }
    }
}

/// Keys whose value differs between two knowledge maps.
fn changed<T: Clone + PartialEq>(
    a: &BTreeMap<String, Knowledge<T>>,
    b: &BTreeMap<String, Knowledge<T>>,
) -> Vec<(String, Option<T>, Option<T>)> {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter_map(|k| {
            let from = a.get(k).and_then(|v| v.value.clone());
            let to = b.get(k).and_then(|v| v.value.clone());
            (from != to).then(|| (k.clone(), from, to))
        })
        .collect()
}

fn view(out: &mut Vec<StateChange>, a: &ViewState, b: &ViewState) {
    if a.text != b.text {
        out.push(match &b.text {
            Some(lines) => StateChange::TextShown {
                lines: lines.clone(),
            },
            None => StateChange::TextCleared,
        });
    }
    if a.menu != b.menu {
        out.push(match &b.menu {
            Some(m) => StateChange::MenuShown {
                rows: m.rows.clone(),
                cursor: m.cursor,
            },
            None => StateChange::MenuClosed,
        });
    }
    let (oa, ob) = (a.opponent.as_ref(), b.opponent.as_ref());
    match (oa, ob) {
        (_, Some(o)) if oa.is_none_or(|p| p.species != o.species || p.level != o.level) => out
            .push(StateChange::OpponentAppeared {
                species: o.species.clone(),
                level: o.level,
            }),
        (Some(_), None) => out.push(StateChange::OpponentGone),
        _ => {}
    }
    let (ha, hb) = (oa.and_then(|o| o.hp), ob.and_then(|o| o.hp));
    if ob.is_some() && ha != hb {
        out.push(StateChange::OpponentHpChanged { from: ha, to: hb });
    }
    sprites(out, &a.npcs, &b.npcs);
    // Its going away says nothing new: only a name coming up is a change.
    if let Some(name) = b
        .map_popup
        .as_ref()
        .filter(|n| a.map_popup.as_ref() != Some(*n))
    {
        out.push(StateChange::MapNameShown { name: name.clone() });
    }
}

/// Sprites on the field, paired by identity: the NPC's `local_id` when
/// known, else the nearest unpaired sprite one tile away (a step).
fn sprites(out: &mut Vec<StateChange>, a: &[VisibleNpc], b: &[VisibleNpc]) {
    let mut left: Vec<&VisibleNpc> = a.iter().collect();
    let mut arrived = Vec::new();
    for n in b {
        let at = left.iter().position(|o| o.same_as(n)).or_else(|| {
            left.iter()
                .position(|o| o.local_id.is_none() && n.local_id.is_none() && o.near(n))
        });
        match at {
            Some(i) => {
                let o = left.remove(i);
                if o != n {
                    out.push(StateChange::SpriteMoved {
                        from: o.clone(),
                        to: n.clone(),
                    });
                }
            }
            None => arrived.push(n),
        }
    }
    for n in left {
        out.push(StateChange::SpriteLeft { npc: n.clone() });
    }
    for n in arrived {
        out.push(StateChange::SpriteAppeared { npc: n.clone() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MoveSlot, OpponentView};

    fn mon(hp: u16) -> PartyMon {
        let mut m = PartyMon {
            species: Knowledge::observed("SPECIES_BULBASAUR".into(), 1),
            level: Knowledge::observed(10, 1),
            hp: Knowledge::observed((hp, 30), 1),
            ..Default::default()
        };
        m.moves[0] = Some(MoveSlot {
            mv: Knowledge::observed("MOVE_TACKLE".into(), 1),
            pp: Knowledge::observed((35, 35), 1),
        });
        m
    }

    #[test]
    fn identical_states_have_no_changes() {
        let s = GameState::default();
        assert!(diff(&s, &s.clone()).is_empty());
    }

    #[test]
    fn reverifying_a_value_is_not_a_change() {
        let a = GameState {
            party: Knowledge::observed(vec![mon(20)], 1),
            ..GameState::default()
        };
        let mut b = a.clone();
        b.party = Knowledge::observed(vec![mon(20)], 99);
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn party_fields_come_out_one_change_each() {
        let a = GameState {
            party: Knowledge::observed(vec![mon(20)], 1),
            ..GameState::default()
        };
        let mut b = a.clone();
        let m = &mut b.party.value.as_mut().unwrap()[0];
        m.hp = Knowledge::observed((12, 30), 5);
        m.level = Knowledge::observed(11, 5);
        m.moves[0].as_mut().unwrap().pp = Knowledge::tracked((34, 35), Some(1));
        assert_eq!(
            diff(&a, &b),
            vec![
                StateChange::PartyLevelChanged {
                    slot: 0,
                    from: Some(10),
                    to: Some(11)
                },
                StateChange::PartyHpChanged {
                    slot: 0,
                    from: Some((20, 30)),
                    to: Some((12, 30))
                },
                StateChange::PartyPpChanged {
                    slot: 0,
                    move_slot: 0,
                    mv: Some("MOVE_TACKLE".into()),
                    from: Some((35, 35)),
                    to: Some((34, 35))
                },
            ]
        );
    }

    #[test]
    fn a_new_party_member_is_added_with_its_fields() {
        let a = GameState {
            party: Knowledge::observed(vec![mon(20)], 1),
            ..GameState::default()
        };
        let mut b = a.clone();
        b.party.value.as_mut().unwrap().push(mon(30));
        let changes = diff(&a, &b);
        assert_eq!(
            changes[..2],
            [
                StateChange::PartySizeChanged {
                    from: Some(1),
                    to: Some(2)
                },
                StateChange::PartyMonAdded {
                    slot: 1,
                    species: Some("SPECIES_BULBASAUR".into())
                },
            ]
        );
        assert!(changes.contains(&StateChange::PartyHpChanged {
            slot: 1,
            from: None,
            to: Some((30, 30))
        }));
    }

    #[test]
    fn item_counts_and_pockets() {
        let mut a = GameState::default();
        let mut b = a.clone();
        b.bag.pockets.insert(
            Pocket::Items,
            Knowledge::observed(vec![("ITEM_POTION".into(), 2)], 3),
        );
        assert_eq!(
            diff(&a, &b),
            vec![
                StateChange::PocketKnown {
                    pocket: Pocket::Items,
                    known: true
                },
                StateChange::ItemCountChanged {
                    pocket: Pocket::Items,
                    item: "ITEM_POTION".into(),
                    from: None,
                    to: Some(2)
                },
            ]
        );
        a = b.clone();
        b.bag.pockets.insert(
            Pocket::Items,
            Knowledge::tracked(vec![("ITEM_POTION".into(), 1)], Some(3)),
        );
        assert_eq!(
            diff(&a, &b),
            vec![StateChange::ItemCountChanged {
                pocket: Pocket::Items,
                item: "ITEM_POTION".into(),
                from: Some(2),
                to: Some(1)
            }]
        );
    }

    #[test]
    fn npcs_appear_move_and_leave() {
        let a = GameState::default();
        let mut b = a.clone();
        b.world.npc_mut("PalletTown", 1).pos = Knowledge::observed((5, 5), 1);
        assert_eq!(
            diff(&a, &b),
            vec![StateChange::NpcAppeared {
                map: "PalletTown".into(),
                local_id: 1,
                at: (5, 5)
            }]
        );
        let mut c = b.clone();
        c.world.npc_mut("PalletTown", 1).pos = Knowledge::observed((5, 6), 2);
        assert_eq!(
            diff(&b, &c),
            vec![StateChange::NpcMoved {
                map: "PalletTown".into(),
                local_id: 1,
                from: (5, 5),
                to: (5, 6)
            }]
        );
    }

    /// Perception can't always tell which way a sprite faces: a sighting
    /// without it keeps the facing known, and an absence is one change.
    #[test]
    fn npc_sightings_and_absence() {
        use crate::{DefaultReducer, EventRecord, GameEvent, StateReducer};
        let reduce = |s: &GameState, frame_id, event| {
            DefaultReducer.reduce(s, &[EventRecord { frame_id, event }])
        };
        let seen = |facing| GameEvent::NpcSeen {
            map: "Route3".into(),
            local_id: 7,
            x: 19,
            y: 9,
            facing,
        };
        let a = reduce(&GameState::default(), 1, seen(Some(Direction::Left)));
        let b = reduce(&a, 2, seen(None));
        assert!(diff(&a, &b).is_empty());
        assert_eq!(
            b.world.npc("Route3", 7).unwrap().facing.value,
            Some(Direction::Left)
        );
        let absent = GameEvent::NpcAbsent {
            map: "Route3".into(),
            local_id: 7,
        };
        let c = reduce(&b, 3, absent.clone());
        assert_eq!(
            diff(&b, &c),
            vec![StateChange::NpcGone {
                map: "Route3".into(),
                local_id: 7
            }]
        );
        assert!(diff(&c, &reduce(&c, 4, absent)).is_empty());
    }

    #[test]
    fn view_changes() {
        let a = GameState::default();
        let mut b = a.clone();
        b.view.text = Some(vec!["Hello!".into()]);
        b.view.opponent = Some(OpponentView {
            species: Some("SPECIES_PIDGEY".into()),
            level: Some(3),
            hp: Some(1000),
            ..Default::default()
        });
        let sprite = VisibleNpc {
            map: "Route1".into(),
            x: 3,
            y: 4,
            local_id: None,
            facing: None,
        };
        b.view.npcs = vec![sprite.clone()];
        let changes = diff(&a, &b);
        assert!(changes.contains(&StateChange::TextShown {
            lines: vec!["Hello!".into()]
        }));
        assert!(changes.contains(&StateChange::OpponentAppeared {
            species: Some("SPECIES_PIDGEY".into()),
            level: Some(3)
        }));
        assert!(changes.contains(&StateChange::SpriteAppeared {
            npc: sprite.clone()
        }));
        let mut c = b.clone();
        c.view.opponent.as_mut().unwrap().hp = Some(500);
        c.view.npcs[0].y = 5;
        assert_eq!(
            diff(&b, &c),
            vec![
                StateChange::OpponentHpChanged {
                    from: Some(1000),
                    to: Some(500)
                },
                StateChange::SpriteMoved {
                    from: sprite.clone(),
                    to: VisibleNpc { y: 5, ..sprite }
                },
            ]
        );
    }

    #[test]
    fn a_map_name_coming_up_is_a_change_and_going_away_is_not() {
        let a = GameState::default();
        let mut b = a.clone();
        b.view.map_popup = Some("ROUTE 3".into());
        assert_eq!(
            diff(&a, &b),
            vec![StateChange::MapNameShown {
                name: "ROUTE 3".into()
            }]
        );
        assert_eq!(diff(&b, &b.clone()), vec![]);
        assert_eq!(diff(&b, &a), vec![]);
        let mut c = b.clone();
        c.view.map_popup = Some("PEWTER CITY".into());
        assert_eq!(
            diff(&b, &c),
            vec![StateChange::MapNameShown {
                name: "PEWTER CITY".into()
            }]
        );
    }
}
