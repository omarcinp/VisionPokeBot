//! `Explore`: the last resort when the plan is stuck on a map. Every
//! person and sign of the map with a script is talked to or read, the ones
//! on screen and nearest first, each once per session, until the belief
//! learns something (a flag, a var, an item, a script path): the plan is
//! then made again from what the game showed. Nothing is scripted: the
//! conversations are followed and recognised as in `RunScript`, their
//! paths resolved from the text read (on the Switch the plan went to
//! Bill's PC before talking to Bill, and nothing tried Bill himself).
//!
//! Trainers are left alone (talking to one starts a battle the plan did
//! not price), and so are objects known to be gone.

use std::collections::BTreeSet;

use pokebot_state::{Direction, GameState, PlayerPose};
use pokebot_world::events::Effect;
use pokebot_world::World;

use super::dialogue::{finish, object_shown};
use super::talk::{talk, TalkStep};
use super::{progress, Intent, Tool, ToolContext, ToolError, ToolOutcome};

/// Targets tried per `Explore` at most.
const MAX_TARGETS: usize = 8;

/// Something on the map to interact with.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Object {
        local_id: u32,
    },
    Sign {
        x: i32,
        y: i32,
        facing: Option<Direction>,
        script: String,
    },
}

/// The map's people and signs worth trying, in order: on screen first,
/// then the nearest; trainers, objects known gone and scriptless ones left
/// out. `shown(local_id)` is [`object_shown`].
pub fn targets(
    world: &World,
    pose: &PlayerPose,
    shown: &dyn Fn(u32) -> Option<bool>,
) -> Vec<Target> {
    let Some(map) = world.map(&pose.map) else {
        return Vec::new();
    };
    let fights = |script: &str| {
        world
            .events()
            .and_then(|e| e.script(script))
            .is_some_and(|s| {
                s.paths
                    .iter()
                    .any(|p| p.does.iter().any(|d| matches!(d, Effect::Battle { .. })))
            })
    };
    let dist = |x: i32, y: i32| (x - pose.x).abs() + (y - pose.y).abs();
    let mut out: Vec<(u8, i32, Target)> = Vec::new();
    for o in &map.objects {
        let Some(script) = o.script.as_deref().filter(|s| !s.is_empty() && *s != "0") else {
            continue;
        };
        let trainer = o
            .trainer_type
            .as_deref()
            .is_some_and(|t| t != "TRAINER_TYPE_NONE");
        if trainer || fights(script) {
            continue;
        }
        let rank = match shown(o.local_id) {
            Some(false) => continue,
            Some(true) => 0,
            None => 1,
        };
        let (x, y) = (o.x.unwrap_or(0), o.y.unwrap_or(0));
        out.push((
            rank,
            dist(x, y),
            Target::Object {
                local_id: o.local_id,
            },
        ));
    }
    for g in &map.signs {
        let Some(script) = g.script.clone() else {
            continue;
        };
        let is_sign = world
            .events()
            .and_then(|e| e.script(&script))
            .is_some_and(|s| s.kind == "sign");
        if !is_sign {
            continue;
        }
        out.push((
            1,
            dist(g.x, g.y),
            Target::Sign {
                x: g.x,
                y: g.y,
                facing: g.facing_dir(),
                script,
            },
        ));
    }
    out.sort();
    out.dedup_by(|a, b| a.2 == b.2);
    out.into_iter().map(|(_, _, t)| t).collect()
}

/// What the belief holds that a conversation can change.
fn learnt(state: &GameState) -> impl PartialEq {
    (
        state.world.flags.clone(),
        state.world.vars.clone(),
        state.world.paths_run.len(),
        state.bag.clone(),
    )
}

#[derive(Default)]
pub struct ExploreTool {
    /// Targets already tried this session, by map.
    tried: BTreeSet<(String, Target)>,
}

