//! What the bot has done in the world, across sessions: the tiles walked,
//! the people and signs seen and talked to (what they said, what was
//! answered, whether it taught anything), and how well each way of getting
//! unstuck ([`crate::recourse`]) has paid off.
//!
//! Kept beside the checkpoint (`ledger.json`) and not reset by a reload:
//! "this person taught nothing while the belief was X" stays true after a
//! faint, because every talk is stamped with a [`fingerprint`] of what the
//! belief knew. A talk is worth repeating once the fingerprint changed
//! (a flag set, an item gained), or with the other answer to its question.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use pokebot_core::{Error, Result};
use pokebot_state::{GameState, PlayerPose};
use serde::{Deserialize, Serialize};

/// Talks kept per target (the newest).
const TALKS_KEPT: usize = 8;
/// Levels our side is judged below its own against a trainer, per battle
/// lost to them (see [`Ledger::handicaps`]), and at most.
const LOSS_HANDICAP_LEVELS: u32 = 3;
const MAX_HANDICAP_LEVELS: u32 = 15;

/// Everything logged, by map.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub maps: BTreeMap<String, MapLog>,
    /// Outcomes of each recourse kind (`explore`, `probe`, …).
    #[serde(default)]
    pub recourses: BTreeMap<String, Stats>,
    /// Trainer battles lost (a white-out) since the trainer was last
    /// beaten, by trainer id: the battle estimate was wrong about them.
    #[serde(default)]
    pub losses: BTreeMap<String, u32>,
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MapLog {
    /// Times the player entered the map.
    #[serde(default)]
    pub visits: u32,
    #[serde(default)]
    pub tiles: BTreeSet<(i32, i32)>,
    /// By target key ([`key`]): `object:3`, `sign:4,5`, `sprite:7,6`.
    #[serde(default)]
    pub targets: BTreeMap<String, TargetLog>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetLog {
    /// Visits of the map it was seen on.
    #[serde(default)]
    pub seen: u32,
    #[serde(default)]
    pub last_at: Option<(i32, i32)>,
    /// The visit it was last seen on (counts `seen` once per visit).
    #[serde(default)]
    pub seen_visit: u32,
    #[serde(default)]
    pub talks: Vec<Talk>,
}

/// One interaction and what came of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Talk {
    /// [`fingerprint`] of the belief before the talk.
    pub knowledge: u64,
    /// The compiled script, when it could be told.
    #[serde(default)]
    pub script: Option<String>,
    /// Text labels recognised, in order.
    #[serde(default)]
    pub said: Vec<String>,
    /// Answers given, YES = true.
    #[serde(default)]
    pub answered: Vec<bool>,
    /// The belief changed.
    pub learnt: bool,
    /// Why it failed, if it did.
    #[serde(default)]
    pub error: Option<String>,
}

/// A recourse kind's record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub tries: u32,
    /// Tries after which the belief had changed.
    pub unblocked: u32,
    pub seconds: f64,
}

impl Stats {
    /// Laplace-smoothed success rate around `prior` (worth two tries).
    pub fn rate(&self, prior: f64) -> f64 {
        (f64::from(self.unblocked) + 2.0 * prior) / (f64::from(self.tries) + 2.0)
    }
}

/// The key of map object `local_id`.
pub fn object_key(local_id: u32) -> String {
    format!("object:{local_id}")
}

/// Whether a target is worth trying now, and with which answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// Never tried.
    New,
    /// Tried, but the belief changed since.
    Changed,
    /// Tried under this belief; its question's other answer is left.
    OtherAnswer(Vec<bool>),
    /// Tried under this belief with every answer worth giving.
    Spent,
}

impl Freshness {
    pub fn is_fresh(&self) -> bool {
        !matches!(self, Freshness::Spent)
    }
}

