//! The full Nintendo Switch controller layout, and the adapter that plays the
//! bot's GBA [`Controller`] on top of any [`SwitchController`].
//!
//! Devices that emulate a Switch controller (the ESP32-S3 WiFi firmware)
//! implement [`SwitchController`], which exposes every button and both
//! sticks. The bot only knows GBA buttons, so it drives them through
//! [`GbaOnSwitch`], which maps each GBA button to where the Nintendo Switch
//! Online GBA app expects it (Start → `+`, Select → `−`, D-pad → D-pad).

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::controller::{
    Button, ButtonSet, ConsoleLink, Controller, ControllerCommand, ControllerReceipt, PressProfile,
};
use crate::{Error, Result};

/// Every digital input of a Switch controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SwitchButton {
    Y,
    B,
    A,
    X,
    L,
    R,
    ZL,
    ZR,
    Minus,
    Plus,
    /// Left stick click.
    LStick,
    /// Right stick click.
    RStick,
    Home,
    Capture,
    Up,
    Down,
    Left,
    Right,
}

impl SwitchButton {
    pub const ALL: [SwitchButton; 18] = [
        SwitchButton::Y,
        SwitchButton::B,
        SwitchButton::A,
        SwitchButton::X,
        SwitchButton::L,
        SwitchButton::R,
        SwitchButton::ZL,
        SwitchButton::ZR,
        SwitchButton::Minus,
        SwitchButton::Plus,
        SwitchButton::LStick,
        SwitchButton::RStick,
        SwitchButton::Home,
        SwitchButton::Capture,
        SwitchButton::Up,
        SwitchButton::Down,
        SwitchButton::Left,
        SwitchButton::Right,
    ];

    fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

impl fmt::Display for SwitchButton {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl FromStr for SwitchButton {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        SwitchButton::ALL
            .into_iter()
            .find(|b| b.to_string().eq_ignore_ascii_case(s))
            .ok_or_else(|| Error::InvalidData(format!("unknown Switch button {s:?}")))
    }
}

/// A set of simultaneously held Switch buttons. Serializes as a sorted list.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "Vec<SwitchButton>", into = "Vec<SwitchButton>")]
pub struct SwitchButtons(u32);

impl SwitchButtons {
    pub const NONE: SwitchButtons = SwitchButtons(0);

    pub fn contains(self, button: SwitchButton) -> bool {
        self.0 & button.bit() != 0
    }

    pub fn with(self, button: SwitchButton) -> Self {
        Self(self.0 | button.bit())
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = SwitchButton> {
        SwitchButton::ALL
            .into_iter()
            .filter(move |b| self.contains(*b))
    }
}

impl From<SwitchButton> for SwitchButtons {
    fn from(button: SwitchButton) -> Self {
        Self(button.bit())
    }
}

impl FromIterator<SwitchButton> for SwitchButtons {
    fn from_iter<I: IntoIterator<Item = SwitchButton>>(iter: I) -> Self {
        iter.into_iter().fold(Self::NONE, Self::with)
    }
}

impl From<Vec<SwitchButton>> for SwitchButtons {
    fn from(buttons: Vec<SwitchButton>) -> Self {
        buttons.into_iter().collect()
    }
}

impl From<SwitchButtons> for Vec<SwitchButton> {
    fn from(set: SwitchButtons) -> Self {
        set.iter().collect()
    }
}

impl fmt::Debug for SwitchButtons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl fmt::Display for SwitchButtons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let names: Vec<String> = self.iter().map(|b| b.to_string()).collect();
        f.write_str(&names.join("+"))
    }
}

impl FromStr for SwitchButtons {
    type Err = Error;

    /// Parses `Home`, `L+R`, or `none`.
    fn from_str(s: &str) -> Result<Self> {
        if s.eq_ignore_ascii_case("none") {
            return Ok(Self::NONE);
        }
        s.split('+').map(str::parse).collect()
    }
}

/// An analog stick position. `x`: 0 = left, 255 = right; `y`: 0 = up,
/// 255 = down (the USB HID convention). 128 is centred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stick {
    pub x: u8,
    pub y: u8,
}

impl Stick {
    pub const CENTER: Stick = Stick { x: 128, y: 128 };
    pub const UP: Stick = Stick { x: 128, y: 0 };
    pub const DOWN: Stick = Stick { x: 128, y: 255 };
    pub const LEFT: Stick = Stick { x: 0, y: 128 };
    pub const RIGHT: Stick = Stick { x: 255, y: 128 };
}

impl Default for Stick {
    fn default() -> Self {
        Self::CENTER
    }
}

/// Everything a Switch controller reports at one instant. Missing fields
/// deserialize as released / centred.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct SwitchState {
    pub buttons: SwitchButtons,
    pub left_stick: Stick,
    pub right_stick: Stick,
}

