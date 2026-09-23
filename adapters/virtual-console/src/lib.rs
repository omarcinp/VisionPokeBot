//! Makes the emulator look like real hardware from the outside:
//!
//! * [`V4l2Sink`] writes its video into a V4L2 output device (a v4l2loopback
//!   virtual camera), which the bot reads like an HDMI capture card;
//! * [`VirtualSerialPort`] + [`spawn_virtual_esp32`] expose a pseudo-terminal
//!   that speaks PABotBase2 like the ESP32 controller bridge.
//!
//! The bot process then uses its production adapters (`capture-card`,
//! `pabotbase`) unchanged.

mod pty;
mod serial_device;
mod v4l2_sink;

pub use pty::VirtualSerialPort;
pub use serial_device::{spawn_virtual_esp32, EmulatorButtons, VirtualEsp32};
pub use v4l2_sink::V4l2Sink;
