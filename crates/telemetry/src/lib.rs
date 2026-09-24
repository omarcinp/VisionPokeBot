//! Live observability: the bot loop publishes frames, state and log entries
//! to a [`Telemetry`] hub; [`serve`] exposes them over HTTP for the web UI.
//!
//! Telemetry is write-only from the bot's point of view. Nothing here feeds
//! back into perception or planning.

mod hub;
pub mod hub_proxy;
mod server;

pub use hub::{LogEntry, LogKind, Stats, Telemetry};
pub use server::{serve, WebServer};
