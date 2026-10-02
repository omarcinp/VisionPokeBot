//! Item balls picked up on the way (`finditem`): a ball not taken that
//! lies a small detour off the walk is worth it when its item, priced at
//! the scheduler's 10 money units a second, beats the detour's time. The
//! planner only plans a ball a goal needs, and the bots walked past every
//! free Potion, Rare Candy and TM.
//!
//! Checked between a walking tool's steps (`Go`, `Train`, `Catch`) at an
//! overworld boundary, like the scheduler's needs; the ball is taken by
//! `RunScript` (walk next to it, face it, press A, read "RED found X!"),
//! whose recorded path sets the ball's hide flag. A ball gone for and not
//! taken isn't gone for again.

use std::collections::BTreeSet;

use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, GameState, PlayerPose, Pocket};
use pokebot_world::events::Effect;
use pokebot_world::path::{self, Obstacles, Walk};
use pokebot_world::route::{self, PlaceGraph, RouteParams, UnknownPolicy};
use pokebot_world::World;
use serde::Serialize;

use super::{Intent, ToolContext, ToolError};
use crate::nav::Gone;

/// Money units worth a second of play (the scheduler's price of
/// medicines, the route's of an Escape Rope).
pub const MONEY_PER_S: f64 = 10.0;
/// The most a ball is worth: a Nugget's ¥10,000 is no reason to cross a
/// map.
pub const MAX_VALUE_S: f64 = 90.0;
/// What no mart sells (price 0: evolution stones, key items, HMs).
pub const UNPRICED_S: f64 = 60.0;
/// A TM teaches a move the shops mostly don't sell.
pub const TM_MIN_S: f64 = 60.0;
/// Facing the ball, the text and the fanfare.
pub const SERVICE_S: f64 = 6.0;
/// Balls farther than this (tiles, straight) aren't looked at.
pub const MAX_TILES: i32 = 20;
/// Nor those whose way there costs more (tiles, tall grass extra).
pub const MAX_WALK: i32 = 40;
/// The tools whose walks a pickup may interrupt.
const WALKS: [&str; 3] = ["Go", "Train", "Catch"];

/// An item ball on a map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ball {
    pub map: String,
    pub object: u32,
    pub tile: (i32, i32),
    pub script: String,
    pub item: String,
    /// The flag `removeobject` sets once it is taken.
    pub flag: Option<String>,
}

/// A ball worth the detour.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pickup {
    pub ball: Ball,
    /// Where it is faced from.
    pub spot: (i32, i32),
    pub value_s: f64,
    /// The way there and back to the walk, plus [`SERVICE_S`].
    pub cost_s: f64,
}

/// What `item` is worth in seconds of play: its price at
/// [`MONEY_PER_S`], at most [`MAX_VALUE_S`]; [`UNPRICED_S`] when no mart
/// sells it, at least [`TM_MIN_S`] for a TM.
pub fn value_s(data: &GameData, item: &str) -> f64 {
    let Some(it) = data.items.get(item) else {
        return 0.0;
    };
    let priced = if it.price == 0 {
        UNPRICED_S
    } else {
        f64::from(it.price) / MONEY_PER_S
    };
    let tm = it.pocket.as_deref() == Some("POCKET_TM_CASE");
    let priced = if tm { priced.max(TM_MIN_S) } else { priced };
    priced.min(MAX_VALUE_S)
}

/// The item balls of `map`: objects whose script finds an item.
pub fn balls_on(world: &World, map: &str) -> Vec<Ball> {
    let (Some(m), Some(events)) = (world.map(map), world.events()) else {
        return Vec::new();
    };
    m.objects
        .iter()
        .filter_map(|o| {
            let label = o.script.as_ref()?;
            let script = events.script(label)?;
            let [only] = script.paths.as_slice() else {
                return None;
            };
            let item = only.does.iter().find_map(|e| match e {
                Effect::Give {
                    give, find: true, ..
                } => Some(give.clone()),
                _ => None,
            })?;
            Some(Ball {
                map: map.to_owned(),
                object: o.local_id,
                tile: (o.x?, o.y?),
                script: label.clone(),
                item,
                flag: o.flag.clone(),
            })
        })
        .collect()
}

