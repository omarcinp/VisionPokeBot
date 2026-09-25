//! Static FireRed world model (built offline from the pret/pokefirered
//! decompilation by `tools/world/build.sh`), plus visual localization and
//! pathfinding over it.

pub mod behavior;
pub mod dialogue;
pub mod events;
pub mod localize;
pub mod obstacles;
pub mod path;
pub mod places;
pub mod predicate;
pub mod route;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pokebot_core::{Error, Result, RgbImage};
use pokebot_state::Direction;
use serde::Deserialize;

pub use dialogue::Dialogue;
pub use events::Events;
pub use localize::Localizer;
pub use places::Places;

/// Pixels per map block (metatile).
pub const BLOCK: i32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    /// Non-zero = the block's collision bit forbids walking onto it.
    pub collision: u8,
    pub elevation: u8,
    pub behavior: u16,
    pub encounter: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Warp {
    pub x: i32,
    pub y: i32,
    /// Destination map id, e.g. `MAP_PALLET_TOWN`.
    pub dest_map: String,
    /// Index of the destination warp; -1 for dynamic warps (return to the
    /// previous location).
    pub dest_warp: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Connection {
    pub direction: String,
    pub offset: i32,
    pub map: String,
}

impl Connection {
    pub fn direction(&self) -> Option<Direction> {
        match self.direction.as_str() {
            "up" => Some(Direction::Up),
            "down" => Some(Direction::Down),
            "left" => Some(Direction::Left),
            "right" => Some(Direction::Right),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObjectEvent {
    pub local_id: u32,
    pub graphics: Option<String>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub movement: Option<String>,
    /// Wanderers stay within ±range of their spawn tile (0 for NPCs that
    /// never move). Missing in data built before the fields existed.
    #[serde(default)]
    pub range_x: i32,
    #[serde(default)]
    pub range_y: i32,
    pub script: Option<String>,
    pub flag: Option<String>,
    pub trainer_type: Option<String>,
}

impl ObjectEvent {
    /// The rectangle `(x0, y0, x1, y1)` (inclusive) the object may occupy;
    /// its spawn tile when it has no position.
    pub fn area(&self) -> (i32, i32, i32, i32) {
        let (x, y) = (self.x.unwrap_or(0), self.y.unwrap_or(0));
        (
            x - self.range_x,
            y - self.range_y,
            x + self.range_x,
            y + self.range_y,
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sign {
    pub x: i32,
    pub y: i32,
    pub script: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Trigger {
    pub x: i32,
    pub y: i32,
    pub var: Option<String>,
    pub value: Option<String>,
    pub script: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MapFile {
    id: String,
    name: String,
    width: i32,
    height: i32,
    pad: i32,
    map_type: Option<String>,
    #[serde(default)]
    requires_flash: bool,
    tiles: Vec<Vec<[u16; 4]>>,
    warps: Vec<Warp>,
    connections: Vec<Connection>,
    objects: Vec<ObjectEvent>,
    signs: Vec<Sign>,
    triggers: Vec<Trigger>,
}

#[derive(Debug)]
pub struct MapData {
    pub id: String,
    pub name: String,
    pub width: i32,
    pub height: i32,
    /// Blocks of context around the map in its render.
    pub pad: i32,
    pub map_type: Option<String>,
    /// Dark until Flash is used (the decomp's `requires_flash`).
    pub requires_flash: bool,
    tiles: Vec<Tile>,
    pub warps: Vec<Warp>,
    pub connections: Vec<Connection>,
    pub objects: Vec<ObjectEvent>,
    pub signs: Vec<Sign>,
    pub triggers: Vec<Trigger>,
    render_path: PathBuf,
    render: OnceLock<std::result::Result<RgbImage, String>>,
}

impl MapData {
    pub fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.width && y < self.height
    }

    pub fn tile(&self, x: i32, y: i32) -> Option<Tile> {
        self.in_bounds(x, y)
            .then(|| self.tiles[(y * self.width + x) as usize])
    }

    /// The map's background render, padded by `pad` blocks on each side.
    pub fn render(&self) -> Result<&RgbImage> {
        self.render
            .get_or_init(|| pokebot_video::png::load(&self.render_path).map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| Error::InvalidData(e.clone()))
    }

    pub fn is_outdoor(&self) -> bool {
        !matches!(
            self.map_type.as_deref(),
            Some("MAP_TYPE_INDOOR") | Some("MAP_TYPE_UNDERGROUND")
        )
    }
}

/// All maps, keyed by name (e.g. `PalletTown_PlayersHouse_2F`), plus the
/// compiled event data when the data dir has it.
pub struct World {
    maps: HashMap<String, MapData>,
    by_id: HashMap<String, String>,
    // One loader for the whole data dir keeps callers to a single path and
    // a single error site; the files are optional so data dirs built before
    // `compile_events.py` existed keep loading.
    events: Option<Events>,
    dialogue: Option<Dialogue>,
    places: Option<Places>,
}

impl World {
    /// Loads `dir` as written by `tools/world/extract_world.py`.
    pub fn load(dir: impl AsRef<Path>) -> Result<World> {
        let dir = dir.as_ref();
        let maps_dir = dir.join("maps");
        let entries = std::fs::read_dir(&maps_dir).map_err(|e| Error::io(&maps_dir, e))?;
        let mut maps = HashMap::new();
        let mut by_id = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
            let file: MapFile = serde_json::from_str(&text)
                .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
            let tiles = file
                .tiles
                .iter()
                .flatten()
                .map(|t| Tile {
                    collision: t[0] as u8,
                    elevation: t[1] as u8,
                    behavior: t[2],
                    encounter: t[3] as u8,
                })
                .collect();
            by_id.insert(file.id.clone(), file.name.clone());
            let render_path = dir.join("renders").join(format!("{}.png", file.name));
            maps.insert(
                file.name.clone(),
                MapData {
                    id: file.id,
                    name: file.name,
                    width: file.width,
                    height: file.height,
                    pad: file.pad,
                    map_type: file.map_type,
                    requires_flash: file.requires_flash,
                    tiles,
                    warps: file.warps,
                    connections: file.connections,
                    objects: file.objects,
                    signs: file.signs,
                    triggers: file.triggers,
                    render_path,
                    render: OnceLock::new(),
                },
            );
        }
        if maps.is_empty() {
            return Err(Error::InvalidData(format!(
                "no maps in {} (run tools/world/build.sh)",
                maps_dir.display()
            )));
        }
        Ok(World {
            maps,
            by_id,
            events: events::load_optional(&dir.join("events.json"))?,
            dialogue: events::load_optional(&dir.join("dialogue.json"))?,
            places: events::load_optional(&dir.join("places.json"))?,
        })
    }

    /// Compiled scripts (`events.json`), when the data dir has them.
    pub fn events(&self) -> Option<&Events> {
        self.events.as_ref()
    }

    pub fn dialogue(&self) -> Option<&Dialogue> {
        self.dialogue.as_ref()
    }

    pub fn places(&self) -> Option<&Places> {
        self.places.as_ref()
    }

    pub fn map(&self, name: &str) -> Option<&MapData> {
        self.maps.get(name)
    }

    /// Map name for a map id such as `MAP_PALLET_TOWN`.
    pub fn name_of(&self, id: &str) -> Option<&str> {
        self.by_id.get(id).map(String::as_str)
    }

    pub fn maps(&self) -> impl Iterator<Item = &MapData> {
        self.maps.values()
    }

    /// Crossings from `map` into its neighbour in `dir`: pairs of (tile on
    /// this map's edge, tile it leads to on the neighbour).
    pub fn crossings(&self, map: &MapData, dir: Direction) -> Vec<((i32, i32), (i32, i32))> {
        let Some(conn) = map.connections.iter().find(|c| c.direction() == Some(dir)) else {
            return Vec::new();
        };
        let Some(other) = self.name_of(&conn.map).and_then(|n| self.map(n)) else {
            return Vec::new();
        };
        let walkable = |m: &MapData, x: i32, y: i32| m.tile(x, y).is_some_and(|t| t.collision == 0);
        let mut out = Vec::new();
        match dir {
            Direction::Up | Direction::Down => {
                let (ay, by) = if dir == Direction::Up {
                    (0, other.height - 1)
                } else {
                    (map.height - 1, 0)
                };
                for x in 0..map.width {
                    let bx = x - conn.offset;
                    if walkable(map, x, ay) && walkable(other, bx, by) {
                        out.push(((x, ay), (bx, by)));
                    }
                }
            }
            Direction::Left | Direction::Right => {
                let (ax, bx) = if dir == Direction::Left {
                    (0, other.width - 1)
                } else {
                    (map.width - 1, 0)
                };
                for y in 0..map.height {
                    let by = y - conn.offset;
                    if walkable(map, ax, y) && walkable(other, bx, by) {
                        out.push(((ax, y), (bx, by)));
                    }
                }
            }
        }
        out
    }

    /// Where a warp lands: the destination warp's tile.
    pub fn warp_destination(&self, warp: &Warp) -> Option<(&MapData, i32, i32)> {
        let map = self.map(self.name_of(&warp.dest_map)?)?;
        let dest = map.warps.get(usize::try_from(warp.dest_warp).ok()?)?;
        Some((map, dest.x, dest.y))
    }
}
