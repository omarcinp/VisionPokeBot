//! Switches that flip a flag back and forth, each state opening some ways
//! and closing others (Pokémon Mansion's statues: FLAG_POKEMON_MANSION_
//! SWITCH_STATE set opens one set of barriers and shuts the other). A
//! route that needs both states can't be planned against one belief: the
//! planner pressed the switch twice in a row and the walk to B1F found "no
//! known route" (Switch, Cinnabar). When no way leads on, the states are
//! searched: walk to a switch reachable now, press it (the flag flips, the
//! player stands at it), until the destination is reachable.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use pokebot_state::{EscapeWarp, PlayerPose};
use pokebot_world::events::{Condition, Effect};
use pokebot_world::predicate::{BeliefView, Predicate, Truth};
use pokebot_world::route::{self, Place, PlaceGraph, UnknownPolicy};
use pokebot_world::World;

use super::{Answer, Dest, Intent, ToolContext, ToolError};
use crate::belief_view::StateBelief;
use crate::nav::Destination;

/// A switch: the script read at `at` on `map` sets `flag` when it is clear
/// (`set_path`) and clears it when it is set (`clear_path`), on YES.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toggle {
    pub map: String,
    pub at: (i32, i32),
    pub script: String,
    pub flag: String,
    pub set_path: usize,
    pub clear_path: usize,
}

/// One press of a switch, from `stand`, leaving its flag at `set`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    pub toggle: Toggle,
    pub stand: PlayerPose,
    pub set: bool,
}

