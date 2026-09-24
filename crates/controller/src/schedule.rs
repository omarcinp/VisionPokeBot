use std::collections::VecDeque;
use std::time::Duration;

use pokebot_core::{ButtonSet, ControllerCommand, PressProfile};

/// Exact rational frame rate (frames per `denominator` seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRate {
    pub numerator: u64,
    pub denominator: u64,
}

impl FrameRate {
    /// GBA: 16.78 MHz clock / 280 896 cycles per frame ≈ 59.7275 Hz.
    pub const GBA: FrameRate = FrameRate {
        numerator: 16_777_216,
        denominator: 280_896,
    };

    /// Number of whole frames needed to cover `duration` (rounded up, at
    /// least one frame for any non-zero duration).
    pub fn frames_for(self, duration: Duration) -> u64 {
        let nanos = duration.as_nanos();
        let num = nanos * u128::from(self.numerator);
        let den = 1_000_000_000u128 * u128::from(self.denominator);
        num.div_ceil(den) as u64
    }

    pub fn frame_period(self) -> Duration {
        let nanos = (1_000_000_000u128 * u128::from(self.denominator)) / u128::from(self.numerator);
        Duration::from_nanos(nanos as u64)
    }

    pub fn duration_of(self, frames: u64) -> Duration {
        let nanos = (u128::from(frames) * 1_000_000_000 * u128::from(self.denominator))
            / u128::from(self.numerator);
        Duration::from_nanos(nanos as u64)
    }
}

/// Converts queued controller commands into one button state per video frame.
///
/// Devices that are clocked by frames (the emulator, or a microcontroller
/// that ticks at the console's refresh rate) call [`InputSchedule::advance`]
/// once per frame. When the queue is empty every button is released.
///
/// Every command gets a sequential id, and the schedule tracks which commands
/// have been fully applied, so a device can report completion.
#[derive(Debug)]
pub struct InputSchedule {
    rate: FrameRate,
    profile: PressProfile,
    segments: VecDeque<Segment>,
    next_id: u64,
    completed: Option<u64>,
}

#[derive(Debug)]
struct Segment {
    buttons: ButtonSet,
    /// Zero only for markers of commands that occupy no time.
    frames: u64,
    command: u64,
}

impl InputSchedule {
    pub fn new(rate: FrameRate, profile: PressProfile) -> Self {
        Self {
            rate,
            profile,
            segments: VecDeque::new(),
            next_id: 0,
            completed: None,
        }
    }

    /// Queues `command` behind any pending input. Returns its id and how many
    /// frames it will occupy. [`ControllerCommand::Neutral`] clears the queue
    /// (cancelled commands count as completed).
    pub fn enqueue(&mut self, command: &ControllerCommand) -> (u64, u64) {
        let id = self.next_id;
        self.next_id += 1;
        if matches!(command, ControllerCommand::Neutral) {
            self.segments.clear();
            self.completed = Some(id);
            return (id, 0);
        }
        let mut total = 0;
        for input in command.timeline(&self.profile) {
            let frames = self.rate.frames_for(input.duration);
            if frames > 0 {
                self.segments.push_back(Segment {
                    buttons: input.buttons,
                    frames,
                    command: id,
                });
                total += frames;
            }
        }
        if total == 0 {
            if self.segments.is_empty() {
                self.completed = Some(id);
            } else {
                self.segments.push_back(Segment {
                    buttons: ButtonSet::NONE,
                    frames: 0,
                    command: id,
                });
            }
        }
        (id, total)
    }

    /// Buttons to hold during the next frame.
    pub fn advance(&mut self) -> ButtonSet {
        self.pop_finished();
        let Some(front) = self.segments.front_mut() else {
            return ButtonSet::NONE;
        };
        let buttons = front.buttons;
        front.frames -= 1;
        self.pop_finished();
        buttons
    }

    /// Id of the newest command whose input has been completely applied.
    pub fn completed_through(&self) -> Option<u64> {
        self.completed
    }

