//! In-game saving (Start menu → SAVE → YES [→ overwrite YES]) and continuing
//! a saved game (title → CONTINUE → skip the recap).

use std::time::Duration;

use pokebot_core::{Button, ControllerCommand};
use pokebot_state::{GameEvent, ScreenState};

use crate::new_game::{advance_or_wait, select};
use crate::{Action, Decision, Expectation, Outcome, Task, TaskContext};

/// Quiet frames in the overworld after the save messages before finishing.
const SETTLED: u32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavePhase {
    OpenMenu,
    Saving,
}

/// Saves the game through the Start menu. SAVE is found by position: it is
/// always third from the bottom (…, SAVE, OPTION, EXIT), whatever else the
/// menu contains. Row pitch differs between menus and the window's bottom is
/// hidden by the help bar, so instead of counting rows the task uses the menu's
/// wrap-around: from the top entry, Up goes to EXIT, then OPTION, then SAVE.
pub struct SaveGameTask {
    phase: SavePhase,
    /// Up presses after wrapping from the top entry to EXIT (0 = not yet).
    ups_from_bottom: u32,
    answered: u32,
    quiet: u32,
    attempts: u32,
}

impl Default for SaveGameTask {
    fn default() -> Self {
        Self {
            phase: SavePhase::OpenMenu,
            ups_from_bottom: 0,
            answered: 0,
            quiet: 0,
            attempts: 0,
        }
    }
}

/// The ▶ is on a menu's first entry.
fn at_top(menu: &pokebot_state::MenuObservation) -> bool {
    menu.cursor_y <= menu.window.y + 8
}

impl Task for SaveGameTask {
    fn name(&self) -> &str {
        "SaveGame"
    }

