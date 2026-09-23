//! Domain types and device-facing traits shared by every PokéBot crate.
//!
//! The bot's only sensor is a [`VideoSource`] and its only actuator is a
//! [`Controller`]. Nothing in this crate knows whether those are backed by an
//! emulator, a capture card, a replay, or an ESP32 bridge.

pub mod controller;
pub mod error;
pub mod frame;
pub mod image;

pub use controller::{
    Button, ButtonSet, Controller, ControllerCommand, ControllerReceipt, PressProfile, TimedInput,
};
pub use error::{Error, Result};
pub use frame::{CapturedFrame, NormalizedFrame, VideoSource, CANONICAL_HEIGHT, CANONICAL_WIDTH};
pub use image::RgbImage;
