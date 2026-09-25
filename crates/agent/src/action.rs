use pokebot_core::ControllerCommand;
use pokebot_state::{
    BattleMenu, DialogueKind, KeyboardFocus, Observation, PlayerPose, ScreenState,
};
use pokebot_vision::detect::dialogue::changed_cells;
use serde::Serialize;

use crate::motion::InputKind;

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
    /// Cancel the inputs early if something interrupts (dialogue, battle,
    /// menu): for long holds such as walking a straight run.
    pub interruptible: bool,
    /// What the timing model learns from this action once confirmed: the
    /// kind of input and how many units of it (tiles, presses) it holds.
    pub timing: Option<(InputKind, usize)>,
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
            interruptible: false,
            timing: None,
        }
    }

    pub fn interruptible(mut self) -> Self {
        self.interruptible = true;
        self
    }

    /// Feeds the timing model when confirmed: `units` units of `kind`.
    pub fn timed(mut self, kind: InputKind, units: usize) -> Self {
        self.timing = Some((kind, units));
        self
    }
}

/// What must be visible for an action to count as successful.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Expectation {
    PartyList,
    PartySelected(u8),
    PartyActions,
    SummaryPage(pokebot_state::SummaryPage),
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
    /// The player stands on this tile.
    PlayerAt(PlayerPose),
    /// The player was located on a map other than this one.
    LeftMap(String),
    DialogueOpen,
    /// The battle ▶ is on this cell of this menu.
    BattleMenuAt(BattleMenu),
    /// The KNOWN MOVES list's selection frame is on this row.
    MoveListAt(u8),
    /// The KNOWN MOVES list is gone.
    MoveListClosed,
    /// The bag is open on a pocket whose title reads like this one (the
    /// same pocket through [`crate::bag::pocket_from_title`], or the same
    /// text); an empty title accepts any pocket.
    BagPocket(String),
    /// The bag list's ▶ is on this visible row.
    BagCursorAt(u8),
    /// Neither the bag nor a menu is showing.
    BagClosed,
    /// The battle bag's USE/CANCEL prompt is open.
    BagPrompt,
    /// The battle bag's USE/CANCEL prompt ▶ is on this row.
    BagPromptAt(u8),
    /// The Pokédex entry page (after a first catch) is gone.
    PokedexPageClosed,
    /// The mart list's ▶ is on this visible row.
    ShopCursorAt(u8),
    /// The mart's quantity box reads this count (not just any count: an
    /// Up that wrapped past the maximum, or overshot, is not met).
    ShopQuantity(u16),
    /// The mart is left: no mart window, menu or dialogue is showing.
    ShopClosed,
    /// Nothing to verify; only wait for the inputs to finish.
    InputsDone,
}

impl Expectation {
    pub fn met(&self, observation: &Observation) -> bool {
        match self {
            Self::PartyList => observation.party_menu.as_ref().is_some_and(|m| !m.actions),
            Self::PartySelected(slot) => observation
                .party_menu
                .as_ref()
                .is_some_and(|m| !m.actions && m.selected == Some(*slot)),
            Self::PartyActions => observation.party_menu.as_ref().is_some_and(|m| m.actions),
            Self::SummaryPage(page) => observation
                .summary
                .as_ref()
                .is_some_and(|s| s.page == *page),
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
            Expectation::PlayerAt(at) => observation.player.as_ref().is_some_and(|p| p.pose == *at),
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
            Expectation::BagPocket(title) => observation.bag.as_ref().is_some_and(|b| {
                title.is_empty()
                    || b.pocket == *title
                    || crate::bag::pocket_from_title(&b.pocket)
                        .is_some_and(|p| crate::bag::pocket_from_title(title) == Some(p))
            }),
            Expectation::BagCursorAt(row) => observation
                .bag
                .as_ref()
                .is_some_and(|b| b.cursor == Some(*row)),
            Expectation::BagClosed => observation.bag.is_none() && observation.menu.is_none(),
            Expectation::BagPrompt => observation.bag.as_ref().is_some_and(|b| b.prompt.is_some()),
            Expectation::BagPromptAt(row) => observation
                .bag
                .as_ref()
                .and_then(|b| b.prompt.as_ref())
                .is_some_and(|(_, at)| at == row),
            Expectation::PokedexPageClosed => !observation.pokedex_page,
            Expectation::ShopCursorAt(row) => observation
                .shop
                .as_ref()
                .is_some_and(|s| s.cursor == Some(*row)),
            Expectation::ShopQuantity(count) => observation
                .shop
                .as_ref()
                .and_then(|s| s.quantity)
                .is_some_and(|(n, _)| n == *count),
            Expectation::ShopClosed => {
                observation.shop.is_none()
                    && observation.menu.is_none()
                    && observation.dialogue.is_none()
            }
            Expectation::InputsDone => true,
        }
    }
}
