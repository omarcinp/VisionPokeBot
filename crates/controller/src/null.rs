use std::time::{Duration, Instant};

use pokebot_core::{Controller, ControllerCommand, ControllerReceipt, PressProfile, Result};

/// Accepts every command and does nothing. Used when replaying recorded
/// sessions, where the video is fixed and inputs cannot change it.
#[derive(Debug, Default)]
pub struct NullController {
    profile: PressProfile,
    next_id: u64,
}

impl NullController {
    pub fn new(profile: PressProfile) -> Self {
        Self {
            profile,
            next_id: 0,
        }
    }
}

impl Controller for NullController {
    fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt> {
        let command_id = self.next_id;
        self.next_id += 1;
        let input_duration = command
            .timeline(&self.profile)
            .iter()
            .map(|input| input.duration)
            .sum::<Duration>();
        Ok(ControllerReceipt {
            command_id,
            issued_at: Instant::now(),
            input_duration,
        })
    }

    fn is_idle(&self) -> Result<bool> {
        Ok(true)
    }
}
