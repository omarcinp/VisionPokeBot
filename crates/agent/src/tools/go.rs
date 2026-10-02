//! `Go`: walk to a destination with the navigator (route across maps,
//! holds and taps within one), settling after any cutscene first. A
//! `Dest::Map` leg ends on arrival on the map, whichever tile.
//!
//! Motion loops (spec §8): a leg that arrives on the same tile four times
//! without getting closer to its goal fails, so the goal loop replans
//! instead of bouncing off an NPC forever. Acts from one tile (a turn, a
//! tap that learns a blocker, the replanned step around it) are not visits:
//! the walker gets its chance to route around before the leg fails.

use std::collections::HashMap;
use std::sync::Arc;

use pokebot_state::{GameEvent, PlayerPose};
use pokebot_world::behavior::is_water;
use pokebot_world::gates::GateTiles;
use pokebot_world::path::{find_path_with, Walk};
use pokebot_world::route::{self, EdgeKind, Leg, Place, UnknownPolicy};
use pokebot_world::World;

use super::field::{self, FieldMove};
use super::scene::SettleStep;
use super::{
    Dest, Expects, Intent, StepContext, Tool, ToolContext, ToolError, ToolOutcome, ToolStep,
    SETTLE_FRAMES,
};
use crate::belief_view::StateBelief;
use crate::motion::{InputKind, SyncerHandle};
use crate::nav::{goal_tiles, Blocked, Destination, Gone, NavStatus, Navigator};
use crate::{Action, Decision, Outcome};

/// Taps in a row that failed to move the player before the leg fails.
const MAX_STALLED: u32 = 6;
/// Arrivals on the same tile, without the leg getting closer to its goal,
/// before it counts as a loop.
pub const MAX_TILE_VISITS: u32 = 4;

pub struct GoTool;

/// One leg to `dest`, as a step.
pub struct GoStep {
    nav: Navigator,
    dest: Destination,
    world: Arc<World>,
    /// Trainers' sight ([`crate::nav::sight_tiles`]); `None` in tests.
    data: Option<Arc<pokebot_gamedata::GameData>>,
    /// Done as soon as the player stands on the destination's map.
    any_tile: bool,
    /// Arrivals per tile since the leg last got closer to its goal.
    visits: HashMap<(String, i32, i32), u32>,
    /// Closest the leg has been to its goal tiles on the destination map.
    best: Option<i32>,
    last_map: Option<String>,
    /// The tile the last act was issued from.
    last_tile: Option<(String, i32, i32)>,
    /// A tile learnt as blocked by the last act, and the frame: taken
    /// back if something (a wild battle's fade, a trainer's "!") came up
    /// right after, since that is what stopped the step.
    last_block: Option<(String, (i32, i32), u64)>,
}

/// What a navigator is built from: the world, the objects known to be
/// gone, and the timing model. Steps keep one to rebuild their legs.
#[derive(Clone)]
pub struct NavParts {
    pub world: Arc<World>,
    pub gone: Gone,
    /// Objects whose hide flag the belief doesn't know.
    pub maybe_gone: Gone,
    pub syncer: Option<SyncerHandle>,
    /// The session's tiles learnt to be blocked.
    pub blocked: Blocked,
    /// Story passages as the belief stood when the parts were taken.
    pub gates: Arc<GateTiles>,
    /// The game data (trainers' sight), when the walk has it.
    pub data: Option<Arc<pokebot_gamedata::GameData>>,
}

impl NavParts {
    pub fn of(ctx: &ToolContext<'_>) -> Self {
        Self {
            world: Arc::clone(&ctx.world),
            gone: ctx.gone.clone(),
            maybe_gone: crate::nav::unknown_gone(&ctx.world, ctx.state()),
            syncer: ctx.syncer.clone(),
            blocked: Arc::clone(&ctx.blocked),
            gates: Arc::new(ctx.gate_tiles()),
            data: Some(Arc::clone(&ctx.data)),
        }
    }
}

impl GoStep {
    pub fn new(ctx: &ToolContext<'_>, dest: Destination) -> Self {
        Self::with(&NavParts::of(ctx), dest)
    }

    pub fn with(parts: &NavParts, dest: Destination) -> Self {
        Self::with_surf(parts, dest, false)
    }

    /// A leg that may cross water: the player surfs (or is about to).
    pub fn with_surf(parts: &NavParts, dest: Destination, surf: bool) -> Self {
        let nav = Navigator::new(Arc::clone(&parts.world), dest.clone())
            .with_gone(parts.gone.clone())
            .with_maybe_gone(parts.maybe_gone.clone())
            .with_blocked(Arc::clone(&parts.blocked))
            .with_gates(&parts.gates)
            .with_surf(surf);
        let nav = match &parts.syncer {
            Some(syncer) => nav.with_syncer(Arc::clone(syncer)),
            None => nav,
        };
        Self {
            nav,
            dest,
            world: Arc::clone(&parts.world),
            data: parts.data.clone(),
            any_tile: false,
            visits: HashMap::new(),
            best: None,
            last_map: None,
            last_tile: None,
            last_block: None,
        }
    }

    /// A leg to any tile of `map`: routed to a walkable tile near its
    /// middle, done on arrival on the map.
    pub fn to_map(parts: &NavParts, map: &str) -> Self {
        let (x, y) = map_tile(&parts.world, map).unwrap_or((0, 0));
        let mut step = Self::with(
            parts,
            Destination::Tile {
                map: map.to_owned(),
                x,
                y,
            },
        );
        step.any_tile = true;
        step.nav = std::mem::replace(
            &mut step.nav,
            Navigator::new(Arc::clone(&parts.world), step.dest.clone()),
        )
        .with_any_tile();
        step
    }

    /// Where the walk ends.
    pub fn destination(&self) -> &Destination {
        &self.dest
    }

    /// Records an act issued from `pose`; `true` when the leg loops. Only
    /// the first act from a tile counts as a visit: the rest (a turn, a
    /// tap that learns a blocker, the step around it) are the walker's
    /// way around.
    fn note_visit(&mut self, pose: &PlayerPose) -> bool {
        if self.last_map.as_deref() != Some(pose.map.as_str()) {
            self.last_map = Some(pose.map.clone());
            self.visits.clear();
            self.best = None;
        }
        let tile = (pose.map.clone(), pose.x, pose.y);
        if self.last_tile.as_ref() == Some(&tile) {
            return false;
        }
        self.last_tile = Some(tile.clone());
        let dist = if pose.map == self.dest.map() {
            goal_tiles(&self.world, &self.dest)
                .into_iter()
                .map(|(x, y)| (x - pose.x).abs() + (y - pose.y).abs())
                .min()
        } else {
            None
        };
        if let Some(d) = dist {
            if self.best.is_none_or(|b| d < b) {
                self.best = Some(d);
                self.visits.clear();
            }
        }
        let n = self.visits.entry(tile).or_insert(0);
        *n += 1;
        *n >= MAX_TILE_VISITS
    }
}

/// A walkable tile of `map` near its middle.
fn map_tile(world: &World, map: &str) -> Option<(i32, i32)> {
    let m = world.map(map)?;
    let (cx, cy) = (m.width / 2, m.height / 2);
    let mut best: Option<(i32, (i32, i32))> = None;
    for y in 0..m.height {
        for x in 0..m.width {
            if m.tile(x, y).is_some_and(|t| t.collision == 0) {
                let d = (x - cx).abs() + (y - cy).abs();
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, (x, y)));
                }
            }
        }
    }
    best.map(|(_, t)| t)
}

impl GoStep {
    /// The sight of the trainers on `map` not believed beaten, while the
    /// lead is under [`LEAD_HP_MIN`] % HP (walking to heal, typically): a
    /// trainer's battle can't be fled, so the walk goes round them when
    /// it can. Empty otherwise.
    fn trainer_sight(
        &self,
        state: &pokebot_state::GameState,
        map: &str,
    ) -> pokebot_world::path::Obstacles {
        use pokebot_planner::intents::LEAD_HP_MIN;
        let worn = state
            .party
            .value
            .as_ref()
            .and_then(|p| p.first())
            .and_then(|m| m.hp.value)
            .is_some_and(|(hp, max)| u32::from(hp) * 100 < u32::from(max) * u32::from(LEAD_HP_MIN));
        let (Some(data), Some(m)) = (self.data.as_ref().filter(|_| worn), self.world.map(map))
        else {
            return Default::default();
        };
        let trainers = data.map_trainers.get(map).map_or(&[][..], |t| t.as_slice());
        crate::nav::sight_tiles(m, trainers, |t| {
            state.world.flags.get(t).and_then(|k| k.value) == Some(true)
        })
    }
}