    pub fn is_idle(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn pending_frames(&self) -> u64 {
        self.segments.iter().map(|s| s.frames).sum()
    }

    pub fn rate(&self) -> FrameRate {
        self.rate
    }

    fn pop_finished(&mut self) {
        while self.segments.front().is_some_and(|s| s.frames == 0) {
            let done = self.segments.pop_front().map(|s| s.command);
            if self.segments.front().map(|s| s.command) != done {
                self.completed = done;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use pokebot_core::{Button, TimedInput};

    use super::*;

    const SIXTY: FrameRate = FrameRate {
        numerator: 60,
        denominator: 1,
    };

    fn drain(schedule: &mut InputSchedule) -> Vec<ButtonSet> {
        let mut out = Vec::new();
        while !schedule.is_idle() {
            out.push(schedule.advance());
        }
        out
    }

    #[test]
    fn frames_round_up() {
        assert_eq!(SIXTY.frames_for(Duration::ZERO), 0);
        assert_eq!(SIXTY.frames_for(Duration::from_millis(1)), 1);
        assert_eq!(SIXTY.frames_for(Duration::from_millis(50)), 3);
        assert_eq!(SIXTY.frames_for(Duration::from_secs(1)), 60);
        assert_eq!(FrameRate::GBA.frames_for(Duration::from_secs(1)), 60);
        assert_eq!(FrameRate::GBA.frames_for(Duration::from_millis(80)), 5);
    }

    #[test]
    fn press_holds_then_releases() {
        let profile = PressProfile {
            press: Duration::from_millis(50),
            release: Duration::from_millis(34),
        };
        let mut schedule = InputSchedule::new(SIXTY, profile);
        assert_eq!(
            schedule.enqueue(&ControllerCommand::Press(Button::A)),
            (0, 6)
        );
        let a: ButtonSet = Button::A.into();
        assert_eq!(
            drain(&mut schedule),
            vec![a, a, a, ButtonSet::NONE, ButtonSet::NONE, ButtonSet::NONE]
        );
        assert_eq!(schedule.advance(), ButtonSet::NONE);
    }

    #[test]
    fn commands_queue_back_to_back_and_neutral_clears() {
        let mut schedule = InputSchedule::new(SIXTY, PressProfile::default());
        let up: ButtonSet = Button::Up.into();
        let b_up = up.with(Button::B);
        schedule.enqueue(&ControllerCommand::Sequence(vec![
            TimedInput {
                buttons: up,
                duration: Duration::from_millis(33),
            },
            TimedInput {
                buttons: b_up,
                duration: Duration::from_millis(16),
            },
        ]));
        schedule.enqueue(&ControllerCommand::Hold {
            buttons: up,
            duration: Duration::from_millis(16),
        });
        assert_eq!(schedule.pending_frames(), 4);
        assert_eq!(schedule.advance(), up);
        assert_eq!(schedule.advance(), up);
        assert_eq!(schedule.advance(), b_up);
        schedule.enqueue(&ControllerCommand::Neutral);
        assert!(schedule.is_idle());
        assert_eq!(schedule.advance(), ButtonSet::NONE);
    }

    #[test]
    fn tracks_command_completion() {
        let mut schedule = InputSchedule::new(SIXTY, PressProfile::default());
        let hold = |ms| ControllerCommand::Hold {
            buttons: Button::A.into(),
            duration: Duration::from_millis(ms),
        };
        assert_eq!(schedule.enqueue(&hold(33)), (0, 2));
        assert_eq!(
            schedule.enqueue(&ControllerCommand::Sequence(vec![])),
            (1, 0)
        );
        assert_eq!(schedule.enqueue(&hold(16)), (2, 1));
        assert_eq!(schedule.completed_through(), None);
        schedule.advance();
        assert_eq!(schedule.completed_through(), None);
        schedule.advance();
        assert_eq!(
            schedule.completed_through(),
            Some(1),
            "empty command completes with its predecessor"
        );
        schedule.advance();
        assert_eq!(schedule.completed_through(), Some(2));
        schedule.enqueue(&hold(100));
        assert_eq!(schedule.enqueue(&ControllerCommand::Neutral), (4, 0));
        assert_eq!(schedule.completed_through(), Some(4));
        assert!(schedule.is_idle());
    }
}
