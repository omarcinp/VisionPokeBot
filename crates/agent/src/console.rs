//! Bringing the game up on a real Switch: wait while the console is off or
//! asleep, unlock it when it comes back, find the software on the HOME menu
//! and start it.
//!
//! What the bot can see:
//! - the capture: an all-black frame means no HDMI signal (off or asleep);
//!   a lit letterbox around the game's viewport means a console screen (the
//!   lock screen, the HOME menu, the user picker, a dialog); otherwise it is
//!   the game;
//! - the controller: whether the Switch has it configured over USB
//!   ([`ConsoleLink::Attached`]), has suspended the bus (asleep), or not at
//!   all (off, or a dock that powers its ports down in sleep).
//!
//! [`ConsoleGuard`] is the deterministic decision part (time, screen and link
//! in; inputs out), [`bring_up_game`] runs it against the devices.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pokebot_core::{Button, ButtonSet, ConsoleLink, ControllerCommand, TimedInput};
use pokebot_runtime::Runtime;
use pokebot_state::GameEvent;

use crate::ExecutorError;

/// What the capture shows, as far as the console is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleScreen {
    /// All black: no signal, or a fade.
    Dark,
    /// The game's viewport with a black letterbox.
    Game,
    /// The letterbox is lit: a console screen, not the game.
    System,
}

impl ConsoleScreen {
    pub fn classify(dark: bool, outside_game: bool) -> Self {
        if outside_game {
            ConsoleScreen::System
        } else if dark {
            ConsoleScreen::Dark
        } else {
            ConsoleScreen::Game
        }
    }
}

/// The game counts as up after showing this long without interruption.
pub const GAME_CONFIRM: Duration = Duration::from_secs(3);
/// Black this long (outside a launch) means off or asleep. FireRed's own
/// fades last well under a second.
pub const OFF_AFTER: Duration = Duration::from_secs(10);
/// A console screen must hold this long before the bot acts on it.
pub const SYSTEM_SETTLE: Duration = Duration::from_secs(2);
/// A launch may show black this long (software booting) before the console
/// counts as off again.
pub const LAUNCH_DARK: Duration = Duration::from_secs(60);
/// Time for the game to appear after a launch's last input.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);
/// HOME presses asking a suspended console to wake up, this far apart.
pub const WAKE_EVERY: Duration = Duration::from_secs(30);
/// HOME-menu slot tried by each launch attempt, in turn: the first slot
/// (the software played last) twice, then the next ones.
const SLOTS: [u32; 4] = [0, 0, 1, 2];

/// One decision of the guard.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConsoleTick {
    /// A state change worth logging.
    pub note: Option<String>,
    /// An input to send now, with its label.
    pub act: Option<(String, ControllerCommand)>,
    /// The game is up.
    pub ready: Option<String>,
}

#[derive(Debug, Clone)]
enum Mode {
    Watch,
    Off {
        next_wake: Duration,
    },
    Launch {
        steps: VecDeque<Step>,
        next_at: Duration,
        deadline: Option<Duration>,
    },
}

#[derive(Debug, Clone)]
struct Step {
    label: String,
    command: ControllerCommand,
    /// Pause after sending it.
    wait: Duration,
}

/// Decides what to press to get from whatever the console shows to the
/// game running. Deterministic: same (time, screen, link) sequence, same
/// inputs.
#[derive(Debug, Clone)]
pub struct ConsoleGuard {
    screen: Option<(ConsoleScreen, Duration)>,
    mode: Mode,
    attempt: u32,
    /// The console just came back from off/asleep: the lock screen ("press
    /// the same button three times") is likely up.
    woke: bool,
}

impl Default for ConsoleGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleGuard {
    pub fn new() -> Self {
        Self {
            screen: None,
            mode: Mode::Watch,
            attempt: 0,
            woke: false,
        }
    }

