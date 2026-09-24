//! Switch controller reports and the GBA → Switch button map (Nintendo Switch
//! Online GBA layout).

use pokebot_core::{Button, ButtonSet};

use crate::message::{op, Message};

/// Physical Switch controller inputs used by the GBA app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SwitchInput {
    A,
    B,
    L,
    R,
    Plus,
    Minus,
    Home,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

pub fn switch_input(button: Button) -> SwitchInput {
    match button {
        Button::A => SwitchInput::A,
        Button::B => SwitchInput::B,
        Button::L => SwitchInput::L,
        Button::R => SwitchInput::R,
        Button::Start => SwitchInput::Plus,
        Button::Select => SwitchInput::Minus,
        Button::Home => SwitchInput::Home,
        Button::Up => SwitchInput::DpadUp,
        Button::Down => SwitchInput::DpadDown,
        Button::Left => SwitchInput::DpadLeft,
        Button::Right => SwitchInput::DpadRight,
    }
}

/// Controller the firmware emulates (PABotBase controller ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerKind {
    WirelessProController,
    WiredController,
}

impl ControllerKind {
    pub const fn id(self) -> u32 {
        match self {
            Self::WirelessProController => 0x1180,
            Self::WiredController => 0x1000,
        }
    }

    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            0x1180 => Some(Self::WirelessProController),
            0x1000 => Some(Self::WiredController),
            _ => None,
        }
    }
}

const NEUTRAL_STICK_OEM: [u8; 3] = [0x00, 0x08, 0x80]; // x = y = 2048, 12-bit packed
const NEUTRAL_STICK_WIRED: u8 = 0x80;
const HAT_NEUTRAL: u8 = 8;

/// One queued command: hold `buttons` for `milliseconds`.
pub fn encode_command(
    kind: ControllerKind,
    id: u8,
    buttons: ButtonSet,
    milliseconds: u16,
) -> Message {
    let mut body = milliseconds.to_le_bytes().to_vec();
    let has = |input: SwitchInput| buttons.iter().any(|b| switch_input(b) == input);
    let bit = |input: SwitchInput, n: u8| if has(input) { 1u8 << n } else { 0 };
    match kind {
        ControllerKind::WirelessProController => {
            let button3 = bit(SwitchInput::B, 2) | bit(SwitchInput::A, 3) | bit(SwitchInput::R, 6);
            let button4 =
                bit(SwitchInput::Minus, 0) | bit(SwitchInput::Plus, 1) | bit(SwitchInput::Home, 4);
            let button5 = bit(SwitchInput::DpadDown, 0)
                | bit(SwitchInput::DpadUp, 1)
                | bit(SwitchInput::DpadRight, 2)
                | bit(SwitchInput::DpadLeft, 3)
                | bit(SwitchInput::L, 6);
            body.extend_from_slice(&[button3, button4, button5]);
            body.extend_from_slice(&NEUTRAL_STICK_OEM);
            body.extend_from_slice(&NEUTRAL_STICK_OEM);
            body.push(0); // vibrator
            Message::new(op::CMD_NS1_OEM_CONTROLLER_BUTTONS, id, body)
        }
        ControllerKind::WiredController => {
            let buttons0 = u16::from(bit(SwitchInput::B, 1))
                | u16::from(bit(SwitchInput::A, 2))
                | u16::from(bit(SwitchInput::L, 4))
                | u16::from(bit(SwitchInput::R, 5))
                | (u16::from(has(SwitchInput::Minus)) << 8)
                | (u16::from(has(SwitchInput::Plus)) << 9)
                | (u16::from(has(SwitchInput::Home)) << 12);
            body.extend_from_slice(&buttons0.to_le_bytes());
            body.push(hat(
                has(SwitchInput::DpadUp),
                has(SwitchInput::DpadDown),
                has(SwitchInput::DpadLeft),
                has(SwitchInput::DpadRight),
            ));
            body.extend_from_slice(&[NEUTRAL_STICK_WIRED; 4]);
            Message::new(op::CMD_NS_WIRED_CONTROLLER_STATE, id, body)
        }
    }
}