impl ToolStep for GoStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        if ctx.quiet_frames < SETTLE_FRAMES {
            return Decision::Wait("letting the scene settle".into());
        }
        if let Some((map, tile, at)) = self.last_block.take() {
            // Flash-7: a tap onto MtMoon_B2F (43, 22) timed out because a
            // wild encounter froze the player, and its fade came only
            // after the timeout; the tile was learnt as blocked.
            let since = ctx.observation.frame_id.saturating_sub(at);
            if u64::from(ctx.quiet_frames) < since {
                self.nav.unlearn(&map, tile);
                ctx.events.push(GameEvent::TileUnblocked {
                    map,
                    x: tile.0,
                    y: tile.1,
                });
            }
        }
        let pose = ctx.observation.player.as_ref().map(|p| p.pose.clone());
        if self.any_tile && pose.as_ref().is_some_and(|p| p.map == self.dest.map()) {
            return Decision::Done(format!("arrived on {}", self.dest.map()));
        }
        if self.nav.stalled() >= MAX_STALLED {
            return Decision::Fail(format!("cannot move toward {:?}", self.dest));
        }
        if let Some(pose) = &pose {
            let avoid = self.trainer_sight(ctx.state, &pose.map);
            self.nav.set_avoid(&pose.map, avoid);
        }
        match self.nav.next(ctx.observation) {
            NavStatus::Arrived => Decision::Done(format!("arrived at {:?}", self.dest)),
            NavStatus::Act(action) => {
                let turn = action
                    .timing
                    .is_some_and(|(kind, _)| kind == InputKind::Turn);
                if let Some(pose) = pose.filter(|_| !turn) {
                    if self.note_visit(&pose) {
                        return Decision::Fail(format!(
                            "looping at {pose}: {MAX_TILE_VISITS} acts from the same tile without progress toward {:?}",
                            self.dest
                        ));
                    }
                }
                Decision::Act(action)
            }
            NavStatus::Wait(reason) => Decision::Wait(reason),
            NavStatus::Fail(reason) => Decision::Fail(reason),
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        if let Some((map, (x, y))) = self.nav.on_outcome(action, outcome, ctx.observation) {
            self.last_block = Some((map.clone(), (x, y), ctx.observation.frame_id));
            ctx.events.push(GameEvent::TileBlocked { map, x, y });
        }
    }

    fn expects(&self) -> Expects {
        Expects::NONE
    }
}

/// Walks to `dest`; the pose reached.
pub fn go(
    ctx: &mut ToolContext<'_>,
    dest: Destination,
) -> Result<Option<PlayerPose>, super::ToolError> {
    let mut step = GoStep::new(ctx, dest);
    ctx.drive(&mut step)?;
    Ok(ctx.pose())
}

/// Walks onto `map`; the pose reached.
pub fn go_to_map(
    ctx: &mut ToolContext<'_>,
    map: &str,
) -> Result<Option<PlayerPose>, super::ToolError> {
    let mut step = GoStep::to_map(&NavParts::of(ctx), map);
    ctx.drive(&mut step)?;
    Ok(ctx.pose())
}

impl Tool for GoTool {
    fn name(&self) -> &str {
        "Go"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Go { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Go { dest } = intent else {
            return ToolOutcome::failed("not a Go");
        };
        let first = match field_route(ctx, dest) {
            Some(legs) => {
                ctx.info(format!(
                    "go: the route needs field moves: {}",
                    legs.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
                walk_legs(ctx, &legs, dest)
            }
            None => match dest {
                Dest::Map { map } => go_to_map(ctx, map),
                other => go(ctx, Destination::from(other)),
            },
        };
        // No way on: switches that flip a flag may open one (Switch, the
        // Pokémon Mansion: "no known route from PokemonMansion_1F to
        // PokemonMansion_B1F").
        let walked = match first {
            Err(ToolError::Failed(why))
                if why.contains("no known route") || why.contains("no path") =>
            {
                super::toggles::through(ctx, dest).unwrap_or(Err(ToolError::Failed(why)))
            }
            other => other,
        };
        // A map may play a scene on arrival (an entry scene, a trigger at
        // the door): it is over, and recognised, before the leg is.
        let walked = walked.and_then(|pose| {
            ctx.drive(&mut SettleStep::new(SETTLE_FRAMES))?;
            Ok(ctx.pose().or(pose))
        });
        match walked {
            Ok(pose) => ToolOutcome {
                pose,
                ..ToolOutcome::ok()
            },
            Err(e) => e.into(),
        }
    }
}

/// Whether `leg` needs a field move or an item (or enters a dark map), or
/// is a ride in an elevator.
fn special(world: &World, leg: &Leg) -> bool {
    match &leg.kind {
        EdgeKind::Gate { .. }
        | EdgeKind::Fly
        | EdgeKind::Walk { surf: true, .. }
        | EdgeKind::Dig
        | EdgeKind::EscapeRope => true,
        EdgeKind::ScriptWarp { .. } => ride(world, leg),
        _ => field::is_dark(world, &leg.to.map) && !field::is_dark(world, &leg.from.map),
    }
}

/// Whether `leg` rides an elevator: a script (the floor panel) sets where
/// the car's door leads (a warp to `MAP_DYNAMIC`), then the player walks
/// out onto another map.
fn ride(world: &World, leg: &Leg) -> bool {
    matches!(leg.kind, EdgeKind::ScriptWarp { .. })
        && leg.to.map != leg.from.map
        && dynamic_door(world, &leg.from.map).is_some()
}

/// The first warp of `map` whose destination a script sets (an
/// elevator's door).
fn dynamic_door(world: &World, map: &str) -> Option<usize> {
    world
        .map(map)?
        .warps
        .iter()
        .position(|w| w.dest_map == "MAP_DYNAMIC")
}

/// The path of elevator panel `script` that takes the car to `to`, and
/// its answers (the floor's row of the menu): among the paths whose warp
/// lands on `to` and whose conditions the belief doesn't know false,
/// those it knows true first, then the one doing the most (a floor path
/// tests whether the car is already there, VAR_ELEVATOR_FLOOR, which
/// nothing tracks: the ride that sets it records where the car is).
pub fn ride_path(
    world: &World,
    script: &str,
    to: &str,
    belief: &dyn pokebot_world::predicate::BeliefView,
) -> Option<(usize, Vec<super::Answer>)> {
    use pokebot_world::events::Effect;
    use pokebot_world::predicate::Truth;
    let s = world.events()?.script(script)?;
    s.paths
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            p.does.iter().any(|e| match e {
                Effect::Warp { warp, .. } | Effect::SetWarp { set_warp: warp, .. } => {
                    world.name_of(warp) == Some(to)
                }
                _ => false,
            })
        })
        .filter_map(|(i, p)| {
            let req = route::requirement_of(&p.when)?;
            let truth: Vec<Truth> = req.iter().map(|q| belief.eval(q)).collect();
            if truth.contains(&Truth::False) {
                return None;
            }
            let known = truth.iter().all(|t| *t == Truth::True);
            Some((i, known, p.does.len()))
        })
        .max_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| b.0.cmp(&a.0))
        })
        .map(|(i, _, _)| {
            let answers = pokebot_planner::intents::path_answers(&s.paths[i]);
            let mut answers = super::parse_answers(&answers);
            // The floor chosen from a list menu reads as the result of the
            // special that drew it (Switch, Silph Co.: `special
            // InitElevatorFloorSelectMenuPos == 0` for 11F, no menu choice;
            // the ride had no answer, the menu was closed, the car stayed).
            let listed = s.paths[i].when.iter().rev().find_map(|c| match c {
                pokebot_world::events::Condition::Special { special, cmp }
                    if special.contains("ElevatorFloor") =>
                {
                    cmp.eq.as_ref()?.as_int()
                }
                _ => None,
            });
            let chosen = answers
                .iter()
                .any(|a| matches!(a, super::Answer::ListRow(_) | super::Answer::Menu(_)));
            if let (false, Some(row)) = (chosen, listed.and_then(|r| u8::try_from(r).ok())) {
                answers.insert(0, super::Answer::ListRow(row));
            }
            (i, answers)
        })
}

