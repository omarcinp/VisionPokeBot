//! The bot's saved knowledge as the belief the planners query: a
//! `SavedKnowledge` (plus the player's pose) answering the world's
//! [`BeliefView`] and the planner's [`GoalBelief`], with a set of facts the
//! plan so far has established layered on top.
//!
//! Anything the knowledge lacks is `Unknown`: an absent flag, an unread bag
//! pocket, a party never looked at.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use pokebot_core::{Error, Result};
use pokebot_gamedata::GameData;
use pokebot_state::{Knowledge, PartyMon, PlayerPose, Pocket, SavedKnowledge};
use pokebot_world::events::DexCount;
use pokebot_world::predicate::{BeliefView, Predicate, Truth};
use serde::Deserialize;

use crate::evaluate::{battle_vs_trainer, Combatant};
use crate::intents::{pocket_of, GoalBelief, GoalPredicate};
use crate::prepare::{PartyMember, OUR_IV};

/// `state.json` as the agent's checkpoint writes it: the saved knowledge
/// with an identity naming where the game was saved.
#[derive(Debug, Clone, Deserialize)]
struct CheckpointFile {
    #[serde(default)]
    identity: Option<Identity>,
    #[serde(flatten)]
    knowledge: SavedKnowledge,
}

#[derive(Debug, Clone, Deserialize)]
struct Identity {
    #[serde(default)]
    saved_at: Option<PlayerPose>,
}

/// Reads a checkpoint `state.json`: the knowledge and, when the identity
/// names it, the pose the game was saved at.
pub fn load_checkpoint(path: impl AsRef<Path>) -> Result<(SavedKnowledge, Option<PlayerPose>)> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let file: CheckpointFile = serde_json::from_str(&text)
        .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
    Ok((file.knowledge, file.identity.and_then(|i| i.saved_at)))
}

/// A stable 64-bit digest of the knowledge a plan was made against
/// (FNV-1a over its JSON, so the same knowledge always gets the same id).
pub fn snapshot_id(knowledge: &SavedKnowledge) -> u64 {
    let json = serde_json::to_string(knowledge).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in json.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The belief the planner plans against.
pub struct StateBelief<'a> {
    pub knowledge: &'a SavedKnowledge,
    pub data: &'a GameData,
    pub pose: Option<PlayerPose>,
    /// Facts established by the intents already in the plan.
    pub established: BTreeSet<GoalPredicate>,
    /// P(win) a trainer must reach for `CanBeat` to hold.
    pub confidence: f64,
    /// Lower bounds on vars whose value is unknown (story vars only move
    /// on: the flag a scene set shows the var it set is at least that).
    pub floors: BTreeMap<String, i64>,
}

impl<'a> StateBelief<'a> {
    pub fn new(
        knowledge: &'a SavedKnowledge,
        data: &'a GameData,
        pose: Option<PlayerPose>,
    ) -> Self {
        StateBelief {
            knowledge,
            data,
            pose,
            established: BTreeSet::new(),
            confidence: 0.9,
            floors: BTreeMap::new(),
        }
    }

