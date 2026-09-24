//! Rules of inference from belief to belief (`inference.json`): what an
//! observed fact says about others, e.g. holding the Boulder Badge means
//! Brock was beaten. Declarative so they can be extended without code.
//!
//! Only `Observed` premises fire, and a conclusion is written `Derived`
//! unless the fact is already `Observed` (an observation is never replaced
//! by a deduction).

use std::path::Path;

use pokebot_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::{Fact, Knowledge, KnowledgeSource, WorldBelief};

/// A fact with a value: `{"flag": "FLAG_X", "is": true}`,
/// `{"var": "VAR_X", "eq": 3}`, `{"var": "VAR_X", "ge": 3}` or
/// `{"visited": "PalletTown", "is": true}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Condition {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub var: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visited: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eq: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ge: Option<u16>,
}

impl Condition {
    pub fn flag(name: &str, is: bool) -> Condition {
        Condition {
            flag: Some(name.to_owned()),
            is: Some(is),
            ..Default::default()
        }
    }

    pub fn visited(map: &str, is: bool) -> Condition {
        Condition {
            visited: Some(map.to_owned()),
            is: Some(is),
            ..Default::default()
        }
    }

    pub fn var_eq(name: &str, eq: u16) -> Condition {
        Condition {
            var: Some(name.to_owned()),
            eq: Some(eq),
            ..Default::default()
        }
    }

    pub fn var_ge(name: &str, ge: u16) -> Condition {
        Condition {
            var: Some(name.to_owned()),
            ge: Some(ge),
            ..Default::default()
        }
    }

    /// The fact this condition is about; `Err` names what is wrong with it.
    pub fn fact(&self) -> std::result::Result<Fact, String> {
        match (&self.flag, &self.var, &self.visited) {
            (Some(f), None, None) => Ok(Fact::flag(f.clone())),
            (None, Some(v), None) => Ok(Fact::var(v.clone())),
            (None, None, Some(m)) => Ok(Fact::visited(m.clone())),
            (None, None, None) => Err("names no flag, var or visited".into()),
            _ => Err("names more than one of flag, var and visited".into()),
        }
    }

    /// Checks the fact and test agree: `is` for flags and visited maps,
    /// `eq`/`ge` for vars; a conclusion (`then`) may only use `is`/`eq`.
    fn validate(&self, conclusion: bool) -> std::result::Result<(), String> {
        let fact = self.fact()?;
        let (is, eq, ge) = (self.is.is_some(), self.eq.is_some(), self.ge.is_some());
        match fact {
            Fact::Flag { .. } | Fact::Visited { .. } if is && !eq && !ge => Ok(()),
            Fact::Flag { .. } | Fact::Visited { .. } => Err(format!("{fact} needs exactly `is`")),
            Fact::Var { .. } if conclusion && eq && !is && !ge => Ok(()),
            Fact::Var { .. } if conclusion => Err(format!("{fact} needs exactly `eq`")),
            Fact::Var { .. } if !is && (eq ^ ge) => Ok(()),
            Fact::Var { .. } => Err(format!("{fact} needs exactly one of `eq` and `ge`")),
        }
    }

    /// Whether the belief holds this by observation. `Some(frame)` is the
    /// frame that observation was verified at.
    pub fn observed_in(&self, belief: &WorldBelief) -> Option<u64> {
        fn observed<T>(k: &Knowledge<T>) -> Option<(&T, u64)> {
            (k.source == KnowledgeSource::Observed)
                .then_some(k.value.as_ref()?)
                .map(|v| (v, k.last_verified_frame.unwrap_or(0)))
        }
        match self.fact().ok()? {
            Fact::Flag { flag } => {
                let (v, frame) = observed(belief.flags.get(&flag)?)?;
                (Some(*v) == self.is).then_some(frame)
            }
            Fact::Visited { visited } => {
                let (v, frame) = observed(belief.visited.get(&visited)?)?;
                (Some(*v) == self.is).then_some(frame)
            }
            Fact::Var { var } => {
                let (v, frame) = observed(belief.vars.get(&var)?)?;
                let holds = match (self.eq, self.ge) {
                    (Some(eq), _) => *v == eq,
                    (None, Some(ge)) => *v >= ge,
                    (None, None) => false,
                };
                holds.then_some(frame)
            }
        }
    }