impl ExploreTool {
    fn explore(&mut self, ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
        let pose = ctx
            .pose()
            .ok_or_else(|| ToolError::Failed("explore: the position is unknown".into()))?;
        let all = {
            let shown = |id: u32| object_shown(&ctx.world, ctx.state(), &pose.map, id);
            targets(&ctx.world, &pose, &shown)
        };
        let fresh: Vec<Target> = all
            .into_iter()
            .filter(|t| !self.tried.contains(&(pose.map.clone(), t.clone())))
            .take(MAX_TARGETS)
            .collect();
        if fresh.is_empty() {
            return Err(ToolError::Failed(format!(
                "explore: nothing left to try on {}",
                pose.map
            )));
        }
        let before = learnt(ctx.state());
        for target in fresh {
            self.tried.insert((pose.map.clone(), target.clone()));
            ctx.emit(progress(
                "Explore",
                format!("{}: trying {target:?}", pose.map),
            ))?;
            let result = match &target {
                Target::Object { local_id } => {
                    talk(ctx, &pose.map, *local_id, Vec::new()).map(|_| ())
                }
                Target::Sign {
                    x,
                    y,
                    facing,
                    script,
                } => {
                    let mut step = TalkStep::toward(
                        ctx,
                        &pose.map,
                        (*x, *y),
                        *facing,
                        Some(script.clone()),
                        Vec::new(),
                    )
                    .with_scene();
                    ctx.drive(&mut step)
                        .and_then(|_| finish(ctx, step.conversation()))
                }
            };
            match result {
                Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
                Err(e) => ctx.emit(progress("Explore", format!("{target:?}: {e}")))?,
                Ok(()) => {}
            }
            if learnt(ctx.state()) != before {
                ctx.emit(progress(
                    "Explore",
                    format!("{target:?} changed what is known: replanning"),
                ))?;
                return Ok(());
            }
        }
        Err(ToolError::Failed(format!(
            "explore: nothing new on {}",
            pose.map
        )))
    }
}

impl Tool for ExploreTool {
    fn name(&self) -> &str {
        "Explore"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Explore)
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        if !matches!(intent, Intent::Explore) {
            return ToolOutcome::failed("not an Explore");
        }
        self.explore(ctx).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bill's Sea Cottage before he is helped: Bill as a Clefairy (on
    /// screen) comes before the PC, human Bill (gone) is left out.
    #[test]
    fn bills_cottage_tries_the_clefairy_first() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = World::load(&dir) else {
            return;
        };
        if world.events().is_none() {
            return;
        }
        let pose = PlayerPose {
            map: "Route25_SeaCottage".into(),
            x: 4,
            y: 6,
        };
        let shown = |id: u32| match id {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        };
        let t = targets(&world, &pose, &shown);
        assert_eq!(t.first(), Some(&Target::Object { local_id: 2 }), "{t:?}");
        assert!(!t.contains(&Target::Object { local_id: 1 }));
        assert!(t.iter().any(|t| matches!(
            t,
            Target::Sign { script, .. } if script == "Route25_SeaCottage_EventScript_Computer"
        )));
    }

    /// Trainers are never walked up to.
    #[test]
    fn trainers_are_left_alone() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = World::load(&dir) else {
            return;
        };
        if world.events().is_none() {
            return;
        }
        let pose = PlayerPose {
            map: "ViridianForest".into(),
            x: 29,
            y: 60,
        };
        let map = world.map(&pose.map).unwrap();
        let t = targets(&world, &pose, &|_| None);
        for target in &t {
            if let Target::Object { local_id } = target {
                let o = map
                    .objects
                    .iter()
                    .find(|o| o.local_id == *local_id)
                    .unwrap();
                assert!(o
                    .trainer_type
                    .as_deref()
                    .is_none_or(|t| t == "TRAINER_TYPE_NONE"));
            }
        }
        assert!(map
            .objects
            .iter()
            .any(|o| o.trainer_type.as_deref() == Some("TRAINER_TYPE_NORMAL")));
    }
}
