//! Control protocol spoken over TCP (default port [`CONTROL_PORT`]): one JSON
//! object per line in each direction. Commands are [`SwitchCommand`]s, so
//! every button and both sticks are reachable. The bot's GBA
//! `ControllerCommand`s are mapped onto them client-side by
//! `pokebot_core::GbaOnSwitch`, so one firmware serves both.
//!
//! ```text
//! device → {"type":"hello","name":"...","firmware":"0.1.0","protocol":2,"controller":"hori-pokken-wired"}
//! client → {"type":"execute","seq":1,"command":{"Press":"Home"}}
//! device → {"type":"accepted","seq":1,"id":7,"input_ms":160}
//! device → {"type":"finished","id":7,"cancelled":false}
//! client → {"type":"execute","seq":2,"command":{"Hold":{"state":{"left_stick":{"x":128,"y":0}},"duration":{"secs":1,"nanos":0}}}}
//! client → {"type":"status","seq":3}
//! device → {"type":"status","seq":3,"idle":true,"pending":0,"usb_mounted":true}
//! ```
//!
//! `id`s are assigned by the device and unique across connections, so every
//! connection receives every `finished` and ignores ids it does not own.

use pokebot_core::{PressProfile, SwitchCommand};
use serde::{Deserialize, Serialize};

/// 1: GBA `ControllerCommand`s. 2: full-layout `SwitchCommand`s.
pub const PROTOCOL_VERSION: u32 = 2;
pub const CONTROL_PORT: u16 = 7878;
pub const CONTROLLER_KIND: &str = "hori-pokken-wired";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Execute {
        seq: u64,
        command: SwitchCommand,
        /// Tap timing for `Press`/`Chord`; the device default otherwise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<PressProfile>,
    },
    Status {
        seq: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceMessage {
    Hello(DeviceInfo),
    Accepted {
        seq: u64,
        id: u64,
        input_ms: u64,
    },
    Rejected {
        seq: u64,
        reason: String,
    },
    Finished {
        id: u64,
        cancelled: bool,
    },
    Status(Status),
    /// A line the device could not parse.
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub firmware: String,
    pub protocol: u32,
    pub controller: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub idle: bool,
    /// Timed inputs queued behind the current one.
    pub pending: usize,
    /// A host (the Switch) has configured the USB device.
    pub usb_mounted: bool,
    /// The host suspended the bus: the Switch is asleep. A button press
    /// then asks it to wake up (USB remote wakeup).
    #[serde(default)]
    pub usb_suspended: bool,
    /// Idle seconds before the keepalive routine plays; `None` = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_secs: Option<u64>,
    /// Keepalive routines played since the device started.
    #[serde(default)]
    pub keepalives: u32,
}

/// Body of `POST /api/command`: either a bare command or one with a profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandRequest {
    WithProfile {
        command: SwitchCommand,
        profile: Option<PressProfile>,
    },
    Bare(SwitchCommand),
}

impl CommandRequest {
    pub fn into_parts(self) -> (SwitchCommand, Option<PressProfile>) {
        match self {
            Self::WithProfile { command, profile } => (command, profile),
            Self::Bare(command) => (command, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pokebot_core::{Stick, SwitchButton, SwitchState};

    use super::*;

    #[test]
    fn wire_format_is_stable() {
        let msg = ClientMessage::Execute {
            seq: 1,
            command: SwitchCommand::Press(SwitchButton::Home),
            profile: None,
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"execute","seq":1,"command":{"Press":"Home"}}"#
        );
        let msg = DeviceMessage::Accepted {
            seq: 1,
            id: 7,
            input_ms: 160,
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"accepted","seq":1,"id":7,"input_ms":160}"#
        );
        let status = DeviceMessage::Status(Status {
            seq: Some(2),
            idle: true,
            pending: 0,
            usb_mounted: false,
            usb_suspended: false,
            keepalive_secs: Some(240),
            keepalives: 3,
        });
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"type":"status","seq":2,"idle":true,"pending":0,"usb_mounted":false,"usb_suspended":false,"keepalive_secs":240,"keepalives":3}"#
        );
        // Firmware without the keepalive still parses.
        let old: DeviceMessage = serde_json::from_str(
            r#"{"type":"status","seq":2,"idle":true,"pending":0,"usb_mounted":false}"#,
        )
        .unwrap();
        let DeviceMessage::Status(old) = old else {
            panic!("not a status")
        };
        assert_eq!(
            (old.usb_suspended, old.keepalive_secs, old.keepalives),
            (false, None, 0)
        );
    }

    #[test]
    fn command_request_accepts_bare_and_wrapped() {
        let bare: CommandRequest = serde_json::from_str(r#"{"Chord":["L","R"]}"#).unwrap();
        assert_eq!(
            bare.into_parts(),
            (SwitchCommand::Chord("L+R".parse().unwrap()), None)
        );
        let neutral: CommandRequest = serde_json::from_str(r#""Neutral""#).unwrap();
        assert_eq!(neutral.into_parts().0, SwitchCommand::Neutral);
        let wrapped: CommandRequest = serde_json::from_str(
            r#"{"command":{"Hold":{"state":{"buttons":["ZR"],"right_stick":{"x":0,"y":128}},
                                   "duration":{"secs":1,"nanos":0}}},
                "profile":{"press":{"secs":0,"nanos":50000000},"release":{"secs":0,"nanos":50000000}}}"#,
        )
        .unwrap();
        let (command, profile) = wrapped.into_parts();
        assert_eq!(
            command,
            SwitchCommand::Hold {
                state: SwitchState {
                    buttons: SwitchButton::ZR.into(),
                    right_stick: Stick::LEFT,
                    ..SwitchState::NEUTRAL
                },
                duration: Duration::from_secs(1)
            }
        );
        assert_eq!(profile.unwrap().press, Duration::from_millis(50));
    }
}
