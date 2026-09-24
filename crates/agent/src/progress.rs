//! The bot's own memory of a save file: which milestones are done and where
//! it saved. Written only right after a successful in-game save, so it always
//! matches the cartridge.

use std::path::{Path, PathBuf};

use pokebot_core::{Error, Result};
use pokebot_state::{Gender, PlayerPose};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub player_name: String,
    pub rival_name: String,
    pub gender: Gender,
    pub starter: crate::Starter,
    /// Milestone names completed, in order.
    pub milestones: Vec<String>,
    /// Where the game was saved (and where CONTINUE resumes).
    pub saved_at: Option<PlayerPose>,
    /// Legacy: party knowledge from before `state.json` (read for migration).
    #[serde(default, skip_serializing)]
    pub party: crate::party::Party,
}

impl Progress {
    /// `roms/Game.sav` → `roms/Game.pokebot.json`
    pub fn path_for(save: &Path) -> PathBuf {
        save.with_extension("pokebot.json")
    }

    pub fn load(path: &Path) -> Result<Progress> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        serde_json::from_str(&text)
            .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))
    }

    pub fn store(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_vec_pretty(self).map_err(|e| Error::InvalidData(e.to_string()))?;
        std::fs::write(path, json).map_err(|e| Error::io(path, e))
    }
}
