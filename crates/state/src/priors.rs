//! Priors over unknown facts (`priors.json`): what the evidence in the
//! belief says about a flag or a visit that was never observed, as a
//! probability the planner weighs against the cost of a probe. Declarative
//! like the inference rules: `{"fact": {"visited": "PewterCity"},
//! "given": [{"flag": "FLAG_BADGE01_GET", "is": true}], "p": 1.0}`.

use std::path::Path;

use pokebot_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::{Condition, Fact, Knowledge, KnowledgeSource, WorldBelief};

/// What is assumed with no evidence either way.
pub const NO_EVIDENCE: f64 = 0.5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorRule {
    /// A flag or a visited map (vars have no "probability of being true").
    pub fact: Fact,
    /// Premises that must all hold by observation for the rule to apply;
    /// empty is a base rate.
    #[serde(default)]
    pub given: Vec<Condition>,
    /// P(fact is true | given), in `0.0..=1.0`.
    pub p: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl PriorRule {
    fn validate(&self) -> std::result::Result<(), String> {
        if matches!(self.fact, Fact::Var { .. }) {
            return Err(format!("{}: priors are over flags and visits", self.fact));
        }
        if !(0.0..=1.0).contains(&self.p) {
            return Err(format!("p = {} is not a probability", self.p));
        }
        for c in &self.given {
            let fact = c.fact().map_err(|e| format!("premise {e}"))?;
            if !premise_well_formed(c) {
                return Err(format!("premise {fact} has no test"));
            }
        }
        Ok(())
    }

    fn matches(&self, belief: &WorldBelief) -> bool {
        self.given.iter().all(|c| c.observed_in(belief).is_some())
    }
}

/// A premise needs `is` for flags/visits and `eq` or `ge` for vars.
fn premise_well_formed(c: &Condition) -> bool {
    match c.fact() {
        Ok(Fact::Flag { .. } | Fact::Visited { .. }) => c.is.is_some(),
        Ok(Fact::Var { .. }) => c.eq.is_some() || c.ge.is_some(),
        Err(_) => false,
    }
}

/// The rules of `priors.json`, in file order.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Priors {
    pub rules: Vec<PriorRule>,
}

impl Priors {
    pub fn new(rules: Vec<PriorRule>) -> Result<Priors> {
        let set = Priors { rules };
        set.validate()?;
        Ok(set)
    }

    /// Reads `path` (e.g. `data/rules/priors.json`).
    pub fn load(path: impl AsRef<Path>) -> Result<Priors> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let set: Priors = serde_json::from_str(&text)
            .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
        set.validate()
            .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
        Ok(set)
    }

    fn validate(&self) -> Result<()> {
        for (i, rule) in self.rules.iter().enumerate() {
            rule.validate()
                .map_err(|e| Error::InvalidData(format!("prior rule {i}: {e}")))?;
        }
        Ok(())
    }

    /// P(`fact` is true) given what the belief holds by observation.
    ///
    /// An observed fact is certain (1.0 or 0.0). Otherwise the most specific
    /// rule whose premises all hold wins (the one with most premises); among
    /// equally specific rules the lowest `p` (the cautious one); with no
    /// rule, [`NO_EVIDENCE`]. A var is never "true", so it gets
    /// [`NO_EVIDENCE`] too.
    pub fn probability(&self, belief: &WorldBelief, fact: &Fact) -> f64 {
        fn certain(k: &Knowledge<bool>) -> Option<f64> {
            match (k.source, k.value) {
                (KnowledgeSource::Observed, Some(true)) => Some(1.0),
                (KnowledgeSource::Observed, Some(false)) => Some(0.0),
                _ => None,
            }
        }
        let known = match fact {
            Fact::Flag { flag } => belief.flags.get(flag).and_then(certain),
            Fact::Visited { visited } => belief.visited.get(visited).and_then(certain),
            Fact::Var { .. } => return NO_EVIDENCE,
        };
        if let Some(p) = known {
            return p;
        }
        let mut best: Option<(usize, f64)> = None;
        for rule in self.rules.iter().filter(|r| r.fact == *fact) {
            if !rule.matches(belief) {
                continue;
            }
            let n = rule.given.len();
            best = Some(match best {
                None => (n, rule.p),
                Some((m, p)) if n > m || (n == m && rule.p < p) => (n, rule.p),
                Some(b) => b,
            });
        }
        best.map_or(NO_EVIDENCE, |(_, p)| p)
    }
}

