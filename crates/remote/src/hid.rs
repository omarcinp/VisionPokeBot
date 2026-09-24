//! The USB HID device the firmware presents to the Switch: a HORI Pokkén
//! Tournament wired controller (VID 0x0F0D, PID 0x0092). The Switch accepts
//! it without pairing, and its 8-byte report carries the full layout: 14
//! buttons, the D-pad as a hat switch, and both sticks. It is the same layout
//! the PABotBase wired-controller mode uses.

use std::ffi::CStr;

use pokebot_core::{Stick, SwitchButton, SwitchButtons, SwitchState};

pub const VENDOR_ID: u16 = 0x0F0D;
pub const PRODUCT_ID: u16 = 0x0092;
pub const MANUFACTURER: &CStr = c"HORI CO.,LTD.";
pub const PRODUCT: &CStr = c"POKKEN CONTROLLER";
/// Endpoint polling interval requested from the host.
pub const POLL_INTERVAL_MS: u8 = 1;

pub const REPORT_BYTES: usize = 8;

/// Report descriptor: 16 buttons, a hat switch, four 8-bit axes, one vendor
/// byte in; 8 vendor bytes out (ignored).
#[rustfmt::skip]
pub const REPORT_DESCRIPTOR: [u8; 86] = [
    0x05, 0x01,        // Usage Page (Generic Desktop)
    0x09, 0x05,        // Usage (Game Pad)
    0xA1, 0x01,        // Collection (Application)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x01,        //   Logical Maximum (1)
    0x35, 0x00,        //   Physical Minimum (0)
    0x45, 0x01,        //   Physical Maximum (1)
    0x75, 0x01,        //   Report Size (1)
    0x95, 0x10,        //   Report Count (16)
    0x05, 0x09,        //   Usage Page (Button)
    0x19, 0x01,        //   Usage Minimum (1)
    0x29, 0x10,        //   Usage Maximum (16)
    0x81, 0x02,        //   Input (Data, Var, Abs)
    0x05, 0x01,        //   Usage Page (Generic Desktop)
    0x25, 0x07,        //   Logical Maximum (7)
    0x46, 0x3B, 0x01,  //   Physical Maximum (315)
    0x75, 0x04,        //   Report Size (4)
    0x95, 0x01,        //   Report Count (1)
    0x65, 0x14,        //   Unit (Degrees)
    0x09, 0x39,        //   Usage (Hat Switch)
    0x81, 0x42,        //   Input (Data, Var, Abs, Null State)
    0x65, 0x00,        //   Unit (None)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x01,        //   Input (Const) — hat padding
    0x26, 0xFF, 0x00,  //   Logical Maximum (255)
    0x46, 0xFF, 0x00,  //   Physical Maximum (255)
    0x09, 0x30,        //   Usage (X)
    0x09, 0x31,        //   Usage (Y)
    0x09, 0x32,        //   Usage (Z)
    0x09, 0x35,        //   Usage (Rz)
    0x75, 0x08,        //   Report Size (8)
    0x95, 0x04,        //   Report Count (4)
    0x81, 0x02,        //   Input (Data, Var, Abs)
    0x06, 0x00, 0xFF,  //   Usage Page (Vendor Defined)
    0x09, 0x20,        //   Usage (0x20)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x02,        //   Input (Data, Var, Abs)
    0x0A, 0x21, 0x26,  //   Usage (0x2621)
    0x95, 0x08,        //   Report Count (8)
    0x91, 0x02,        //   Output (Data, Var, Abs)
    0xC0,              // End Collection
];

/// Bit of each button in the report's 16-bit button field. The D-pad is the
/// hat switch instead.
pub fn button_bit(button: SwitchButton) -> Option<u16> {
    let bit = match button {
        SwitchButton::Y => 0,
        SwitchButton::B => 1,
        SwitchButton::A => 2,
        SwitchButton::X => 3,
        SwitchButton::L => 4,
        SwitchButton::R => 5,
        SwitchButton::ZL => 6,
        SwitchButton::ZR => 7,
        SwitchButton::Minus => 8,
        SwitchButton::Plus => 9,
        SwitchButton::LStick => 10,
        SwitchButton::RStick => 11,
        SwitchButton::Home => 12,
        SwitchButton::Capture => 13,
        SwitchButton::Up | SwitchButton::Down | SwitchButton::Left | SwitchButton::Right => {
            return None
        }
    };
    Some(1 << bit)
}

pub const HAT_NEUTRAL: u8 = 8;

/// One input report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchReport {
    pub buttons: u16,
    /// 0 = up, clockwise to 7 = up-left, 8 = neutral.
    pub hat: u8,
    pub lx: u8,
    pub ly: u8,
    pub rx: u8,
    pub ry: u8,
}

impl SwitchReport {
    pub const NEUTRAL: SwitchReport = SwitchReport {
        buttons: 0,
        hat: HAT_NEUTRAL,
        lx: Stick::CENTER.x,
        ly: Stick::CENTER.y,
        rx: Stick::CENTER.x,
        ry: Stick::CENTER.y,
    };

