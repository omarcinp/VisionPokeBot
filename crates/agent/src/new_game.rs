//! Start a new game from power-on: title screen, the new-game tutorial,
//! Professor Oak's introduction, gender, player name and rival name, until
//! the player is confirmed to be in control in their bedroom.
//!
//! Every step is driven by what is on screen; the phase only tells the task
//! how to interpret a menu or keyboard it sees (e.g. the first two-row menu
//! is the gender question, the five-row one the rival presets).

use std::time::Duration;

use pokebot_core::{Button, ControllerCommand};
use pokebot_state::{
    DialogueObservation, GameEvent, Gender, KeyboardFocus, MenuObservation, NamingObservation,
    ScreenState,
};

use crate::keyboard::{key_position, moves, taps, validate_name};
use crate::{Action, Decision, Expectation, Outcome, Task, TaskContext};

/// Rival names offered in the list (after "NEW NAME").
pub const RIVAL_PRESETS: [&str; 4] = ["GREEN", "GARY", "KAZ", "TORU"];

/// Timed-out attempts allowed per phase before giving up.
const MAX_ATTEMPTS: u32 = 12;
/// Frames of "nothing on screen" after the outro before checking control.
const SETTLE_FRAMES: u32 = 90;

#[derive(Debug, Clone)]
pub struct NewGameConfig {
    pub gender: Gender,
    pub player_name: String,
    pub rival_name: String,
    /// Soft-reset (A+B+Start+Select) first so the task can start from any state.
    pub soft_reset: bool,
}

impl Default for NewGameConfig {
    fn default() -> Self {
        Self {
            gender: Gender::Boy,
            player_name: "RED".into(),
            rival_name: "GREEN".into(),
            soft_reset: true,
        }
    }
}

impl NewGameConfig {
    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.player_name)?;
        validate_name(&self.rival_name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    SoftReset,
    AwaitTitle,
    Title,
    AfterTitle,
    Intro,
    Gender,
    PlayerIntro,
    PlayerName,
    ConfirmPlayer,
    RivalIntro,
    RivalChoice,
    RivalName,
    ConfirmRival,
    Outro,
    OpenStartMenu,
    CloseStartMenu,
    Done,
}

impl Phase {
    fn describe(self) -> &'static str {
        match self {
            Phase::SoftReset => "soft reset",
            Phase::AwaitTitle => "waiting for the title screen",
            Phase::Title => "leaving the title screen",
            Phase::AfterTitle => "entering the new game",
            Phase::Intro => "tutorial and Oak's introduction",
            Phase::Gender => "choosing gender",
            Phase::PlayerIntro => "waiting for the name prompt",
            Phase::PlayerName => "typing the player's name",
            Phase::ConfirmPlayer => "confirming the player's name",
            Phase::RivalIntro => "meeting the rival",
            Phase::RivalChoice => "choosing the rival's name",
            Phase::RivalName => "typing the rival's name",
            Phase::ConfirmRival => "confirming the rival's name",
            Phase::Outro => "finishing the introduction",
            Phase::OpenStartMenu => "checking control (opening Start menu)",
            Phase::CloseStartMenu => "checking control (closing Start menu)",
            Phase::Done => "done",
        }
    }
}

pub struct NewGameTask {
    config: NewGameConfig,
    phase: Phase,
    attempts: u32,
    /// Consecutive frames with no UI during the outro.
    quiet_frames: u32,
    /// A transition (fade/shrink) happened since the outro began.
    outro_transition: bool,
    /// Pressed Start on the keyboard; next is A on OK.
    confirming_name: bool,
    rival_from_list: bool,
}

impl NewGameTask {
    pub fn new(config: NewGameConfig) -> Result<Self, String> {
        config.validate()?;
        let phase = if config.soft_reset {
            Phase::SoftReset
        } else {
            Phase::AwaitTitle
        };
        Ok(Self {
            config,
            phase,
            attempts: 0,
            quiet_frames: 0,
            outro_transition: false,
            confirming_name: false,
            rival_from_list: false,
        })
    }