/// [`Priors::probability`].
pub fn probability(priors: &Priors, belief: &WorldBelief, fact: &Fact) -> f64 {
    priors.probability(belief, fact)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priors(json: &str) -> Priors {
        Priors::new(serde_json::from_str(json).unwrap()).unwrap()
    }

    const RULES: &str = r#"[
        {"fact": {"visited": "CeruleanCity"}, "given": [], "p": 0.3},
        {"fact": {"visited": "CeruleanCity"},
         "given": [{"flag": "FLAG_BADGE02_GET", "is": true}], "p": 1.0},
        {"fact": {"visited": "CeruleanCity"},
         "given": [{"flag": "FLAG_BADGE01_GET", "is": true}], "p": 0.6},
        {"fact": {"visited": "CeruleanCity"},
         "given": [{"flag": "FLAG_BADGE01_GET", "is": true}], "p": 0.4},
        {"fact": {"visited": "CeruleanCity"},
         "given": [{"flag": "FLAG_BADGE01_GET", "is": true}, {"visited": "Route4", "is": true}],
         "p": 0.9}
    ]"#;

    #[test]
    fn observed_facts_are_certain() {
        let p = priors(RULES);
        let mut b = WorldBelief::default();
        b.visited
            .insert("CeruleanCity".into(), Knowledge::observed(true, 1));
        assert_eq!(p.probability(&b, &Fact::visited("CeruleanCity")), 1.0);
        b.visited
            .insert("CeruleanCity".into(), Knowledge::observed(false, 1));
        assert_eq!(p.probability(&b, &Fact::visited("CeruleanCity")), 0.0);
        // Tracked is not certain: the rules apply.
        b.visited
            .insert("CeruleanCity".into(), Knowledge::tracked(true, None));
        assert_eq!(p.probability(&b, &Fact::visited("CeruleanCity")), 0.3);
    }

    #[test]
    fn most_specific_rule_wins_and_ties_go_to_the_lowest() {
        let p = priors(RULES);
        let mut b = WorldBelief::default();
        assert_eq!(p.probability(&b, &Fact::visited("CeruleanCity")), 0.3);
        assert_eq!(
            p.probability(&b, &Fact::visited("PalletTown")),
            NO_EVIDENCE,
            "no rule"
        );
        assert_eq!(p.probability(&b, &Fact::var("VAR_X")), NO_EVIDENCE);
        b.flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::observed(true, 1));
        assert_eq!(
            p.probability(&b, &Fact::visited("CeruleanCity")),
            0.4,
            "tie → lowest"
        );
        b.visited
            .insert("Route4".into(), Knowledge::observed(true, 2));
        assert_eq!(
            p.probability(&b, &Fact::visited("CeruleanCity")),
            0.9,
            "two premises beat one"
        );
        b.flags
            .insert("FLAG_BADGE02_GET".into(), Knowledge::observed(true, 3));
        assert_eq!(
            p.probability(&b, &Fact::visited("CeruleanCity")),
            0.9,
            "the one-premise 1.0 rule is less specific"
        );
        // Derived premises don't count as evidence.
        let mut d = WorldBelief::default();
        d.flags
            .insert("FLAG_BADGE02_GET".into(), Knowledge::derived(true, 3));
        assert_eq!(p.probability(&d, &Fact::visited("CeruleanCity")), 0.3);
    }

    #[test]
    fn invalid_rules_are_rejected() {
        let bad = |json: &str| {
            Priors::new(serde_json::from_str(json).unwrap())
                .unwrap_err()
                .to_string()
        };
        assert!(bad(r#"[{"fact": {"var": "VAR_X"}, "p": 0.5}]"#).contains("rule 0"));
        assert!(bad(r#"[{"fact": {"flag": "F"}, "p": 1.5}]"#).contains("probability"));
        assert!(
            bad(r#"[{"fact": {"flag": "F"}, "given": [{"flag": "G"}], "p": 0.5}]"#)
                .contains("no test")
        );
    }

    #[test]
    fn seed_file_gives_the_towns_on_the_way_to_each_badge() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/rules/priors.json");
        let p = Priors::load(&path).unwrap();
        let mut b = WorldBelief::default();
        assert_eq!(
            probability(&p, &b, &Fact::visited("PewterCity")),
            NO_EVIDENCE
        );
        b.flags
            .insert("FLAG_BADGE01_GET".into(), Knowledge::observed(true, 1));
        for town in ["PalletTown", "ViridianCity", "PewterCity"] {
            assert_eq!(p.probability(&b, &Fact::visited(town)), 1.0, "{town}");
        }
        assert_eq!(
            p.probability(&b, &Fact::visited("CeruleanCity")),
            NO_EVIDENCE
        );
        b.flags
            .insert("FLAG_BADGE04_GET".into(), Knowledge::observed(true, 2));
        for town in [
            "Route4",
            "CeruleanCity",
            "VermilionCity",
            "LavenderTown",
            "CeladonCity",
        ] {
            assert_eq!(p.probability(&b, &Fact::visited(town)), 1.0, "{town}");
        }
    }
}
