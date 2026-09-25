//! Facts a route or plan may depend on, and the view of the belief that
//! answers them. The belief itself lives in `crates/state`; the agent adapts
//! it to [`BeliefView`] so this crate stays independent of it.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl CmpOp {
    pub fn holds(self, lhs: i64, rhs: i64) -> bool {
        match self {
            CmpOp::Eq => lhs == rhs,
            CmpOp::Ne => lhs != rhs,
            CmpOp::Lt => lhs < rhs,
            CmpOp::Gt => lhs > rhs,
            CmpOp::Le => lhs <= rhs,
            CmpOp::Ge => lhs >= rhs,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Gt => ">",
            CmpOp::Le => "<=",
            CmpOp::Ge => ">=",
        }
    }
}

/// One fact about the game. Names are the decompilation's (`FLAG_BADGE02_GET`,
/// `MOVE_CUT`, `ITEM_POKE_BALL`, map names like `PalletTown`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    Flag {
        name: String,
        is: bool,
    },
    Var {
        name: String,
        op: CmpOp,
        value: i64,
    },
    /// The map's `FLAG_WORLD_MAP_*` is set (the place can be flown to).
    Visited {
        map: String,
    },
    HasItem {
        item: String,
        n: u32,
    },
    PartyHasMove {
        mv: String,
    },
    /// Gym badge 1–8 (`FLAG_BADGE0n_GET`).
    Badge {
        n: u8,
    },
    At {
        map: String,
    },
}

impl Predicate {
    /// `FLAG_BADGE02_GET` → `Badge { n: 2 }`; any other flag → `Flag`.
    pub fn from_flag(flag: &str, is: bool) -> Predicate {
        let badge = flag
            .strip_prefix("FLAG_BADGE")
            .and_then(|s| s.strip_suffix("_GET"))
            .and_then(|n| n.parse::<u8>().ok());
        match badge {
            Some(n) if is && (1..=8).contains(&n) => Predicate::Badge { n },
            _ => Predicate::Flag {
                name: flag.to_string(),
                is,
            },
        }
    }

    /// The flag behind a badge predicate.
    pub fn badge_flag(n: u8) -> String {
        format!("FLAG_BADGE{n:02}_GET")
    }
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Predicate::Flag { name, is: true } => write!(f, "{name}"),
            Predicate::Flag { name, is: false } => write!(f, "!{name}"),
            Predicate::Var { name, op, value } => write!(f, "{name} {} {value}", op.symbol()),
            Predicate::Visited { map } => write!(f, "Visited({map})"),
            Predicate::HasItem { item, n } => write!(f, "HasItem({item} x{n})"),
            Predicate::PartyHasMove { mv } => write!(f, "PartyHasMove({mv})"),
            Predicate::Badge { n } => write!(f, "Badge({n})"),
            Predicate::At { map } => write!(f, "At({map})"),
        }
    }
}

/// A conjunction of predicates; empty means always satisfied.
pub type Requirement = Vec<Predicate>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Truth {
    True,
    False,
    Unknown,
}

/// What the belief says about single facts.
pub trait BeliefView {
    fn eval(&self, p: &Predicate) -> Truth;
}

/// A requirement checked against a belief: which predicates fail and which
/// the belief can't answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Check {
    pub failed: Vec<Predicate>,
    pub unknown: Vec<Predicate>,
}

impl Check {
    pub fn truth(&self) -> Truth {
        if !self.failed.is_empty() {
            Truth::False
        } else if !self.unknown.is_empty() {
            Truth::Unknown
        } else {
            Truth::True
        }
    }
}

/// Evaluates the conjunction `req` against `belief`.
pub fn check(belief: &dyn BeliefView, req: &[Predicate]) -> Check {
    let mut out = Check::default();
    for p in req {
        match belief.eval(p) {
            Truth::True => {}
            Truth::False => out.failed.push(p.clone()),
            Truth::Unknown => out.unknown.push(p.clone()),
        }
    }
    out
}