    fn enter(&mut self, phase: Phase, ctx: &mut TaskContext<'_>) {
        if phase == self.phase {
            return;
        }
        self.phase = phase;
        self.attempts = 0;
        self.quiet_frames = 0;
        self.confirming_name = false;
        ctx.events.push(GameEvent::GoalProgress {
            goal: self.name().to_owned(),
            phase: format!("{phase:?}"),
            detail: phase.describe().to_owned(),
        });
    }

    fn rival_preset_row(&self) -> Option<u8> {
        RIVAL_PRESETS
            .iter()
            .position(|p| *p == self.config.rival_name)
            .map(|i| i as u8 + 1)
    }

    fn name_for_phase(&self) -> &str {
        if self.phase == Phase::PlayerName {
            &self.config.player_name
        } else {
            &self.config.rival_name
        }
    }
}

impl Task for NewGameTask {
    fn name(&self) -> &str {
        "NewGame"
    }

    fn next(&mut self, ctx: &mut TaskContext<'_>) -> Decision {
        if let Some(fail) = self.attempts_exhausted() {
            return fail;
        }
        let o = ctx.observation;
        let screen = o.screen.value;
        match self.phase {
            Phase::SoftReset => Decision::Act(Action::new(
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
            Phase::AwaitTitle => match screen {
                ScreenState::TitleScreen => {
                    self.enter(Phase::Title, ctx);
                    self.next(ctx)
                }
                // Title already passed (e.g. a skip press landed on it).
                ScreenState::InfoPage
                | ScreenState::Dialogue
                | ScreenState::Menu
                | ScreenState::MainMenu => {
                    self.enter(Phase::AfterTitle, ctx);
                    self.next(ctx)
                }
                _ => Decision::Act(Action::new(
                    "press Start to skip the intro",
                    vec![ControllerCommand::Press(Button::Start)],
                    Expectation::ScreenIs(ScreenState::TitleScreen),
                    150,
                )),
            },
            Phase::Title => {
                if screen != ScreenState::TitleScreen {
                    self.enter(Phase::AfterTitle, ctx);
                    return self.next(ctx);
                }
                Decision::Act(Action::new(
                    "press Start on the title screen",
                    vec![ControllerCommand::Press(Button::Start)],
                    Expectation::ScreenIsNot(ScreenState::TitleScreen),
                    120,
                ))
            }
            Phase::AfterTitle => match (screen, &o.menu) {
                // With a save file, FireRed shows CONTINUE / NEW GAME.
                (ScreenState::MainMenu, Some(menu)) if menu.rows >= 2 => {
                    select(menu, 1, "select NEW GAME")
                }
                (ScreenState::InfoPage | ScreenState::Dialogue, _) => {
                    self.enter(Phase::Intro, ctx);
                    self.next(ctx)
                }
                _ => Decision::Wait("waiting for the new game to start".into()),
            },
            Phase::Intro => {
                if let Some(menu) = o.menu.filter(|_| o.dialogue.is_some()) {
                    if menu.rows == 2 {
                        self.enter(Phase::Gender, ctx);
                        return self.next(ctx);
                    }
                }
                advance_or_wait(o.dialogue.as_ref(), "tutorial/introduction running")
            }
            Phase::Gender => match &o.menu {
                Some(menu) => {
                    let row = match self.config.gender {
                        Gender::Boy => 0,
                        Gender::Girl => 1,
                    };
                    select(menu, row, &format!("choose {:?}", self.config.gender))
                }
                None => Decision::Wait("waiting for the gender menu".into()),
            },
            Phase::PlayerIntro => {
                if o.naming.is_some() {
                    self.enter(Phase::PlayerName, ctx);
                    return self.next(ctx);
                }
                advance_or_wait(o.dialogue.as_ref(), "waiting for the name prompt")
            }
            Phase::PlayerName | Phase::RivalName => match &o.naming {
                Some(naming) => self.type_name(naming),
                None => Decision::Wait("waiting for the keyboard".into()),
            },
            Phase::ConfirmPlayer | Phase::ConfirmRival => match (&o.menu, &o.naming) {
                (_, Some(_)) => {
                    // Answered NO somewhere: back to the keyboard.
                    let phase = if self.phase == Phase::ConfirmPlayer {
                        Phase::PlayerName
                    } else {
                        Phase::RivalName
                    };
                    self.enter(phase, ctx);
                    self.next(ctx)
                }
                (Some(menu), None) if menu.rows == 2 => select(menu, 0, "answer YES"),
                _ => advance_or_wait(o.dialogue.as_ref(), "waiting for the YES/NO question"),
            },
            Phase::RivalIntro => {
                if let Some(menu) = &o.menu {
                    if menu.rows >= 5 {
                        self.enter(Phase::RivalChoice, ctx);
                        return self.next(ctx);
                    }
                }
                advance_or_wait(o.dialogue.as_ref(), "Oak introducing the rival")
            }
            Phase::RivalChoice => match &o.menu {
                Some(menu) => {
                    let row = self.rival_preset_row().unwrap_or(0);
                    let label = if row == 0 {
                        "choose NEW NAME".to_owned()
                    } else {
                        format!("choose {}", self.config.rival_name)
                    };
                    select(menu, row, &label)
                }
                None => Decision::Wait("waiting for the rival name list".into()),
            },
            Phase::Outro => {
                if screen == ScreenState::Transition {
                    self.outro_transition = true;
                }
                let quiet = o.dialogue.is_none()
                    && o.menu.is_none()
                    && o.naming.is_none()
                    && screen == ScreenState::Unknown;
                self.quiet_frames = if quiet { self.quiet_frames + 1 } else { 0 };
                if self.outro_transition && self.quiet_frames >= SETTLE_FRAMES {
                    self.enter(Phase::OpenStartMenu, ctx);
                    return self.next(ctx);
                }
                advance_or_wait(o.dialogue.as_ref(), "introduction ending")
            }
            Phase::OpenStartMenu => Decision::Act(Action::new(
                "open the Start menu to confirm control",
                vec![ControllerCommand::Press(Button::Start)],
                Expectation::MenuOpen,
                60,
            )),
            Phase::CloseStartMenu => Decision::Act(Action::new(
                "close the Start menu",
                vec![ControllerCommand::Press(Button::B)],
                Expectation::MenuClosed,
                60,
            )),
            Phase::Done => Decision::Done(format!(
                "{} ({:?}) is in control; rival is {}",
                self.config.player_name, self.config.gender, self.config.rival_name
            )),
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut TaskContext<'_>) {
        if outcome == Outcome::TimedOut {
            self.attempts += 1;
            if self.phase == Phase::OpenStartMenu {
                // Not in control yet: keep watching the outro.
                self.enter(Phase::Outro, ctx);
            }
            return;
        }
        let o = ctx.observation;
        match (self.phase, &action.expect) {
            (Phase::SoftReset, _) => self.enter(Phase::AwaitTitle, ctx),
            (Phase::AwaitTitle, _) => self.enter(Phase::Title, ctx),
            (Phase::Title, _) => self.enter(Phase::AfterTitle, ctx),
            (Phase::Gender, Expectation::MenuClosed) => {
                ctx.events.push(GameEvent::GenderChosen {
                    gender: self.config.gender,
                });
                self.enter(Phase::PlayerIntro, ctx);
            }
            (
                Phase::PlayerName | Phase::RivalName,
                Expectation::KeyboardFocus(KeyboardFocus::Buttons),
            ) => {
                self.confirming_name = true;
            }
            (Phase::PlayerName, Expectation::NamingClosed) => self.enter(Phase::ConfirmPlayer, ctx),
            (Phase::RivalName, Expectation::NamingClosed) => self.enter(Phase::ConfirmRival, ctx),
            (Phase::ConfirmPlayer, Expectation::MenuClosed) if o.naming.is_none() => {
                ctx.events.push(GameEvent::PlayerNamed {
                    name: self.config.player_name.clone(),
                });
                self.enter(Phase::RivalIntro, ctx);
            }
            (Phase::RivalChoice, Expectation::MenuClosed) => {
                if self.rival_preset_row().is_some() {
                    self.rival_from_list = true;
                    self.enter(Phase::ConfirmRival, ctx);
                } else {
                    self.enter(Phase::RivalName, ctx);
                }
            }
            (Phase::ConfirmRival, Expectation::MenuClosed) if o.naming.is_none() => {
                ctx.events.push(GameEvent::RivalNamed {
                    name: self.config.rival_name.clone(),
                });
                self.outro_transition = false;
                self.enter(Phase::Outro, ctx);
            }
            (Phase::OpenStartMenu, _) => self.enter(Phase::CloseStartMenu, ctx),
            (Phase::CloseStartMenu, _) => {
                ctx.events.push(GameEvent::ControlConfirmed);
                self.enter(Phase::Done, ctx);
            }
            _ => {}
        }
    }
}

impl NewGameTask {
    fn attempts_exhausted(&self) -> Option<Decision> {
        (self.attempts >= MAX_ATTEMPTS).then(|| {
            Decision::Fail(format!(
                "{} failed {} times",
                self.phase.describe(),
                self.attempts
            ))
        })
    }

    /// One closed-loop typing step: delete extras, move to the next letter,
    /// press it, and finally confirm with Start then A on OK.
    fn type_name(&mut self, naming: &NamingObservation) -> Decision {
        let name: Vec<char> = self.name_for_phase().chars().collect();
        let typed = usize::from(naming.typed);
        if typed > name.len() {
            return Decision::Act(Action::new(
                "delete a character",
                vec![ControllerCommand::Press(Button::B)],
                Expectation::TypedCount(naming.typed - 1),
                60,
            ));
        }
        if typed == name.len() {
            if !self.confirming_name {
                return Decision::Act(Action::new(
                    "press Start (cursor to OK)",
                    vec![ControllerCommand::Press(Button::Start)],
                    Expectation::KeyboardFocus(KeyboardFocus::Buttons),
                    45,
                ));
            }
            let name: String = name.iter().collect();
            return Decision::Act(Action::new(
                format!("confirm name {name}"),
                vec![ControllerCommand::Press(Button::A)],
                Expectation::NamingClosed,
                120,
            ));
        }
        self.confirming_name = false;
        let letter = name[typed];
        let (column, row) = key_position(letter).expect("validated name");
        let target = KeyboardFocus::Key { column, row };
        match naming.focus {
            KeyboardFocus::Buttons => Decision::Act(Action::new(
                "move from the button column back to the letters",
                vec![ControllerCommand::Press(Button::Right)],
                Expectation::FocusOnAnyKey,
                45,
            )),
            focus if focus == target => Decision::Act(Action::new(
                format!("type {letter}"),
                vec![ControllerCommand::Press(Button::A)],
                Expectation::TypedCount(naming.typed + 1),
                60,
            )),
            KeyboardFocus::Key { column: c, row: r } => {
                let path = moves((c, r), (column, row));
                Decision::Act(Action::new(
                    format!("move cursor to {letter} ({} taps)", path.len()),
                    vec![taps(&path)],
                    Expectation::KeyboardFocus(target),
                    30 + 10 * path.len() as u64,
                ))
            }
        }
    }
}

/// Moves the menu cursor one row toward `row`, or confirms when there.
pub(crate) fn select(menu: &MenuObservation, row: u8, label: &str) -> Decision {
    if row >= menu.rows {
        return Decision::Fail(format!("{label}: menu has only {} rows", menu.rows));
    }
    if menu.cursor_row == row {
        return Decision::Act(Action::new(
            label.to_owned(),
            vec![ControllerCommand::Press(Button::A)],
            Expectation::MenuClosed,
            90,
        ));
    }
    let (button, next) = if menu.cursor_row < row {
        (Button::Down, menu.cursor_row + 1)
    } else {
        (Button::Up, menu.cursor_row - 1)
    };
    Decision::Act(Action::new(
        format!("{label}: cursor {button:?}"),
        vec![ControllerCommand::Press(button)],
        Expectation::MenuCursorAt(next),
        45,
    ))
}

/// Presses A when the game waits for it, otherwise waits.
pub(crate) fn advance_or_wait(dialogue: Option<&DialogueObservation>, waiting: &str) -> Decision {
    match dialogue {
        Some(d) if d.ready_for_a() => Decision::Act(Action::new(
            "advance text",
            vec![ControllerCommand::Press(Button::A)],
            Expectation::TextAdvanced {
                kind: d.kind,
                baseline: d.text_cells.clone(),
            },
            90,
        )),
        Some(_) => Decision::Wait("text is printing".into()),
        None => Decision::Wait(waiting.into()),
    }
}