    /// Launch attempts started so far.
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// `now` is the time since the guard started.
    pub fn step(&mut self, now: Duration, screen: ConsoleScreen, link: ConsoleLink) -> ConsoleTick {
        let since = match self.screen {
            Some((s, since)) if s == screen => since,
            _ => {
                self.screen = Some((screen, now));
                now
            }
        };
        let held = now.saturating_sub(since);
        let mut tick = ConsoleTick::default();
        if screen == ConsoleScreen::Game && held >= GAME_CONFIRM {
            tick.ready = Some(match self.attempt {
                0 => "the game is on screen".to_owned(),
                n => format!("the game is on screen after {n} launch attempt(s)"),
            });
            return tick;
        }
        match &mut self.mode {
            Mode::Watch => {
                let asleep = matches!(link, ConsoleLink::Detached | ConsoleLink::Suspended);
                if screen == ConsoleScreen::Dark && (held >= OFF_AFTER || asleep) {
                    tick.note = Some(format!(
                        "the Switch is off or asleep (no picture, controller link {link:?}); \
                         waiting for it to be turned on"
                    ));
                    self.mode = Mode::Off { next_wake: now };
                } else if screen == ConsoleScreen::System && held >= SYSTEM_SETTLE {
                    tick.note = Some(self.start_launch(now));
                }
            }
            Mode::Off { next_wake } => {
                if screen != ConsoleScreen::Dark {
                    tick.note = Some(format!("the Switch is on ({screen:?} on screen)"));
                    self.woke = true;
                    self.mode = Mode::Watch;
                } else if link == ConsoleLink::Suspended && now >= *next_wake {
                    // A pressed button makes the controller ask for a USB
                    // remote wakeup, like HOME on a wired pad.
                    tick.act = Some((
                        "wake the Switch: press HOME (USB remote wakeup)".to_owned(),
                        ControllerCommand::Press(Button::Home),
                    ));
                    *next_wake = now + WAKE_EVERY;
                }
            }
            Mode::Launch {
                steps,
                next_at,
                deadline,
            } => {
                if screen == ConsoleScreen::Dark && held >= LAUNCH_DARK {
                    tick.note = Some("no picture since the launch: waiting again".to_owned());
                    self.mode = Mode::Off { next_wake: now };
                    return tick;
                }
                // Inputs go only to a console screen: over the game (or a
                // booting one) HOME would suspend it.
                if screen == ConsoleScreen::System && now >= *next_at {
                    if let Some(step) = steps.pop_front() {
                        *next_at = now + step.wait;
                        if steps.is_empty() {
                            *deadline = Some(now + LAUNCH_TIMEOUT);
                        }
                        tick.act = Some((step.label, step.command));
                        return tick;
                    }
                }
                if deadline.is_some_and(|d| now >= d) {
                    if screen == ConsoleScreen::System {
                        tick.note = Some(self.start_launch(now));
                    } else {
                        self.mode = Mode::Watch;
                    }
                }
            }
        }
        tick
    }

    /// Plans the next launch attempt; returns its description.
    fn start_launch(&mut self, now: Duration) -> String {
        let attempt = self.attempt;
        self.attempt += 1;
        let slot = SLOTS[attempt as usize % SLOTS.len()];
        let mut steps = VecDeque::new();
        let unlock = self.woke || attempt % 2 == 1;
        if unlock {
            steps.push_back(Step {
                label: "unlock: press A three times".into(),
                command: three_taps(Button::A),
                wait: Duration::from_secs(2),
            });
        }
        steps.push_back(Step {
            label:
                "press HOME: resumes a suspended game, else the cursor goes to the first software"
                    .into(),
            command: ControllerCommand::Press(Button::Home),
            wait: Duration::from_secs(2),
        });
        for n in 0..slot {
            steps.push_back(Step {
                label: format!("move right to slot {}", n + 2),
                command: ControllerCommand::Press(Button::Right),
                wait: Duration::from_millis(500),
            });
        }
        steps.push_back(Step {
            label: format!("start the software in slot {}: press A", slot + 1),
            command: ControllerCommand::Press(Button::A),
            wait: Duration::from_secs(4),
        });
        steps.push_back(Step {
            label: "confirm (user picker, closing other software): press A".into(),
            command: ControllerCommand::Press(Button::A),
            wait: Duration::ZERO,
        });
        self.woke = false;
        self.mode = Mode::Launch {
            steps,
            next_at: now,
            deadline: None,
        };
        format!(
            "a console screen instead of the game: launch attempt {} (slot {}{})",
            attempt + 1,
            slot + 1,
            if unlock { ", unlock first" } else { "" }
        )
    }
}

/// The same button tapped three times (the Switch's lock screen).
fn three_taps(button: Button) -> ControllerCommand {
    let press = ButtonSet::from(button);
    let tap = |buttons: ButtonSet, ms: u64| TimedInput {
        buttons,
        duration: Duration::from_millis(ms),
    };
    ControllerCommand::Sequence(vec![
        tap(press, 100),
        tap(ButtonSet::NONE, 250),
        tap(press, 100),
        tap(ButtonSet::NONE, 250),
        tap(press, 100),
        tap(ButtonSet::NONE, 250),
    ])
}

/// How often the controller is asked about the console's USB link.
const LINK_POLL: Duration = Duration::from_secs(5);
/// Console-screen snapshots kept for building detectors later.
const MAX_SNAPSHOTS: usize = 50;

