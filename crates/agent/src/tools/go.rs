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
use pokebot_world::path::{find_path_with, Walk};
use pokebot_world::route::{self, EdgeKind, Leg, Place, UnknownPolicy};
use pokebot_world::World;

use super::field::{self, FieldMove};
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
    pub syncer: Option<SyncerHandle>,
    /// The session's tiles learnt to be blocked.
    pub blocked: Blocked,
}

impl NavParts {
    pub fn of(ctx: &ToolContext<'_>) -> Self {
        Self {
            world: Arc::clone(&ctx.world),
            gone: ctx.gone.clone(),
            syncer: ctx.syncer.clone(),
            blocked: Arc::clone(&ctx.blocked),
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
            .with_blocked(Arc::clone(&parts.blocked))
            .with_surf(surf);
        let nav = match &parts.syncer {
            Some(syncer) => nav.with_syncer(Arc::clone(syncer)),
            None => nav,
        };
        Self {
            nav,
            dest,
            world: Arc::clone(&parts.world),
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
        let walked = match field_route(ctx, dest) {
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
        match walked {
            Ok(pose) => ToolOutcome {
                pose,
                ..ToolOutcome::ok()
            },
            Err(e) => e.into(),
        }
    }
}

/// Whether `leg` needs a field move (or enters a dark map).
fn special(world: &World, leg: &Leg) -> bool {
    match &leg.kind {
        EdgeKind::Gate { .. } | EdgeKind::Fly | EdgeKind::Walk { surf: true, .. } => true,
        _ => field::is_dark(world, &leg.to.map) && !field::is_dark(world, &leg.from.map),
    }
}

/// The route to `dest` from where the player stands, when it needs field
/// moves (a Cut or Rock Smash gate, Surf, Fly) or enters a dark map with
/// a Flash user in the party; `None` when the navigator alone walks it.
/// Requirements the belief doesn't know count as unmet (the planner
/// establishes them first).
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
    let result = match dest {
        Dest::Map { map } => {
            route::route_to_map(world, graph, &belief, pose, map, UnknownPolicy::Pessimistic)
        }
        Dest::Tile { map, x, y } => route::route(
            world,
            graph,
            &belief,
            pose,
            &Place::tile(map, *x, *y),
            UnknownPolicy::Pessimistic,
        ),
        // Talking and warp legs end next to their target: the navigator's.
        Dest::Facing { .. } | Dest::Warp { .. } => return None,
    };
    if !result.found() {
        return None;
    }
    let flash = field::carrier(state, FieldMove::Flash.move_id()).is_some();
    let needed = result.legs.iter().any(|l| match &l.kind {
        EdgeKind::Gate { .. } | EdgeKind::Fly | EdgeKind::Walk { surf: true, .. } => true,
        // Entering a dark map matters only with someone to use Flash.
        _ => special(world, l) && flash,
    });
    needed.then_some(result.legs)
}

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

/// The shore tile and the first water tile of a surf leg's path.
fn surf_entry(world: &World, leg: &Leg) -> Option<((i32, i32), (i32, i32))> {
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
        (leg.from.x, leg.from.y),
        &walk,
        |_| 0,
        |p| p == to,
        |p| (p.0 - to.0).abs() + (p.1 - to.1).abs(),
    )?;
    let mut prev = (leg.from.x, leg.from.y);
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
                if !on_water(ctx) {
                    let (shore, water) = surf_entry(&world, leg).ok_or_else(|| {
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
                ctx.drive(&mut step)?;
            }
            EdgeKind::Fly => use_move(
                ctx,
                FieldMove::Fly,
                Some(Dest::Map {
                    map: leg.to.map.clone(),
                }),
            )?,
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

    #[test]
    fn four_arrivals_on_one_tile_without_progress_fail_the_leg() {
        let Some(world) = world() else { return };
        let parts = NavParts {
            world,
            gone: Gone::new(),
            syncer: None,
            blocked: Blocked::default(),
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
            syncer: None,
            blocked: Blocked::default(),
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

    #[test]
    fn a_map_destination_is_a_walkable_tile_near_the_middle() {
        let Some(world) = world() else { return };
        let (x, y) = map_tile(&world, "PalletTown").expect("a tile");
        let m = world.map("PalletTown").unwrap();
        assert_eq!(m.tile(x, y).unwrap().collision, 0);
        assert!((x - m.width / 2).abs() + (y - m.height / 2).abs() < 6);
    }
}
