//! Live observability: the bot loop publishes frames, state and log entries
//! to a [`Telemetry`] hub; [`serve`] exposes them over HTTP for the web UI.
//!
//! Observations remain a side output. [`GameControl`] separately arbitrates
//! human and bot input, with explicit stop/resume and bounded manual leases.

mod control;
mod hub;
pub mod hub_proxy;
mod server;

pub use control::GameControl;
pub use hub::{LogEntry, LogKind, Stats, Telemetry};
pub use server::{serve, WebServer};
