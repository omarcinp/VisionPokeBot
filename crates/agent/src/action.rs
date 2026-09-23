use pokebot_core::ControllerCommand;
use pokebot_state::{
    BattleMenu, DialogueKind, KeyboardFocus, Observation, PlayerPose, ScreenState,
};
use pokebot_vision::detect::dialogue::changed_cells;
use serde::Serialize;

/// Text cells that must change for text to count as advanced.
const TEXT_CHANGE_CELLS: usize = 3;

/// One semantic step: the inputs to send and the observable effect that
/// proves they worked.
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub label: String,
    pub commands: Vec<ControllerCommand>,
    pub expect: Expectation,
    /// Frames to wait for the expectation after the inputs finish.
    pub timeout_frames: u64,
}

impl Action {
    pub fn new(
        label: impl Into<String>,
        commands: Vec<ControllerCommand>,
        expect: Expectation,
        timeout_frames: u64,
    ) -> Self {
        Self {
            label: label.into(),
            commands,
            expect,
            timeout_frames,
        }
    }
}

/// What must be visible for an action to count as successful.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Expectation {
    /// The dialogue's text changed (new page) or the dialogue closed.
    TextAdvanced {
        kind: DialogueKind,
        #[serde(skip)]
        baseline: Vec<u8>,
    },
    ScreenIs(ScreenState),
    ScreenIsNot(ScreenState),
    MenuCursorAt(u8),
    MenuOpen,
    MenuClosed,
    /// The menu ▶ moved above (`up`) or below its previous screen row.
    MenuCursorMoved {
        from_y: u32,
        up: bool,
    },
    /// A question with a YES/NO menu is showing.
    Question,
    KeyboardFocus(KeyboardFocus),
    /// The naming cursor is on any letter key (not the button column).
    FocusOnAnyKey,
    TypedCount(u8),
    NamingClosed,
    /// The player was located somewhere other than `from`.
    PlayerMovedFrom(PlayerPose),
    /// The player was located on a map other than this one.
    LeftMap(String),
    DialogueOpen,
    /// The battle ▶ is on this cell of this menu.
    BattleMenuAt(BattleMenu),
    /// The KNOWN MOVES list's selection frame is on this row.
    MoveListAt(u8),
    /// The KNOWN MOVES list is gone.
    MoveListClosed,
    /// Nothing to verify; only wait for the inputs to finish.
    InputsDone,
}

impl Expectation {
    pub fn met(&self, observation: &Observation) -> bool {
        match self {
            Expectation::TextAdvanced { kind, baseline } => match &observation.dialogue {
                Some(d) if d.kind == *kind => {
                    changed_cells(baseline, &d.text_cells) >= TEXT_CHANGE_CELLS
                }
                _ => true,
            },
            Expectation::ScreenIs(screen) => observation.screen.value == *screen,
            Expectation::ScreenIsNot(screen) => observation.screen.value != *screen,
            Expectation::MenuCursorAt(row) => {
                observation.menu.is_some_and(|m| m.cursor_row == *row)
            }
            Expectation::MenuOpen => observation.menu.is_some(),
            Expectation::MenuClosed => observation.menu.is_none(),
            Expectation::MenuCursorMoved { from_y, up } => observation.menu.is_some_and(|m| {
                if *up {
                    m.cursor_y < *from_y
                } else {
                    m.cursor_y > *from_y
                }
            }),
            Expectation::Question => observation.dialogue.is_some() && observation.menu.is_some(),
            Expectation::KeyboardFocus(focus) => {
                observation.naming.is_some_and(|n| n.focus == *focus)
            }
            Expectation::FocusOnAnyKey => observation
                .naming
                .is_some_and(|n| matches!(n.focus, KeyboardFocus::Key { .. })),
            Expectation::TypedCount(count) => observation.naming.is_some_and(|n| n.typed == *count),
            Expectation::NamingClosed => {
                observation.naming.is_none() && observation.screen.value != ScreenState::Naming
            }
            Expectation::PlayerMovedFrom(from) => {
                observation.player.as_ref().is_some_and(|p| p.pose != *from)
            }
            Expectation::LeftMap(map) => observation
                .player
                .as_ref()
                .is_some_and(|p| p.pose.map != *map),
            Expectation::DialogueOpen => observation.dialogue.is_some(),
            Expectation::BattleMenuAt(menu) => {
                observation.battle.as_ref().and_then(|b| b.menu) == Some(*menu)
            }
            Expectation::MoveListAt(row) => observation
                .move_list
                .as_ref()
                .is_some_and(|l| l.selected == Some(*row)),
            Expectation::MoveListClosed => observation.move_list.is_none(),
            Expectation::InputsDone => true,
        }
    }
}