    /// Writes this as a `Derived` fact at `frame` unless the belief already
    /// holds it by observation or the same derivation. Returns whether the
    /// belief changed.
    fn derive(&self, belief: &mut WorldBelief, frame: u64) -> bool {
        fn put<T: PartialEq>(slot: &mut Knowledge<T>, value: T, frame: u64) -> bool {
            if slot.source == KnowledgeSource::Observed && slot.value.is_some() {
                return false;
            }
            let new = Knowledge::derived(value, frame);
            if *slot == new {
                return false;
            }
            *slot = new;
            true
        }
        match self.fact() {
            Ok(Fact::Flag { flag }) => {
                let Some(is) = self.is else { return false };
                put(belief.flags.entry(flag).or_default(), is, frame)
            }
            Ok(Fact::Visited { visited }) => {
                let Some(is) = self.is else { return false };
                put(belief.visited.entry(visited).or_default(), is, frame)
            }
            Ok(Fact::Var { var }) => {
                let Some(eq) = self.eq else { return false };
                put(belief.vars.entry(var).or_default(), eq, frame)
            }
            Err(_) => false,
        }
    }
}

/// Accepts a single premise or a list of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(Condition),
    Many(Vec<Condition>),
}

fn premises<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Condition>, D::Error> {
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(c) => vec![c],
        OneOrMany::Many(v) => v,
    })
}

/// `{"if": premise | [premises], "then": conclusion}`; every premise must
/// hold by observation for the conclusion to be derived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceRule {
    #[serde(rename = "if", deserialize_with = "premises")]
    pub premises: Vec<Condition>,
    #[serde(rename = "then")]
    pub conclusion: Condition,
    /// Free text for the reader of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl InferenceRule {
    fn validate(&self) -> std::result::Result<(), String> {
        if self.premises.is_empty() {
            return Err("has no premise".into());
        }
        for p in &self.premises {
            p.validate(false).map_err(|e| format!("premise {e}"))?;
        }
        self.conclusion
            .validate(true)
            .map_err(|e| format!("conclusion {e}"))
    }

    /// The latest frame among the premises' observations, when all hold.
    fn fires_at(&self, belief: &WorldBelief) -> Option<u64> {
        self.premises
            .iter()
            .map(|p| p.observed_in(belief))
            .try_fold(0, |acc, f| Some(acc.max(f?)))
    }
}

/// The rules of `inference.json`, in file order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InferenceRules {
    pub rules: Vec<InferenceRule>,
}

impl InferenceRules {
    pub fn new(rules: Vec<InferenceRule>) -> Result<InferenceRules> {
        let set = InferenceRules { rules };
        set.validate()?;
        Ok(set)
    }

    /// Reads `path` (e.g. `data/rules/inference.json`).
    pub fn load(path: impl AsRef<Path>) -> Result<InferenceRules> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let set: InferenceRules = serde_json::from_str(&text)
            .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
        set.validate()
            .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
        Ok(set)
    }

    fn validate(&self) -> Result<()> {
        for (i, rule) in self.rules.iter().enumerate() {
            rule.validate()
                .map_err(|e| Error::InvalidData(format!("inference rule {i} {e}")))?;
        }
        Ok(())
    }

    /// Runs the rules to a fixed point (in file order, passes until nothing
    /// changes). Returns the number of facts derived or changed.
    pub fn apply(&self, belief: &mut WorldBelief) -> usize {
        let mut changed = 0;
        loop {
            let before = changed;
            for rule in &self.rules {
                if let Some(frame) = rule.fires_at(belief) {
                    if rule.conclusion.derive(belief, frame) {
                        changed += 1;
                    }
                }
            }
            if changed == before {
                return changed;
            }
        }
    }
}