/// A stable hash of what a conversation can change: flag and var values,
/// the script paths run and the bag's items (values only: a re-read with
/// nothing new doesn't count).
pub fn fingerprint(state: &GameState) -> u64 {
    let flags: Vec<(&String, Option<bool>)> = state
        .world
        .flags
        .iter()
        .map(|(k, v)| (k, v.value))
        .collect();
    let vars: Vec<(&String, Option<u16>)> =
        state.world.vars.iter().map(|(k, v)| (k, v.value)).collect();
    let paths: BTreeSet<&(String, usize)> = state.world.paths_run.iter().collect();
    let bag: Vec<_> = state
        .bag
        .pockets
        .iter()
        .map(|(p, k)| (format!("{p:?}"), k.value.clone()))
        .collect();
    let text = serde_json::to_string(&(flags, vars, paths, bag)).unwrap_or_default();
    // FNV-1a: stable across builds, unlike `DefaultHasher`.
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl Ledger {
    /// `ledger.json` beside `state.json`.
    pub fn path_for(state: &Path) -> PathBuf {
        state.with_file_name("ledger.json")
    }

    /// Reads `path` (empty when missing); [`Ledger::store`] writes back to it.
    pub fn load(path: &Path) -> Result<Ledger> {
        let mut ledger = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(e) => return Err(Error::io(path, e)),
        };
        ledger.path = Some(path.to_owned());
        Ok(ledger)
    }

    /// Writes the ledger if it has a path and changed.
    pub fn store(&mut self) -> Result<()> {
        let Some(path) = self.path.as_ref().filter(|_| self.dirty) else {
            return Ok(());
        };
        let json = serde_json::to_vec(self).map_err(|e| Error::InvalidData(e.to_string()))?;
        crate::checkpoint::write_atomic(path, &json)?;
        self.dirty = false;
        Ok(())
    }

    pub fn map(&self, map: &str) -> Option<&MapLog> {
        self.maps.get(map)
    }

    /// The player entered `map`.
    pub fn entered(&mut self, map: &str) {
        self.maps.entry(map.to_owned()).or_default().visits += 1;
        self.dirty = true;
    }

    /// The player stood on a tile.
    pub fn walked(&mut self, pose: &PlayerPose) {
        let log = self.maps.entry(pose.map.clone()).or_default();
        if log.tiles.insert((pose.x, pose.y)) {
            self.dirty = true;
        }
    }

    /// The player went from `from` to `to`: a straight step of a few
    /// tiles on one map (frames the perception skipped) books the tiles
    /// between; anything else just `to`.
    pub fn walked_between(&mut self, from: &PlayerPose, to: &PlayerPose) {
        let (dx, dy) = (to.x - from.x, to.y - from.y);
        if from.map != to.map || (dx != 0 && dy != 0) || dx.abs().max(dy.abs()) > 8 {
            return self.walked(to);
        }
        let n = dx.abs().max(dy.abs());
        for k in 0..=n {
            self.walked(&PlayerPose {
                map: to.map.clone(),
                x: from.x + dx.signum() * k,
                y: from.y + dy.signum() * k,
            });
        }
    }

    /// A target was on screen at `at`.
    pub fn seen(&mut self, map: &str, key: &str, at: (i32, i32)) {
        let log = self.maps.entry(map.to_owned()).or_default();
        let visit = log.visits;
        let target = log.targets.entry(key.to_owned()).or_default();
        if target.seen == 0 || target.seen_visit != visit {
            target.seen += 1;
            target.seen_visit = visit;
            self.dirty = true;
        }
        if target.last_at != Some(at) {
            target.last_at = Some(at);
            self.dirty = true;
        }
    }

    pub fn talked(&mut self, map: &str, key: &str, talk: Talk) {
        let talks = &mut self
            .maps
            .entry(map.to_owned())
            .or_default()
            .targets
            .entry(key.to_owned())
            .or_default()
            .talks;
        talks.push(talk);
        if talks.len() > TALKS_KEPT {
            talks.remove(0);
        }
        self.dirty = true;
    }

    /// Whether talking to `key` again can teach anything under the belief
    /// `knowledge`. `yes_safe` tells whether answering YES to its script's
    /// question is safe (spends nothing, starts no battle).
    pub fn freshness(&self, map: &str, key: &str, knowledge: u64, yes_safe: bool) -> Freshness {
        let Some(target) = self.map(map).and_then(|m| m.targets.get(key)) else {
            return Freshness::New;
        };
        if target.talks.is_empty() {
            return Freshness::New;
        }
        let now: Vec<&Talk> = target
            .talks
            .iter()
            .filter(|t| t.knowledge == knowledge)
            .collect();
        if now.is_empty() {
            return Freshness::Changed;
        }
        if now.iter().any(|t| t.learnt) {
            return Freshness::Spent;
        }
        // A question was asked: its other first answer, when not tried.
        let tried: BTreeSet<bool> = now
            .iter()
            .filter_map(|t| t.answered.first())
            .copied()
            .collect();
        match tried.iter().next() {
            Some(&first) if tried.len() == 1 && (first || yes_safe) => {
                Freshness::OtherAnswer(vec![!first])
            }
            _ => Freshness::Spent,
        }
    }

    /// A trainer battle was lost: the next plan for it asks for more.
    pub fn lost_to(&mut self, trainer: &str) {
        *self.losses.entry(trainer.to_owned()).or_default() += 1;
        self.dirty = true;
    }

    /// A trainer was beaten: its losses are over.
    pub fn beat(&mut self, trainer: &str) {
        if self.losses.remove(trainer).is_some() {
            self.dirty = true;
        }
    }

    /// Levels to judge our party below its own against each trainer lost
    /// to: the estimate that sent it to lose was too kind (fleet worker 5
    /// lost to MISTY 66 times from the same save, the plan unchanged, her
    /// STARYU's RECOVER outside the model). Each loss asks for more
    /// training before the next try, and some other goal may come first.
    pub fn handicaps(&self) -> BTreeMap<String, u8> {
        self.losses
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(t, n)| {
                let levels = (n * LOSS_HANDICAP_LEVELS).min(MAX_HANDICAP_LEVELS);
                (t.clone(), levels as u8)
            })
            .collect()
    }

    pub fn stats(&self, kind: &str) -> Stats {
        self.recourses.get(kind).copied().unwrap_or_default()
    }

    /// Books a recourse run of `kind`.
    pub fn tried(&mut self, kind: &str, unblocked: bool, seconds: f64) {
        let s = self.recourses.entry(kind.to_owned()).or_default();
        s.tries += 1;
        s.unblocked += u32::from(unblocked);
        s.seconds += seconds;
        self.dirty = true;
    }

    /// Share of talks, over every map, that taught something (for
    /// estimating a target's chance), with counts.
    pub fn talk_rate(&self) -> (u32, u32) {
        self.maps
            .values()
            .flat_map(|m| m.targets.values())
            .flat_map(|t| &t.talks)
            .fold((0, 0), |(learnt, all), t| {
                (learnt + u32::from(t.learnt), all + 1)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn talk(knowledge: u64, answered: Vec<bool>, learnt: bool) -> Talk {
        Talk {
            knowledge,
            script: None,
            said: Vec::new(),
            answered,
            learnt,
            error: None,
        }
    }

    #[test]
    fn a_talk_is_fresh_again_once_the_belief_changes() {
        let mut l = Ledger::default();
        assert_eq!(l.freshness("M", "object:1", 7, false), Freshness::New);
        l.talked("M", "object:1", talk(7, vec![], false));
        assert_eq!(l.freshness("M", "object:1", 7, false), Freshness::Spent);
        assert_eq!(l.freshness("M", "object:1", 8, false), Freshness::Changed);
    }

    #[test]
    fn a_question_is_asked_again_with_its_other_answer() {
        let mut l = Ledger::default();
        l.talked("M", "object:1", talk(7, vec![false], false));
        // YES only when it is known to be safe.
        assert_eq!(l.freshness("M", "object:1", 7, false), Freshness::Spent);
        assert_eq!(
            l.freshness("M", "object:1", 7, true),
            Freshness::OtherAnswer(vec![true])
        );
        l.talked("M", "object:1", talk(7, vec![true], false));
        assert_eq!(l.freshness("M", "object:1", 7, true), Freshness::Spent);
        // NO is always safe to try after a YES.
        let mut l = Ledger::default();
        l.talked("M", "object:2", talk(7, vec![true], false));
        assert_eq!(
            l.freshness("M", "object:2", 7, false),
            Freshness::OtherAnswer(vec![false])
        );
    }

    #[test]
    fn a_straight_step_books_the_tiles_between() {
        let pose = |map: &str, x, y| PlayerPose {
            map: map.into(),
            x,
            y,
        };
        let mut l = Ledger::default();
        l.walked_between(&pose("M", 9, 7), &pose("M", 9, 10));
        l.walked_between(&pose("M", 9, 10), &pose("M", 11, 11));
        l.walked_between(&pose("M", 11, 11), &pose("N", 1, 1));
        let tiles: Vec<_> = l.maps["M"].tiles.iter().copied().collect();
        assert_eq!(tiles, vec![(9, 7), (9, 8), (9, 9), (9, 10), (11, 11)]);
        assert!(l.maps["N"].tiles.contains(&(1, 1)));
    }

    #[test]
    fn sightings_count_once_per_visit_and_talks_are_bounded() {
        let mut l = Ledger::default();
        l.entered("M");
        l.seen("M", "object:1", (3, 4));
        l.seen("M", "object:1", (3, 5));
        l.entered("M");
        l.seen("M", "object:1", (3, 5));
        let t = &l.maps["M"].targets["object:1"];
        assert_eq!((t.seen, t.last_at), (2, Some((3, 5))));
        for _ in 0..20 {
            l.talked("M", "object:1", talk(1, vec![], false));
        }
        assert_eq!(l.maps["M"].targets["object:1"].talks.len(), TALKS_KEPT);
        assert_eq!(l.talk_rate(), (0, TALKS_KEPT as u32));
    }

    #[test]
    fn the_fingerprint_ignores_rereads_but_not_new_values() {
        use pokebot_state::Knowledge;
        let mut s = GameState::default();
        s.world
            .flags
            .insert("FLAG_A".into(), Knowledge::observed(true, 1));
        let a = fingerprint(&s);
        s.world
            .flags
            .insert("FLAG_A".into(), Knowledge::observed(true, 99));
        assert_eq!(fingerprint(&s), a);
        s.world
            .flags
            .insert("FLAG_B".into(), Knowledge::observed(false, 99));
        assert_ne!(fingerprint(&s), a);
    }

    #[test]
    fn stats_start_at_the_prior_and_follow_the_outcomes() {
        let mut l = Ledger::default();
        assert!((l.stats("explore").rate(0.3) - 0.3).abs() < 1e-9);
        l.tried("explore", false, 60.0);
        l.tried("explore", false, 60.0);
        assert!(l.stats("explore").rate(0.3) < 0.3);
        l.tried("explore", true, 60.0);
        assert!(l.stats("explore").rate(0.3) > 0.2);
    }

    /// Each loss to a trainer judges the party lower against them, up to
    /// a limit; beating them clears it.
    #[test]
    fn losses_handicap_a_trainer_until_beaten() {
        let mut l = Ledger::default();
        assert!(l.handicaps().is_empty());
        l.lost_to("TRAINER_LEADER_MISTY");
        l.lost_to("TRAINER_LEADER_MISTY");
        assert_eq!(l.handicaps().get("TRAINER_LEADER_MISTY"), Some(&6));
        for _ in 0..10 {
            l.lost_to("TRAINER_LEADER_MISTY");
        }
        assert_eq!(l.handicaps().get("TRAINER_LEADER_MISTY"), Some(&15));
        l.beat("TRAINER_LEADER_MISTY");
        assert!(l.handicaps().is_empty());
        // Kept across a reload (the ledger file).
        l.lost_to("TRAINER_LEADER_BROCK");
        let back: Ledger = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back.handicaps().get("TRAINER_LEADER_BROCK"), Some(&3));
    }
}