/// Device side: recovers the GBA buttons and duration from a command.
/// Switch inputs the GBA app does not use (X, Y, ZL, Home, ...) are ignored.
pub fn decode_command(message: &Message) -> Option<(ButtonSet, u16)> {
    let body = &message.body;
    let ms = u16::from_le_bytes([*body.first()?, *body.get(1)?]);
    let mut set = ButtonSet::NONE;
    let mut add = |on: bool, button: Button| {
        if on {
            set = set.with(button);
        }
    };
    match message.opcode {
        op::CMD_NS1_OEM_CONTROLLER_BUTTONS => {
            let (b3, b4, b5) = (*body.get(2)?, *body.get(3)?, *body.get(4)?);
            add(b3 & 1 << 2 != 0, Button::B);
            add(b3 & 1 << 3 != 0, Button::A);
            add(b3 & 1 << 6 != 0, Button::R);
            add(b4 & 1 << 0 != 0, Button::Select);
            add(b4 & 1 << 1 != 0, Button::Start);
            add(b5 & 1 << 0 != 0, Button::Down);
            add(b5 & 1 << 1 != 0, Button::Up);
            add(b5 & 1 << 2 != 0, Button::Right);
            add(b5 & 1 << 3 != 0, Button::Left);
            add(b5 & 1 << 6 != 0, Button::L);
        }
        op::CMD_NS_WIRED_CONTROLLER_STATE => {
            let b = u16::from_le_bytes([*body.get(2)?, *body.get(3)?]);
            add(b & 1 << 1 != 0, Button::B);
            add(b & 1 << 2 != 0, Button::A);
            add(b & 1 << 4 != 0, Button::L);
            add(b & 1 << 5 != 0, Button::R);
            add(b & 1 << 8 != 0, Button::Select);
            add(b & 1 << 9 != 0, Button::Start);
            let (up, down, left, right) = match *body.get(4)? & 0x0f {
                0 => (true, false, false, false),
                1 => (true, false, false, true),
                2 => (false, false, false, true),
                3 => (false, true, false, true),
                4 => (false, true, false, false),
                5 => (false, true, true, false),
                6 => (false, false, true, false),
                7 => (true, false, true, false),
                _ => (false, false, false, false),
            };
            add(up, Button::Up);
            add(down, Button::Down);
            add(left, Button::Left);
            add(right, Button::Right);
        }
        _ => return None,
    }
    Some((set, ms))
}

/// D-pad hat value: 0 = up, clockwise to 7 = up-left, 8 = neutral.
/// Opposite directions cancel.
fn hat(up: bool, down: bool, left: bool, right: bool) -> u8 {
    let vertical = i8::from(up) - i8::from(down);
    let horizontal = i8::from(right) - i8::from(left);
    match (vertical, horizontal) {
        (1, 0) => 0,
        (1, 1) => 1,
        (0, 1) => 2,
        (-1, 1) => 3,
        (-1, 0) => 4,
        (-1, -1) => 5,
        (0, -1) => 6,
        (1, -1) => 7,
        _ => HAT_NEUTRAL,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_gba_button_has_a_distinct_switch_input() {
        let mapped: HashSet<_> = Button::ALL.into_iter().map(switch_input).collect();
        assert_eq!(mapped.len(), Button::ALL.len());
    }

    #[test]
    fn commands_round_trip_for_both_controllers() {
        for kind in [
            ControllerKind::WirelessProController,
            ControllerKind::WiredController,
        ] {
            for set in [
                "A",
                "B+Up",
                "Start+Select",
                "L+R+Down+Right",
                "none",
                "Up+Left",
            ] {
                let buttons: ButtonSet = set.parse().unwrap();
                let message = encode_command(kind, 3, buttons, 1234);
                assert_eq!(
                    decode_command(&message),
                    Some((buttons, 1234)),
                    "{kind:?} {set}"
                );
            }
        }
    }

    #[test]
    fn oem_layout_matches_protocol() {
        let message = encode_command(
            ControllerKind::WirelessProController,
            0,
            "A+Start+Up".parse().unwrap(),
            100,
        );
        assert_eq!(message.opcode, 0x97);
        assert_eq!(
            message.body,
            vec![100, 0, 0x08, 0x02, 0x02, 0x00, 0x08, 0x80, 0x00, 0x08, 0x80, 0]
        );
        let wired = encode_command(
            ControllerKind::WiredController,
            0,
            "B+Down".parse().unwrap(),
            50,
        );
        assert_eq!(
            wired.body,
            vec![50, 0, 0x02, 0x00, 4, 0x80, 0x80, 0x80, 0x80]
        );
    }
}