    pub fn from_state(state: &SwitchState) -> Self {
        let pressed = |b| state.buttons.contains(b);
        let buttons = state
            .buttons
            .iter()
            .filter_map(button_bit)
            .fold(0, |acc, bit| acc | bit);
        Self {
            buttons,
            hat: hat(
                pressed(SwitchButton::Up),
                pressed(SwitchButton::Down),
                pressed(SwitchButton::Left),
                pressed(SwitchButton::Right),
            ),
            lx: state.left_stick.x,
            ly: state.left_stick.y,
            rx: state.right_stick.x,
            ry: state.right_stick.y,
        }
    }

    /// Inverse of [`SwitchReport::from_state`]. Opposite D-pad directions
    /// have already cancelled in the hat.
    pub fn to_state(&self) -> SwitchState {
        let mut buttons: SwitchButtons = SwitchButton::ALL
            .into_iter()
            .filter(|b| button_bit(*b).is_some_and(|bit| self.buttons & bit != 0))
            .collect();
        let (up, right, down, left) = match self.hat {
            0 => (true, false, false, false),
            1 => (true, true, false, false),
            2 => (false, true, false, false),
            3 => (false, true, true, false),
            4 => (false, false, true, false),
            5 => (false, false, true, true),
            6 => (false, false, false, true),
            7 => (true, false, false, true),
            _ => (false, false, false, false),
        };
        for (on, button) in [
            (up, SwitchButton::Up),
            (right, SwitchButton::Right),
            (down, SwitchButton::Down),
            (left, SwitchButton::Left),
        ] {
            if on {
                buttons = buttons.with(button);
            }
        }
        SwitchState {
            buttons,
            left_stick: Stick {
                x: self.lx,
                y: self.ly,
            },
            right_stick: Stick {
                x: self.rx,
                y: self.ry,
            },
        }
    }

    pub fn to_bytes(&self) -> [u8; REPORT_BYTES] {
        let [lo, hi] = self.buttons.to_le_bytes();
        [lo, hi, self.hat, self.lx, self.ly, self.rx, self.ry, 0]
    }

    pub fn from_bytes(bytes: &[u8; REPORT_BYTES]) -> Self {
        Self {
            buttons: u16::from_le_bytes([bytes[0], bytes[1]]),
            hat: bytes[2] & 0x0f,
            lx: bytes[3],
            ly: bytes[4],
            rx: bytes[5],
            ry: bytes[6],
        }
    }
}

impl Default for SwitchReport {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

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
    use pokebot_core::ButtonSet;

    use super::*;

    fn buttons(s: &str) -> SwitchState {
        s.parse::<SwitchButtons>().unwrap().into()
    }

    #[test]
    fn descriptor_is_balanced() {
        let opens = REPORT_DESCRIPTOR.iter().filter(|&&b| b == 0xA1).count();
        assert_eq!(opens, 1);
        assert_eq!(REPORT_DESCRIPTOR.last(), Some(&0xC0));
    }

    #[test]
    fn neutral_report_bytes() {
        assert_eq!(
            SwitchReport::from_state(&SwitchState::NEUTRAL).to_bytes(),
            [0, 0, 8, 0x80, 0x80, 0x80, 0x80, 0]
        );
    }

    #[test]
    fn every_button_has_its_own_bit() {
        let bits: Vec<u16> = SwitchButton::ALL
            .into_iter()
            .filter_map(button_bit)
            .collect();
        assert_eq!(bits.len(), 14);
        assert_eq!(bits.iter().fold(0, |a, b| a | b).count_ones(), 14);
    }

    #[test]
    fn matches_pabotbase_wired_layout() {
        // Same bytes PABotBase puts after the duration for GBA "B+Down".
        let gba: ButtonSet = "B+Down".parse().unwrap();
        let report = SwitchReport::from_state(&gba.into());
        assert_eq!(
            &report.to_bytes()[..7],
            &[0x02, 0x00, 4, 0x80, 0x80, 0x80, 0x80]
        );
        let report = SwitchReport::from_state(&buttons("Home+Capture+ZR"));
        assert_eq!(report.buttons, 1 << 12 | 1 << 13 | 1 << 7);
    }

    #[test]
    fn states_round_trip_through_bytes() {
        let states = [
            SwitchState::NEUTRAL,
            SwitchState {
                buttons: "Y+X+ZL+LStick+RStick+Up+Right".parse().unwrap(),
                left_stick: Stick::LEFT,
                right_stick: Stick { x: 17, y: 230 },
            },
            buttons("A+B+L+R+Minus+Plus+Home+Capture+Down+Left"),
        ];
        for state in states {
            let report = SwitchReport::from_state(&state);
            assert_eq!(
                SwitchReport::from_bytes(&report.to_bytes()).to_state(),
                state
            );
        }
    }

    #[test]
    fn opposite_directions_cancel() {
        let report = SwitchReport::from_state(&buttons("Up+Down+Left"));
        assert_eq!(report.to_state().buttons.to_string(), "Left");
    }
}