impl SwitchState {
    pub const NEUTRAL: SwitchState = SwitchState {
        buttons: SwitchButtons::NONE,
        left_stick: Stick::CENTER,
        right_stick: Stick::CENTER,
    };

    pub fn is_neutral(&self) -> bool {
        *self == Self::NEUTRAL
    }
}

impl From<SwitchButtons> for SwitchState {
    fn from(buttons: SwitchButtons) -> Self {
        Self {
            buttons,
            ..Self::NEUTRAL
        }
    }
}

impl From<SwitchButton> for SwitchState {
    fn from(button: SwitchButton) -> Self {
        SwitchButtons::from(button).into()
    }
}

/// Hold `state` for `duration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedSwitchInput {
    pub state: SwitchState,
    pub duration: Duration,
}

/// The Switch counterpart of [`ControllerCommand`], with the same semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwitchCommand {
    /// Tap one button.
    Press(SwitchButton),
    /// Tap several buttons simultaneously.
    Chord(SwitchButtons),
    /// Hold buttons and/or sticks for a duration, then release.
    Hold {
        state: SwitchState,
        duration: Duration,
    },
    /// Explicit timeline of inputs, executed back to back.
    Sequence(Vec<TimedSwitchInput>),
    /// Cancel any queued input and release everything immediately.
    Neutral,
}

impl SwitchCommand {
    /// Expands the command into the timeline a device must reproduce.
    /// [`SwitchCommand::Neutral`] expands to nothing: its effect is to clear
    /// the device's queue.
    pub fn timeline(&self, profile: &PressProfile) -> Vec<TimedSwitchInput> {
        let tap = |state: SwitchState| {
            vec![
                TimedSwitchInput {
                    state,
                    duration: profile.press,
                },
                TimedSwitchInput {
                    state: SwitchState::NEUTRAL,
                    duration: profile.release,
                },
            ]
        };
        match self {
            SwitchCommand::Press(button) => tap((*button).into()),
            SwitchCommand::Chord(buttons) => tap((*buttons).into()),
            SwitchCommand::Hold { state, duration } => vec![TimedSwitchInput {
                state: *state,
                duration: *duration,
            }],
            SwitchCommand::Sequence(inputs) => inputs.clone(),
            SwitchCommand::Neutral => Vec::new(),
        }
    }
}

/// A device that emulates a full Switch controller. Same contract as
/// [`Controller`]: commands queue and run back to back, `execute` does not
/// wait for them.
pub trait SwitchController {
    fn execute(&mut self, command: SwitchCommand) -> Result<ControllerReceipt>;

    /// True when every queued input has been fully applied.
    fn is_idle(&self) -> Result<bool>;

    /// See [`Controller::console_link`].
    fn console_link(&mut self) -> Result<ConsoleLink> {
        Ok(ConsoleLink::Unknown)
    }
}

impl<C: SwitchController + ?Sized> SwitchController for Box<C> {
    fn execute(&mut self, command: SwitchCommand) -> Result<ControllerReceipt> {
        (**self).execute(command)
    }

    fn is_idle(&self) -> Result<bool> {
        (**self).is_idle()
    }

    fn console_link(&mut self) -> Result<ConsoleLink> {
        (**self).console_link()
    }
}

// ------------------------------------------------------------ GBA layout ----

/// Where the Nintendo Switch Online GBA app expects each GBA button.
pub fn gba_to_switch(button: Button) -> SwitchButton {
    match button {
        Button::A => SwitchButton::A,
        Button::B => SwitchButton::B,
        Button::L => SwitchButton::L,
        Button::R => SwitchButton::R,
        Button::Start => SwitchButton::Plus,
        Button::Select => SwitchButton::Minus,
        Button::Home => SwitchButton::Home,
        Button::Up => SwitchButton::Up,
        Button::Down => SwitchButton::Down,
        Button::Left => SwitchButton::Left,
        Button::Right => SwitchButton::Right,
    }
}

impl From<ButtonSet> for SwitchButtons {
    fn from(set: ButtonSet) -> Self {
        set.iter().map(gba_to_switch).collect()
    }
}

impl From<ButtonSet> for SwitchState {
    fn from(set: ButtonSet) -> Self {
        SwitchButtons::from(set).into()
    }
}

impl SwitchState {
    /// The GBA buttons this state presses in the GBA app. Inputs the app does
    /// not use (X, Y, ZL, Home, sticks, ...) are ignored.
    pub fn gba_buttons(&self) -> ButtonSet {
        Button::ALL
            .into_iter()
            .filter(|b| self.buttons.contains(gba_to_switch(*b)))
            .collect()
    }
}

