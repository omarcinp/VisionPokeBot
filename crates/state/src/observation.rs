use serde::{Deserialize, Serialize};

use crate::ScreenState;

/// A perceived value and the detector that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed<T> {
    pub value: T,
    pub detector: String,
}

/// Cheap whole-frame measurements, computed for every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FrameMetrics {
    /// Mean luma, 0–255 (BT.601 integer weights).
    pub mean_luma: u8,
    /// Pixels that differ from the previous frame.
    pub changed_pixels: u32,
}

/// Axis-aligned rectangle in canonical 240×160 coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    pub fn intersects(&self, other: &Region) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    /// Grown by `margin` on every side (clamped at 0).
    pub fn inflate(&self, margin: u32) -> Region {
        let x = self.x.saturating_sub(margin);
        let y = self.y.saturating_sub(margin);
        Region::new(
            x,
            y,
            self.x + self.width + margin - x,
            self.y + self.height + margin - y,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DialogueKind {
    /// The standard message box at the bottom of the screen.
    MessageBox,
    /// Full-screen information page (new-game tutorial).
    InfoPage,
    /// The dark-blue battle message box.
    BattleText,
}

/// A text window and whether the game is waiting for a button press.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogueObservation {
    pub kind: DialogueKind,
    pub region: Region,
    /// The "continue" arrow is showing: the text is fully printed and the
    /// game waits for A.
    pub waiting_for_input: bool,
    pub arrow: Option<Region>,
    /// Consecutive frames the text has not changed. The last page of a
    /// conversation shows no arrow; unchanged text means it finished printing.
    pub stable_frames: u32,
    /// Coarse luma grid of the text area (8×8 cells, arrow cells zeroed),
    /// used to confirm that text advanced. Not serialized.
    #[serde(skip)]
    pub text_cells: Vec<u8>,
}

/// A list menu with the ▶ cursor (YES/NO, gender, name presets, Start menu).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuObservation {
    /// Interior of the menu window.
    pub window: Region,
    pub rows: u8,
    pub cursor_row: u8,
    /// Screen y of the ▶ (menus differ in row pitch; the Start menu uses 15 px).
    pub cursor_y: u32,
}

/// Which battle menu is open and where its ▶ is (column, row in the 2×2 grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BattleMenu {
    /// FIGHT / BAG / POKéMON / RUN
    Command { column: u8, row: u8 },
    /// The four move slots.
    Moves { column: u8, row: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BattleObservation {
    pub menu: Option<BattleMenu>,
    /// Our active Pokémon as read from its HUD (name may contain `?`).
    pub player_name: Option<String>,
    pub player_level: Option<u8>,
    /// Exact HP numbers under our bar.
    pub player_hp_numbers: Option<(u16, u16)>,
    pub opponent_name: Option<String>,
    pub opponent_level: Option<u8>,
    /// HP bar fill, per mille (None when the bar isn't visible).
    pub player_hp: Option<u16>,
    pub opponent_hp: Option<u16>,
}

/// Where the naming screen's cursor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyboardFocus {
    Key {
        column: u8,
        row: u8,
    },
    /// On the lower/BACK/OK button column.
    Buttons,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamingObservation {
    pub focus: KeyboardFocus,
    /// Characters entered so far.
    pub typed: u8,
}

/// What is visible in one frame. Observations never update persistent state
/// directly; the [`EventExtractor`](crate::EventExtractor) turns them into
/// events first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub frame_id: u64,
    pub screen: Observed<ScreenState>,
    pub metrics: FrameMetrics,
    pub dialogue: Option<DialogueObservation>,
    pub menu: Option<MenuObservation>,
    pub naming: Option<NamingObservation>,
    /// Player position, when the overworld could be matched to the map.
    pub player: Option<PoseObservation>,
    pub battle: Option<BattleObservation>,
}

impl DialogueObservation {
    /// Frames of unchanged text after which a page without an arrow counts as
    /// fully printed.
    pub const SETTLED_FRAMES: u32 = 45;

    /// The game is waiting for A: arrow shown, or text finished printing.
    pub fn ready_for_a(&self) -> bool {
        self.waiting_for_input || self.stable_frames >= Self::SETTLED_FRAMES
    }
}

impl Observation {
    /// An observation with no UI elements recognised.
    pub fn bare(frame_id: u64, screen: Observed<ScreenState>, metrics: FrameMetrics) -> Self {
        Self {
            frame_id,
            screen,
            metrics,
            dialogue: None,
            menu: None,
            naming: None,
            player: None,
            battle: None,
        }
    }
}

/// Compass direction on the tile grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    pub const ALL: [Direction; 4] = [
        Direction::Up,
        Direction::Down,
        Direction::Left,
        Direction::Right,
    ];

    pub fn delta(self) -> (i32, i32) {
        match self {
            Direction::Up => (0, -1),
            Direction::Down => (0, 1),
            Direction::Left => (-1, 0),
            Direction::Right => (1, 0),
        }
    }

    pub fn opposite(self) -> Direction {
        match self {
            Direction::Up => Direction::Down,
            Direction::Down => Direction::Up,
            Direction::Left => Direction::Right,
            Direction::Right => Direction::Left,
        }
    }
}

/// Where the player stands: map name (as in the decompilation, e.g.
/// `PalletTown_PlayersHouse_2F`) and tile coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlayerPose {
    pub map: String,
    pub x: i32,
    pub y: i32,
}

impl std::fmt::Display for PlayerPose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}, {})", self.map, self.x, self.y)
    }
}

/// Result of matching the frame against the world model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoseObservation {
    pub pose: PlayerPose,
    /// Share of sampled pixels that matched the map render (per mille).
    pub score: u16,
}
