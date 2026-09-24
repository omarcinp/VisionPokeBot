//! Development stand-in for a console + capture card + controller bridge.
//!
//! Hosts a libretro core (mGBA) on a dedicated thread and exposes exactly two
//! things to the rest of the bot:
//!
//! * [`EmulatorVideoSource`] — the rendered frames, like an HDMI capture card;
//! * [`EmulatorController`] — the joypad, like the ESP32 controller bridge.
//!
//! The core's memory, save-state, cheat and debug entry points are never bound
//! or called. The single exception is opaque battery-save persistence in
//! [`cartridge`], which is what a physical cartridge does on its own; its bytes
//! only ever travel between the core and a `.sav` file.

mod cartridge;
mod device;
mod ffi;
mod host;

pub use device::{
    launch, ClockMode, CoreInfo, EmulatorConfig, EmulatorController, EmulatorVideoSource,
};