impl From<&ControllerCommand> for SwitchCommand {
    fn from(command: &ControllerCommand) -> Self {
        match command {
            ControllerCommand::Press(button) => SwitchCommand::Press(gba_to_switch(*button)),
            ControllerCommand::Chord(buttons) => SwitchCommand::Chord((*buttons).into()),
            ControllerCommand::Hold { buttons, duration } => SwitchCommand::Hold {
                state: (*buttons).into(),
                duration: *duration,
            },
            ControllerCommand::Sequence(inputs) => SwitchCommand::Sequence(
                inputs
                    .iter()
                    .map(|t| TimedSwitchInput {
                        state: t.buttons.into(),
                        duration: t.duration,
                    })
                    .collect(),
            ),
            ControllerCommand::Neutral => SwitchCommand::Neutral,
        }
    }
}

/// Adapter: the bot's GBA [`Controller`] played on a [`SwitchController`].
pub struct GbaOnSwitch<C>(pub C);

impl<C> GbaOnSwitch<C> {
    pub fn inner(&self) -> &C {
        &self.0
    }

    pub fn inner_mut(&mut self) -> &mut C {
        &mut self.0
    }
}

impl<C: SwitchController> Controller for GbaOnSwitch<C> {
    fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt> {
        self.0.execute(SwitchCommand::from(&command))
    }

    fn is_idle(&self) -> Result<bool> {
        self.0.is_idle()
    }

    fn console_link(&mut self) -> Result<ConsoleLink> {
        self.0.console_link()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::TimedInput;

    #[test]
    fn buttons_parse_display_and_serialize() {
        let set: SwitchButtons = "home+zl+up".parse().unwrap();
        assert_eq!(set.to_string(), "ZL+Home+Up");
        assert_eq!(
            serde_json::to_string(&set).unwrap(),
            r#"["ZL","Home","Up"]"#
        );
        assert!("Z".parse::<SwitchButtons>().is_err());
    }

    #[test]
    fn state_defaults_to_neutral_when_fields_are_missing() {
        let state: SwitchState = serde_json::from_str(r#"{"buttons":["A"]}"#).unwrap();
        assert_eq!(state, SwitchState::from(SwitchButton::A));
        let state: SwitchState =
            serde_json::from_str(r#"{"left_stick":{"x":255,"y":128}}"#).unwrap();
        assert_eq!(state.left_stick, Stick::RIGHT);
        assert_eq!(state.right_stick, Stick::CENTER);
        assert!(state.buttons.is_empty());
    }

    #[test]
    fn gba_layout_is_injective_and_round_trips() {
        let mapped: SwitchButtons = Button::ALL.into_iter().map(gba_to_switch).collect();
        assert_eq!(mapped.iter().count(), Button::ALL.len());
        let gba: ButtonSet = "A+Start+Select+Left".parse().unwrap();
        let state = SwitchState::from(gba);
        assert_eq!(state.buttons.to_string(), "A+Minus+Plus+Left");
        assert_eq!(state.gba_buttons(), gba);
        let extra = SwitchState::from(state.buttons.with(SwitchButton::Home));
        assert_eq!(extra.gba_buttons(), gba);
    }

    #[test]
    fn gba_commands_keep_their_timeline() {
        let profile = PressProfile::default();
        let commands = [
            ControllerCommand::Press(Button::Start),
            ControllerCommand::Chord("A+B".parse().unwrap()),
            ControllerCommand::Hold {
                buttons: Button::Up.into(),
                duration: Duration::from_millis(300),
            },
            ControllerCommand::Sequence(vec![TimedInput {
                buttons: Button::R.into(),
                duration: Duration::from_millis(5),
            }]),
            ControllerCommand::Neutral,
        ];
        for command in commands {
            let gba = command.timeline(&profile);
            let switch = SwitchCommand::from(&command).timeline(&profile);
            assert_eq!(gba.len(), switch.len(), "{command:?}");
            for (g, s) in gba.iter().zip(&switch) {
                assert_eq!(g.duration, s.duration);
                assert_eq!(s.state, SwitchState::from(g.buttons));
            }
        }
    }

    struct Recorder(Vec<SwitchCommand>);

    impl SwitchController for Recorder {
        fn execute(&mut self, command: SwitchCommand) -> Result<ControllerReceipt> {
            self.0.push(command);
            Ok(ControllerReceipt {
                command_id: self.0.len() as u64,
                issued_at: Instant::now(),
                input_duration: Duration::ZERO,
            })
        }

        fn is_idle(&self) -> Result<bool> {
            Ok(true)
        }
    }

    #[test]
    fn adapter_forwards_mapped_commands() {
        let mut gba = GbaOnSwitch(Recorder(Vec::new()));
        gba.execute(ControllerCommand::Press(Button::Select))
            .unwrap();
        gba.execute(ControllerCommand::Neutral).unwrap();
        assert_eq!(
            gba.inner().0,
            vec![
                SwitchCommand::Press(SwitchButton::Minus),
                SwitchCommand::Neutral
            ]
        );
    }
}