/// Slots per pocket (`BAG_*_COUNT`).
fn slots(pocket: Pocket) -> usize {
    match pocket {
        Pocket::Items => 42,
        Pocket::KeyItems => 30,
        Pocket::PokeBalls => 13,
        Pocket::TmCase => 58,
        Pocket::BerryPouch => 43,
    }
}

/// Whether the bag takes one more `item`: its stack isn't at 999, or its
/// pocket has a free slot. An unknown pocket is given the benefit of the
/// doubt (a full one says so, and the ball is tried once).
pub fn bag_takes(state: &GameState, data: &GameData, item: &str) -> bool {
    let Some(pocket) = data
        .items
        .get(item)
        .and_then(|i| i.pocket.as_deref())
        .and_then(Pocket::from_decomp)
    else {
        return true;
    };
    let Some(held) = state
        .bag
        .pockets
        .get(&pocket)
        .and_then(|k| k.value.as_ref())
    else {
        return true;
    };
    match held.iter().find(|(i, _)| i == item) {
        Some((_, n)) => *n < 999,
        None => held.len() < slots(pocket),
    }
}

/// Where the player is and what a pickup must keep to.
pub struct Around<'a> {
    pub world: &'a World,
    pub data: &'a GameData,
    pub state: &'a GameState,
    /// Objects known gone (taken balls).
    pub gone: &'a Gone,
    /// Tiles found blocked on the player's map.
    pub blocked: &'a Obstacles,
    pub tried: &'a BTreeSet<(String, u32)>,
    /// The walk's destination map and the graph to price the way on to it
    /// from the ball; `None` (or the player's map): the way back is to
    /// where the player stands.
    pub onward: Option<(&'a PlaceGraph, &'a str)>,
}