/// A belief made of plain maps; facts not listed are `Unknown`. For tests
/// and offline planning from a checkpoint.
#[derive(Debug, Clone, Default)]
pub struct MapBelief {
    pub flags: BTreeMap<String, bool>,
    pub vars: BTreeMap<String, i64>,
    pub visited: BTreeMap<String, bool>,
    /// Item → count held.
    pub items: BTreeMap<String, u32>,
    pub moves: BTreeMap<String, bool>,
    pub badges: BTreeMap<u8, bool>,
    pub at: Option<String>,
}

impl MapBelief {
    pub fn flag(mut self, name: &str, is: bool) -> Self {
        self.flags.insert(name.to_string(), is);
        self
    }

    pub fn var(mut self, name: &str, value: i64) -> Self {
        self.vars.insert(name.to_string(), value);
        self
    }

    pub fn visited(mut self, map: &str, is: bool) -> Self {
        self.visited.insert(map.to_string(), is);
        self
    }

    pub fn item(mut self, item: &str, n: u32) -> Self {
        self.items.insert(item.to_string(), n);
        self
    }

    pub fn party_move(mut self, mv: &str, known: bool) -> Self {
        self.moves.insert(mv.to_string(), known);
        self
    }

    pub fn badge(mut self, n: u8, has: bool) -> Self {
        self.badges.insert(n, has);
        self
    }

    pub fn at(mut self, map: &str) -> Self {
        self.at = Some(map.to_string());
        self
    }
}

fn known(v: Option<bool>) -> Truth {
    match v {
        Some(true) => Truth::True,
        Some(false) => Truth::False,
        None => Truth::Unknown,
    }
}

impl BeliefView for MapBelief {
    fn eval(&self, p: &Predicate) -> Truth {
        match p {
            Predicate::Flag { name, is } => known(self.flags.get(name).map(|v| v == is)),
            Predicate::Var { name, op, value } => {
                known(self.vars.get(name).map(|v| op.holds(*v, *value)))
            }
            Predicate::Visited { map } => known(self.visited.get(map).copied()),
            Predicate::HasItem { item, n } => known(self.items.get(item).map(|have| have >= n)),
            Predicate::PartyHasMove { mv } => known(self.moves.get(mv).copied()),
            Predicate::Badge { n } => known(self.badges.get(n).copied()),
            Predicate::At { map } => known(self.at.as_ref().map(|m| m == map)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_flags_become_badges() {
        assert_eq!(
            Predicate::from_flag("FLAG_BADGE02_GET", true),
            Predicate::Badge { n: 2 }
        );
        assert_eq!(
            Predicate::from_flag("FLAG_BADGE02_GET", false),
            Predicate::Flag {
                name: "FLAG_BADGE02_GET".into(),
                is: false
            }
        );
        assert_eq!(
            Predicate::from_flag("FLAG_TEMP_14", true),
            Predicate::Flag {
                name: "FLAG_TEMP_14".into(),
                is: true
            }
        );
        assert_eq!(Predicate::badge_flag(5), "FLAG_BADGE05_GET");
    }

    #[test]
    fn check_reports_failed_and_unknown() {
        let b = MapBelief::default()
            .badge(2, true)
            .party_move("MOVE_SURF", false)
            .item("ITEM_POKE_BALL", 3);
        let req = vec![
            Predicate::Badge { n: 2 },
            Predicate::PartyHasMove {
                mv: "MOVE_SURF".into(),
            },
            Predicate::HasItem {
                item: "ITEM_POKE_BALL".into(),
                n: 5,
            },
            Predicate::Visited {
                map: "PalletTown".into(),
            },
        ];
        let c = check(&b, &req);
        assert_eq!(c.truth(), Truth::False);
        assert_eq!(c.failed, req[1..3].to_vec());
        assert_eq!(c.unknown, vec![req[3].clone()]);
        assert_eq!(check(&b, &req[..1]).truth(), Truth::True);
        assert_eq!(check(&b, &req[3..]).truth(), Truth::Unknown);
        assert_eq!(check(&b, &[]).truth(), Truth::True);
    }
}
