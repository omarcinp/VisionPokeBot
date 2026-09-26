//! Development stand-in for a console + capture card + controller bridge.
//!
//! Hosts a libretro core (mGBA, or gpSP for linking) on a dedicated thread
//! and exposes exactly two things to the rest of the bot:
//!
//! * [`EmulatorVideoSource`] — the rendered frames, like an HDMI capture card;
//! * [`EmulatorController`] — the joypad, like the ESP32 controller bridge.
//!
//! Optionally the console's link port is plugged into another emulator
//! ([`LinkRole`]), like two consoles trading over the Wireless Adapter. Its
//! traffic is opaque packets between the two cores; the bot never sees it.
//!
//! The core's memory, save-state, cheat and debug entry points are never bound
//! or called. The single exception is opaque battery-save persistence in
//! [`cartridge`], which is what a physical cartridge does on its own; its bytes
//! only ever travel between the core and a `.sav` file.

mod cartridge;
mod device;
mod ffi;
mod host;
mod link_port;

pub use device::{
    launch, ClockMode, CoreInfo, EmulatorConfig, EmulatorController, EmulatorVideoSource,
};
pub use link_port::LinkRole;