/// The route to `dest` from where the player stands, when it needs field
/// moves (a Cut or Rock Smash gate, Surf, Fly, Dig or an Escape Rope out
/// of a cave), enters a dark map with a
/// Flash user in the party, or rides an elevator the navigator can't get
/// there without; `None` when the navigator alone walks it. Requirements
/// the belief doesn't know count as unmet (the planner establishes them
/// first).
pub fn field_route(ctx: &mut ToolContext<'_>, dest: &Dest) -> Option<Vec<Leg>> {
    let pose = ctx.pose()?;
    let world = Arc::clone(&ctx.world);
    let graph = ctx
        .scheduler
        .graph
        .get_or_insert_with(|| crate::scheduler::graph(&world));
    plan_field_route(&world, graph, ctx.runtime.state(), &pose, dest)
}

/// [`field_route`] without a context.
pub fn plan_field_route(
    world: &World,
    graph: &route::PlaceGraph,
    state: &pokebot_state::GameState,
    pose: &PlayerPose,
    dest: &Dest,
) -> Option<Vec<Leg>> {
    let belief = StateBelief(state);
    // Unknown facts count as unmet, but for a trainer: one not known beaten
    // is a battle on the way, not a wall (Switch, Rocket Hideout: out of
    // Giovanni's side, the elevator to B1F passes GRUNT_12, never met on
    // the stairs down; "no known route" out).
    let policy = UnknownPolicy::Optimistic {
        penalty_of: battle_on_the_way,
    };
    let result = match dest {
        Dest::Map { map } => route::route_to_map(world, graph, &belief, pose, map, policy),
        Dest::Tile { map, x, y } => route::route(
            world,
            graph,
            &belief,
            pose,
            &Place::tile(map, *x, *y),
            policy,
        ),
        // Talking and warp legs end next to their target: the navigator's.
        Dest::Facing { .. } | Dest::Warp { .. } => return None,
    };
    if !result.found() {
        return None;
    }
    let flash = field::carrier(state, FieldMove::Flash.move_id()).is_some();
    let needed = result.legs.iter().any(|l| match &l.kind {
        EdgeKind::Gate { .. }
        | EdgeKind::Fly
        | EdgeKind::Walk { surf: true, .. }
        | EdgeKind::Dig
        | EdgeKind::EscapeRope => true,
        EdgeKind::ScriptWarp { .. } => false,
        // Entering a dark map matters only with someone to use Flash.
        _ => special(world, l) && flash,
    });
    // A ride the route chose is cheaper than any walk it priced: taken.
    // The navigator's own "walks there" was too coarse to veto it (Switch,
    // Silph Co. 3F: it found a way to 11F past the locked doors, walked
    // into them, "no path to warp (13, 14)", while the elevator was a
    // few steps away).
    let rides = result.legs.iter().any(|l| ride(world, l));
    // Through maps whose passages the story opens (Silph Co.'s doors), the
    // route's warps are followed: the navigator's own search across maps
    // is coarser there (Switch, Silph Co. 5F: it chose the pad at (2, 20)
    // behind a grunt standing in the corridor, "no path to warp", every
    // replan, while the route went by the pad at (15, 7)).
    let doors = |map: &str| {
        graph.gates().get(map).is_some_and(|on| {
            on.values()
                .any(|g| g.kind == pokebot_world::gates::GateKind::Metatile)
        })
    };
    // A warp onto its own map (Saffron Gym's pads) is one the navigator's
    // search between maps never takes: next to SABRINA, reached only by
    // the pads, every RunScript failed "no path next to (14, 11)".
    let gated = result
        .legs
        .iter()
        .any(|l| l.kind == EdgeKind::Warp && (doors(&l.from.map) || l.from.map == l.to.map));
    (needed || rides || gated).then_some(result.legs)
}

/// The price of assuming `p` for a route the tools walk: a trainer not
/// known beaten is fought on the way; anything else unknown is unmet.
pub(crate) fn battle_on_the_way(p: &pokebot_world::predicate::Predicate) -> f64 {
    match p {
        pokebot_world::predicate::Predicate::Flag { name, is: true }
            if name.starts_with("TRAINER_") =>
        {
            BATTLE_ON_THE_WAY_S
        }
        _ => f64::INFINITY,
    }
}

/// Seconds a battle on the way adds to a route.
const BATTLE_ON_THE_WAY_S: f64 = 90.0;

/// Walks to a place of the route (the navigator routes across maps).
fn walk_to(ctx: &mut ToolContext<'_>, place: &Place) -> Result<(), ToolError> {
    if ctx
        .pose()
        .is_some_and(|p| p.map == place.map && (p.x, p.y) == (place.x, place.y))
    {
        return Ok(());
    }
    go(
        ctx,
        Destination::Tile {
            map: place.map.clone(),
            x: place.x,
            y: place.y,
        },
    )
    .map(|_| ())
}

/// The shore tile and the first water tile of a surf leg's path from
/// `start` on the leg's map (its start, or a sandbar on the way: the
/// surfer lands on it and surfs again from it).
fn surf_entry_from(
    world: &World,
    leg: &Leg,
    start: (i32, i32),
) -> Option<((i32, i32), (i32, i32))> {
    let map = world.map(&leg.from.map)?;
    let obstacles = crate::nav::static_obstacles(map);
    let walk = Walk {
        obstacles: &obstacles,
        surf: true,
        opened: None,
    };
    let to = (leg.to.x, leg.to.y);
    let path = find_path_with(
        map,
        start,
        &walk,
        |_| 0,
        |p| p == to,
        |p| (p.0 - to.0).abs() + (p.1 - to.1).abs(),
    )?;
    let mut prev = start;
    for step in path {
        if map
            .tile(step.to.0, step.to.1)
            .is_some_and(|t| is_water(t.behavior))
        {
            return Some((prev, step.to));
        }
        prev = step.to;
    }
    None
}

fn on_water(ctx: &ToolContext<'_>) -> bool {
    ctx.pose().is_some_and(|p| {
        ctx.world
            .map(&p.map)
            .and_then(|m| m.tile(p.x, p.y))
            .is_some_and(|t| is_water(t.behavior))
    })
}

fn use_move(ctx: &mut ToolContext<'_>, mv: FieldMove, at: Option<Dest>) -> Result<(), ToolError> {
    ctx.invoke(&Intent::FieldMove {
        mv: mv.move_id().to_owned(),
        at,
        push: Vec::new(),
    })
    .result
}

/// Walks onto `map` first when the way there needs field moves: the tools
/// that walk to a target of their own (Talk, Buy, a hunt) drive the
/// navigator, which knows no Cut tree or water (Switch goal run: a Heal
/// from Vermilion Gym's side of the Cut tree failed "no path to door";
/// Go had cut its way in). Nothing to do on `map`, or when the navigator
/// walks there alone.
pub fn reach_map(ctx: &mut ToolContext<'_>, map: &str) -> Result<(), ToolError> {
    if ctx.pose().is_none_or(|p| p.map == map) {
        return Ok(());
    }
    let dest = Dest::Map {
        map: map.to_owned(),
    };
    let Some(legs) = field_route(ctx, &dest) else {
        return Ok(());
    };
    ctx.info(format!(
        "the way to {map} needs field moves: {}",
        legs.iter()
            .filter(|l| special(&ctx.world, l))
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" | ")
    ));
    walk_legs(ctx, &legs, &dest).map(|_| ())
}