/// Every switch of the world, in map and position order.
pub fn toggles(world: &World) -> Vec<Toggle> {
    let Some(events) = world.events() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for map in world.maps() {
        let spots = map
            .signs
            .iter()
            .filter_map(|s| Some(((s.x, s.y), s.script.as_deref()?)))
            .chain(
                map.objects
                    .iter()
                    .filter_map(|o| Some(((o.x?, o.y?), o.script.as_deref()?))),
            );
        for (at, script) in spots {
            let Some(s) = events.script(script) else {
                continue;
            };
            let (mut set, mut clear, mut flag) = (None, None, None::<String>);
            for (i, p) in s.paths.iter().enumerate() {
                // Only the answer and the flag decide the path.
                if !p
                    .when
                    .iter()
                    .all(|c| matches!(c, Condition::Flag { .. } | Condition::Answer { .. }))
                {
                    continue;
                }
                let Some((f, is)) = p.when.iter().find_map(|c| match c {
                    Condition::Flag { flag, is } => Some((flag.clone(), *is)),
                    _ => None,
                }) else {
                    continue;
                };
                if flag.as_ref().is_some_and(|g| *g != f) {
                    continue;
                }
                let sets = p
                    .does
                    .iter()
                    .any(|e| matches!(e, Effect::Set { set } if *set == f));
                let clears = p
                    .does
                    .iter()
                    .any(|e| matches!(e, Effect::Clear { clear } if *clear == f));
                if !is && sets {
                    set = Some(i);
                    flag = Some(f);
                } else if is && clears {
                    clear = Some(i);
                    flag = Some(f);
                }
            }
            if let (Some(set_path), Some(clear_path), Some(flag)) = (set, clear, flag) {
                out.push(Toggle {
                    map: map.name.clone(),
                    at,
                    script: script.to_owned(),
                    flag,
                    set_path,
                    clear_path,
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.map, a.at).cmp(&(&b.map, b.at)));
    out
}

/// A one-way opener: the script read at `at` on `map` sets `flag` (clear
/// before) on its path `path`, answered `yes`, with no battle (a gym
/// quiz's right answer opening the next door; the wrong one fights).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opener {
    pub map: String,
    pub at: (i32, i32),
    pub script: String,
    pub flag: String,
    pub path: usize,
    pub yes: bool,
    /// The only way it is read from (a sign), if any.
    pub facing: Option<pokebot_state::Direction>,
}

/// Every opener on `maps`, in map and position order.
pub fn openers(world: &World, maps: &[&str]) -> Vec<Opener> {
    let Some(events) = world.events() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in maps {
        let Some(map) = world.map(name) else {
            continue;
        };
        let spots = map
            .signs
            .iter()
            .filter_map(|s| Some(((s.x, s.y), s.script.as_deref()?, s.facing_dir())))
            .chain(
                map.objects
                    .iter()
                    .filter_map(|o| Some(((o.x?, o.y?), o.script.as_deref()?, None))),
            );
        for (at, script, facing) in spots {
            let Some(s) = events.script(script) else {
                continue;
            };
            for (i, p) in s.paths.iter().enumerate() {
                let (mut answer, mut flag, mut other) = (None, None, false);
                for c in &p.when {
                    match c {
                        Condition::Answer { .. } => {
                            answer =
                                crate::tools::dialogue::question_branches(std::slice::from_ref(c))
                                    .first()
                                    .copied()
                        }
                        Condition::Flag { flag: f, is: false } => flag = Some(f.clone()),
                        _ => other = true,
                    }
                }
                let (Some(yes), Some(f), false) = (answer, flag, other) else {
                    continue;
                };
                let sets = p
                    .does
                    .iter()
                    .any(|e| matches!(e, Effect::Set { set } if *set == f));
                let fights = p.does.iter().any(|e| matches!(e, Effect::Battle { .. }));
                if sets && !fights {
                    out.push(Opener {
                        map: map.name.clone(),
                        at,
                        script: script.to_owned(),
                        flag: f,
                        path: i,
                        yes,
                        facing,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| (&a.map, a.at, &a.script).cmp(&(&b.map, b.at, &b.script)));
    out.dedup_by(|a, b| a.script == b.script && a.flag == b.flag);
    out
}

/// The belief with `flags` at their values.
struct Flags<'a> {
    base: &'a dyn BeliefView,
    flags: &'a std::collections::BTreeMap<String, bool>,
}

impl BeliefView for Flags<'_> {
    fn eval(&self, p: &Predicate) -> Truth {
        match p {
            Predicate::Flag { name, is } => match self.flags.get(name) {
                Some(v) if v == is => Truth::True,
                Some(_) => Truth::False,
                None => self.base.eval(p),
            },
            _ => self.base.eval(p),
        }
    }

    fn escape(&self) -> Option<EscapeWarp> {
        self.base.escape()
    }
}

/// One opener run, from `stand`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub opener: Opener,
    pub stand: PlayerPose,
}

/// Openers to run, in order, before `dest` can be reached from `pose`;
/// `None` when they don't help (or `dest` is reachable already).
pub fn open_plan(
    world: &World,
    graph: &PlaceGraph,
    base: &dyn BeliefView,
    pose: &PlayerPose,
    dest: &Dest,
    openers: &[Opener],
    policy: UnknownPolicy,
) -> Option<Vec<Open>> {
    type State = std::collections::BTreeMap<String, bool>;
    let unset = |o: &Opener, set: &State| {
        !set.contains_key(&o.flag)
            && base.eval(&Predicate::Flag {
                name: o.flag.clone(),
                is: true,
            }) != Truth::True
    };
    let mut seen: BTreeSet<Vec<String>> = BTreeSet::new();
    let mut queue: VecDeque<(PlayerPose, State, Vec<Open>)> =
        VecDeque::from([(pose.clone(), State::new(), Vec::new())]);
    while let Some((here, set, runs)) = queue.pop_front() {
        let belief = Flags { base, flags: &set };
        if !runs.is_empty() && reaches(world, graph, &belief, &here, dest, policy) {
            return Some(runs);
        }
        if runs.len() >= MAX_OPENS {
            continue;
        }
        for o in openers.iter().filter(|o| unset(o, &set)) {
            let mut next = set.clone();
            next.insert(o.flag.clone(), true);
            let key: Vec<String> = next.keys().cloned().collect();
            if seen.contains(&key) {
                continue;
            }
            let Some(map) = world.map(&o.map) else {
                continue;
            };
            let stand = crate::nav::facing_spots(map, o.at.0, o.at.1)
                .into_iter()
                .filter(|(_, dir)| o.facing.is_none_or(|f| f == *dir))
                .map(|((x, y), _)| PlayerPose {
                    map: o.map.clone(),
                    x,
                    y,
                })
                .find(|s| {
                    (s.map == here.map && (s.x, s.y) == (here.x, here.y))
                        || route::route(
                            world,
                            graph,
                            &belief,
                            &here,
                            &Place::tile(&s.map, s.x, s.y),
                            policy,
                        )
                        .found()
                });
            let Some(stand) = stand else {
                continue;
            };
            seen.insert(key);
            let mut runs = runs.clone();
            runs.push(Open {
                opener: o.clone(),
                stand: stand.clone(),
            });
            queue.push_back((stand, next, runs));
        }
    }
    None
}

/// Openers a way runs at most.
const MAX_OPENS: usize = 8;

/// The belief with `flag` at `value`.
struct Overlay<'a> {
    base: &'a dyn BeliefView,
    flag: &'a str,
    value: bool,
}

impl BeliefView for Overlay<'_> {
    fn eval(&self, p: &Predicate) -> Truth {
        match p {
            Predicate::Flag { name, is } if name == self.flag => {
                if *is == self.value {
                    Truth::True
                } else {
                    Truth::False
                }
            }
            _ => self.base.eval(p),
        }
    }

    fn escape(&self) -> Option<EscapeWarp> {
        self.base.escape()
    }
}

/// Whether `dest` is reachable from `pose` under `belief`.
fn reaches(
    world: &World,
    graph: &PlaceGraph,
    belief: &dyn BeliefView,
    pose: &PlayerPose,
    dest: &Dest,
    policy: UnknownPolicy,
) -> bool {
    match dest {
        Dest::Tile { map, x, y } => route::route(
            world,
            graph,
            belief,
            pose,
            &Place::tile(map, *x, *y),
            policy,
        )
        .found(),
        // Next to it: one of the tiles it is talked to from.
        Dest::Facing { map, x, y } => world.map(map).is_some_and(|m| {
            crate::nav::facing_spots(m, *x, *y)
                .into_iter()
                .any(|((sx, sy), _)| {
                    route::route(
                        world,
                        graph,
                        belief,
                        pose,
                        &Place::tile(map, sx, sy),
                        policy,
                    )
                    .found()
                })
        }),
        Dest::Map { map } | Dest::Warp { map, .. } => {
            route::route_to_map(world, graph, belief, pose, map, policy).found()
        }
    }
}

/// Presses to make before `dest` can be reached from `pose`, the fewest
/// first; `None` when no switch helps (or `dest` is reachable already).
pub fn plan(
    world: &World,
    graph: &PlaceGraph,
    base: &dyn BeliefView,
    pose: &PlayerPose,
    dest: &Dest,
    toggles: &[Toggle],
    policy: UnknownPolicy,
) -> Option<Vec<Press>> {
    let flags: BTreeSet<&str> = toggles.iter().map(|t| t.flag.as_str()).collect();
    for flag in flags {
        let value = match base.eval(&Predicate::Flag {
            name: flag.to_owned(),
            is: true,
        }) {
            Truth::True => true,
            Truth::False => false,
            // A switch's state unknown: the game starts it clear.
            Truth::Unknown => false,
        };
        let switches: Vec<&Toggle> = toggles.iter().filter(|t| t.flag == flag).collect();
        let mut seen: BTreeSet<(Option<usize>, bool)> = BTreeSet::from([(None, value)]);
        let mut queue: VecDeque<(PlayerPose, bool, Vec<Press>)> =
            VecDeque::from([(pose.clone(), value, Vec::new())]);
        while let Some((here, value, presses)) = queue.pop_front() {
            let belief = Overlay { base, flag, value };
            if !presses.is_empty() && reaches(world, graph, &belief, &here, dest, policy) {
                return Some(presses);
            }
            if presses.len() >= MAX_PRESSES {
                continue;
            }
            for (i, t) in switches.iter().enumerate() {
                if !seen.insert((Some(i), !value)) {
                    continue;
                }
                let Some(map) = world.map(&t.map) else {
                    continue;
                };
                let stand = crate::nav::facing_spots(map, t.at.0, t.at.1)
                    .into_iter()
                    .map(|((x, y), _)| PlayerPose {
                        map: t.map.clone(),
                        x,
                        y,
                    })
                    .find(|s| {
                        (s.map == here.map && (s.x, s.y) == (here.x, here.y))
                            || route::route(
                                world,
                                graph,
                                &belief,
                                &here,
                                &Place::tile(&s.map, s.x, s.y),
                                policy,
                            )
                            .found()
                    });
                let Some(stand) = stand else {
                    seen.remove(&(Some(i), !value));
                    continue;
                };
                let mut next = presses.clone();
                next.push(Press {
                    toggle: (*t).clone(),
                    stand: stand.clone(),
                    set: !value,
                });
                queue.push_back((stand, !value, next));
            }
        }
    }
    None
}

/// Presses a route through switches takes at most.
const MAX_PRESSES: usize = 8;

/// When `dest` can't be walked to: the switch presses that open the way,
/// pressed, then the walk. `None` when no switch helps.
pub fn through(
    ctx: &mut ToolContext<'_>,
    dest: &Dest,
) -> Option<Result<Option<PlayerPose>, ToolError>> {
    let pose = ctx.pose()?;
    let world = Arc::clone(&ctx.world);
    let all = toggles(&world);
    let presses = {
        let graph = ctx
            .scheduler
            .graph
            .get_or_insert_with(|| crate::scheduler::graph(&world));
        let belief = StateBelief(ctx.runtime.state());
        let policy = UnknownPolicy::Optimistic {
            penalty_of: super::go::battle_on_the_way,
        };
        match plan(&world, graph, &belief, &pose, dest, &all, policy) {
            Some(p) => p,
            None => {
                let dest_map = match dest {
                    Dest::Tile { map, .. }
                    | Dest::Map { map }
                    | Dest::Facing { map, .. }
                    | Dest::Warp { map, .. } => map.as_str(),
                };
                let ops = openers(&world, &[pose.map.as_str(), dest_map]);
                let runs = open_plan(&world, graph, &belief, &pose, dest, &ops, policy)?;
                return Some(run_openers(ctx, &runs, dest));
            }
        }
    };
    ctx.info(format!(
        "switches open the way: {}",
        presses
            .iter()
            .map(|p| format!(
                "{} at {} ({}, {}) {}",
                p.toggle.script,
                p.toggle.map,
                p.toggle.at.0,
                p.toggle.at.1,
                if p.set { "set" } else { "clear" }
            ))
            .collect::<Vec<_>>()
            .join(", then ")
    ));
    Some((|| {
        for p in &presses {
            let path = if p.set {
                p.toggle.set_path
            } else {
                p.toggle.clear_path
            };
            ctx.invoke(&Intent::RunScript {
                script: p.toggle.script.clone(),
                path: Some(path),
                answers: vec![Answer::Yes],
            })
            .result?;
        }
        match dest {
            Dest::Map { map } => super::go::go_to_map(ctx, map),
            other => super::go::go(ctx, Destination::from(other)),
        }
    })())
}

/// Runs `runs` in turn, then walks to `dest`.
fn run_openers(
    ctx: &mut ToolContext<'_>,
    runs: &[Open],
    dest: &Dest,
) -> Result<Option<PlayerPose>, ToolError> {
    ctx.info(format!(
        "scripts open the way: {}",
        runs.iter()
            .map(|r| format!("{} ({})", r.opener.script, r.opener.flag))
            .collect::<Vec<_>>()
            .join(", then ")
    ));
    for r in runs {
        ctx.invoke(&Intent::RunScript {
            script: r.opener.script.clone(),
            path: Some(r.opener.path),
            answers: vec![if r.opener.yes {
                Answer::Yes
            } else {
                Answer::No
            }],
        })
        .result?;
    }
    match dest {
        Dest::Map { map } => super::go::go_to_map(ctx, map),
        other => super::go::go(ctx, Destination::from(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> Option<World> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        World::load(&dir).ok()
    }

    /// The Mansion's five statues are the world's switches, all on its
    /// switch-state flag.
    #[test]
    fn the_mansions_statues_are_switches() {
        let Some(world) = world() else { return };
        let all = toggles(&world);
        assert!(all.len() >= 5, "{all:?}");
        assert!(all
            .iter()
            .filter(|t| t.map.starts_with("PokemonMansion"))
            .all(|t| t.flag == "FLAG_POKEMON_MANSION_SWITCH_STATE"));
        assert!(all
            .iter()
            .any(|t| t.map == "PokemonMansion_1F" && t.at == (5, 5)));
    }

    /// Switch, Cinnabar: B1F from the Mansion's door is no walk in either
    /// switch state; the presses that open it are found, each switch
    /// reached in the state the presses before it left.
    #[test]
    fn the_mansions_basement_is_reached_through_its_switches() {
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let all = toggles(&world);
        let pose = PlayerPose {
            map: "PokemonMansion_1F".into(),
            x: 8,
            y: 33,
        };
        let dest = Dest::Map {
            map: "PokemonMansion_B1F".into(),
        };
        let policy = UnknownPolicy::Optimistic {
            penalty_of: crate::tools::go::battle_on_the_way,
        };
        let flag = "FLAG_POKEMON_MANSION_SWITCH_STATE";
        let mut state = pokebot_state::GameState::default();
        for start in [false, true] {
            state
                .world
                .flags
                .insert(flag.into(), pokebot_state::Knowledge::observed(start, 1));
            let belief = StateBelief(&state);
            assert!(!reaches(&world, &graph, &belief, &pose, &dest, policy));
            let presses = plan(&world, &graph, &belief, &pose, &dest, &all, policy)
                .unwrap_or_else(|| panic!("a way from {start}"));
            // Each press flips the state; the last leaves B1F reachable.
            let mut value = start;
            let mut here = pose.clone();
            for p in &presses {
                assert_eq!(p.set, !value, "{presses:?}");
                let now = Overlay {
                    base: &belief,
                    flag,
                    value,
                };
                assert!(
                    route::route(
                        &world,
                        &graph,
                        &now,
                        &here,
                        &Place::tile(&p.stand.map, p.stand.x, p.stand.y),
                        policy
                    )
                    .found(),
                    "{p:?}"
                );
                value = p.set;
                here = p.stand.clone();
            }
            let last = Overlay {
                base: &belief,
                flag,
                value,
            };
            assert!(reaches(&world, &graph, &last, &here, &dest, policy));
        }
    }

    /// Switch, the Mansion's B1F: the SECRET KEY at (5, 7), "no path next
    /// to" it whichever state the plan pressed; from the statue at
    /// (24, 29), in either state, it is reached (through presses when the
    /// state shuts it).
    #[test]
    fn the_secret_key_is_reached_in_either_switch_state() {
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let all = toggles(&world);
        let pose = PlayerPose {
            map: "PokemonMansion_B1F".into(),
            x: 24,
            y: 30,
        };
        let key = Dest::Facing {
            map: "PokemonMansion_B1F".into(),
            x: 5,
            y: 7,
        };
        let policy = UnknownPolicy::Optimistic {
            penalty_of: crate::tools::go::battle_on_the_way,
        };
        let flag = "FLAG_POKEMON_MANSION_SWITCH_STATE";
        let mut state = pokebot_state::GameState::default();
        let mut shut = 0;
        for value in [false, true] {
            state
                .world
                .flags
                .insert(flag.into(), pokebot_state::Knowledge::observed(value, 1));
            let belief = StateBelief(&state);
            if reaches(&world, &graph, &belief, &pose, &key, policy) {
                continue;
            }
            shut += 1;
            let presses = plan(&world, &graph, &belief, &pose, &key, &all, policy)
                .unwrap_or_else(|| panic!("a way from {value}"));
            assert!(!presses.is_empty());
        }
        assert!(shut > 0, "one state shuts the key away");
    }

    /// Switch, Cinnabar Gym: past the first quiz, Blaine is behind four
    /// more doors, each opened by the right answer at the machine the door
    /// before reveals. The machines are found and run in that order.
    #[test]
    fn the_gyms_quiz_doors_are_opened_in_turn() {
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let map = "CinnabarIsland_Gym";
        let pose = PlayerPose {
            map: map.into(),
            x: 22,
            y: 11,
        };
        let dest = Dest::Facing {
            map: map.into(),
            x: 5,
            y: 4,
        };
        let policy = UnknownPolicy::Optimistic {
            penalty_of: crate::tools::go::battle_on_the_way,
        };
        let mut state = pokebot_state::GameState::default();
        for n in 1..=6 {
            state.world.flags.insert(
                format!("FLAG_CINNABAR_GYM_QUIZ_{n}"),
                pokebot_state::Knowledge::observed(n == 1, 1),
            );
        }
        let belief = StateBelief(&state);
        assert!(!reaches(&world, &graph, &belief, &pose, &dest, policy));
        let ops = openers(&world, &[map]);
        assert!(
            ops.iter().any(|o| o.flag == "FLAG_CINNABAR_GYM_QUIZ_2"),
            "{ops:?}"
        );
        let runs = open_plan(&world, &graph, &belief, &pose, &dest, &ops, policy)
            .unwrap_or_else(|| panic!("no way through {ops:?}"));
        let flags: Vec<&str> = runs.iter().map(|r| r.opener.flag.as_str()).collect();
        let quiz = |n: u32| format!("FLAG_CINNABAR_GYM_QUIZ_{n}");
        assert_eq!(
            flags,
            (2..=flags.len() as u32 + 1).map(quiz).collect::<Vec<_>>(),
            "{runs:?}"
        );
        assert!(flags.len() >= 4, "{runs:?}");
        // Each machine is read from below.
        for r in &runs {
            assert_eq!((r.stand.x, r.stand.y), (r.opener.at.0, r.opener.at.1 + 1));
        }
    }
}