/// [`InferenceRules::apply`].
pub fn apply(belief: &mut WorldBelief, rules: &InferenceRules) -> usize {
    rules.apply(belief)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(json: &str) -> InferenceRules {
        InferenceRules::new(serde_json::from_str(json).unwrap()).unwrap()
    }

    const BADGE: &str = r#"[
        {"if": {"flag": "FLAG_BADGE01_GET", "is": true},
         "then": {"flag": "FLAG_DEFEATED_BROCK", "is": true}}
    ]"#;

    #[test]
    fn observed_premises_derive_conclusions() {
        let r = rules(BADGE);
        let mut b = WorldBelief::default();
        assert_eq!(r.apply(&mut b), 0);
        assert_eq!(b.flag("FLAG_DEFEATED_BROCK"), Knowledge::unknown());
        b.flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::observed(true, 7));
        assert_eq!(r.apply(&mut b), 1);
        assert_eq!(b.flag("FLAG_DEFEATED_BROCK"), Knowledge::derived(true, 7));
        // Idempotent.
        assert_eq!(r.apply(&mut b), 0);
    }

    #[test]
    fn tracked_premises_do_not_fire() {
        let r = rules(BADGE);
        let mut b = WorldBelief::default();
        b.flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::tracked(true, Some(3)));
        assert_eq!(r.apply(&mut b), 0);
        b.flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::observed(false, 3));
        assert_eq!(r.apply(&mut b), 0);
        assert_eq!(b.flag("FLAG_DEFEATED_BROCK"), Knowledge::unknown());
    }

    #[test]
    fn observations_are_never_overwritten() {
        let r = rules(BADGE);
        let mut b = WorldBelief::default();
        b.flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::observed(true, 7));
        b.flags
            .insert("FLAG_DEFEATED_BROCK".into(), Knowledge::observed(false, 9));
        assert_eq!(r.apply(&mut b), 0);
        assert_eq!(b.flag("FLAG_DEFEATED_BROCK"), Knowledge::observed(false, 9));
        // A tracked value is replaced by the derivation.
        b.flags.insert(
            "FLAG_DEFEATED_BROCK".into(),
            Knowledge::tracked(false, None),
        );
        assert_eq!(r.apply(&mut b), 1);
        assert_eq!(b.flag("FLAG_DEFEATED_BROCK"), Knowledge::derived(true, 7));
    }

    #[test]
    fn premises_can_be_many_and_over_vars_and_visits() {
        let r = rules(
            r#"[
            {"if": [{"var": "VAR_MAP_SCENE_X", "ge": 2}, {"visited": "PewterCity", "is": true}],
             "then": {"var": "VAR_Y", "eq": 1}}
        ]"#,
        );
        let mut b = WorldBelief::default();
        b.vars
            .insert("VAR_MAP_SCENE_X".into(), Knowledge::observed(3, 4));
        assert_eq!(r.apply(&mut b), 0, "one premise missing");
        b.visited
            .insert("PewterCity".into(), Knowledge::observed(true, 6));
        assert_eq!(r.apply(&mut b), 1);
        assert_eq!(
            b.var("VAR_Y"),
            Knowledge::derived(1, 6),
            "latest premise frame"
        );
        b.vars
            .insert("VAR_MAP_SCENE_X".into(), Knowledge::observed(1, 8));
        let mut fresh = WorldBelief {
            vars: b.vars.clone(),
            visited: b.visited.clone(),
            ..Default::default()
        };
        fresh.vars.remove("VAR_Y");
        assert_eq!(r.apply(&mut fresh), 0, "ge not met");
    }

    #[test]
    fn invalid_rules_are_rejected_with_their_index() {
        let bad = |json: &str| {
            InferenceRules::new(serde_json::from_str(json).unwrap())
                .unwrap_err()
                .to_string()
        };
        assert!(
            bad(r#"[{"if": {"flag": "A", "is": true}, "then": {"flag": "B"}}]"#)
                .contains("rule 0 conclusion")
        );
        assert!(bad(
            r#"[{"if": {"flag": "A", "is": true}, "then": {"flag": "B", "is": true}},
                {"if": {"var": "V", "is": true}, "then": {"flag": "B", "is": true}}]"#
        )
        .contains("rule 1 premise"));
        // `[]` also reads as an empty premise (serde takes a sequence for a
        // struct), which names no fact.
        let empty = bad(r#"[{"if": [], "then": {"flag": "B", "is": true}}]"#);
        assert!(
            empty.contains("rule 0 premise") && empty.contains("names no flag"),
            "{empty}"
        );
        assert!(
            bad(r#"[{"if": {"var": "V", "ge": 1}, "then": {"var": "W", "ge": 2}}]"#)
                .contains("exactly `eq`")
        );
        assert!(serde_json::from_str::<Vec<InferenceRule>>(
            r#"[{"if": {"flag": "A", "is": true}, "then": {"flag": "B", "is": true}, "extra": 1}]"#
        )
        .is_err());
    }

    #[test]
    fn seed_file_loads_and_covers_the_eight_badges() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/rules/inference.json");
        let r = InferenceRules::load(&path).unwrap();
        let leaders = [
            "BROCK",
            "MISTY",
            "LT_SURGE",
            "ERIKA",
            "KOGA",
            "SABRINA",
            "BLAINE",
            "LEADER_GIOVANNI",
        ];
        let mut b = WorldBelief::default();
        for i in 1..=8 {
            b.flags
                .insert(format!("FLAG_BADGE0{i}_GET"), Knowledge::observed(true, i));
        }
        assert_eq!(apply(&mut b, &r), 8);
        for (i, leader) in leaders.iter().enumerate() {
            assert_eq!(
                b.flag(&format!("FLAG_DEFEATED_{leader}")),
                Knowledge::derived(true, i as u64 + 1),
                "{leader}"
            );
        }
        assert!(InferenceRules::load(path.with_file_name("missing.json")).is_err());
    }
}