    fn next(&mut self, ctx: &mut TaskContext<'_>) -> Decision {
        let o = ctx.observation;
        if self.attempts > 8 {
            return Decision::Fail("could not save".into());
        }
        match self.phase {
            SavePhase::OpenMenu => match (&o.menu, &o.dialogue) {
                (Some(menu), None) => {
                    let up = |label: &str| {
                        Decision::Act(Action::new(
                            label,
                            vec![ControllerCommand::Press(Button::Up)],
                            Expectation::MenuCursorMoved {
                                from_y: menu.cursor_y,
                                up: true,
                            },
                            45,
                        ))
                    };
                    match self.ups_from_bottom {
                        // Menus remember their cursor: first go to the top.
                        0 if !at_top(menu) => up("cursor up to the first entry"),
                        0 => Decision::Act(Action::new(
                            "wrap to EXIT",
                            vec![ControllerCommand::Press(Button::Up)],
                            Expectation::MenuCursorMoved {
                                from_y: menu.cursor_y,
                                up: false,
                            },
                            45,
                        )),
                        1 => up("cursor to OPTION"),
                        2 => up("cursor to SAVE"),
                        // The save summary and "Would you like to save…?" open;
                        // the YES/NO appears once the text has printed.
                        _ => Decision::Act(Action::new(
                            "choose SAVE",
                            vec![ControllerCommand::Press(Button::A)],
                            Expectation::DialogueOpen,
                            60,
                        )),
                    }
                }
                (None, None) if o.player.is_some() => Decision::Act(Action::new(
                    "open the Start menu",
                    vec![ControllerCommand::Press(Button::Start)],
                    Expectation::MenuOpen,
                    60,
                )),
                (_, Some(_)) => advance_or_wait(o.dialogue.as_ref(), "dialogue"),
                _ => Decision::Wait("waiting for the overworld".into()),
            },
            SavePhase::Saving => {
                if let (Some(menu), Some(_)) = (&o.menu, &o.dialogue) {
                    // "Would you like to save the game?" / "…overwrite it?"
                    self.quiet = 0;
                    return select(menu, 0, "answer YES");
                }
                if o.dialogue.is_some() {
                    self.quiet = 0;
                    return advance_or_wait(o.dialogue.as_ref(), "saving");
                }
                if self.answered > 0 && o.menu.is_none() && o.player.is_some() {
                    self.quiet += 1;
                    if self.quiet >= SETTLED {
                        ctx.events.push(GameEvent::GameSaved);
                        return Decision::Done("game saved".into());
                    }
                    return Decision::Wait("save finishing".into());
                }
                if o.menu.is_some() && self.answered > 0 {
                    // Start menu still open after saving: close it.
                    return Decision::Act(Action::new(
                        "close the Start menu",
                        vec![ControllerCommand::Press(Button::B)],
                        Expectation::MenuClosed,
                        45,
                    ));
                }
                Decision::Wait("waiting for the save prompt".into())
            }
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, _ctx: &mut TaskContext<'_>) {
        match outcome {
            Outcome::TimedOut => self.attempts += 1,
            Outcome::Confirmed if action.label == "answer YES" => self.answered += 1,
            Outcome::Confirmed if action.label == "wrap to EXIT" => self.ups_from_bottom = 1,
            Outcome::Confirmed if action.label == "cursor to OPTION" => self.ups_from_bottom = 2,
            Outcome::Confirmed if action.label == "cursor to SAVE" => self.ups_from_bottom = 3,
            Outcome::Confirmed if action.label == "choose SAVE" => self.phase = SavePhase::Saving,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinuePhase {
    SoftReset,
    AwaitTitle,
    MainMenu,
    Resume,
}

/// Loads the saved game: soft reset, title, CONTINUE, then skip the
/// "Previously on your quest…" recap until the player is on the map.
pub struct ContinueTask {
    phase: ContinuePhase,
    located: u32,
    attempts: u32,
}

impl Default for ContinueTask {
    fn default() -> Self {
        Self {
            phase: ContinuePhase::SoftReset,
            located: 0,
            attempts: 0,
        }
    }
}

impl Task for ContinueTask {
    fn name(&self) -> &str {
        "ContinueGame"
    }

    fn next(&mut self, ctx: &mut TaskContext<'_>) -> Decision {
        let o = ctx.observation;
        if self.attempts > 20 {
            return Decision::Fail(format!("stuck in {:?}", self.phase));
        }
        match self.phase {
            ContinuePhase::SoftReset => Decision::Act(Action::new(
                "soft reset (A+B+Start+Select)",
                vec![ControllerCommand::Hold {
                    buttons: [Button::A, Button::B, Button::Start, Button::Select]
                        .into_iter()
                        .collect(),
                    duration: Duration::from_millis(250),
                }],
                Expectation::InputsDone,
                0,
            )),
            ContinuePhase::AwaitTitle => match o.screen.value {
                ScreenState::TitleScreen => Decision::Act(Action::new(
                    "press Start on the title screen",
                    vec![ControllerCommand::Press(Button::Start)],
                    Expectation::ScreenIsNot(ScreenState::TitleScreen),
                    120,
                )),
                ScreenState::MainMenu => {
                    self.phase = ContinuePhase::MainMenu;
                    self.next(ctx)
                }
                _ => Decision::Act(Action::new(
                    "press Start to skip the intro",
                    vec![ControllerCommand::Press(Button::Start)],
                    Expectation::ScreenIs(ScreenState::TitleScreen),
                    150,
                )),
            },
            ContinuePhase::MainMenu => match (&o.menu, o.screen.value) {
                (Some(menu), ScreenState::MainMenu) => select(menu, 0, "choose CONTINUE"),
                _ => Decision::Wait("waiting for the main menu".into()),
            },
            ContinuePhase::Resume => {
                if o.player.is_some() && o.dialogue.is_none() && o.menu.is_none() {
                    self.located += 1;
                    if self.located >= 60 {
                        return Decision::Done("continued the saved game".into());
                    }
                    return Decision::Wait("confirming we're in control".into());
                }
                self.located = 0;
                // The recap plays by itself; B skips it.
                Decision::Act(Action::new(
                    "press B to skip the recap",
                    vec![ControllerCommand::Press(Button::B)],
                    Expectation::InputsDone,
                    45,
                ))
            }
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, _ctx: &mut TaskContext<'_>) {
        if outcome == Outcome::TimedOut {
            self.attempts += 1;
            return;
        }
        match (self.phase, action.label.as_str()) {
            (ContinuePhase::SoftReset, _) => self.phase = ContinuePhase::AwaitTitle,
            (ContinuePhase::AwaitTitle, "press Start on the title screen") => {
                self.phase = ContinuePhase::MainMenu
            }
            (ContinuePhase::MainMenu, "choose CONTINUE") => self.phase = ContinuePhase::Resume,
            _ => {}
        }
    }
}
