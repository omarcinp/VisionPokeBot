//! `Go`: walk to a destination with the navigator (route across maps,
//! holds and taps within one), settling after any cutscene first.

use std::sync::Arc;

use pokebot_state::PlayerPose;
use pokebot_world::World;

use super::{
    Dest, Expects, Intent, StepContext, Tool, ToolContext, ToolOutcome, ToolStep, SETTLE_FRAMES,
};
use crate::motion::SyncerHandle;
use crate::nav::{Destination, Gone, NavStatus, Navigator};
use crate::{Action, Decision, Outcome};

/// Taps in a row that failed to move the player before the leg fails.
const MAX_STALLED: u32 = 6;

pub struct GoTool;

/// One leg to `dest`, as a step.
pub struct GoStep {
    nav: Navigator,
    dest: Destination,
}

/// What a navigator is built from: the world, the objects known to be
/// gone, and the timing model. Steps keep one to rebuild their legs.
#[derive(Clone)]
pub struct NavParts {
    pub world: Arc<World>,
    pub gone: Gone,
    pub syncer: Option<SyncerHandle>,
}

impl NavParts {
    pub fn of(ctx: &ToolContext<'_>) -> Self {
        Self {
            world: Arc::clone(&ctx.world),
            gone: ctx.gone.clone(),
            syncer: ctx.syncer.clone(),
        }
    }
}

impl GoStep {
    pub fn new(ctx: &ToolContext<'_>, dest: Destination) -> Self {
        Self::with(&NavParts::of(ctx), dest)
    }

    pub fn with(parts: &NavParts, dest: Destination) -> Self {
        let nav =
            Navigator::new(Arc::clone(&parts.world), dest.clone()).with_gone(parts.gone.clone());
        let nav = match &parts.syncer {
            Some(syncer) => nav.with_syncer(Arc::clone(syncer)),
            None => nav,
        };
        Self { nav, dest }
    }

    /// Where the walk ends.
    pub fn destination(&self) -> &Destination {
        &self.dest
    }
}

impl ToolStep for GoStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        if ctx.quiet_frames < SETTLE_FRAMES {
            return Decision::Wait("letting the scene settle".into());
        }
        if self.nav.stalled() >= MAX_STALLED {
            return Decision::Fail(format!("cannot move toward {:?}", self.dest));
        }
        match self.nav.next(ctx.observation) {
            NavStatus::Arrived => Decision::Done(format!("arrived at {:?}", self.dest)),
            NavStatus::Act(action) => Decision::Act(action),
            NavStatus::Wait(reason) => Decision::Wait(reason),
            NavStatus::Fail(reason) => Decision::Fail(reason),
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, _ctx: &mut StepContext<'_>) {
        self.nav.on_outcome(action, outcome);
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
        let dest: Destination = Destination::from(dest);
        match go(ctx, dest) {
            Ok(pose) => ToolOutcome {
                pose,
                ..ToolOutcome::ok()
            },
            Err(e) => e.into(),
        }
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