/// The ball on the player's map best worth its detour, if any: not taken
/// (its flag not set, not seen gone), not tried, the bag has room for its
/// item, near ([`MAX_TILES`], [`MAX_WALK`]), its way there and back
/// clear of unbeaten trainers' sight, and worth more than that way.
pub fn choose(around: &Around<'_>, pose: &PlayerPose) -> Option<Pickup> {
    let world = around.world;
    let state = around.state;
    let map = world.map(&pose.map)?;
    let here = (pose.x, pose.y);
    let near: Vec<Ball> = balls_on(world, &pose.map)
        .into_iter()
        .filter(|b| {
            let key = (b.map.clone(), b.object);
            let taken = b
                .flag
                .as_deref()
                .is_some_and(|f| state.world.flag(f).value == Some(true));
            let shown = super::dialogue::object_shown(world, state, &b.map, b.object);
            !taken
                && shown != Some(false)
                && !around.gone.contains(&key)
                && !around.tried.contains(&key)
                && (b.tile.0 - here.0).abs() + (b.tile.1 - here.1).abs() <= MAX_TILES
                && bag_takes(state, around.data, &b.item)
        })
        .collect();
    if near.is_empty() {
        return None;
    }
    let mut obstacles = crate::nav::object_obstacles(map, around.gone);
    obstacles.extend(around.blocked.iter().copied());
    obstacles.remove(&here);
    let walk = Walk {
        obstacles: &obstacles,
        surf: false,
        opened: None,
    };
    let out = path::reach(map, here, &walk, |_| 0);
    let tile_s = RouteParams::default().tile_s;
    let belief = crate::belief_view::StateBelief(state);
    let onward = around.onward.filter(|(_, dest)| *dest != pose.map);
    let baseline = onward.map(|(graph, dest)| {
        route::open_route_to_map(
            world,
            graph,
            &belief,
            pose,
            dest,
            UnknownPolicy::Pessimistic,
        )
        .cost_s
    });
    // The walker goes round trainers' sight only with a worn lead: the
    // plain way there and back must not cross it (emulator, Mt. Moon 1F:
    // the way to the Paralyze Heal crossed Bug Catcher Kent's line).
    let trainers = around
        .data
        .map_trainers
        .get(&pose.map)
        .map_or(&[][..], |t| t.as_slice());
    let sight = crate::nav::sight_tiles(map, trainers, |t| {
        state.world.flags.get(t).and_then(|k| k.value) == Some(true)
    });
    // Nor a trigger that may still fire (fleet workers 4 and 6, Nugget
    // Bridge: the way to TM45 crossed the grunt's trigger; his battle came
    // as a plain one, the farm's expected white-out read as a failure).
    let armed: std::collections::HashSet<(i32, i32)> = world
        .events()
        .map(|e| {
            e.triggers
                .iter()
                .filter(|t| t.map == pose.map && t.script.is_some())
                .filter(|t| {
                    route::requirement_of(&t.when).is_none_or(|req| {
                        req.iter().all(|q| {
                            pokebot_world::predicate::BeliefView::eval(&belief, q)
                                != pokebot_world::predicate::Truth::False
                        })
                    })
                })
                .map(|t| (t.x, t.y))
                .collect()
        })
        .unwrap_or_default();
    let seen = |steps: Option<Vec<path::Step>>| {
        steps.is_some_and(|steps| {
            steps
                .iter()
                .any(|s| sight.contains(&s.to) || armed.contains(&s.to))
        })
    };
    let mut best: Option<Pickup> = None;
    for ball in near {
        let value = value_s(around.data, &ball.item);
        let Some((spot, there)) = crate::nav::facing_spots(map, ball.tile.0, ball.tile.1)
            .into_iter()
            .map(|(t, _)| t)
            .filter(|t| (t.0 - ball.tile.0).abs() + (t.1 - ball.tile.1).abs() == 1)
            .filter_map(|t| Some((t, out.cost(t)?)))
            .min_by_key(|&(t, c)| (c, t))
        else {
            continue;
        };
        if there > MAX_WALK || f64::from(there) * tile_s + SERVICE_S >= value {
            continue;
        }
        if seen(out.path(spot)) {
            continue;
        }
        // On to the destination from the ball, less the way from here;
        // else back to here.
        let priced_on = match (onward, baseline) {
            (Some((graph, dest)), Some(base)) if base.is_finite() => {
                let from = PlayerPose {
                    map: pose.map.clone(),
                    x: spot.0,
                    y: spot.1,
                };
                let via = route::open_route_to_map(
                    world,
                    graph,
                    &belief,
                    &from,
                    dest,
                    UnknownPolicy::Pessimistic,
                )
                .cost_s;
                via.is_finite().then(|| (via - base).max(0.0))
            }
            _ => None,
        };
        let back_s = match priced_on {
            Some(s) => s,
            None => {
                let mut obstacles = obstacles.clone();
                obstacles.remove(&spot);
                let walk = Walk {
                    obstacles: &obstacles,
                    surf: false,
                    opened: None,
                };
                let back = path::reach(map, spot, &walk, |_| 0);
                match back.cost(here) {
                    Some(c) if !seen(back.path(here)) => f64::from(c) * tile_s,
                    _ => continue,
                }
            }
        };
        let cost = f64::from(there) * tile_s + back_s + SERVICE_S;
        if cost >= value {
            continue;
        }
        let better = best
            .as_ref()
            .is_none_or(|b| value - cost > b.value_s - b.cost_s);
        if better {
            best = Some(Pickup {
                ball,
                spot,
                value_s: value,
                cost_s: cost,
            });
        }
    }
    best
}

/// Whether a pickup may suspend what runs: a scheduled run, only walking
/// tools on the stack (not a battle, a conversation, a heal), no script
/// being followed, no recovery or pickup under way, and no urgent heal,
/// audit or location check due.
pub fn may_detour(scheduler: &crate::scheduler::Scheduler, running: &[&str], script: bool) -> bool {
    use crate::scheduler::Need;
    let walking = !running.is_empty() && running.iter().all(|t| WALKS.contains(t));
    scheduler.enabled
        && !scheduler.recovering
        && !scheduler.picking
        && !script
        && walking
        && !scheduler.queue.iter().any(|n| {
            matches!(
                n,
                Need::HealUrgent | Need::AuditParty | Need::ConfirmLocation
            )
        })
}

