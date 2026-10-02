//! `Explore`: the last resort when the plan is stuck on a map. Every
//! person and sign of the map with a script is talked to or read, the ones
//! on screen and nearest first, each once per session, until the belief
//! learns something (a flag, a var, an item, a script path): the plan is
//! then made again from what the game showed. Nothing is scripted: the
//! conversations are followed and recognised as in `RunScript`, their
//! paths resolved from the text read (on the Switch the plan went to
//! Bill's PC before talking to Bill, and nothing tried Bill himself).
//!
//! Someone on screen no map object can be (a script walked them off
//! their tiles: Bill once out of the teleporter) is talked to first, and
//! the text read tells which script it was. Trainers are left alone
//! (talking to one starts a battle the plan did not price), and so are
//! objects known to be gone.
//!
//! What is worth trying comes from the [`crate::ledger`]: a target never
//! talked to, or last talked to before the belief changed, or whose
//! question can still get its other answer (YES only where
//! [`yes_is_safe`]). Every talk is logged there with what was said and
//! answered, so a restart doesn't repeat what already taught nothing.
//! `Explore { map }` walks to `map` first: the recourse ladder
//! ([`crate::recourse`]) widens the search from the stuck map outward.

use pokebot_state::{Direction, GameState, PlayerPose};
use pokebot_world::events::Effect;
use pokebot_world::World;

use super::dialogue::{finish, object_shown, yes_is_safe, Conversation};
use super::effects::LabelIndex;
use super::talk::{talk, TalkStep};
use super::{progress, Answer, Dest, Intent, Tool, ToolContext, ToolError, ToolOutcome};
use crate::ledger::{self, fingerprint, Freshness, Ledger, Talk};

/// Targets tried per `Explore` at most.
pub const MAX_TARGETS: usize = 8;
/// Frames watched for someone to show up when nothing is left to try: an
/// unnamed sprite counts after 90 frames on its tile, and the player may
/// just have walked up (on the emulator Bill, off his tiles, was on screen
/// but not yet counted).
const WATCH_FRAMES: u64 = 150;

/// Something on the map to interact with.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    /// A sprite on screen no object's tiles explain.
    Sprite {
        x: i32,
        y: i32,
    },
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

impl Target {
    /// Its key in the [`Ledger`].
    pub fn key(&self) -> String {
        match self {
            Target::Sprite { x, y } => format!("sprite:{x},{y}"),
            Target::Object { local_id } => ledger::object_key(*local_id),
            Target::Sign { x, y, .. } => format!("sign:{x},{y}"),
        }
    }

    /// The compiled script it runs, when the map data says.
    fn script(&self, world: &World, map: &str) -> Option<String> {
        match self {
            Target::Sprite { .. } => None,
            Target::Object { local_id } => super::lookup::object_script(world, map, *local_id),
            Target::Sign { script, .. } => Some(script.clone()),
        }
    }
}

/// Tiles of the player's map the walk reaches from `pose`, round people,
/// tiles learnt blocked and the passages the belief holds shut (fleet
/// continue-2, Silph Co. 5F: the worker behind door 3, seen shut, was
/// tried again each time what was known changed, "looping" at the door).
fn reachable(ctx: &ToolContext<'_>, pose: &PlayerPose) -> Option<pokebot_world::path::Reach> {
    let blocked = ctx
        .blocked
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .on_map(&pose.map);
    reach_from(&ctx.world, pose, &ctx.gone, &blocked, &ctx.gate_tiles())
}

/// [`reachable`] from its parts.
fn reach_from(
    world: &World,
    pose: &PlayerPose,
    gone: &crate::nav::Gone,
    blocked: &pokebot_world::path::Obstacles,
    gates: &pokebot_world::gates::GateTiles,
) -> Option<pokebot_world::path::Reach> {
    let map = world.map(&pose.map)?;
    let mut obstacles = crate::nav::object_obstacles(map, gone);
    obstacles.extend(blocked.iter().copied());
    obstacles.extend(gates.closed_on(&pose.map));
    obstacles.remove(&(pose.x, pose.y));
    let opened = gates.opened_on(&pose.map);
    let walk = pokebot_world::path::Walk {
        obstacles: &obstacles,
        surf: false,
        opened: Some(&opened),
    };
    Some(pokebot_world::path::reach(
        map,
        (pose.x, pose.y),
        &walk,
        |_| 0,
    ))
}

/// Whether a tile `(x, y)` is faced from (next to it, or across a
/// counter) is reached.
fn beside(
    map: &pokebot_world::MapData,
    reach: &pokebot_world::path::Reach,
    (x, y): (i32, i32),
) -> bool {
    crate::nav::facing_spots(map, x, y)
        .into_iter()
        .any(|(spot, _)| reach.cost(spot).is_some())
}