/// Walks next to `at` on `map` through Cut trees (or water) when the
/// player is on `map` and the navigator can't get next to it on its own
/// (Switch, Celadon Gym: a Cut tree stands between the door and ERIKA;
/// [`reach_map`] only brings the player onto the map, and every RunScript
/// failed "no path next to (6, 4)"). Nothing to do otherwise.
pub fn reach_facing(ctx: &mut ToolContext<'_>, map: &str, at: (i32, i32)) -> Result<(), ToolError> {
    let Some(pose) = ctx.pose().filter(|p| p.map == map) else {
        return Ok(());
    };
    let world = Arc::clone(&ctx.world);
    let Some(m) = world.map(map) else {
        return Ok(());
    };
    let spots: Vec<(i32, i32)> = crate::nav::facing_spots(m, at.0, at.1)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    let gates = ctx.gate_tiles();
    let none = pokebot_world::path::Obstacles::new();
    if walks_to(m, (pose.x, pose.y), &spots, &ctx.gone, &gates, &none) {
        // The map's own way, but tiles found blocked on it shut it: someone
        // stands where the map has floor (Switch, Viridian Gym: Black Belt
        // Takashi walked down to challenge the player and stayed in the
        // one-tile corridor to Giovanni, "no path next to (2, 2)"). Going
        // out and back puts everyone on their own tile again.
        let learnt = ctx
            .blocked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_map(map);
        if !learnt.is_empty() && !walks_to(m, (pose.x, pose.y), &spots, &ctx.gone, &gates, &learnt)
        {
            reenter(ctx, map, &learnt)?;
        }
        return Ok(());
    }
    for (x, y) in spots {
        let dest = Dest::Tile {
            map: map.to_owned(),
            x,
            y,
        };
        if let Some(legs) = field_route(ctx, &dest) {
            ctx.info(format!(
                "the way next to {map} ({}, {}) needs field moves: {}",
                at.0,
                at.1,
                legs.iter()
                    .filter(|l| special(&ctx.world, l))
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" | ")
            ));
            return walk_legs(ctx, &legs, &dest).map(|_| ());
        }
    }
    // Behind a barrier a switch opens (Switch, the Pokémon Mansion's B1F:
    // the SECRET KEY, "no path next to (5, 7)" in either switch state the
    // plan pressed): the switches' states are searched.
    let next_to = Dest::Facing {
        map: map.to_owned(),
        x: at.0,
        y: at.1,
    };
    if let Some(walked) = super::toggles::through(ctx, &next_to) {
        return walked.map(|_| ());
    }
    Ok(())
}

/// Whether the navigator walks from `from` to one of `spots` on `m` around
/// its own obstacles: objects and closed gates (Switch, Silph Co. 11F: its
/// shut door was left out here, the way next to the door read plain, and
/// the walk then found none: "no path next to (6, 16)").
/// Leaves `map` by the nearest warp the walk reaches past `learnt`, comes
/// back, and forgets the tiles learnt blocked on it: whoever stood on
/// them is back on their own tile. Nothing happens when no warp is
/// reached.
fn reenter(
    ctx: &mut ToolContext<'_>,
    map: &str,
    learnt: &pokebot_world::path::Obstacles,
) -> Result<(), ToolError> {
    let Some(pose) = ctx.pose().filter(|p| p.map == map) else {
        return Ok(());
    };
    let world = Arc::clone(&ctx.world);
    let Some(m) = world.map(map) else {
        return Ok(());
    };
    let gates = ctx.gate_tiles();
    let mut warps: Vec<(i32, usize)> = m
        .warps
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            let tiles: Vec<(i32, i32)> = crate::nav::goal_tiles(
                &world,
                &Destination::Warp {
                    map: map.to_owned(),
                    warp: *i,
                },
            )
            .into_iter()
            .collect();
            walks_to(m, (pose.x, pose.y), &tiles, &ctx.gone, &gates, learnt)
        })
        .map(|(i, w)| ((w.x - pose.x).abs() + (w.y - pose.y).abs(), i))
        .collect();
    warps.sort();
    let Some(&(_, warp)) = warps.first() else {
        return Ok(());
    };
    ctx.info(format!(
        "{map}: someone stands in the way; out by warp {warp} and back"
    ));
    go(
        ctx,
        Destination::Warp {
            map: map.to_owned(),
            warp,
        },
    )?;
    go_to_map(ctx, map)?;
    ctx.blocked
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forget(map);
    Ok(())
}

fn walks_to(
    m: &pokebot_world::MapData,
    from: (i32, i32),
    spots: &[(i32, i32)],
    gone: &Gone,
    gates: &GateTiles,
    learnt: &pokebot_world::path::Obstacles,
) -> bool {
    let mut obstacles = crate::nav::object_obstacles(m, gone);
    obstacles.extend(gates.closed_on(&m.name));
    obstacles.extend(learnt.iter().copied());
    obstacles.remove(&from);
    let opened = gates.opened_on(&m.name);
    let walk = Walk {
        obstacles: &obstacles,
        surf: false,
        opened: Some(&opened),
    };
    find_path_with(m, from, &walk, |_| 0, |t| spots.contains(&t), |_| 0).is_some()
}

