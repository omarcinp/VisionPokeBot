use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Logical GBA buttons. Hardware adapters map these to whatever the physical
/// device exposes (e.g. Start → `+` on a Switch controller).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Button {
    A,
    B,
    L,
    R,
    Start,
    Select,
    Up,
    Down,
    Left,
    Right,
    /// The console's HOME button: not a GBA input, used only to get back
    /// into the game from a console menu. Emulators ignore it.
    Home,
}

impl Button {
    /// The GBA's buttons (without [`Button::Home`]).
    pub const ALL: [Button; 10] = [
        Button::A,
        Button::B,
        Button::L,
        Button::R,
        Button::Start,
        Button::Select,
        Button::Up,
        Button::Down,
        Button::Left,
        Button::Right,
    ];

    fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

impl fmt::Display for Button {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl FromStr for Button {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Button::ALL
            .into_iter()
            .chain([Button::Home])
            .find(|b| b.to_string().eq_ignore_ascii_case(s))
            .ok_or_else(|| Error::InvalidData(format!("unknown button {s:?}")))
    }
}

/// A set of simultaneously held buttons. Serializes as a sorted list.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "Vec<Button>", into = "Vec<Button>")]
pub struct ButtonSet(u16);

impl ButtonSet {
    pub const NONE: ButtonSet = ButtonSet(0);

    pub fn contains(self, button: Button) -> bool {
        self.0 & button.bit() != 0
    }

    pub fn with(self, button: Button) -> Self {
        Self(self.0 | button.bit())
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Button> {
        Button::ALL.into_iter().filter(move |b| self.contains(*b))
    }
}

impl From<Button> for ButtonSet {
    fn from(button: Button) -> Self {
        Self(button.bit())
    }
}

impl FromIterator<Button> for ButtonSet {
    fn from_iter<I: IntoIterator<Item = Button>>(iter: I) -> Self {
        iter.into_iter().fold(Self::NONE, Self::with)
    }
}

impl From<Vec<Button>> for ButtonSet {
    fn from(buttons: Vec<Button>) -> Self {
        buttons.into_iter().collect()
    }
}

impl From<ButtonSet> for Vec<Button> {
    fn from(set: ButtonSet) -> Self {
        set.iter().collect()
    }
}

impl fmt::Debug for ButtonSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl fmt::Display for ButtonSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let names: Vec<String> = self.iter().map(|b| b.to_string()).collect();
        f.write_str(&names.join("+"))
    }
}

impl FromStr for ButtonSet {
    type Err = Error;

    /// Parses `A`, `B+Up`, or `none`.
    fn from_str(s: &str) -> Result<Self> {
        if s.eq_ignore_ascii_case("none") {
            return Ok(Self::NONE);
        }
        s.split('+').map(str::parse).collect()
    }
}

/// Hold `buttons` (possibly none) for `duration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedInput {
    pub buttons: ButtonSet,
    pub duration: Duration,
}

/// How long a tap holds its button(s) and how long it then stays released so
/// that consecutive taps register as separate presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressProfile {
    pub press: Duration,
    pub release: Duration,
}

impl Default for PressProfile {
    fn default() -> Self {
        Self {
            press: Duration::from_millis(80),
            release: Duration::from_millis(80),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControllerCommand {
    /// Tap one button.
    Press(Button),
    /// Tap several buttons simultaneously.
    Chord(ButtonSet),
    /// Hold buttons for a duration, then release.
    Hold {
        buttons: ButtonSet,
        duration: Duration,
    },
    /// Explicit timeline of inputs, executed back to back.
    Sequence(Vec<TimedInput>),
    /// Cancel any queued input and release every button immediately.
    Neutral,
}

impl ControllerCommand {
    /// Expands the command into the timeline every adapter must reproduce.
    /// [`ControllerCommand::Neutral`] expands to nothing: its effect is to
    /// clear the adapter's queue.
    pub fn timeline(&self, profile: &PressProfile) -> Vec<TimedInput> {
        let tap = |buttons: ButtonSet| {
            vec![
                TimedInput {
                    buttons,
                    duration: profile.press,
                },
                TimedInput {
                    buttons: ButtonSet::NONE,
                    duration: profile.release,
                },
            ]
        };
        match self {
            ControllerCommand::Press(button) => tap((*button).into()),
            ControllerCommand::Chord(buttons) => tap(*buttons),
            ControllerCommand::Hold { buttons, duration } => vec![TimedInput {
                buttons: *buttons,
                duration: *duration,
            }],
            ControllerCommand::Sequence(inputs) => inputs.clone(),
            ControllerCommand::Neutral => Vec::new(),
        }
    }
}

/// Acknowledgement that a command was accepted by the device. It says nothing
/// about what the game did with it — only video can confirm that.
#[derive(Debug, Clone)]
pub struct ControllerReceipt {
    pub command_id: u64,
    pub issued_at: Instant,
    /// Total time the command occupies the input line once it starts.
    pub input_duration: Duration,
}

/// The bot's only actuator. Implementations: emulator joypad, ESP32 /
/// PABotBase bridge, no-op controller for replays.
///
/// Commands are queued and executed back to back; `execute` does not wait for
/// the input to finish.
pub trait Controller {
    fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt>;

    /// True when every queued input has been fully applied.
    fn is_idle(&self) -> Result<bool>;

    /// What the controller knows about the console at the other end of its
    /// cable. Devices that can't tell answer [`ConsoleLink::Unknown`].
    fn console_link(&mut self) -> Result<ConsoleLink> {
        Ok(ConsoleLink::Unknown)
    }
}

impl<C: Controller + ?Sized> Controller for Box<C> {
    fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt> {
        (**self).execute(command)
    }

    fn is_idle(&self) -> Result<bool> {
        (**self).is_idle()
    }

    fn console_link(&mut self) -> Result<ConsoleLink> {
        (**self).console_link()
    }
}

/// The controller's view of the console, from its USB connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConsoleLink {
    /// The console has the controller configured and is awake.
    Attached,
    /// Configured, but the console suspended the bus: it is asleep.
    Suspended,
    /// Not configured: the console is off or asleep with its USB ports
    /// unpowered, or the cable is out.
    Detached,
    /// The device can't tell (emulator, serial bridge, older firmware).
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_set_parses_and_displays() {
        let set: ButtonSet = "b+up".parse().unwrap();
        assert!(set.contains(Button::B) && set.contains(Button::Up));
        assert_eq!(set.to_string(), "B+Up");
        assert_eq!("none".parse::<ButtonSet>().unwrap(), ButtonSet::NONE);
        assert!("Z".parse::<ButtonSet>().is_err());
    }

    #[test]
    fn button_set_serializes_as_sorted_list() {
        let set: ButtonSet = [Button::Up, Button::A].into_iter().collect();
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(json, r#"["A","Up"]"#);
        assert_eq!(serde_json::from_str::<ButtonSet>(&json).unwrap(), set);
    }

    #[test]
    fn press_expands_to_tap_then_release() {
        let profile = PressProfile::default();
        let timeline = ControllerCommand::Press(Button::A).timeline(&profile);
        assert_eq!(
            timeline,
            vec![
                TimedInput {
                    buttons: Button::A.into(),
                    duration: profile.press
                },
                TimedInput {
                    buttons: ButtonSet::NONE,
                    duration: profile.release
                },
            ]
        );
        assert!(ControllerCommand::Neutral.timeline(&profile).is_empty());
    }
}
