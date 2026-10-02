//! The belief as the route and goal planners read it: `GameState` adapted
//! to `pokebot_world::predicate::BeliefView`. `crates/world` must not
//! depend on `crates/state`, so the adapter lives here.

use pokebot_state::GameState;
use pokebot_world::predicate::{BeliefView, Predicate, Truth};

/// A `GameState` answering predicates. Facts with no value are `Unknown`,
/// whatever their provenance otherwise.
pub struct StateBelief<'a>(pub &'a GameState);

/// What the planner found the knowledge implies, for facts it doesn't
/// hold: flags, and least values of vars (`Plan::implied_flags`,
/// `Plan::var_floors`).
#[derive(Debug, Clone, Default)]
pub struct Inferred {
    pub flags: std::collections::BTreeMap<String, bool>,
    pub floors: std::collections::BTreeMap<String, i64>,
}

/// `base` with the gaps [`Inferred`] fills: the walk reads the belief as
/// the plan did (fleet continue-2: the plan's way to Cinnabar ran through
/// Pallet Town, whose passages need FLAG_OPENED_START_MENU and Oak's
/// scene var above 0, both unknown to the belief but implied; the walk
/// found "no known route" and wandered off west).
pub struct InferredBelief<'a> {
    pub base: &'a dyn BeliefView,
    pub inferred: &'a Inferred,
}

impl BeliefView for InferredBelief<'_> {
    fn eval(&self, p: &Predicate) -> Truth {
        let truth = self.base.eval(p);
        if truth != Truth::Unknown {
            return truth;
        }
        match p {
            Predicate::Flag { name, is } => self
                .inferred
                .flags
                .get(name)
                .map_or(Truth::Unknown, |v| known(Some(v == is))),
            Predicate::Var { name, op, value } => {
                use pokebot_world::predicate::CmpOp;
                match (self.inferred.floors.get(name), op) {
                    (Some(floor), CmpOp::Ge) if floor >= value => Truth::True,
                    (Some(floor), CmpOp::Gt) if floor > value => Truth::True,
                    (Some(floor), CmpOp::Ne) if floor > value => Truth::True,
                    (Some(floor), CmpOp::Eq | CmpOp::Le) if floor > value => Truth::False,
                    (Some(floor), CmpOp::Lt) if floor >= value => Truth::False,
                    _ => Truth::Unknown,
                }
            }
            _ => Truth::Unknown,
        }
    }

    fn escape(&self) -> Option<pokebot_state::EscapeWarp> {
        self.base.escape()
    }
}

fn known(v: Option<bool>) -> Truth {
    match v {
        Some(true) => Truth::True,
        Some(false) => Truth::False,
        None => Truth::Unknown,
    }
}

impl BeliefView for StateBelief<'_> {
    fn eval(&self, p: &Predicate) -> Truth {
        let s = self.0;
        match p {
            Predicate::Flag { name, is } => known(s.world.flag(name).value.map(|v| v == *is)),
            Predicate::Var { name, op, value } => known(
                s.world
                    .var(name)
                    .value
                    .map(|v| op.holds(i64::from(v), *value)),
            ),
            Predicate::Visited { map } => known(s.world.visited(map).value),
            Predicate::HasItem { item, n } => {
                // Any pocket's list that is known: count the item there.
                let mut counted = None;
                for k in s.bag.pockets.values() {
                    if let Some(list) = &k.value {
                        let have: u32 = list
                            .iter()
                            .filter(|(name, _)| name == item)
                            .map(|(_, count)| u32::from(*count))
                            .sum();
                        counted = Some(counted.unwrap_or(0) + have);
                    }
                }
                match counted {
                    Some(have) if have >= *n => Truth::True,
                    // Not seen in the known pockets: false only when every
                    // pocket is known.
                    Some(_)
                        if pokebot_state::Pocket::ALL
                            .iter()
                            .all(|p| s.bag.pockets.get(p).is_some_and(|k| k.value.is_some())) =>
                    {
                        Truth::False
                    }
                    _ => Truth::Unknown,
                }
            }
            Predicate::PartyHasMove { mv } => known(s.party.value.as_ref().map(|party| {
                party.iter().any(|m| {
                    m.moves
                        .iter()
                        .flatten()
                        .any(|slot| slot.mv.value.as_deref() == Some(mv.as_str()))
                })
            })),
            Predicate::Badge { n } => {
                let flag = Predicate::badge_flag(*n);
                match s.world.flag(&flag).value {
                    Some(v) => known(Some(v)),
                    None => known(
                        s.progression
                            .badges
                            .value
                            .as_ref()
                            .map(|badges| badges.len() >= usize::from(*n)),
                    ),
                }
            }
            Predicate::At { map } => known(s.player.pose.value.as_ref().map(|p| p.map == *map)),
        }
    }

    fn escape(&self) -> Option<pokebot_state::EscapeWarp> {
        self.0.world.escape.value.clone()
    }
}

#[cfg(test)]
mod tests {
    use pokebot_state::{Knowledge, PlayerPose, Pocket};
    use pokebot_world::predicate::CmpOp;

    use super::*;

    #[test]
    fn flags_vars_visited_and_position() {
        let mut s = GameState::default();
        s.world
            .flags
            .insert("FLAG_A".into(), Knowledge::observed(true, 1));
        s.world
            .vars
            .insert("VAR_X".into(), Knowledge::tracked(3, None));
        s.world
            .visited
            .insert("PalletTown".into(), Knowledge::observed(false, 1));
        s.player.pose = Knowledge::observed(
            PlayerPose {
                map: "Route1".into(),
                x: 1,
                y: 2,
            },
            1,
        );
        let b = StateBelief(&s);
        assert_eq!(
            b.eval(&Predicate::Flag {
                name: "FLAG_A".into(),
                is: true
            }),
            Truth::True
        );
        assert_eq!(
            b.eval(&Predicate::Flag {
                name: "FLAG_B".into(),
                is: true
            }),
            Truth::Unknown
        );
        assert_eq!(
            b.eval(&Predicate::Var {
                name: "VAR_X".into(),
                op: CmpOp::Ge,
                value: 3
            }),
            Truth::True
        );
        assert_eq!(
            b.eval(&Predicate::Visited {
                map: "PalletTown".into()
            }),
            Truth::False
        );
        assert_eq!(
            b.eval(&Predicate::At {
                map: "Route1".into()
            }),
            Truth::True
        );
        assert_eq!(b.eval(&Predicate::Badge { n: 1 }), Truth::Unknown);
        s.world
            .flags
            .insert(Predicate::badge_flag(1), Knowledge::observed(true, 2));
        assert_eq!(
            StateBelief(&s).eval(&Predicate::Badge { n: 1 }),
            Truth::True
        );
    }

    #[test]
    fn items_need_a_known_pocket() {
        let mut s = GameState::default();
        let p = Predicate::HasItem {
            item: "ITEM_POKE_BALL".into(),
            n: 2,
        };
        assert_eq!(StateBelief(&s).eval(&p), Truth::Unknown);
        s.bag.pockets.insert(
            Pocket::PokeBalls,
            Knowledge::observed(vec![("ITEM_POKE_BALL".to_string(), 5u16)], 1),
        );
        assert_eq!(StateBelief(&s).eval(&p), Truth::True);
        let other = Predicate::HasItem {
            item: "ITEM_POTION".into(),
            n: 1,
        };
        // Other pockets unknown: it may be there.
        assert_eq!(StateBelief(&s).eval(&other), Truth::Unknown);
    }
}
