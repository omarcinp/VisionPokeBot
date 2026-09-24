//! A Nintendo Switch wired controller driven over the network.
//!
//! The ESP32-S3 firmware (`firmware/esp32s3-controller`) is a thin shell
//! around this crate: it brings up WiFi and TinyUSB, then runs
//! [`server::Device`] with a TinyUSB [`server::HidSink`]. The host simulator
//! (`pokebot-remote-sim`) and the emulator's virtual console run the very
//! same device code with other sinks, and `pokebot-esp32-wifi` is the bot's
//! `Controller` that talks to any of them.

pub mod hid;
pub mod protocol;
pub mod queue;
pub mod server;

pub use hid::SwitchReport;
pub use protocol::{ClientMessage, DeviceInfo, DeviceMessage, Status, CONTROL_PORT};
pub use server::{Device, DeviceConfig, HidSink};
