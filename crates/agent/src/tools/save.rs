//! `Save`: the in-game save through the Start menu, the cartridge save
//! persisted, and `state.json` written when the context has a checkpoint.

use super::{AsStep, Expects, Intent, Tool, ToolContext, ToolError, ToolOutcome};
use crate::{checkpoint, SaveGameTask};

pub struct SaveTool;

impl Tool for SaveTool {
    fn name(&self) -> &str {
        "Save"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Save)
    }

    fn run(&mut self, _intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let mut task = SaveGameTask::default();
        let result = (|| -> Result<(), ToolError> {
            ctx.drive(&mut AsStep {
                task: &mut task,
                expects: Expects::MENUS,
            })?;
            ctx.runtime.persist_save()?;
            if let Some((path, identity)) = &ctx.checkpoint {
                let mut identity = identity.clone();
                identity.saved_at = ctx.state().player.pose.value.clone();
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| pokebot_core::Error::io(dir, e))?;
                }
                checkpoint::store(path, &identity, &ctx.state().saved_knowledge())?;
                ctx.info(format!("checkpoint written to {}", path.display()));
            }
            Ok(())
        })();
        result.into()
    }
}