/// Waits until the game shows on the Switch, doing whatever it takes: waits
/// while the console is off, unlocks it, starts the software from the HOME
/// menu, retrying forever. Returns at once for sources that are only the
/// game (the emulator). Saves a capture of each new console screen to
/// `snapshots` (at most [`MAX_SNAPSHOTS`] are kept), to build detectors from.
pub fn bring_up_game(
    runtime: &mut Runtime,
    stop: &AtomicBool,
    snapshots: Option<&Path>,
) -> Result<String, ExecutorError> {
    if !runtime.watches_console() {
        return Ok("the video is the game alone".into());
    }
    let started = Instant::now();
    let mut guard = ConsoleGuard::new();
    let mut link = ConsoleLink::Unknown;
    let mut link_polled: Option<Instant> = None;
    let mut last_screen = None;
    let mut video_error_logged = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(ExecutorError::Stopped);
        }
        let screen = match runtime.observe() {
            Ok(_) => {
                video_error_logged = false;
                ConsoleScreen::classify(runtime.dark(), runtime.outside_game())
            }
            Err(e) => {
                // A capture without signal may stop delivering frames.
                if !video_error_logged {
                    runtime.error(format!("console: video: {e}; treating it as no picture"));
                    video_error_logged = true;
                }
                std::thread::sleep(Duration::from_secs(1));
                ConsoleScreen::Dark
            }
        };
        if link_polled.is_none_or(|t| t.elapsed() >= LINK_POLL) {
            link_polled = Some(Instant::now());
            let now = match runtime.console_link() {
                Ok(l) => l,
                Err(e) => {
                    runtime.error(format!("console: controller: {e}"));
                    ConsoleLink::Unknown
                }
            };
            if now != link {
                runtime.info(format!("console: controller link {link:?} → {now:?}"));
                link = now;
            }
        }
        if last_screen != Some(screen) {
            if screen == ConsoleScreen::System {
                if let Some(dir) = snapshots {
                    save_snapshot(runtime, dir);
                }
            }
            last_screen = Some(screen);
        }
        let tick = guard.step(started.elapsed(), screen, link);
        if let Some(note) = tick.note {
            report(runtime, "Watch", &note)?;
        }
        if let Some((label, command)) = tick.act {
            report(runtime, "Input", &label)?;
            // Not fatal: the controller reconnects on the next command.
            if let Err(e) = runtime.execute(command) {
                runtime.error(format!("console: {label}: {e}"));
            }
        }
        if let Some(ready) = tick.ready {
            report(runtime, "Ready", &ready)?;
            return Ok(ready);
        }
    }
}

fn report(runtime: &mut Runtime, phase: &str, detail: &str) -> Result<(), ExecutorError> {
    runtime.info(format!("console: {detail}"));
    runtime.emit(GameEvent::GoalProgress {
        goal: "Console".into(),
        phase: phase.into(),
        detail: detail.into(),
    })?;
    Ok(())
}

