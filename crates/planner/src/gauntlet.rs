//! Battles won in one go: what a white-out undoes (`DoWhiteOut` runs
//! `EventScript_ResetEliteFourEnd`, clearing `FLAG_DEFEATED_LORELEI` …
//! `FLAG_DEFEATED_CHAMP` and `VAR_MAP_SCENE_POKEMON_LEAGUE`). The rooms
//! lock behind the player, so there is no healing, training or catching
//! between them: the party must be ready for every one before the first
//! (fleet continue-6 went in ready for LORELEI alone, 98%, with LANCE and
//! the Champion at 0%, saved in her room, and lost there every reload).

use std::collections::BTreeSet;

use pokebot_state::GameEvent;
use pokebot_world::events::{Condition, Effect, Events, Val};
use pokebot_world::gates::{is_local_flag, is_local_var};
use pokebot_world::predicate::Truth;

use crate::belief_adapter::StateBelief;
use crate::intents::{first_battle, GoalBelief, GoalPredicate};

/// One battle of the gauntlet: the flag its win sets (and a white-out
/// clears), and each trainer fought for it with the path's conditions
/// (the Champion's team follows the starter; the rematch teams come after
/// the Hall of Fame).
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    pub flag: String,
    pub battles: Vec<(String, Vec<Requirement>)>,
}

/// A condition of a battle, and whether a new game meets it: a flag
/// nothing observed is as a new game left it.
#[derive(Debug, Clone, PartialEq)]
pub struct Requirement {
    pub holds: GoalPredicate,
    pub at_start: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gauntlet {
    pub stages: Vec<Stage>,
    /// What a white-out leaves true: the gauntlet not started.
    pub reset: Vec<GoalPredicate>,
    /// The maps its battles are fought on (the rooms, locked behind).
    pub maps: BTreeSet<String>,
    /// The white-out's resets, as the belief takes them.
    pub undo: Vec<GameEvent>,
}

impl Gauntlet {
    pub fn from_events(events: &Events) -> Gauntlet {
        let reset = events
            .whiteout
            .does
            .iter()
            .flat_map(GoalPredicate::from_effect)
            .collect();
        let mut stages: Vec<Stage> = events
            .whiteout
            .cleared_flags()
            .map(|flag| Stage {
                flag: flag.to_string(),
                battles: Vec::new(),
            })
            .collect();
        let undo = events
            .whiteout
            .does
            .iter()
            .filter_map(|e| match e {
                Effect::Clear { clear: flag } | Effect::Undefeated { undefeated: flag } => {
                    Some(GameEvent::FlagTracked {
                        flag: flag.clone(),
                        value: false,
                    })
                }
                Effect::Var { var, change } => change
                    .eq
                    .as_ref()
                    .and_then(Val::as_int)
                    .and_then(|v| u16::try_from(v).ok())
                    .map(|value| GameEvent::VarTracked {
                        var: var.clone(),
                        value,
                    }),
                _ => None,
            })
            .collect();
        let initial: BTreeSet<&str> = events.initial.set.iter().map(String::as_str).collect();
        let mut maps = BTreeSet::new();
        for script in events.scripts.values() {
            for path in &script.paths {
                let Some(trainer) = first_battle(path) else {
                    continue;
                };
                for stage in &mut stages {
                    let sets = path
                        .does
                        .iter()
                        .any(|e| matches!(e, Effect::Set { set } if *set == stage.flag));
                    if !sets {
                        continue;
                    }
                    maps.extend(script.map.iter().cloned());
                    if stage.battles.iter().any(|(t, _)| t == trainer) {
                        continue;
                    }
                    // What picks this trainer over the stage's others: not
                    // the stage's own flag, the trainer's own defeat or a
                    // map's temporary state.
                    let when = path
                        .when
                        .iter()
                        .filter(|c| match c {
                            Condition::Flag { flag, .. } => {
                                *flag != stage.flag && !is_local_flag(flag)
                            }
                            Condition::Var { var, .. } => !is_local_var(var),
                            Condition::Trainer { .. } => false,
                            _ => true,
                        })
                        .filter_map(|c| {
                            let at_start = match c {
                                Condition::Flag { flag, is } => {
                                    initial.contains(flag.as_str()) == *is
                                }
                                _ => true,
                            };
                            GoalPredicate::from_condition(c)
                                .map(|holds| Requirement { holds, at_start })
                        })
                        .collect();
                    stage.battles.push((trainer.to_string(), when));
                }
            }
        }
        stages.retain(|s| !s.battles.is_empty());
        Gauntlet {
            stages,
            reset,
            maps,
            undo,
        }
    }

    pub fn contains(&self, trainer: &str) -> bool {
        self.stages
            .iter()
            .any(|s| s.battles.iter().any(|(t, _)| t == trainer))
    }

    /// Every trainer the party must beat in one go with `trainer`:
    /// `trainer` first, then the trainers of the stages not yet won, each
    /// stage's battles the belief doesn't rule out (a flag it doesn't know
    /// as a new game left it; all, when it can't tell). Just `trainer`
    /// outside a gauntlet.
    pub fn with(&self, trainer: &str, belief: &StateBelief<'_>) -> Vec<String> {
        let mut out = vec![trainer.to_string()];
        if !self.contains(trainer) {
            return out;
        }
        for stage in &self.stages {
            if stage.battles.iter().any(|(t, _)| t == trainer)
                || belief.eval_goal(&GoalPredicate::flag(&stage.flag, true)) == Truth::True
            {
                continue;
            }
            for (t, when) in &stage.battles {
                let ruled_out = when.iter().any(|r| match belief.eval_goal(&r.holds) {
                    Truth::False => true,
                    Truth::Unknown => !r.at_start,
                    Truth::True => false,
                });
                if !ruled_out && !out.contains(t) {
                    out.push(t.clone());
                }
            }
        }
        out
    }

    /// Whether the gauntlet is under way (in one of its rooms, a battle
    /// won): a save then would be reloaded inside it, with no way out to
    /// heal or train.
    pub fn in_progress(&self, belief: &StateBelief<'_>) -> bool {
        belief
            .pose
            .as_ref()
            .is_some_and(|p| self.maps.contains(&p.map))
            || self
                .reset
                .iter()
                .any(|p| belief.eval_goal(p) == Truth::False)
    }
}
