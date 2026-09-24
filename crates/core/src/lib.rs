//! Domain types and device-facing traits shared by every PokéBot crate.
//!
//! The bot's only sensor is a [`VideoSource`] and its only actuator is a
//! [`Controller`]. Nothing in this crate knows whether those are backed by an
//! emulator, a capture card, a replay, or an ESP32 bridge. Devices that
//! emulate a whole Switch controller implement [`SwitchController`] and are
//! driven through the [`GbaOnSwitch`] adapter.

pub mod controller;
pub mod error;
pub mod frame;
pub mod image;
pub mod switch;

pub use controller::{
    Button, ButtonSet, ConsoleLink, Controller, ControllerCommand, ControllerReceipt, PressProfile,
    TimedInput,
};
pub use error::{Error, Result};
pub use frame::{CapturedFrame, NormalizedFrame, VideoSource, CANONICAL_HEIGHT, CANONICAL_WIDTH};
pub use image::RgbImage;
pub use switch::{
    GbaOnSwitch, Stick, SwitchButton, SwitchButtons, SwitchCommand, SwitchController, SwitchState,
    TimedSwitchInput,
};