    pub fn with_established(&self, established: BTreeSet<GoalPredicate>) -> StateBelief<'a> {
        StateBelief {
            knowledge: self.knowledge,
            data: self.data,
            pose: self.pose.clone(),
            established,
            confidence: self.confidence,
            floors: self.floors.clone(),
        }
    }

    /// The party as the readiness planner wants it (known moves, level).
    pub fn party_members(&self) -> Option<Vec<PartyMember>> {
        let party = self.knowledge.party.value.as_ref()?;
        let members: Vec<PartyMember> = party.iter().filter_map(member_of).collect();
        (!members.is_empty()).then_some(members)
    }

    /// The party as combatants for the evaluator.
    pub fn combatants(&self) -> Option<Vec<Combatant>> {
        let members = self.party_members()?;
        let fighters: Vec<Combatant> = members
            .iter()
            .filter_map(|m| {
                let moves = if m.moves.is_empty() {
                    self.data.default_moves(&m.species, m.level)
                } else {
                    m.moves.clone()
                };
                Combatant::new(self.data, &m.species, m.level, moves, OUR_IV)
            })
            .collect();
        (!fighters.is_empty()).then_some(fighters)
    }

    /// P(win) against `trainer` with the party as known.
    pub fn p_win(&self, trainer: &str) -> Option<f64> {
        let party = self.combatants()?;
        battle_vs_trainer(self.data, &party, trainer).map(|e| e.p_win)
    }

    /// Items held in `pocket`, when that pocket was read.
    fn pocket_count(&self, pocket: Pocket, item: &str) -> Option<u32> {
        let list = self.knowledge.bag.pockets.get(&pocket)?.value.as_ref()?;
        Some(
            list.iter()
                .filter(|(name, _)| name == item)
                .map(|(_, n)| u32::from(*n))
                .sum(),
        )
    }

    /// Items held, when the pocket the item lives in was read.
    pub fn item_count(&self, item: &str) -> Option<u32> {
        match pocket_of(self.data, item) {
            Some(pocket) => self.pocket_count(pocket, item),
            None => {
                // Unknown to the game data: every pocket must be known.
                let mut total = 0;
                for pocket in Pocket::ALL {
                    total += self.pocket_count(pocket, item)?;
                }
                Some(total)
            }
        }
    }

    fn party_has_move(&self, mv: &str) -> Truth {
        let Some(party) = self.knowledge.party.value.as_ref() else {
            return Truth::Unknown;
        };
        let mut unknown = false;
        for mon in party {
            for slot in mon.moves.iter().flatten() {
                match slot.mv.value.as_deref() {
                    Some(m) if m == mv => return Truth::True,
                    Some(_) => {}
                    None => unknown = true,
                }
            }
        }
        if unknown {
            Truth::Unknown
        } else {
            Truth::False
        }
    }

    fn established_truth(&self, p: &GoalPredicate) -> Option<Truth> {
        if self.established.contains(p) {
            return Some(Truth::True);
        }
        if let Some(neg) = p.negation() {
            if self.established.contains(&neg) {
                return Some(Truth::False);
            }
        }
        // Establishing a badge flag establishes the badge and vice versa.
        if let GoalPredicate::World(Predicate::Flag { name, is: true }) = p {
            let as_badge = GoalPredicate::World(Predicate::from_flag(name, true));
            if as_badge != *p && self.established.contains(&as_badge) {
                return Some(Truth::True);
            }
        }
        // A var the plan set answers any comparison on it (the latest
        // value the story reaches: story vars only move on).
        if let GoalPredicate::World(Predicate::Var { name, op, value }) = p {
            let set = self
                .established
                .iter()
                .filter_map(|q| match q {
                    GoalPredicate::World(Predicate::Var {
                        name: n,
                        op: pokebot_world::predicate::CmpOp::Eq,
                        value: v,
                    }) if n == name => Some(*v),
                    _ => None,
                })
                .max();
            if let Some(v) = set {
                return Some(if op.holds(v, *value) {
                    Truth::True
                } else {
                    Truth::False
                });
            }
        }
        if let GoalPredicate::World(Predicate::HasItem { item, n }) = p {
            let held: u32 = self
                .established
                .iter()
                .filter_map(|q| match q {
                    GoalPredicate::World(Predicate::HasItem { item: i, n }) if i == item => {
                        Some(*n)
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            if held >= *n {
                return Some(Truth::True);
            }
        }
        if let GoalPredicate::Money { money } = p {
            let has = self
                .established
                .iter()
                .filter_map(|q| match q {
                    GoalPredicate::Money { money } => Some(*money),
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            if has >= *money {
                return Some(Truth::True);
            }
        }
        if let GoalPredicate::LeadHp { lead_hp } = p {
            let healed = self
                .established
                .iter()
                .filter_map(|q| match q {
                    GoalPredicate::LeadHp { lead_hp } => Some(*lead_hp),
                    GoalPredicate::Healed { healed: true } => Some(100),
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            if healed >= *lead_hp {
                return Some(Truth::True);
            }
        }
        let bound = match p {
            GoalPredicate::PokedexCaught { ge } => Some((DexCount::Caught, *ge)),
            GoalPredicate::PokedexSeen { ge } => Some((DexCount::Seen, *ge)),
            _ => None,
        };
        if let Some((which, ge)) = bound {
            let held = self
                .established
                .iter()
                .filter_map(|q| match (which, q) {
                    (DexCount::Caught, GoalPredicate::PokedexCaught { ge })
                    | (DexCount::Seen, GoalPredicate::PokedexSeen { ge }) => Some(*ge),
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            if held >= ge {
                return Some(Truth::True);
            }
        }
        None
    }

    /// Species seen or caught: the total the Trainer Card showed when it
    /// was read (`Pokedex.counts`), else the species observed one by one,
    /// plus the catches the plan so far establishes. The flag says whether
    /// the count is exact (a read total) or a lower bound.
    pub fn pokedex_count(&self, which: DexCount) -> (u32, bool) {
        let dex = &self.knowledge.pokedex;
        let known = |map: &BTreeMap<String, Knowledge<bool>>| {
            map.iter()
                .filter(|(_, k)| k.value == Some(true))
                .map(|(s, _)| s.clone())
                .collect::<BTreeSet<String>>()
        };
        let mut observed = known(&dex.caught);
        if which == DexCount::Seen {
            observed.extend(known(&dex.seen));
        }
        let new = self
            .established
            .iter()
            .filter(|p| matches!(p, GoalPredicate::Caught { caught } if !observed.contains(caught)))
            .count() as u32;
        let observed = observed.len() as u32;
        let total = dex.counts.value.and_then(|c| match which {
            DexCount::Seen => c.seen,
            DexCount::Caught => Some(c.caught),
        });
        match total {
            Some(total) => (u32::from(total).max(observed) + new, true),
            None => (observed + new, false),
        }
    }

    fn pokedex_truth(&self, which: DexCount, ge: u16) -> Truth {
        let (count, exact) = self.pokedex_count(which);
        if count >= u32::from(ge) {
            Truth::True
        } else if exact {
            Truth::False
        } else {
            Truth::Unknown
        }
    }

    fn eval_world(&self, p: &Predicate) -> Truth {
        let w = &self.knowledge.world;
        match p {
            Predicate::Flag { name, is } => known(w.flag(name).value.map(|v| v == *is)),
            Predicate::Var { name, op, value } => match w.var(name).value {
                Some(v) => known(Some(op.holds(i64::from(v), *value))),
                None => match self.floors.get(name) {
                    // Every value from the floor up answers alike, or not.
                    Some(&floor) => {
                        use pokebot_world::predicate::CmpOp;
                        let all = match op {
                            CmpOp::Gt => floor > *value,
                            CmpOp::Ge => floor >= *value,
                            CmpOp::Ne => *value < floor,
                            _ => false,
                        };
                        let none = match op {
                            CmpOp::Lt => floor >= *value,
                            CmpOp::Le => floor > *value,
                            CmpOp::Eq => *value < floor,
                            _ => false,
                        };
                        if all {
                            Truth::True
                        } else if none {
                            Truth::False
                        } else {
                            Truth::Unknown
                        }
                    }
                    None => Truth::Unknown,
                },
            },
            Predicate::Visited { map } => known(w.visited(map).value),
            Predicate::HasItem { item, n } => known(self.item_count(item).map(|have| have >= *n)),
            Predicate::PartyHasMove { mv } => self.party_has_move(mv),
            Predicate::Badge { n } => known(w.flag(&Predicate::badge_flag(*n)).value),
            Predicate::At { map } => known(self.pose.as_ref().map(|p| p.map == *map)),
        }
    }
}

fn known(v: Option<bool>) -> Truth {
    match v {
        Some(true) => Truth::True,
        Some(false) => Truth::False,
        None => Truth::Unknown,
    }
}

fn member_of(mon: &PartyMon) -> Option<PartyMember> {
    let species = mon.species.value.clone()?;
    let level = mon.level.value?;
    let moves: Vec<String> = mon
        .moves
        .iter()
        .flatten()
        .filter_map(|s| s.mv.value.clone())
        .collect();
    Some(PartyMember {
        species,
        level,
        exp: None,
        moves,
    })
}

impl BeliefView for StateBelief<'_> {
    fn eval(&self, p: &Predicate) -> Truth {
        let goal = GoalPredicate::World(p.clone());
        if let Some(t) = self.established_truth(&goal) {
            return t;
        }
        self.eval_world(p)
    }
}

impl GoalBelief for StateBelief<'_> {
    fn eval_goal(&self, p: &GoalPredicate) -> Truth {
        if let Some(t) = self.established_truth(p) {
            return t;
        }
        match p {
            GoalPredicate::World(w) => self.eval_world(w),
            GoalPredicate::Caught { caught } => known(
                self.knowledge
                    .pokedex
                    .caught
                    .get(caught)
                    .and_then(|k| k.value),
            ),
            GoalPredicate::PokedexCaught { ge } => self.pokedex_truth(DexCount::Caught, *ge),
            GoalPredicate::PokedexSeen { ge } => self.pokedex_truth(DexCount::Seen, *ge),
            GoalPredicate::CanBeat { can_beat } => {
                if !self.data.trainers.contains_key(can_beat) {
                    return Truth::Unknown;
                }
                known(self.p_win(can_beat).map(|p| p >= self.confidence))
            }
            GoalPredicate::Money { money } => {
                known(self.knowledge.money.value.map(|have| have >= *money))
            }
            GoalPredicate::Healed { healed } => {
                let Some(party) = self.knowledge.party.value.as_ref() else {
                    return Truth::Unknown;
                };
                let mut full = true;
                for mon in party {
                    match mon.hp.value {
                        Some((cur, max)) => full &= cur == max,
                        None => return Truth::Unknown,
                    }
                }
                known(Some(full == *healed))
            }
            GoalPredicate::LeadHp { lead_hp } => {
                let Some(party) = self.knowledge.party.value.as_ref() else {
                    return Truth::Unknown;
                };
                let Some((cur, max)) = party.first().and_then(|m| m.hp.value) else {
                    return Truth::Unknown;
                };
                known(Some(
                    u32::from(cur) * 100 >= u32::from(max.max(1)) * u32::from(*lead_hp),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_state::{Knowledge, MoveSlot};

    fn data() -> Option<GameData> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        GameData::load(root.join("data/world/gamedata.json")).ok()
    }

    #[test]
    fn absent_knowledge_is_unknown_and_established_facts_win() {
        let Some(data) = data() else { return };
        let mut k = SavedKnowledge::default();
        k.world
            .flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::observed(true, 1));
        k.money = Knowledge::observed(3000, 1);
        let mut b = StateBelief::new(&k, &data, None);
        assert_eq!(b.eval(&Predicate::Badge { n: 1 }), Truth::True);
        assert_eq!(b.eval(&Predicate::Badge { n: 2 }), Truth::Unknown);
        assert_eq!(
            b.eval(&Predicate::HasItem {
                item: "ITEM_POKE_BALL".into(),
                n: 1
            }),
            Truth::Unknown
        );
        assert_eq!(
            b.eval(&Predicate::At {
                map: "PewterCity".into()
            }),
            Truth::Unknown
        );
        assert_eq!(
            b.eval_goal(&GoalPredicate::Money { money: 2000 }),
            Truth::True
        );
        assert_eq!(
            b.eval_goal(&GoalPredicate::Money { money: 4000 }),
            Truth::False
        );
        b.established.insert(GoalPredicate::badge(2));
        assert_eq!(b.eval(&Predicate::Badge { n: 2 }), Truth::True);
        assert_eq!(
            b.eval(&Predicate::Flag {
                name: "FLAG_BADGE02_GET".into(),
                is: false
            }),
            Truth::False
        );
    }

    #[test]
    fn pokedex_counts_are_a_lower_bound_until_the_card_is_read() {
        let Some(data) = data() else { return };
        let mut k = SavedKnowledge::default();
        for s in ["SPECIES_RATTATA", "SPECIES_PIDGEY", "SPECIES_CATERPIE"] {
            k.pokedex
                .caught
                .insert(s.into(), Knowledge::observed(true, 1));
        }
        let mut b = StateBelief::new(&k, &data, None);
        assert_eq!(b.pokedex_count(DexCount::Caught), (3, false));
        assert_eq!(b.eval_goal(&GoalPredicate::pokedex_caught(3)), Truth::True);
        assert_eq!(
            b.eval_goal(&GoalPredicate::pokedex_caught(10)),
            Truth::Unknown
        );
        // Planned catches raise the bound; one already counted doesn't.
        b.established.insert(GoalPredicate::caught("SPECIES_ZUBAT"));
        b.established
            .insert(GoalPredicate::caught("SPECIES_RATTATA"));
        assert_eq!(b.pokedex_count(DexCount::Caught), (4, false));
        b.established.insert(GoalPredicate::pokedex_caught(10));
        assert_eq!(b.eval_goal(&GoalPredicate::pokedex_caught(10)), Truth::True);
        // The card's total is exact: below it the goal is False.
        let mut k2 = k.clone();
        k2.pokedex.counts = Knowledge::observed(
            pokebot_state::PokedexCounts {
                seen: Some(12),
                caught: 7,
            },
            2,
        );
        let b2 = StateBelief::new(&k2, &data, None);
        assert_eq!(b2.pokedex_count(DexCount::Caught), (7, true));
        assert_eq!(b2.pokedex_count(DexCount::Seen), (12, true));
        // The card alone: caught exact, seen still a bound.
        let mut k3 = k.clone();
        k3.pokedex.counts = Knowledge::observed(
            pokebot_state::PokedexCounts {
                seen: None,
                caught: 7,
            },
            2,
        );
        let b3 = StateBelief::new(&k3, &data, None);
        assert_eq!(b3.pokedex_count(DexCount::Caught), (7, true));
        assert_eq!(b3.pokedex_count(DexCount::Seen), (3, false));
        assert_eq!(
            b2.eval_goal(&GoalPredicate::pokedex_caught(10)),
            Truth::False
        );
        assert_eq!(b2.eval_goal(&GoalPredicate::pokedex_seen(10)), Truth::True);
    }

    #[test]
    fn party_moves_and_can_beat_come_from_the_party() {
        let Some(data) = data() else { return };
        let mut k = SavedKnowledge::default();
        let mut mon = PartyMon {
            species: Knowledge::observed("SPECIES_IVYSAUR".into(), 1),
            level: Knowledge::observed(18, 1),
            ..PartyMon::default()
        };
        mon.moves[0] = Some(MoveSlot {
            mv: Knowledge::observed("MOVE_VINE_WHIP".into(), 1),
            pp: Knowledge::unknown(),
        });
        k.party = Knowledge::observed(vec![mon], 1);
        let b = StateBelief::new(&k, &data, None);
        assert_eq!(
            b.eval(&Predicate::PartyHasMove {
                mv: "MOVE_VINE_WHIP".into()
            }),
            Truth::True
        );
        assert_eq!(
            b.eval(&Predicate::PartyHasMove {
                mv: "MOVE_CUT".into()
            }),
            Truth::False
        );
        assert_eq!(
            b.eval_goal(&GoalPredicate::can_beat("TRAINER_LEADER_BROCK")),
            Truth::True
        );
        assert_eq!(
            b.eval_goal(&GoalPredicate::can_beat("TRAINER_ELITE_FOUR_LANCE")),
            Truth::False
        );
    }
}
