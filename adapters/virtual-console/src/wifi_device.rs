use std::time::Duration;

use pokebot_core::{ButtonSet, Controller, ControllerCommand};
use pokebot_emulator_libretro::EmulatorController;
use pokebot_remote::{HidSink, SwitchReport};

/// Presses whatever the WiFi controller firmware's device code would send to
/// the Switch over USB, as the GBA app would read it. The firmware times
/// every input itself, so the emulator just mirrors the current report until
/// it changes.
pub struct EmulatorHid {
    controller: EmulatorController,
    current: ButtonSet,
}

impl EmulatorHid {
    pub fn new(controller: EmulatorController) -> Self {
        Self {
            controller,
            current: ButtonSet::NONE,
        }
    }
}

impl HidSink for EmulatorHid {
    fn send(&mut self, report: &SwitchReport) -> bool {
        // The GBA app ignores the Switch-only inputs (Home, X, sticks, ...).
        let buttons = report.to_state().gba_buttons();
        if buttons == self.current {
            return true;
        }
        self.current = buttons;
        // Fails only once the emulator has shut down.
        let _ = self.controller.execute(ControllerCommand::Neutral);
        if !buttons.is_empty() {
            let _ = self.controller.execute(ControllerCommand::Hold {
                buttons,
                duration: Duration::from_secs(3600),
            });
        }
        true
    }

    fn mounted(&self) -> bool {
        true
    }
}
