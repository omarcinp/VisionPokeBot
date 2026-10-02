//! Metatile behaviours (from `include/constants/metatile_behaviors.h`) that
//! matter for walking.

use pokebot_state::Direction;

pub const TALL_GRASS: u16 = 0x02;
pub const JUMP_EAST: u16 = 0x38;
pub const JUMP_WEST: u16 = 0x39;
pub const JUMP_NORTH: u16 = 0x3A;
pub const JUMP_SOUTH: u16 = 0x3B;
pub const EAST_ARROW_WARP: u16 = 0x62;
pub const WEST_ARROW_WARP: u16 = 0x63;
pub const NORTH_ARROW_WARP: u16 = 0x64;
pub const SOUTH_ARROW_WARP: u16 = 0x65;
pub const WARP_DOOR: u16 = 0x69;
pub const COUNTER: u16 = 0x80;
/// The Pokémon Center PC (`MB_PC`): A facing it runs `EventScript_PC`.
pub const PC: u16 = 0x83;
/// A floor switch a Strength boulder presses (`MB_STRENGTH_BUTTON`): the
/// game runs the tile's coord event when a boulder lands on it.
pub const STRENGTH_BUTTON: u16 = 0x20;

/// Whether a warp on a tile of this behaviour ever fires (`field_control_
/// avatar.c`): walked onto (cave door, ladder, hole, warp pad, escalator,
/// union room; `IsWarpMetatileBehavior`), pushed into from below (a door,
/// `TryDoorWarp`), or stood on and pushed (arrow mats, stairs). A warp on
/// any other tile is only a landing spot (the Pokémon Mansion's and
/// Seafoam's holes land on plain floor) or a door a script draws first.
pub fn warp_fires(b: u16) -> bool {
    matches!(b, 0x60 | 0x61 | 0x66..=0x6B | 0x71)
        || arrow_warp(b).is_some()
        || stair_warp(b).is_some()
}

/// Water of any kind (needs Surf).
pub fn is_water(b: u16) -> bool {
    matches!(b, 0x10..=0x15 | 0x19..=0x1B)
}

/// Tiles drawn as animated water: Surf's water and the currents
/// (`MB_EASTWARD_CURRENT`..`MB_SOUTHWARD_CURRENT`, Seafoam's lower floors),
/// whose frames the render doesn't match.
pub fn animates_as_water(b: u16) -> bool {
    is_water(b) || matches!(b, 0x50..=0x53)
}

/// Ledge on this tile, and the only direction it can be jumped.
pub fn ledge(b: u16) -> Option<Direction> {
    match b {
        JUMP_EAST => Some(Direction::Right),
        JUMP_WEST => Some(Direction::Left),
        JUMP_NORTH => Some(Direction::Up),
        JUMP_SOUTH => Some(Direction::Down),
        _ => None,
    }
}

/// Arrow warps (house exits): stand on the tile and push this direction.
pub fn arrow_warp(b: u16) -> Option<Direction> {
    match b {
        EAST_ARROW_WARP => Some(Direction::Right),
        WEST_ARROW_WARP => Some(Direction::Left),
        NORTH_ARROW_WARP => Some(Direction::Up),
        SOUTH_ARROW_WARP => Some(Direction::Down),
        _ => None,
    }
}

/// Stair warps (0x6C–0x6F): stand on them and push sideways, toward the
/// side the stairs lead (like arrow warps).
pub fn stair_warp(b: u16) -> Option<Direction> {
    match b {
        0x6C | 0x6E => Some(Direction::Right),
        0x6D | 0x6F => Some(Direction::Left),
        _ => None,
    }
}

/// Whether a tile's behaviour blocks crossing its `side` edge (one-way
/// fences, desks...). Moving from A to B in `dir` is blocked if A blocks its
/// `dir` side or B blocks its opposite side (as the game checks).
pub fn blocks_edge(b: u16, side: Direction) -> bool {
    use Direction::*;
    let sides: &[Direction] = match b {
        0x30 => &[Right],
        0x31 => &[Left],
        0x32 => &[Up],
        0x33 => &[Down],
        0x34 => &[Up, Right],
        0x35 => &[Up, Left],
        0x36 => &[Down, Right],
        0x37 => &[Down, Left],
        _ => &[],
    };
    sides.contains(&side)
}