/// Carries out the legs of a route that needs field moves: plain legs are
/// walked by the navigator up to the next special one, which uses its
/// move through the `FieldMove` tool.
pub fn walk_legs(
    ctx: &mut ToolContext<'_>,
    legs: &[Leg],
    dest: &Dest,
) -> Result<Option<PlayerPose>, ToolError> {
    let world = Arc::clone(&ctx.world);
    for leg in legs {
        if !special(&world, leg) {
            // A warp of the route: taken where the route takes it.
            if leg.kind == EdgeKind::Warp {
                if let Some(warp) = world.map(&leg.from.map).and_then(|m| {
                    m.warps
                        .iter()
                        .position(|w| (w.x, w.y) == (leg.from.x, leg.from.y))
                }) {
                    go(
                        ctx,
                        Destination::Warp {
                            map: leg.from.map.clone(),
                            warp,
                        },
                    )?;
                }
            }
            continue;
        }
        match &leg.kind {
            EdgeKind::Gate { kind } => {
                walk_to(ctx, &leg.from)?;
                let (gx, gy) = ((leg.from.x + leg.to.x) / 2, (leg.from.y + leg.to.y) / 2);
                let gate = world
                    .places()
                    .and_then(|p| {
                        p.gates
                            .iter()
                            .find(|g| g.map == leg.from.map && (g.x, g.y) == (gx, gy))
                    })
                    .ok_or_else(|| {
                        ToolError::Failed(format!("no {kind} at {} ({gx}, {gy})", leg.from.map))
                    })?;
                let mv = FieldMove::from_move(&gate.requires.r#move).ok_or_else(|| {
                    ToolError::Failed(format!("{} is not a field move", gate.requires.r#move))
                })?;
                use_move(
                    ctx,
                    mv,
                    Some(Dest::Facing {
                        map: gate.map.clone(),
                        x: gx,
                        y: gy,
                    }),
                )?;
                // The tool marked it gone until the map loads again.
                walk_to(ctx, &leg.to)?;
            }
            EdgeKind::Walk { surf: true, .. } => {
                // A sandbar on the way lands the surfer, who surfs again
                // from it (Switch, Route 21: landed on the islet at (9, 31),
                // the walk pressed into the water, learnt it "blocked" tile
                // by tile and failed "looping").
                let mut landings = 0;
                loop {
                    if !on_water(ctx) {
                        let start = ctx
                            .pose()
                            .filter(|p| p.map == leg.from.map && landings > 0)
                            .map_or((leg.from.x, leg.from.y), |p| (p.x, p.y));
                        let (shore, water) =
                            surf_entry_from(&world, leg, start).ok_or_else(|| {
                                ToolError::Failed(format!("no water on the surf leg {leg}"))
                            })?;
                        walk_to(ctx, &Place::tile(&leg.from.map, shore.0, shore.1))?;
                        use_move(
                            ctx,
                            FieldMove::Surf,
                            Some(Dest::Facing {
                                map: leg.from.map.clone(),
                                x: water.0,
                                y: water.1,
                            }),
                        )?;
                    }
                    let to = Destination::Tile {
                        map: leg.to.map.clone(),
                        x: leg.to.x,
                        y: leg.to.y,
                    };
                    let mut step = GoStep::with_surf(&NavParts::of(ctx), to, true);
                    match ctx.drive(&mut step) {
                        Ok(_) => break,
                        Err(ToolError::Failed(_)) if landings < MAX_LANDINGS && !on_water(ctx) => {
                            landings += 1;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            EdgeKind::ScriptWarp { script } => {
                // Switch, Rocket Hideout B4F: the stairs from B3F land west
                // of a wall, Giovanni's side is the lift's; every Beat of
                // the grunts there failed "no path next to (19, 14)". The
                // panel is read, the floor chosen, and the door walked out.
                walk_to(ctx, &leg.from)?;
                let (path, answers) = {
                    let belief = StateBelief(ctx.state());
                    ride_path(&world, script, &leg.to.map, &belief)
                }
                .ok_or_else(|| {
                    ToolError::Failed(format!(
                        "{script}: no path takes the lift to {}",
                        leg.to.map
                    ))
                })?;
                // Within a RunScript (its walk to the script's map), the
                // tool is busy: the panel is run in place.
                if ctx.running().contains(&"RunScript") {
                    super::dialogue::run_nested(ctx, script, Some(path), &answers)?;
                } else {
                    ctx.invoke(&Intent::RunScript {
                        script: script.clone(),
                        path: Some(path),
                        answers,
                    })
                    .result?;
                }
                if ctx.pose().is_some_and(|p| p.map == leg.from.map) {
                    if let Some(warp) = dynamic_door(&world, &leg.from.map) {
                        go(
                            ctx,
                            Destination::Warp {
                                map: leg.from.map.clone(),
                                warp,
                            },
                        )?;
                    }
                }
                if let Some(p) = ctx.pose().filter(|p| p.map != leg.to.map) {
                    return Err(ToolError::Failed(format!(
                        "the lift left the player on {}, not {}",
                        p.map, leg.to.map
                    )));
                }
            }
            EdgeKind::Fly => {
                // Flown from where the route prices it: FLY works only
                // outdoors (fleet worker 5 in the Route 16 house, handed
                // HM02 there: "Can't use that here." 783 times).
                if let Some(start) = fly_start(ctx.pose().as_ref(), leg) {
                    walk_to(ctx, start)?;
                }
                use_move(
                    ctx,
                    FieldMove::Fly,
                    Some(Dest::Map {
                        map: leg.to.map.clone(),
                    }),
                )?
            }
            EdgeKind::Dig | EdgeKind::EscapeRope => {
                // From anywhere on the cave's maps: the route prices it
                // from where the player stands, often right there.
                if ctx.pose().is_none_or(|p| p.map != leg.from.map) {
                    walk_to(ctx, &leg.from)?;
                }
                let to = PlayerPose {
                    map: leg.to.map.clone(),
                    x: leg.to.x,
                    y: leg.to.y,
                };
                super::escape::escape(ctx, &leg.kind, &to)?;
            }
            _ => {
                // Into a dark map: light it up on arrival when someone
                // knows Flash.
                go_to_map(ctx, &leg.to.map)?;
                if field::carrier(ctx.state(), FieldMove::Flash.move_id()).is_some() {
                    use_move(ctx, FieldMove::Flash, None)?;
                }
            }
        }
    }
    match dest {
        Dest::Map { map } => go_to_map(ctx, map),
        other => go(ctx, Destination::from(other)),
    }
}

/// Sandbars a surf leg lands on before it gives up.
const MAX_LANDINGS: u32 = 4;

/// Where a FLY leg is walked to first: its start, unless the player is
/// on that map already (anywhere outdoors does).
fn fly_start<'l>(pose: Option<&PlayerPose>, leg: &'l Leg) -> Option<&'l Place> {
    pose.is_none_or(|p| p.map != leg.from.map)
        .then_some(&leg.from)
}

impl From<&Destination> for Dest {
    fn from(d: &Destination) -> Dest {
        match d.clone() {
            Destination::Tile { map, x, y } => Dest::Tile { map, x, y },
            Destination::Facing { map, x, y } => Dest::Facing { map, x, y },
            Destination::Warp { map, warp } => Dest::Warp { map, warp },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> Option<Arc<World>> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        World::load(&dir).ok().map(Arc::new)
    }

    /// Fleet worker 5 was handed HM02 in the Route 16 house and used FLY
    /// there ("Can't use that here.", 783 times): a FLY leg is flown from
    /// its start, outdoors; from anywhere on that map as it is.
    #[test]
    fn a_fly_leg_is_flown_from_outdoors() {
        let leg = Leg {
            from: Place::tile("Route16", 10, 5),
            to: Place::tile("CeladonCity", 48, 12),
            kind: EdgeKind::Fly,
            cost_s: 12.0,
            requires: pokebot_world::route::fly_requirement("CeladonCity"),
        };
        let at = |map: &str, x, y| PlayerPose {
            map: map.into(),
            x,
            y,
        };
        assert_eq!(
            fly_start(Some(&at("Route16_House", 4, 3)), &leg),
            Some(&leg.from)
        );
        assert_eq!(fly_start(Some(&at("Route16", 20, 8)), &leg), None);
    }

    #[test]
    fn four_arrivals_on_one_tile_without_progress_fail_the_leg() {
        let Some(world) = world() else { return };
        let parts = NavParts {
            world,
            gone: Gone::new(),
            maybe_gone: Default::default(),
            syncer: None,
            blocked: Blocked::default(),
            gates: Arc::default(),
            data: None,
        };
        let mut step = GoStep::with(
            &parts,
            Destination::Tile {
                map: "PalletTown".into(),
                x: 10,
                y: 10,
            },
        );
        let at = |x, y| PlayerPose {
            map: "PalletTown".into(),
            x,
            y,
        };
        // Getting closer resets the count.
        assert!(!step.note_visit(&at(5, 5)));
        assert!(!step.note_visit(&at(6, 5)));
        // Flash-6: several acts from one tile (a turn, a tap that learns
        // the blocker, the step around it) are one visit, not four.
        for _ in 0..8 {
            assert!(!step.note_visit(&at(6, 5)));
        }
        // Bouncing back to it (from a tile no closer to the goal): the
        // fourth arrival without progress loops.
        for _ in 0..MAX_TILE_VISITS - 2 {
            assert!(!step.note_visit(&at(5, 5)));
            assert!(!step.note_visit(&at(6, 5)));
        }
        assert!(!step.note_visit(&at(5, 5)));
        assert!(
            step.note_visit(&at(6, 5)),
            "the fourth arrival without progress loops"
        );
        // Progress from a new tile clears it again.
        assert!(!step.note_visit(&at(7, 5)));
        assert!(!step.note_visit(&at(7, 5)));
        // On another map every tile counts, and a map change resets.
        let other = |x| PlayerPose {
            map: "Route1".into(),
            x,
            y: 1,
        };
        for _ in 0..MAX_TILE_VISITS - 1 {
            assert!(!step.note_visit(&other(1)));
            assert!(!step.note_visit(&other(2)));
        }
        assert!(step.note_visit(&other(1)));
    }

    /// Flash-7: a tap onto MtMoon_B2F (43, 22) timed out because a wild
    /// encounter froze the player; the fade came after the timeout and
    /// the tile was learnt as blocked. A block learnt right before
    /// something came up is taken back.
    #[test]
    fn a_block_learnt_right_before_an_interruption_is_taken_back() {
        use crate::Expectation;
        use pokebot_state::{
            Direction, GameState, Observation, Observed, PoseObservation, ScreenState,
        };
        let Some(world) = world() else { return };
        let parts = NavParts {
            world,
            gone: Gone::new(),
            maybe_gone: Default::default(),
            syncer: None,
            blocked: Blocked::default(),
            gates: Arc::default(),
            data: None,
        };
        let mut step = GoStep::with(
            &parts,
            Destination::Facing {
                map: "MtMoon_B2F".into(),
                x: 13,
                y: 11,
            },
        );
        let pose = PlayerPose {
            map: "MtMoon_B2F".into(),
            x: 42,
            y: 22,
        };
        let observation = |frame: u64| {
            let mut o = Observation::bare(
                frame,
                Observed {
                    value: ScreenState::Unknown,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.player = Some(PoseObservation {
                pose: pose.clone(),
                score: 1000,
            });
            o
        };
        let state = GameState::default();
        let mut events = Vec::new();
        macro_rules! ctx {
            ($o:expr, $events:expr, $quiet:expr) => {
                StepContext {
                    observation: $o,
                    state: &state,
                    events: $events,
                    quiet_frames: $quiet,
                    frame: None,
                    learned: &[],
                }
            };
        }
        // The tap Right, facing Right, timed out on a quiet frame.
        step.nav
            .walker
            .note_tap(pose.clone(), Direction::Right, (43, 22), true);
        let tap = Action::new(
            "walk next to (13, 11): Right",
            vec![],
            Expectation::PlayerMovedFrom(pose.clone()),
            30,
        )
        .timed(InputKind::WalkTile, 1);
        let o = observation(100);
        step.on_outcome(&tap, Outcome::TimedOut, &mut ctx!(&o, &mut events, 500));
        assert!(matches!(
            events.last(),
            Some(GameEvent::TileBlocked { x: 43, y: 22, .. })
        ));
        assert!(parts
            .blocked
            .lock()
            .unwrap()
            .on_map("MtMoon_B2F")
            .contains(&(43, 22)));
        // 300 frames later, quiet for only the last 100 (a battle ran):
        // the block is taken back.
        let o = observation(400);
        let _ = step.next(&mut ctx!(&o, &mut events, 100));
        assert!(matches!(
            events.last(),
            Some(GameEvent::TileUnblocked { x: 43, y: 22, .. })
        ));
        assert!(!parts
            .blocked
            .lock()
            .unwrap()
            .on_map("MtMoon_B2F")
            .contains(&(43, 22)));
        // A block followed by quiet frames only stays.
        step.nav
            .walker
            .note_tap(pose.clone(), Direction::Right, (43, 22), true);
        let o = observation(500);
        step.on_outcome(&tap, Outcome::TimedOut, &mut ctx!(&o, &mut events, 160));
        let o = observation(600);
        let _ = step.next(&mut ctx!(&o, &mut events, 260));
        assert!(parts
            .blocked
            .lock()
            .unwrap()
            .on_map("MtMoon_B2F")
            .contains(&(43, 22)));
    }

    fn cutter(badges: bool) -> pokebot_state::GameState {
        use pokebot_state::{Knowledge, MoveSlot, PartyMon};
        let mut state = pokebot_state::GameState::default();
        let mut mon = PartyMon::default();
        mon.moves[0] = Some(MoveSlot {
            mv: Knowledge::observed("MOVE_CUT".into(), 1),
            pp: Knowledge::observed((30, 30), 1),
        });
        state.party = Knowledge::observed(vec![mon], 1);
        // Until Bill's ticket, a Slowbro and a lass stand in front of the
        // tree (Cerulean's entry script puts them there).
        state
            .world
            .flags
            .insert("FLAG_GOT_SS_TICKET".into(), Knowledge::observed(true, 1));
        if badges {
            state
                .world
                .flags
                .insert("FLAG_BADGE02_GET".into(), Knowledge::observed(true, 1));
        }
        state
    }

    /// A party whose lead knows `mv`, having come into Mt. Moon from
    /// Route 4's west mouth.
    fn in_mt_moon_knowing(mv: &str) -> pokebot_state::GameState {
        use pokebot_state::{EscapeWarp, Knowledge, MoveSlot, PartyMon};
        let mut state = pokebot_state::GameState::default();
        let mut mon = PartyMon::default();
        mon.moves[0] = Some(MoveSlot {
            mv: Knowledge::observed(mv.into(), 1),
            pp: Knowledge::observed((10, 10), 1),
        });
        state.party = Knowledge::observed(vec![mon], 1);
        state.world.escape = Knowledge::observed(
            EscapeWarp {
                map: "Route4".into(),
                x: 19,
                y: 6,
                entered: "MtMoon_1F".into(),
            },
            1,
        );
        state
    }

    /// Deep in Mt. Moon with DIG known, the way back out to Route 4 digs
    /// out where the player stands, to the tile below the mouth it was
    /// entered by; without it, the navigator walks.
    #[test]
    fn the_way_out_of_a_cave_digs_when_dig_is_known() {
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let pose = PlayerPose {
            map: "MtMoon_B2F".into(),
            x: 17,
            y: 30,
        };
        let dest = Dest::Map {
            map: "Route4".into(),
        };
        let legs = plan_field_route(
            &world,
            &graph,
            &in_mt_moon_knowing("MOVE_DIG"),
            &pose,
            &dest,
        )
        .expect("a route that digs");
        assert_eq!(legs[0].kind, EdgeKind::Dig, "{legs:?}");
        assert_eq!(legs[0].from.map, "MtMoon_B2F");
        assert_eq!(
            (legs[0].to.map.as_str(), legs[0].to.x, legs[0].to.y),
            ("Route4", 19, 6)
        );
        assert!(special(&world, &legs[0]));
        let walker = in_mt_moon_knowing("MOVE_TACKLE");
        assert!(plan_field_route(&world, &graph, &walker, &pose, &dest).is_none());
    }

    /// Cerulean's cut tree (26, 32): with CUT known and the Cascade Badge,
    /// the walk to the tile south of it goes through the tree (a gate leg
    /// the `FieldMove` tool carries out); without the badge the plain
    /// navigator walks (or fails) as before.
    #[test]
    fn a_route_through_a_cut_tree_is_a_field_route() {
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let pose = PlayerPose {
            map: "CeruleanCity".into(),
            x: 26,
            y: 29,
        };
        let dest = Dest::Tile {
            map: "CeruleanCity".into(),
            x: 26,
            y: 33,
        };
        let legs = plan_field_route(&world, &graph, &cutter(true), &pose, &dest)
            .expect("a route through the tree");
        let gate = legs
            .iter()
            .find(|l| matches!(&l.kind, EdgeKind::Gate { kind } if kind == "cut_tree"))
            .expect("a gate leg");
        assert_eq!((gate.from.x, gate.from.y), (26, 31));
        assert_eq!((gate.to.x, gate.to.y), (26, 33));
        assert!(plan_field_route(&world, &graph, &cutter(false), &pose, &dest).is_none());
    }

    /// Switch, Celadon Gym: a Cut tree stands between the door and ERIKA
    /// (6, 4); every RunScript failed "no path next to (6, 4)". With CUT and
    /// the Cascade Badge the way from the door to a tile next to her cuts a
    /// tree; without the badge there is none.
    #[test]
    fn the_way_to_erika_cuts_a_tree_inside_the_gym() {
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let pose = PlayerPose {
            map: "CeladonCity_Gym".into(),
            x: 6,
            y: 17,
        };
        let map = world.map("CeladonCity_Gym").unwrap();
        let spots: Vec<(i32, i32)> = crate::nav::facing_spots(map, 6, 4)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let route = |state: &pokebot_state::GameState| {
            spots.iter().find_map(|&(x, y)| {
                plan_field_route(
                    &world,
                    &graph,
                    state,
                    &pose,
                    &Dest::Tile {
                        map: "CeladonCity_Gym".into(),
                        x,
                        y,
                    },
                )
            })
        };
        let legs = route(&cutter(true)).expect("a way next to ERIKA");
        assert!(legs
            .iter()
            .any(|l| matches!(&l.kind, EdgeKind::Gate { kind } if kind == "cut_tree")));
        assert!(route(&cutter(false)).is_none());
    }

    /// Switch goal run: out of Vermilion Gym, on the gym's side of the
    /// Cut tree, a Heal walked to the Center with the navigator ("no path
    /// to door (15, 6)"). The way to the Center's map is a field route,
    /// which `reach_map` walks before the tool's own approach.
    #[test]
    fn the_way_to_another_map_through_a_cut_tree_is_a_field_route() {
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let mut state = cutter(false);
        state.world.flags.insert(
            "FLAG_BADGE02_GET".into(),
            pokebot_state::Knowledge::observed(true, 1),
        );
        let pose = PlayerPose {
            map: "VermilionCity".into(),
            x: 14,
            y: 25,
        };
        let dest = Dest::Map {
            map: "VermilionCity_PokemonCenter_1F".into(),
        };
        let legs = plan_field_route(&world, &graph, &state, &pose, &dest)
            .expect("the Center is past the tree");
        assert!(legs
            .iter()
            .any(|l| matches!(&l.kind, EdgeKind::Gate { kind } if kind == "cut_tree")));
    }

    #[test]
    fn a_map_destination_is_a_walkable_tile_near_the_middle() {
        let Some(world) = world() else { return };
        let (x, y) = map_tile(&world, "PalletTown").expect("a tile");
        let m = world.map("PalletTown").unwrap();
        assert_eq!(m.tile(x, y).unwrap().collision, 0);
        assert!((x - m.width / 2).abs() + (y - m.height / 2).abs() < 6);
    }

    /// Rocket Hideout B4F from the B3F stairs, (11, 15): Giovanni's side,
    /// where TRAINER_TEAM_ROCKET_GRUNT_17 stands at (19, 14), is the
    /// lift's (Switch: "no path next to (19, 14)"). With the Lift Key's
    /// flag the way beside him rides the elevator (whose floor paths test
    /// VAR_ELEVATOR_FLOOR, which nothing tracks); without it there is
    /// none. The navigator doesn't walk it, so it is a field route.
    /// Switch, Rocket Hideout: Giovanni beaten, the way out of his side of
    /// B4F rides the elevator to B1F past GRUNT_12, never met on the
    /// stairs down. Not known beaten, he is a battle on the way, not a
    /// wall ("no known route from RocketHideout_B4F").
    #[test]
    fn a_trainer_not_known_beaten_is_no_wall_on_the_way_out() {
        use pokebot_state::Knowledge;
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let mut state = pokebot_state::GameState::default();
        state.world.flags.insert(
            "FLAG_CAN_USE_ROCKET_HIDEOUT_LIFT".into(),
            Knowledge::observed(true, 1),
        );
        let pose = PlayerPose {
            map: "RocketHideout_B4F".into(),
            x: 20,
            y: 6,
        };
        let dest = Dest::Map {
            map: "CeladonCity_GameCorner".into(),
        };
        let legs = plan_field_route(&world, &graph, &state, &pose, &dest).expect("a ride out");
        assert!(legs.iter().any(|l| ride(&world, l)), "{legs:?}");
    }

    /// Switch, Silph Co.'s lift: the floor reads as the special drawing
    /// the list (`InitElevatorFloorSelectMenuPos == n`), no menu choice.
    /// The ride to 11F answers its row, 0; to 10F, 1.
    #[test]
    fn silph_cos_floor_is_the_lists_row() {
        let Some(world) = world() else { return };
        let state = pokebot_state::GameState::default();
        let script = "SilphCo_Elevator_EventScript_FloorSelect";
        for (floor, row) in [("SilphCo_11F", 0), ("SilphCo_10F", 1)] {
            let (_, answers) =
                ride_path(&world, script, floor, &StateBelief(&state)).expect("a path");
            assert_eq!(
                answers.first(),
                Some(&super::super::Answer::ListRow(row)),
                "{floor}"
            );
        }
    }

    /// Switch, Silph Co. 11F (13, 3), its door shut: the way next to the
    /// door isn't walked on the floor (it read so, gates left out, and
    /// the walk found none).
    #[test]
    fn a_shut_door_keeps_the_way_next_to_it_off_the_floor() {
        use pokebot_state::Knowledge;
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let m = world.map("SilphCo_11F").unwrap();
        let spots: Vec<(i32, i32)> = crate::nav::facing_spots(m, 6, 16)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let mut state = pokebot_state::GameState::default();
        state
            .world
            .flags
            .insert("FLAG_SILPH_11F_DOOR".into(), Knowledge::observed(false, 1));
        let shut = GateTiles::believed(&world, graph.gates(), &StateBelief(&state));
        assert!(!walks_to(
            m,
            (13, 3),
            &spots,
            &Gone::new(),
            &shut,
            &Default::default()
        ));
    }

    /// Switch, Viridian Gym: Black Belt Takashi walked down to challenge
    /// the player and stayed at (10, 4), in the one-tile corridor to
    /// Giovanni. By the map Giovanni is a walk away; with the tile found
    /// blocked he isn't: someone stands there, and going out and back is
    /// the way (`reenter`).
    #[test]
    fn a_trainer_left_in_a_corridor_is_waited_out_by_reentering() {
        let Some(world) = world() else { return };
        let m = world.map("ViridianCity_Gym").unwrap();
        let spots: Vec<(i32, i32)> = crate::nav::facing_spots(m, 2, 2)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let shut = GateTiles::default();
        let from = (10, 6);
        assert!(walks_to(
            m,
            from,
            &spots,
            &Gone::new(),
            &shut,
            &Default::default()
        ));
        let learnt: pokebot_world::path::Obstacles = [(10, 4)].into_iter().collect();
        assert!(!walks_to(m, from, &spots, &Gone::new(), &shut, &learnt));
    }

    /// Switch, Saffron Gym's door (14, 22): SABRINA's room is reached only
    /// by the gym's pads, warps onto the gym itself; every RunScript failed
    /// "no path next to (14, 11)". The route's pads are followed.
    /// Switch, Route 21: the surf leg south crosses the sandbar at
    /// (9, 31); landed there, the surfer surfs again from it, into the
    /// water next to it.
    #[test]
    fn a_sandbar_on_a_surf_leg_is_surfed_from_again() {
        let Some(world) = world() else { return };
        let leg = Leg {
            from: Place::tile("Route21_North", 9, 20),
            to: Place::tile("Route21_North", 9, 49),
            kind: EdgeKind::Walk {
                tiles: 29,
                surf: true,
            },
            cost_s: 0.0,
            requires: Vec::new(),
        };
        let map = world.map("Route21_North").unwrap();
        let water = |t: (i32, i32)| map.tile(t.0, t.1).is_some_and(|t| is_water(t.behavior));
        assert!(!water((9, 31)), "the sandbar");
        let (shore, into) = surf_entry_from(&world, &leg, (9, 31)).expect("water on the way");
        assert!(!water(shore) && water(into), "{shore:?} -> {into:?}");
        assert_eq!((shore.0 - into.0).abs() + (shore.1 - into.1).abs(), 1);
        assert!(into.1 >= 31, "on toward (9, 49): {into:?}");
    }

    #[test]
    fn saffron_gyms_pads_lead_next_to_sabrina() {
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let state = pokebot_state::GameState::default();
        let pose = PlayerPose {
            map: "SaffronCity_Gym".into(),
            x: 14,
            y: 22,
        };
        let dest = Dest::Tile {
            map: "SaffronCity_Gym".into(),
            x: 14,
            y: 12,
        };
        let legs = plan_field_route(&world, &graph, &state, &pose, &dest).expect("followed");
        assert!(
            legs.iter()
                .any(|l| l.kind == EdgeKind::Warp && l.to.map == "SaffronCity_Gym"),
            "{legs:?}"
        );
    }

    /// Switch, Saffron Gym: the pads are warps onto the gym itself, and a
    /// warp read as done on reaching the map it leads to, i.e. at once.
    /// The legs walked from the door never moved the player, and every
    /// RunScript to SABRINA failed "no path to (14, 12)". Walked leg by
    /// leg: each pad is stepped toward, done only on its landing tile,
    /// and the last walk reaches the tile below SABRINA.
    #[test]
    fn saffron_gyms_pads_are_walked_from_the_door_to_sabrina() {
        use crate::nav::{Destination, NavStatus, Navigator};
        use pokebot_state::{Observation, Observed, PoseObservation, ScreenState};
        let Some(world) = world() else { return };
        let at = |x: i32, y: i32| {
            let mut o = Observation::bare(
                1,
                Observed {
                    value: ScreenState::Unknown,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.player = Some(PoseObservation {
                pose: PlayerPose {
                    map: "SaffronCity_Gym".into(),
                    x,
                    y,
                },
                score: 1000,
            });
            o
        };
        let graph = crate::scheduler::graph(&world);
        let state = pokebot_state::GameState::default();
        let door = PlayerPose {
            map: "SaffronCity_Gym".into(),
            x: 14,
            y: 22,
        };
        let dest = Dest::Tile {
            map: "SaffronCity_Gym".into(),
            x: 14,
            y: 12,
        };
        let legs = plan_field_route(&world, &graph, &state, &door, &dest).expect("the pads' route");
        let mut stand = (door.x, door.y);
        let mut pads = 0;
        for leg in legs.iter().filter(|l| l.kind == EdgeKind::Warp) {
            let warp = world
                .map("SaffronCity_Gym")
                .and_then(|m| {
                    m.warps
                        .iter()
                        .position(|w| (w.x, w.y) == (leg.from.x, leg.from.y))
                })
                .expect("a pad");
            let to_pad = || {
                Navigator::new(
                    Arc::clone(&world),
                    Destination::Warp {
                        map: "SaffronCity_Gym".into(),
                        warp,
                    },
                )
            };
            match to_pad().next(&at(stand.0, stand.1)) {
                NavStatus::Act(_) => {}
                NavStatus::Arrived => panic!("pad {warp} done from {stand:?} before a step"),
                NavStatus::Wait(r) | NavStatus::Fail(r) => panic!("pad {warp}: {r}"),
            }
            assert!(
                matches!(to_pad().next(&at(leg.to.x, leg.to.y)), NavStatus::Arrived),
                "pad {warp} lands on ({}, {})",
                leg.to.x,
                leg.to.y
            );
            stand = (leg.to.x, leg.to.y);
            pads += 1;
        }
        assert!(pads >= 2, "{legs:?}");
        let mut last = Navigator::new(
            Arc::clone(&world),
            Destination::Tile {
                map: "SaffronCity_Gym".into(),
                x: 14,
                y: 12,
            },
        );
        match last.next(&at(stand.0, stand.1)) {
            NavStatus::Act(_) => {}
            NavStatus::Arrived => panic!("already below SABRINA at {stand:?}"),
            NavStatus::Wait(r) | NavStatus::Fail(r) => panic!("from {stand:?}: {r}"),
        }
    }

    /// Switch, Silph Co. 5F (21, 21) by the Card Key: the navigator's own
    /// way to 3F went by the pad at (2, 20), behind a grunt standing in the
    /// corridor, and failed every replan. Through Silph's gated floors the
    /// route is followed, and its warp is the pad at (15, 7).
    #[test]
    fn silph_cos_floors_follow_the_routes_warps() {
        use pokebot_state::Knowledge;
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let mut state = pokebot_state::GameState::default();
        for on in graph.gates().values() {
            for gate in on.values() {
                for p in gate.ways.iter().flatten() {
                    if let pokebot_world::predicate::Predicate::Flag { name, .. } = p {
                        if name.starts_with("FLAG_SILPH_") {
                            state
                                .world
                                .flags
                                .insert(name.clone(), Knowledge::observed(false, 1));
                        }
                    }
                }
            }
        }
        let pose = PlayerPose {
            map: "SilphCo_5F".into(),
            x: 21,
            y: 21,
        };
        let dest = Dest::Map {
            map: "SilphCo_3F".into(),
        };
        let legs = plan_field_route(&world, &graph, &state, &pose, &dest).expect("followed");
        let warp = legs
            .iter()
            .find(|l| l.kind == EdgeKind::Warp && l.from.map == "SilphCo_5F")
            .expect("a warp off 5F");
        assert_eq!((warp.from.x, warp.from.y), (15, 7), "{legs:?}");
    }

    /// Switch, Silph Co. 3F (29, 2), the Card Key not held, every door
    /// shut: the navigator thought it could walk to 11F past them and
    /// walked into a door ("no path to warp (13, 14)"). The route's
    /// elevator ride, a few steps away, is taken.
    #[test]
    fn silph_co_rides_the_elevator_past_its_shut_doors() {
        use pokebot_state::Knowledge;
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let mut state = pokebot_state::GameState::default();
        let doors: Vec<String> = graph
            .gates()
            .values()
            .flat_map(|on| on.values())
            .flat_map(|g| g.ways.iter().flatten())
            .filter_map(|p| match p {
                pokebot_world::predicate::Predicate::Flag { name, .. }
                    if name.starts_with("FLAG_SILPH_") =>
                {
                    Some(name.clone())
                }
                _ => None,
            })
            .collect();
        assert!(!doors.is_empty());
        for door in doors {
            state
                .world
                .flags
                .insert(door, Knowledge::observed(false, 1));
        }
        let pose = PlayerPose {
            map: "SilphCo_3F".into(),
            x: 29,
            y: 2,
        };
        let dest = Dest::Map {
            map: "SilphCo_11F".into(),
        };
        let legs = plan_field_route(&world, &graph, &state, &pose, &dest).expect("a ride");
        assert!(legs.iter().any(|l| ride(&world, l)), "{legs:?}");
    }

    #[test]
    fn the_way_to_the_lift_side_of_b4f_rides_the_elevator() {
        use pokebot_state::Knowledge;
        let Some(world) = world() else { return };
        let graph = crate::scheduler::graph(&world);
        let mut state = pokebot_state::GameState::default();
        // A B1F grunt stands on the way to the car until beaten.
        state.world.flags.insert(
            "TRAINER_TEAM_ROCKET_GRUNT_12".into(),
            Knowledge::observed(true, 1),
        );
        let pose = PlayerPose {
            map: "RocketHideout_B4F".into(),
            x: 11,
            y: 15,
        };
        let dest = Dest::Tile {
            map: "RocketHideout_B4F".into(),
            x: 19,
            y: 15,
        };
        assert!(plan_field_route(&world, &graph, &state, &pose, &dest).is_none());
        state.world.flags.insert(
            "FLAG_CAN_USE_ROCKET_HIDEOUT_LIFT".into(),
            Knowledge::observed(true, 1),
        );
        let legs = plan_field_route(&world, &graph, &state, &pose, &dest)
            .expect("a ride to Giovanni's side");
        let lift = legs
            .iter()
            .find(|l| ride(&world, l))
            .expect("an elevator leg");
        assert_eq!(lift.from.map, "RocketHideout_Elevator");
        assert_eq!(
            (lift.to.map.as_str(), lift.to.x, lift.to.y),
            ("RocketHideout_B4F", 20, 23)
        );
        assert!(special(&world, lift));
        assert_eq!(dynamic_door(&world, "RocketHideout_Elevator"), Some(0));

        // (b) The panel's path to B4F: its set_warp lands there, and the
        // answer is B4F's row of the menu.
        let EdgeKind::ScriptWarp { script } = &lift.kind else {
            unreachable!()
        };
        assert_eq!(script, "RocketHideout_Elevator_EventScript_FloorSelect");
        let (path, answers) = ride_path(&world, script, "RocketHideout_B4F", &StateBelief(&state))
            .expect("a path to B4F");
        let p = &world.events().unwrap().script(script).unwrap().paths[path];
        assert!(p.does.iter().any(|e| matches!(e,
            pokebot_world::events::Effect::SetWarp { set_warp, .. } if set_warp == "MAP_ROCKET_HIDEOUT_B4F")));
        // A multichoice that wraps (cont-1: counted up past its top, the
        // car rode to B1F): B4F's row from the cursor seen.
        assert_eq!(answers, vec![super::super::Answer::Menu(2)]);
        // The car's floor unknown, the path that rides (and records it).
        assert!(p.does.iter().any(|e| matches!(e,
            pokebot_world::events::Effect::Set { set } if set == "FLAG_TEMP_2")));
        // Without the key no path goes.
        let mut locked = state.clone();
        locked.world.flags.insert(
            "FLAG_CAN_USE_ROCKET_HIDEOUT_LIFT".into(),
            Knowledge::observed(false, 1),
        );
        assert!(ride_path(&world, script, "RocketHideout_B4F", &StateBelief(&locked)).is_none());
    }

    /// An elevator's door leads to `MAP_DYNAMIC` (its panel sets where):
    /// walking out of it is done on whatever map the player stands on
    /// next, not never.
    #[test]
    fn walking_out_of_an_elevator_is_done_off_the_car() {
        use pokebot_state::{GameState, Observation, Observed, PoseObservation, ScreenState};
        let Some(world) = world() else { return };
        let parts = NavParts {
            world,
            gone: Gone::new(),
            maybe_gone: Default::default(),
            syncer: None,
            blocked: Blocked::default(),
            gates: Arc::default(),
            data: None,
        };
        let mut step = GoStep::with(
            &parts,
            Destination::Warp {
                map: "RocketHideout_Elevator".into(),
                warp: 0,
            },
        );
        let mut o = Observation::bare(
            1,
            Observed {
                value: ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.player = Some(PoseObservation {
            pose: PlayerPose {
                map: "RocketHideout_B4F".into(),
                x: 20,
                y: 23,
            },
            score: 1000,
        });
        let state = GameState::default();
        let mut events = Vec::new();
        let mut ctx = StepContext {
            observation: &o,
            state: &state,
            events: &mut events,
            quiet_frames: SETTLE_FRAMES,
            frame: None,
            learned: &[],
        };
        assert!(matches!(step.next(&mut ctx), Decision::Done(_)));
    }
}