/// Picks up the ball [`choose`] finds, between the steps of a walking
/// tool: `true` when one was gone for (the step looks afresh). Never in
/// a scene, a recovery, another pickup, or with a heal or an audit due.
pub fn pick_up_on_the_way(ctx: &mut ToolContext<'_>) -> Result<bool, ToolError> {
    if !may_detour(&ctx.scheduler, ctx.running(), ctx.running_script.is_some()) {
        return Ok(false);
    }
    let Some(pose) = ctx.pose() else {
        return Ok(false);
    };
    if ctx.scheduler.pickup_checked.as_ref() == Some(&pose) {
        return Ok(false);
    }
    ctx.scheduler.pickup_checked = Some(pose.clone());
    let world = std::sync::Arc::clone(&ctx.world);
    let data = std::sync::Arc::clone(&ctx.data);
    // The graph is only built for a walk to another map with a ball near.
    let near = balls_on(&world, &pose.map).iter().any(|b| {
        (b.tile.0 - pose.x).abs() + (b.tile.1 - pose.y).abs() <= MAX_TILES
            && !ctx.gone.contains(&(b.map.clone(), b.object))
    });
    if !near {
        return Ok(false);
    }
    let destination = ctx.scheduler.destination.clone();
    if destination.as_deref().is_some_and(|d| d != pose.map) && ctx.scheduler.graph.is_none() {
        ctx.scheduler.graph = Some(crate::scheduler::graph(&world));
    }
    let mut blocked = ctx
        .blocked
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .on_map(&pose.map);
    // Passages the belief holds shut too (fleet continue-3, Silph Co. 5F:
    // door 2 seen shut, and the PROTEIN and TM01 behind it were walked to
    // in turn, each failing "looping" at the door).
    blocked.extend(ctx.gate_tiles().closed_on(&pose.map));
    let choice = choose(
        &Around {
            world: &world,
            data: &data,
            state: ctx.state(),
            gone: &ctx.gone,
            blocked: &blocked,
            tried: &ctx.scheduler.pickups_tried,
            onward: ctx.scheduler.graph.as_ref().zip(destination.as_deref()),
        },
        &pose,
    );
    let Some(choice) = choice else {
        return Ok(false);
    };
    ctx.runtime
        .explain("pickup: an item ball on the way", &choice);
    let ball = &choice.ball;
    ctx.scheduler
        .pickups_tried
        .insert((ball.map.clone(), ball.object));
    ctx.scheduler.picking = true;
    let outcome = ctx.invoke(&Intent::RunScript {
        script: ball.script.clone(),
        path: Some(0),
        answers: Vec::new(),
    });
    ctx.scheduler.picking = false;
    let taken = outcome.learned.iter().any(|e| {
        matches!(e, GameEvent::ItemsChanged { item, delta, .. } if *item == ball.item && *delta > 0)
    });
    match outcome.result {
        Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
        Err(ToolError::Failed(why)) if why.contains("whited out") => {
            return Err(ToolError::Failed(why))
        }
        Err(e) => ctx.info(format!("pickup: {} not taken: {e}", ball.script)),
        Ok(()) if taken => ctx.info(format!("pickup: took {} from {}", ball.item, ball.script)),
        Ok(()) => ctx.info(format!("pickup: {} gave nothing", ball.script)),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_state::Knowledge;

    fn loaded() -> Option<(World, GameData)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let world = World::load(&dir).ok()?;
        world.events()?;
        Some((world, GameData::load(dir.join("gamedata.json")).ok()?))
    }

    fn at(map: &str, x: i32, y: i32) -> PlayerPose {
        PlayerPose {
            map: map.into(),
            x,
            y,
        }
    }

    fn pick(
        world: &World,
        data: &GameData,
        state: &GameState,
        tried: &BTreeSet<(String, u32)>,
        pose: &PlayerPose,
    ) -> Option<Pickup> {
        let gone = crate::nav::belief_gone(world, state);
        choose(
            &Around {
                world,
                data,
                state,
                gone: &gone,
                blocked: &Obstacles::new(),
                tried,
                onward: None,
            },
            pose,
        )
    }

    /// A Potion is worth its ¥300 as 30 s; a Rare Candy or a Nugget no
    /// more than the cap; a TM at least a minute; a Moon Stone, sold
    /// nowhere, a minute.
    #[test]
    fn an_item_is_worth_its_price_in_seconds() {
        let Some((_, d)) = loaded() else { return };
        assert_eq!(value_s(&d, "ITEM_POTION"), 30.0);
        assert_eq!(value_s(&d, "ITEM_ANTIDOTE"), 10.0);
        assert_eq!(value_s(&d, "ITEM_ESCAPE_ROPE"), 55.0);
        assert_eq!(value_s(&d, "ITEM_RARE_CANDY"), MAX_VALUE_S);
        assert_eq!(value_s(&d, "ITEM_NUGGET"), MAX_VALUE_S);
        assert_eq!(value_s(&d, "ITEM_TM09"), MAX_VALUE_S);
        assert!(value_s(&d, "ITEM_TM28") >= TM_MIN_S);
        assert_eq!(value_s(&d, "ITEM_MOON_STONE"), UNPRICED_S);
        assert_eq!(value_s(&d, "ITEM_NONE_SUCH"), 0.0);
    }

    #[test]
    fn the_balls_of_a_map_are_its_finditem_objects() {
        let Some((world, _)) = loaded() else { return };
        let balls = balls_on(&world, "ViridianForest");
        assert_eq!(balls.len(), 4, "{balls:?}");
        let antidote = balls.iter().find(|b| b.item == "ITEM_ANTIDOTE").unwrap();
        assert_eq!((antidote.object, antidote.tile), (7, (40, 21)));
        assert_eq!(
            antidote.flag.as_deref(),
            Some("FLAG_HIDE_VIRIDIAN_FOREST_ANTIDOTE")
        );
        // Route 1 has none; Mt. Moon 1F six.
        assert!(balls_on(&world, "Route1").is_empty());
        assert_eq!(balls_on(&world, "MtMoon_1F").len(), 6);
    }

    /// Viridian Forest, three tiles from the Antidote at (40, 21): a
    /// ¥100 item, worth the few seconds there and back. Forty tiles away,
    /// not looked at; taken (its flag set), seen gone, or tried before,
    /// not again.
    #[test]
    fn a_ball_a_few_tiles_off_is_picked_up_once() {
        let Some((world, d)) = loaded() else { return };
        let state = GameState::default();
        let none = BTreeSet::new();
        let near = at("ViridianForest", 40, 24);
        let p = pick(&world, &d, &state, &none, &near).expect("the Antidote");
        assert_eq!(p.ball.item, "ITEM_ANTIDOTE");
        assert_eq!(p.spot, (40, 22));
        assert!(p.cost_s < p.value_s, "{p:?}");
        // Forty tiles off.
        assert!(
            pick(&world, &d, &state, &none, &at("ViridianForest", 40, 61))
                .is_none_or(|p| p.ball.item != "ITEM_ANTIDOTE")
        );
        // Taken: its hide flag set.
        let mut taken = GameState::default();
        taken.world.flags.insert(
            "FLAG_HIDE_VIRIDIAN_FOREST_ANTIDOTE".into(),
            Knowledge::observed(true, 1),
        );
        assert!(pick(&world, &d, &taken, &none, &near).is_none());
        // Seen gone.
        let mut seen = GameState::default();
        seen.world.npc_mut("ViridianForest", 7).present = Knowledge::observed(false, 1);
        assert!(pick(&world, &d, &seen, &none, &near).is_none());
        // Tried and not taken.
        let tried = BTreeSet::from([("ViridianForest".to_owned(), 7)]);
        assert!(pick(&world, &d, &state, &tried, &near).is_none());
    }

    /// Mt. Moon 1F, the Geodude hunt walking (18, 34): the TM09 ball at
    /// (11, 35) is a few tiles off, and a TM is worth the walk.
    #[test]
    fn a_tm_near_a_hunt_is_picked_up() {
        let Some((world, d)) = loaded() else { return };
        let state = GameState::default();
        let p = pick(
            &world,
            &d,
            &state,
            &BTreeSet::new(),
            &at("MtMoon_1F", 18, 34),
        )
        .expect("a ball");
        assert_eq!(p.ball.item, "ITEM_TM09", "{p:?}");
    }

    /// Emulator, Mt. Moon 1F, the TM taken at (12, 31): the way to the
    /// Paralyze Heal at (2, 22) crossed Bug Catcher Kent's line and the
    /// detour became a battle. Kent unbeaten, it is left; beaten, it is
    /// worth its way.
    #[test]
    fn a_ball_past_an_unbeaten_trainers_sight_is_left() {
        let Some((world, d)) = loaded() else { return };
        let tried = BTreeSet::from([("MtMoon_1F".to_owned(), 9)]);
        let from = at("MtMoon_1F", 12, 31);
        let mut state = GameState::default();
        let p = pick(&world, &d, &state, &tried, &from);
        assert!(
            p.as_ref()
                .is_none_or(|p| p.ball.item != "ITEM_PARALYZE_HEAL"),
            "{p:?}"
        );
        state.world.flags.insert(
            "TRAINER_BUG_CATCHER_KENT".into(),
            Knowledge::observed(true, 1),
        );
        let p = pick(&world, &d, &state, &tried, &from).expect("a ball");
        assert_eq!(p.ball.item, "ITEM_PARALYZE_HEAL", "{p:?}");
    }

    /// Fleet workers 4 and 6, Nugget Bridge: from the farm's tile (11, 16)
    /// the way to TM45 (11, 4) crosses the grunt's trigger, armed while
    /// VAR_MAP_SCENE_ROUTE24 is 0; left then, taken once he is beaten.
    #[test]
    fn a_ball_past_an_armed_trigger_is_left() {
        let Some((world, d)) = loaded() else { return };
        let from = at("Route24", 11, 16);
        let mut state = GameState::default();
        state
            .world
            .vars
            .insert("VAR_MAP_SCENE_ROUTE24".into(), Knowledge::observed(0, 1));
        let tm45 = |state: &GameState| {
            pick(&world, &d, state, &BTreeSet::new(), &from)
                .is_some_and(|p| p.ball.item == "ITEM_TM45")
        };
        assert!(!tm45(&state));
        state
            .world
            .vars
            .insert("VAR_MAP_SCENE_ROUTE24".into(), Knowledge::observed(1, 1));
        assert!(tm45(&state));
    }

    /// Fleet continue-3, Silph Co. 5F at (9, 18): door 2 seen shut, the
    /// TM01 behind it was walked to and failed "looping" at the door. The
    /// passages the belief holds shut are walls to a pickup too.
    #[test]
    fn a_ball_behind_a_shut_door_is_left() {
        let Some((world, d)) = loaded() else { return };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cont3_silph_5f_state.json");
        let Ok(Some(checkpoint)) = crate::checkpoint::load(&path) else {
            return;
        };
        use pokebot_state::StateReducer;
        let state = pokebot_state::DefaultReducer.reduce(
            &GameState::default(),
            &[pokebot_state::EventRecord {
                frame_id: 0,
                event: GameEvent::CheckpointRestored {
                    knowledge: Box::new(checkpoint.knowledge),
                },
            }],
        );
        assert_eq!(
            state
                .world
                .flags
                .get("FLAG_SILPH_5F_DOOR_2")
                .and_then(|k| k.value),
            Some(false)
        );
        let from = at("SilphCo_5F", 9, 18);
        let gone = crate::nav::belief_gone(&world, &state);
        let pick_with = |blocked: &Obstacles| {
            choose(
                &Around {
                    world: &world,
                    data: &d,
                    state: &state,
                    gone: &gone,
                    blocked,
                    tried: &BTreeSet::new(),
                    onward: None,
                },
                &from,
            )
            .map(|p| p.ball.item)
        };
        // As it was: the shut door no wall.
        let before = pick_with(&Obstacles::new());
        let shut: Obstacles = pokebot_world::gates::GateTiles::believed(
            &world,
            &pokebot_world::gates::derive(&world),
            &crate::belief_view::StateBelief(&state),
        )
        .closed_on("SilphCo_5F")
        .collect();
        let after = pick_with(&shut);
        // The PROTEIN behind door 1 and the TM01 behind door 2, both
        // seen shut, were each walked to in turn.
        let walled =
            |item: &Option<String>| matches!(item.as_deref(), Some("ITEM_PROTEIN" | "ITEM_TM01"));
        assert!(walled(&before), "{before:?}");
        assert!(!walled(&after), "{after:?}");
    }

    /// A cheap item is not worth a long way: the Antidote from twelve
    /// tiles off, round the trees, costs more than its ¥100.
    #[test]
    fn a_cheap_item_far_off_is_left() {
        let Some((world, d)) = loaded() else { return };
        let state = GameState::default();
        let far = at("ViridianForest", 40, 33);
        assert!(pick(&world, &d, &state, &BTreeSet::new(), &far)
            .is_none_or(|p| p.ball.item != "ITEM_ANTIDOTE"));
    }

    /// Only a walk is detoured from: not a battle or a conversation on
    /// the way, not a heal, not with an urgent need due.
    #[test]
    fn only_a_walk_with_nothing_urgent_is_detoured_from() {
        use crate::scheduler::{Need, Scheduler};
        let s = Scheduler {
            enabled: true,
            ..Default::default()
        };
        assert!(may_detour(&s, &["Go"], false));
        assert!(may_detour(&s, &["Catch", "Go"], false));
        assert!(may_detour(&s, &["Train"], false));
        // Nothing running, or not a walk.
        assert!(!may_detour(&s, &[], false));
        assert!(!may_detour(&s, &["Go", "Battle"], false));
        assert!(!may_detour(&s, &["Go", "Unstick"], false));
        assert!(!may_detour(&s, &["Heal", "Go"], false));
        assert!(!may_detour(&s, &["Go", "RunScript"], false));
        // A script being followed.
        assert!(!may_detour(&s, &["Go"], true));
        // Not scheduled, recovering, already picking.
        assert!(!may_detour(&Scheduler::default(), &["Go"], false));
        for flag in [0, 1] {
            let mut busy = Scheduler {
                enabled: true,
                ..Default::default()
            };
            if flag == 0 {
                busy.recovering = true;
            } else {
                busy.picking = true;
            }
            assert!(!may_detour(&busy, &["Go"], false));
        }
        // Urgent needs first; a heal that can wait doesn't stop it.
        for need in [Need::HealUrgent, Need::AuditParty, Need::ConfirmLocation] {
            let mut due = Scheduler {
                enabled: true,
                ..Default::default()
            };
            due.queue.push_back(need);
            assert!(!may_detour(&due, &["Go"], false), "{need:?}");
        }
        let mut soon = Scheduler {
            enabled: true,
            ..Default::default()
        };
        soon.queue.push_back(Need::HealSoon);
        assert!(may_detour(&soon, &["Go"], false));
    }

    /// A full pocket takes no new item; a stack already held takes one more.
    #[test]
    fn a_full_pocket_takes_no_new_item() {
        let Some((_, d)) = loaded() else { return };
        let mut state = GameState::default();
        assert!(bag_takes(&state, &d, "ITEM_POTION"));
        let full: Vec<(String, u16)> = (0..42).map(|i| (format!("ITEM_{i}"), 1)).collect();
        state
            .bag
            .pockets
            .insert(Pocket::Items, Knowledge::observed(full, 1));
        assert!(!bag_takes(&state, &d, "ITEM_POTION"));
        assert!(bag_takes(&state, &d, "ITEM_POKE_BALL"));
        let mut held: Vec<(String, u16)> = (0..41).map(|i| (format!("ITEM_{i}"), 1)).collect();
        held.push(("ITEM_POTION".into(), 3));
        state
            .bag
            .pockets
            .insert(Pocket::Items, Knowledge::observed(held, 1));
        assert!(bag_takes(&state, &d, "ITEM_POTION"));
    }
}