/// Where `target` stands on `map`.
fn target_tile(world: &World, map: &str, target: &Target) -> Option<(i32, i32)> {
    match target {
        Target::Sprite { x, y } | Target::Sign { x, y, .. } => Some((*x, *y)),
        Target::Object { local_id } => super::lookup::object_tile(world, map, *local_id),
    }
}

/// `targets` of `map` with what the ledger says of each under the belief
/// `state`, the spent ones left out.
pub fn fresh(
    world: &World,
    ledger: &Ledger,
    state: &GameState,
    pose: &PlayerPose,
    unnamed: &[(i32, i32)],
) -> Vec<(Target, Freshness)> {
    let knowledge = fingerprint(state);
    let shown = |id: u32| object_shown(world, state, &pose.map, id);
    targets(world, pose, unnamed, &shown)
        .into_iter()
        .map(|t| {
            let yes_safe = t
                .script(world, &pose.map)
                .and_then(|s| world.events()?.script(&s).map(yes_is_safe))
                .unwrap_or(false);
            let f = ledger.freshness(&pose.map, &t.key(), knowledge, yes_safe);
            (t, f)
        })
        .filter(|(_, f)| f.is_fresh())
        .collect()
}

/// The map's people and signs worth trying, in order: on screen first
/// (`unnamed` sprites, then objects), then the nearest; trainers, objects
/// known gone and scriptless ones left out. `shown(local_id)` is
/// [`object_shown`].
pub fn targets(
    world: &World,
    pose: &PlayerPose,
    unnamed: &[(i32, i32)],
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
    let mut out: Vec<(u8, i32, Target)> = unnamed
        .iter()
        .map(|&(x, y)| (0, dist(x, y), Target::Sprite { x, y }))
        .collect();
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

#[derive(Default)]
pub struct ExploreTool;

impl ExploreTool {
    /// The fresh targets around the player at `pose`: unnamed sprites
    /// counted in the view or seen on the frame now.
    fn fresh(&self, ctx: &ToolContext<'_>, pose: &PlayerPose) -> Vec<(Target, Freshness)> {
        let state = ctx.state();
        let mut unnamed: Vec<(i32, i32)> = state
            .view
            .npcs
            .iter()
            .filter(|n| n.map == pose.map && n.local_id.is_none())
            .map(|n| (n.x, n.y))
            .collect();
        let located = ctx
            .observation()
            .filter(|o| o.player.as_ref().is_some_and(|p| p.pose.map == pose.map));
        for s in located.iter().flat_map(|o| &o.sprites) {
            if s.local_id.is_none() && !unnamed.contains(&(s.x, s.y)) {
                unnamed.push((s.x, s.y));
            }
        }
        let reach = reachable(ctx, pose);
        let map = ctx.world.map(&pose.map);
        fresh(&ctx.world, &ctx.ledger, state, pose, &unnamed)
            .into_iter()
            .filter(|(t, _)| {
                let (Some(tile), Some(r), Some(map)) =
                    (target_tile(&ctx.world, &pose.map, t), &reach, map)
                else {
                    return true;
                };
                beside(map, r, tile)
            })
            .take(MAX_TARGETS)
            .collect()
    }

    fn explore(&mut self, map: Option<&str>, ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
        if let Some(map) = map.filter(|m| ctx.pose().is_none_or(|p| p.map != *m)) {
            ctx.emit(progress("Explore", format!("going to {map}")))?;
            ctx.invoke(&Intent::Go {
                dest: Dest::Map {
                    map: map.to_owned(),
                },
            })
            .result?;
        }
        let pose = ctx
            .pose()
            .ok_or_else(|| ToolError::Failed("explore: the position is unknown".into()))?;
        let mut fresh = self.fresh(ctx, &pose);
        if fresh.is_empty() {
            // Nothing yet: watch a while for someone to show up.
            let start = ctx.observation().map_or(0, |o| o.frame_id);
            while ctx.observe()?.frame_id < start + WATCH_FRAMES {}
            fresh = self.fresh(ctx, &pose);
        }
        if fresh.is_empty() {
            return Err(ToolError::Failed(format!(
                "explore: nothing left to try on {}",
                pose.map
            )));
        }
        let before = fingerprint(ctx.state());
        for (target, freshness) in fresh {
            let answers: Vec<Answer> = match &freshness {
                Freshness::OtherAnswer(a) => a
                    .iter()
                    .map(|&yes| if yes { Answer::Yes } else { Answer::No })
                    .collect(),
                _ => Vec::new(),
            };
            let knowledge = fingerprint(ctx.state());
            ctx.emit(progress(
                "Explore",
                format!("{}: trying {target:?} ({freshness:?})", pose.map),
            ))?;
            let result = self.interact(ctx, &pose.map, &target, answers);
            let (conversation, error) = match result {
                Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
                Err(e) => {
                    ctx.emit(progress("Explore", format!("{target:?}: {e}")))?;
                    (None, Some(e.to_string()))
                }
                Ok(c) => (Some(c), None),
            };
            let changed = fingerprint(ctx.state()) != before;
            let talk = Talk {
                knowledge,
                script: conversation.as_ref().and_then(|c| script_of(ctx, c)),
                said: conversation
                    .as_ref()
                    .map(|c| c.recognised.clone())
                    .unwrap_or_default(),
                answered: conversation
                    .as_ref()
                    .map(|c| c.answered.clone())
                    .unwrap_or_default(),
                learnt: changed,
                error,
            };
            ctx.ledger.talked(&pose.map, &target.key(), talk);
            if let Err(e) = ctx.ledger.store() {
                ctx.info(format!("ledger: {e}"));
            }
            if changed {
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

    /// Talks to or reads `target`; the conversation followed.
    fn interact(
        &mut self,
        ctx: &mut ToolContext<'_>,
        map: &str,
        target: &Target,
        answers: Vec<Answer>,
    ) -> Result<Conversation, ToolError> {
        let (at, facing, script) = match target {
            Target::Object { local_id } => return talk(ctx, map, *local_id, answers),
            Target::Sprite { x, y } => ((*x, *y), None, None),
            Target::Sign {
                x,
                y,
                facing,
                script,
            } => ((*x, *y), *facing, Some(script.clone())),
        };
        let mut step = TalkStep::toward(ctx, map, at, facing, script, answers).with_scene();
        ctx.drive(&mut step)?;
        finish(ctx, step.conversation())?;
        Ok(step.into_conversation())
    }
}

/// The compiled script a conversation was, when it can be told.
fn script_of(ctx: &ToolContext<'_>, c: &Conversation) -> Option<String> {
    c.script.clone().or_else(|| {
        let events = ctx.world.events()?;
        c.resolved_script(&LabelIndex::build_for(events, &c.recognised))
    })
}

impl Tool for ExploreTool {
    fn name(&self) -> &str {
        "Explore"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Explore { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Explore { map } = intent else {
            return ToolOutcome::failed("not an Explore");
        };
        self.explore(map.as_deref(), ctx).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bill's Sea Cottage before he is helped: Bill as a Clefairy (on
    /// screen) comes before the PC, human Bill (gone) is left out.
    /// Fleet continue-2, Silph Co. 5F at (20, 13): the worker at (16, 13)
    /// is behind door 3; seen shut, he is out of reach, and not tried.
    #[test]
    fn someone_behind_a_shut_door_is_out_of_reach() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = World::load(&dir) else { return };
        let story = pokebot_world::gates::derive(&world);
        let pose = PlayerPose {
            map: "SilphCo_5F".into(),
            x: 20,
            y: 13,
        };
        let map = world.map("SilphCo_5F").unwrap();
        let worker = (16, 13);
        let reached = |open: bool| {
            let mut state = pokebot_state::GameState::default();
            state.world.flags.insert(
                "FLAG_SILPH_5F_DOOR_3".into(),
                pokebot_state::Knowledge::observed(open, 1),
            );
            let gates = pokebot_world::gates::GateTiles::believed(
                &world,
                &story,
                &crate::belief_view::StateBelief(&state),
            );
            let gone = crate::nav::belief_gone(&world, &state);
            let reach = reach_from(&world, &pose, &gone, &Default::default(), &gates).unwrap();
            beside(map, &reach, worker)
        };
        assert!(!reached(false));
        assert!(reached(true));
    }

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
        let t = targets(&world, &pose, &[], &shown);
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
        let t = targets(&world, &pose, &[], &|_| None);
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

    /// After the Cell Separator Bill walks out of the teleporter to a tile
    /// his map object can't stand on (live: (7, 6), unnamed): he is tried
    /// first, before the PC.
    #[test]
    fn someone_no_object_explains_is_tried_first() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = World::load(&dir) else {
            return;
        };
        if world.events().is_none() {
            return;
        }
        let pose = PlayerPose {
            map: "Route25_SeaCottage".into(),
            x: 7,
            y: 5,
        };
        let t = targets(&world, &pose, &[(7, 6)], &|_| Some(false));
        assert_eq!(t.first(), Some(&Target::Sprite { x: 7, y: 6 }), "{t:?}");
    }
}