/// Writes the full captured frame; drops the oldest beyond the cap.
fn save_snapshot(runtime: &Runtime, dir: &Path) {
    let Some(captured) = runtime.last_captured() else {
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = dir.join(format!("{stamp}-console-f{}.png", captured.frame_id));
    let saved = std::fs::create_dir_all(dir)
        .map_err(|e| e.to_string())
        .and_then(|()| pokebot_video::png::save(&captured.image, &path).map_err(|e| e.to_string()));
    match saved {
        Ok(()) => runtime.info(format!("console: saved {}", path.display())),
        Err(e) => runtime.error(format!("console: snapshot {}: {e}", path.display())),
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "png"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let excess = files.len().saturating_sub(MAX_SNAPSHOTS);
    for old in &files[..excess] {
        let _ = std::fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;
    const MS: fn(u64) -> Duration = Duration::from_millis;

    /// Feeds `screen` from `from` to `to` in 100 ms ticks; the labels of
    /// the inputs sent, notes and readiness.
    fn run(
        guard: &mut ConsoleGuard,
        from: Duration,
        to: Duration,
        screen: ConsoleScreen,
        link: ConsoleLink,
    ) -> (Vec<String>, Vec<String>, Option<String>) {
        let (mut acts, mut notes) = (Vec::new(), Vec::new());
        let mut t = from;
        while t < to {
            let tick = guard.step(t, screen, link);
            acts.extend(tick.act.map(|(label, _)| label));
            notes.extend(tick.note);
            if tick.ready.is_some() {
                return (acts, notes, tick.ready);
            }
            t += MS(100);
        }
        (acts, notes, None)
    }

    use ConsoleLink::*;
    use ConsoleScreen::*;

    #[test]
    fn the_game_on_screen_is_ready_after_three_seconds() {
        let mut g = ConsoleGuard::new();
        let (acts, _, ready) = run(&mut g, S(0), S(10), Game, Attached);
        assert!(acts.is_empty());
        assert!(ready.is_some());
    }

    #[test]
    fn short_fades_are_not_the_console_turning_off() {
        let mut g = ConsoleGuard::new();
        let (acts, notes, _) = run(&mut g, S(0), S(5), Dark, Attached);
        assert!(acts.is_empty() && notes.is_empty());
        let (_, _, ready) = run(&mut g, S(5), S(9), Game, Attached);
        assert!(ready.is_some());
    }

    #[test]
    fn off_waits_without_pressing_until_the_picture_returns() {
        let mut g = ConsoleGuard::new();
        let (acts, notes, _) = run(&mut g, S(0), S(600), Dark, Detached);
        assert!(acts.is_empty(), "{acts:?}");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("off or asleep"), "{notes:?}");
    }

    #[test]
    fn a_suspended_console_gets_home_presses_to_wake_it() {
        let mut g = ConsoleGuard::new();
        let (acts, _, _) = run(&mut g, S(0), S(95), Dark, Suspended);
        // At once, then every 30 s.
        assert_eq!(acts.len(), 4, "{acts:?}");
        assert!(acts.iter().all(|a| a.contains("press HOME")));
    }

    #[test]
    fn waking_up_unlocks_then_starts_the_first_software() {
        let mut g = ConsoleGuard::new();
        run(&mut g, S(0), S(20), Dark, Detached);
        // Turned on: the lock screen (a console screen) comes up.
        let (acts, notes, ready) = run(&mut g, S(20), S(40), System, Attached);
        assert!(notes[0].contains("is on"), "{notes:?}");
        assert!(
            notes[1].contains("attempt 1 (slot 1, unlock first)"),
            "{notes:?}"
        );
        assert_eq!(
            acts,
            vec![
                "unlock: press A three times",
                "press HOME: resumes a suspended game, else the cursor goes to the first software",
                "start the software in slot 1: press A",
                "confirm (user picker, closing other software): press A",
            ]
        );
        assert!(ready.is_none());
        // The game boots (black), then shows.
        run(&mut g, S(40), S(50), Dark, Attached);
        let (acts, _, ready) = run(&mut g, S(50), S(55), Game, Attached);
        assert!(acts.is_empty());
        assert_eq!(
            ready.as_deref(),
            Some("the game is on screen after 1 launch attempt(s)")
        );
    }

    #[test]
    fn no_input_while_the_game_shows_mid_launch() {
        let mut g = ConsoleGuard::new();
        g.woke = true;
        let (acts, _, _) = run(&mut g, S(0), S(3), System, Attached);
        assert_eq!(acts, vec!["unlock: press A three times"]);
        // Unlocking went straight back into the suspended game: no HOME.
        let (acts, _, ready) = run(&mut g, S(3), S(7), Game, Attached);
        assert!(acts.is_empty(), "{acts:?}");
        assert!(ready.is_some());
    }

    #[test]
    fn failed_launches_retry_with_unlock_and_later_slots() {
        let mut g = ConsoleGuard::new();
        let (_, notes, _) = run(&mut g, S(0), S(400), System, Attached);
        let attempts: Vec<&String> = notes.iter().filter(|n| n.contains("attempt")).collect();
        assert!(attempts.len() >= 4, "{notes:?}");
        assert!(attempts[0].ends_with("attempt 1 (slot 1)"), "{attempts:?}");
        assert!(attempts[1].ends_with("attempt 2 (slot 1, unlock first)"));
        assert!(attempts[2].ends_with("attempt 3 (slot 2)"));
        assert!(attempts[3].ends_with("attempt 4 (slot 3, unlock first)"));
    }

    #[test]
    fn slot_two_is_reached_with_right() {
        let mut g = ConsoleGuard::new();
        g.attempt = 2;
        let (acts, _, _) = run(&mut g, S(0), S(20), System, Attached);
        assert_eq!(
            acts,
            vec![
                "press HOME: resumes a suspended game, else the cursor goes to the first software",
                "move right to slot 2",
                "start the software in slot 2: press A",
                "confirm (user picker, closing other software): press A",
            ]
        );
    }

    #[test]
    fn classify_puts_a_lit_letterbox_first() {
        assert_eq!(ConsoleScreen::classify(true, true), System);
        assert_eq!(ConsoleScreen::classify(true, false), Dark);
        assert_eq!(ConsoleScreen::classify(false, false), Game);
    }

    #[test]
    fn three_taps_press_the_same_button_three_times() {
        let ControllerCommand::Sequence(inputs) = three_taps(Button::A) else {
            panic!("not a sequence")
        };
        let presses = inputs.iter().filter(|t| !t.buttons.is_empty()).count();
        assert_eq!(presses, 3);
    }
}
