//! HTN-style methods (`data/rules/methods.json`, spec §4.2): fixed
//! decompositions of compound goals the search would otherwise have to
//! rediscover (`GetBadge(3)` needs Cut first; the League needs the eight
//! badges, Strength and the Elite Four in order). A method lists the
//! subgoals to plan in order before the goal itself; the planner tries it
//! before searching from primitives and falls back when it can't be met.
//! Methods are data, so another game brings its own.

use std::path::Path;

use pokebot_core::{Error, Result};
use pokebot_world::predicate::Predicate;
use serde::{Deserialize, Serialize};

use crate::intents::GoalPredicate;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Method {
    pub name: String,
    /// The goal this decomposes. `has_item` with `n: 0` matches any count.
    pub achieves: GoalPredicate,
    /// Planned in order, each against the belief plus what the previous
    /// ones established.
    #[serde(default)]
    pub subgoals: Vec<GoalPredicate>,
    /// The subgoals alone establish the goal (the game sets the flag
    /// itself); otherwise the goal is planned from primitives after them.
    #[serde(default)]
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Method {
    pub fn matches(&self, goal: &GoalPredicate) -> bool {
        match (&self.achieves, goal) {
            (
                GoalPredicate::World(Predicate::HasItem { item, n: 0 }),
                GoalPredicate::World(Predicate::HasItem { item: want, .. }),
            ) => item == want,
            (a, g) => a == g,
        }
    }
}

/// The methods of `methods.json`, in file order (the first match wins).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Methods {
    pub methods: Vec<Method>,
}

impl Methods {
    pub fn new(methods: Vec<Method>) -> Result<Methods> {
        let set = Methods { methods };
        set.validate()?;
        Ok(set)
    }

    /// Reads `path` (e.g. `data/rules/methods.json`).
    pub fn load(path: impl AsRef<Path>) -> Result<Methods> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let set: Methods = serde_json::from_str(&text)
            .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
        set.validate()
            .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
        Ok(set)
    }

    fn validate(&self) -> Result<()> {
        for (i, m) in self.methods.iter().enumerate() {
            if m.subgoals.is_empty() && !m.complete {
                return Err(Error::InvalidData(format!(
                    "method {i} ({}): no subgoals and not complete",
                    m.name
                )));
            }
            if m.subgoals.iter().any(|s| m.matches(s)) {
                return Err(Error::InvalidData(format!(
                    "method {i} ({}): a subgoal is the goal itself",
                    m.name
                )));
            }
        }
        Ok(())
    }

    /// The first method that decomposes `goal`.
    pub fn for_goal(&self, goal: &GoalPredicate) -> Option<&Method> {
        self.methods.iter().find(|m| m.matches(goal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_matches() {
        let json = r#"[
          {"name": "GetBadge(3)", "achieves": {"badge": {"n": 3}},
           "subgoals": [{"party_has_move": {"mv": "MOVE_CUT"}}]},
          {"name": "StockBalls", "achieves": {"has_item": {"item": "ITEM_POKE_BALL", "n": 0}},
           "subgoals": [{"money": 1000}]}
        ]"#;
        let m: Methods = serde_json::from_str(json).unwrap();
        m.validate().unwrap();
        assert_eq!(
            m.for_goal(&GoalPredicate::badge(3)).unwrap().name,
            "GetBadge(3)"
        );
        assert!(m.for_goal(&GoalPredicate::badge(1)).is_none());
        assert_eq!(
            m.for_goal(&GoalPredicate::has_item("ITEM_POKE_BALL", 7))
                .unwrap()
                .name,
            "StockBalls"
        );
        let bad = Methods {
            methods: vec![Method {
                name: "loop".into(),
                achieves: GoalPredicate::badge(1),
                subgoals: vec![GoalPredicate::badge(1)],
                complete: false,
                note: None,
            }],
        };
        assert!(bad.validate().is_err());
    }
}
