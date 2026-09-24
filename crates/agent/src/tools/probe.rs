//! `Probe`: open a screen to make a fact `Observed`. Only the bag pocket
//! audit exists; the trainer card, Fly map and Pokédex readers are stream
//! B2's and report `Unsupported` until then.

use std::sync::Arc;

use pokebot_gamedata::GameData;
use pokebot_state::Pocket;

use super::{
    progress, Expects, Intent, ProbeFact, StepContext, Tool, ToolContext, ToolError, ToolOutcome,
    ToolStep, SETTLE_FRAMES,
};
use crate::bag::PocketAudit;
use crate::Decision;

pub struct ProbeTool;

/// The pocket audit as a step: Start opens the menu only once the scene
/// has settled (pressed during a scripted pause it lands mid-cutscene).
struct AuditStep {
    audit: PocketAudit,
    data: Arc<GameData>,
}

impl ToolStep for AuditStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if !self.audit.observed()
            && o.bag.is_none()
            && o.menu.is_none()
            && ctx.quiet_frames < SETTLE_FRAMES
        {
            return Decision::Wait("letting the scene settle before the bag".into());
        }
        self.audit.next(o, &self.data, ctx.events)
    }

    fn expects(&self) -> Expects {
        Expects::MENUS
    }
}

/// Reads `pocket` through the Start menu and closes every menu again.
pub fn audit_pocket(ctx: &mut ToolContext<'_>, pocket: Pocket) -> Result<String, ToolError> {
    let mut step = AuditStep {
        audit: PocketAudit::new(pocket),
        data: Arc::clone(&ctx.data),
    };
    let summary = ctx.drive(&mut step)?;
    ctx.emit(progress("Bag", summary.clone()))?;
    Ok(summary)
}

impl Tool for ProbeTool {
    fn name(&self) -> &str {
        "Probe"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Probe { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Probe { fact } = intent else {
            return ToolOutcome::failed("not a Probe");
        };
        match fact {
            ProbeFact::Pocket { pocket } => audit_pocket(ctx, *pocket).map(|_| ()).into(),
            ProbeFact::TrainerCard => {
                ToolError::Unsupported("trainer card reader (stream B2)".into()).into()
            }
            ProbeFact::FlyMap => ToolError::Unsupported("fly map reader (stream B2)".into()).into(),
            ProbeFact::Pokedex => {
                ToolError::Unsupported("pokédex reader (stream B2)".into()).into()
            }
        }
    }
}
