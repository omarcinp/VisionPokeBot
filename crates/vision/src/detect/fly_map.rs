//! The region map: Fly's destination map and the Town Map (the same Kanto
//! drawing, `region_map.c`), and which fly spots carry the fly icon.
//!
//! The map is a fixed 30×20-tile background: sea in the north-east and
//! south, land in the north-west, grey map edges left and right. Kanto's
//! grid cell (x, y) has its top-left at (32 + 8x, 32 + 8y). On the Fly map
//! every visited town gets the fly icon sprite centred on its cell
//! (`CreateFlyIcons`, `MAPSECTYPE_VISITED`): a 16×16 sprite at
//! (8x + 36, 8y + 36) whose drawn ellipse covers columns 2..=13 and rows
//! 2..=12, outlined in brown with a yellow rim and 22 yellow pixels. The
//! Town Map never draws them. The cursor (drawn above the icons) hides the
//! rim's corners, the player icon is drawn below them.
//!
//! Colours are the game's `region_map.pal` and `misc_icon.pal` as rendered
//! by mGBA; the screen rule was measured on the repo's rendering of the
//! game's tilemap (`tools/world/extract_region_map.py`), not on a capture:
//! no available save has the Town Map or Fly.

use pokebot_core::RgbImage;
use pokebot_state::{FlyMapObservation, Region};

use super::{px, share_any};
use crate::color::{near, Rgb, TOLERANCE};

/// A fly destination and its cell on the Kanto grid (`region_map.json`
/// `sections`, first cell of the map's section; the Route 4 and Route 10
/// Pokémon Centers have their own sections).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlySpot {
    /// Map name as in `places.json` `fly_spots`.
    pub map: &'static str,
    pub cell: (u32, u32),
}

/// Kanto's fly spots in grid order (row-major), which is the order the game
/// creates their icons in.
pub const KANTO_FLY_SPOTS: [FlySpot; 13] = [
    FlySpot {
        map: "IndigoPlateau_Exterior",
        cell: (2, 3),
    },
    FlySpot {
        map: "Route4",
        cell: (8, 3),
    },
    FlySpot {
        map: "CeruleanCity",
        cell: (14, 3),
    },
    FlySpot {
        map: "Route10",
        cell: (18, 3),
    },
    FlySpot {
        map: "PewterCity",
        cell: (4, 4),
    },
    FlySpot {
        map: "CeladonCity",
        cell: (11, 6),
    },
    FlySpot {
        map: "SaffronCity",
        cell: (14, 6),
    },
    FlySpot {
        map: "LavenderTown",
        cell: (18, 6),
    },
    FlySpot {
        map: "ViridianCity",
        cell: (4, 8),
    },
    FlySpot {
        map: "VermilionCity",
        cell: (14, 9),
    },
    FlySpot {
        map: "PalletTown",
        cell: (4, 11),
    },
    FlySpot {
        map: "FuchsiaCity",
        cell: (12, 12),
    },
    FlySpot {
        map: "CinnabarIsland",
        cell: (4, 14),
    },
];

const GRID_ORIGIN: (u32, u32) = (32, 32);
const CELL: u32 = 8;

/// The fly icon's drawn area: the 16×16 sprite is centred on the cell, so
/// its ellipse (sprite column and row 2) starts 2 px before the cell's
/// top-left.
const ICON_INSET: u32 = 2;
const ICON_SIZE: (u32, u32) = (12, 11);
/// `misc_icon.pal` entry 8: the icon's rim.
const ICON_YELLOW: Rgb = [255, 255, 24];
/// Yellow pixels for an icon to count as drawn: 22 when unobstructed, about
/// 12 with the cursor's brackets over its corners.
const ICON_MIN_YELLOW: u32 = 8;

const SEA: [Rgb; 2] = [[164, 180, 255], [156, 213, 255]];
const LAND: [Rgb; 4] = [[57, 172, 8], [24, 131, 8], [115, 230, 32], [82, 205, 8]];
const EDGE: [Rgb; 3] = [[189, 189, 180], [205, 205, 189], [255, 255, 255]];

/// Tiles (21..=26, 2..=4): open sea north of Cerulean.
const NORTH_SEA: Region = Region {
    x: 168,
    y: 16,
    width: 48,
    height: 24,
};
/// Tiles (3..=18, 2..=4): the mountains north of Pewter and Mt. Moon.
const NORTH_LAND: Region = Region {
    x: 24,
    y: 16,
    width: 128,
    height: 24,
};
/// Tiles (0..=2, 2..=19): the map's left edge.
const WEST_EDGE: Region = Region {
    x: 0,
    y: 16,
    width: 24,
    height: 144,
};
/// Tiles (10..=26, 19): the sea along the bottom.
const SOUTH_SEA: Region = Region {
    x: 80,
    y: 152,
    width: 136,
    height: 8,
};

/// The region map with Kanto's fly spots sorted into lit (icon drawn) and
/// dark.
pub fn detect(image: &RgbImage) -> Option<FlyMapObservation> {
    detect_spots(image, &KANTO_FLY_SPOTS)
}

pub fn detect_spots(image: &RgbImage, spots: &[FlySpot]) -> Option<FlyMapObservation> {
    if !is_map(image) {
        return None;
    }
    let (mut lit, mut dark) = (Vec::new(), Vec::new());
    for spot in spots {
        if icon_yellow(image, spot.cell) >= ICON_MIN_YELLOW {
            lit.push(spot.map.to_owned());
        } else {
            dark.push(spot.map.to_owned());
        }
    }
    Some(FlyMapObservation {
        lit,
        dark,
        cursor: cursor_cell(image),
    })
}

/// The map cursor (`cursor.png`): a 16×16 sprite centred on its cell
/// (`spriteX = 8x + 36`), four white brackets 2 px thick that pulse
/// between two frames: frame 0 spans sprite pixels 2..=13, frame 1
/// 1..=14. The cell whose bracket pixels are (nearly) all white.
const CURSOR_FRAMES: [[&str; 16]; 2] = [
    [
        "................",
        "................",
        "..#####..#####..",
        "..#####..#####..",
        "..##........##..",
        "..##........##..",
        "..##........##..",
        "................",
        "................",
        "..##........##..",
        "..##........##..",
        "..##........##..",
        "..#####..#####..",
        "..#####..#####..",
        "................",
        "................",
    ],
    [
        "................",
        ".#####....#####.",
        ".#####....#####.",
        ".##..........##.",
        ".##..........##.",
        ".##..........##.",
        "................",
        "................",
        "................",
        "................",
        ".##..........##.",
        ".##..........##.",
        ".##..........##.",
        ".#####....#####.",
        ".#####....#####.",
        "................",
    ],
];
const CURSOR_WHITE: Rgb = [255, 255, 255];
/// Kanto's grid: 22 columns, 15 rows of cells hold map sections.
const GRID_CELLS: (u32, u32) = (22, 15);

pub fn cursor_cell(image: &RgbImage) -> Option<(u32, u32)> {
    let mut best: Option<(u32, (u32, u32))> = None;
    for cy in 0..GRID_CELLS.1 {
        for cx in 0..GRID_CELLS.0 {
            let x0 = GRID_ORIGIN.0 + CELL * cx - 4;
            let y0 = GRID_ORIGIN.1 + CELL * cy - 4;
            for frame in &CURSOR_FRAMES {
                let (mut hit, mut total) = (0u32, 0u32);
                for (r, row) in frame.iter().enumerate() {
                    for (c, ch) in row.chars().enumerate() {
                        if ch != '#' {
                            continue;
                        }
                        let (x, y) = (x0 + c as u32, y0 + r as u32);
                        total += 1;
                        if x < image.width()
                            && y < image.height()
                            && near(px(image, x, y), CURSOR_WHITE, 8)
                        {
                            hit += 1;
                        }
                    }
                }
                // Other sprites (the player icon) may cover a bracket.
                let score = hit * 1000 / total.max(1);
                if score >= 850 && best.is_none_or(|(b, _)| score > b) {
                    best = Some((score, (cx, cy)));
                }
            }
        }
    }
    best.map(|(_, cell)| cell)
}

/// The fly spot drawn on `cell`, if any.
pub fn spot_at(cell: (u32, u32)) -> Option<&'static FlySpot> {
    KANTO_FLY_SPOTS.iter().find(|s| s.cell == cell)
}

/// The grid cell of `map`'s fly spot.
pub fn spot_cell(map: &str) -> Option<(u32, u32)> {
    KANTO_FLY_SPOTS
        .iter()
        .find(|s| s.map == map)
        .map(|s| s.cell)
}

/// Sea, land and edge probes where the Kanto map is uniform (all 1000‰ on
/// the rendering).
fn is_map(image: &RgbImage) -> bool {
    share_any(image, NORTH_SEA, &SEA, 2) >= 800
        && share_any(image, SOUTH_SEA, &SEA, 2) >= 800
        && share_any(image, NORTH_LAND, &LAND, 2) >= 800
        && share_any(image, WEST_EDGE, &EDGE, 2) >= 600
}

/// Yellow pixels inside the fly icon's area over `cell`.
fn icon_yellow(image: &RgbImage, cell: (u32, u32)) -> u32 {
    let x0 = GRID_ORIGIN.0 + CELL * cell.0 - ICON_INSET;
    let y0 = GRID_ORIGIN.1 + CELL * cell.1 - ICON_INSET;
    let mut count = 0;
    for y in y0..y0 + ICON_SIZE.1 {
        for x in x0..x0 + ICON_SIZE.0 {
            if x < image.width()
                && y < image.height()
                && near(px(image, x, y), ICON_YELLOW, TOLERANCE)
            {
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::detect::testing::fill;

    fn root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn fixture(name: &str) -> Option<RgbImage> {
        pokebot_video::png::load(root().join("captures/fixtures").join(name)).ok()
    }

    /// The game's rendering of the Kanto map (from `extract_region_map.py`).
    fn region_map() -> Option<RgbImage> {
        pokebot_video::png::load(root().join("data/world/region_map.png")).ok()
    }

    /// The fly icon's ellipse (`fly_icon.png` rows 2..=12, columns 2..=13):
    /// `a` brown outline, `9` dark yellow, `8` yellow, `4`/`3` greys, `1`
    /// white.
    const ICON: [&str; 11] = [
        "aaaaaaaaaaaa",
        "a9888884449a",
        "a8884443114a",
        "a8843111134a",
        "a8841113314a",
        "a8841111114a",
        "a8431111134a",
        "a8411113314a",
        "a4111111148a",
        "a4111114488a",
        "a9444448889a",
    ];

    fn draw_icon(image: &mut RgbImage, cell: (u32, u32)) {
        let x0 = GRID_ORIGIN.0 + CELL * cell.0 - ICON_INSET;
        let y0 = GRID_ORIGIN.1 + CELL * cell.1 - ICON_INSET;
        for (r, row) in ICON.iter().enumerate() {
            for (c, ch) in row.chars().enumerate() {
                let colour = match ch {
                    'a' => [90, 74, 49],
                    '9' => [189, 156, 65],
                    '8' => ICON_YELLOW,
                    '4' => [98, 98, 98],
                    '3' => [148, 148, 148],
                    _ => [255, 255, 255],
                };
                image.put_pixel(x0 + c as u32, y0 + r as u32, colour);
            }
        }
    }

    /// The cursor sprite (`cursor.png` frame 0, 16×16 centred on the cell):
    /// four white brackets, lines 2 px thick and 5 px long.
    fn draw_cursor(image: &mut RgbImage, cell: (u32, u32)) {
        let x0 = GRID_ORIGIN.0 + CELL * cell.0 - 4;
        let y0 = GRID_ORIGIN.1 + CELL * cell.1 - 4;
        let white = [255, 255, 255];
        for (bx, by) in [(2, 2), (9, 2), (2, 9), (9, 9)] {
            let (lx, ly) = (if bx == 2 { 2 } else { 12 }, if by == 2 { 2 } else { 12 });
            fill(image, x0 + bx, y0 + ly, 5, 2, white);
            fill(image, x0 + lx, y0 + by, 2, 5, white);
        }
    }

    #[test]
    fn the_fly_spot_table_matches_the_world_data() {
        let Ok(places) = std::fs::read_to_string(root().join("data/world/places.json")) else {
            return;
        };
        let Ok(grid) = std::fs::read_to_string(root().join("data/world/region_map.json")) else {
            return;
        };
        let places: serde_json::Value = serde_json::from_str(&places).unwrap();
        let grid: serde_json::Value = serde_json::from_str(&grid).unwrap();
        assert_eq!(grid["grid"]["x"], 32);
        assert_eq!(grid["grid"]["y"], 32);
        assert_eq!(grid["grid"]["cell"], 8);
        let mut expected = Vec::new();
        for spot in places["fly_spots"].as_array().unwrap() {
            let map = spot["map"].as_str().unwrap();
            let section = grid["maps"][map]["section"].as_str().unwrap();
            // The Pokémon Centers on Routes 4 and 10 have their own sections.
            let section = match map {
                "Route4" => "MAPSEC_ROUTE_4_POKECENTER",
                "Route10" => "MAPSEC_ROUTE_10_POKECENTER",
                _ => section,
            };
            // Sevii Islands are on other region maps.
            let Some(cells) = grid["sections"][section].as_array() else {
                continue;
            };
            let cell = cells[0].as_array().unwrap();
            expected.push((
                map.to_owned(),
                (
                    cell[0].as_u64().unwrap() as u32,
                    cell[1].as_u64().unwrap() as u32,
                ),
            ));
        }
        let mut table: Vec<(String, (u32, u32))> = KANTO_FLY_SPOTS
            .iter()
            .map(|s| (s.map.to_owned(), s.cell))
            .collect();
        table.sort();
        expected.sort();
        assert_eq!(table, expected);
        // Row-major order in the table itself.
        let order: Vec<(u32, u32)> = KANTO_FLY_SPOTS
            .iter()
            .map(|s| (s.cell.1, s.cell.0))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn the_town_map_has_no_lit_spot() {
        let Some(image) = region_map() else {
            return;
        };
        let map = detect(&image).unwrap();
        assert!(map.lit.is_empty());
        assert_eq!(map.dark.len(), KANTO_FLY_SPOTS.len());
    }

    #[test]
    fn drawn_icons_light_their_spots_even_under_the_cursor() {
        let Some(mut image) = region_map() else {
            return;
        };
        draw_icon(&mut image, (4, 4));
        draw_icon(&mut image, (14, 3));
        draw_icon(&mut image, (4, 11));
        draw_cursor(&mut image, (4, 4));
        let map = detect(&image).unwrap();
        assert_eq!(map.lit, vec!["CeruleanCity", "PewterCity", "PalletTown"]);
        assert_eq!(map.dark.len(), KANTO_FLY_SPOTS.len() - 3);
        assert!(
            icon_yellow(&image, (4, 4)) < 22,
            "the cursor hides rim corners"
        );
    }

    /// `synthetic-fly-map.png`: the rendering with the game's sprites blitted
    /// at their positions: player icon and cursor in Pewter, fly icons over
    /// Pewter, Viridian and Pallet.
    #[test]
    fn the_synthetic_fly_map_is_read() {
        let Some(image) = fixture("synthetic-fly-map.png") else {
            return;
        };
        let map = detect(&image).unwrap();
        assert_eq!(map.lit, vec!["PewterCity", "ViridianCity", "PalletTown"]);
        assert_eq!(map.cursor, Some((4, 4)));
    }

    /// Both frames of the pulsing cursor are found on their cell; none on
    /// the bare map.
    #[test]
    fn the_cursor_cell_is_found_in_both_frames() {
        let Some(image) = region_map() else {
            return;
        };
        assert_eq!(detect(&image).unwrap().cursor, None);
        for (frame, cell) in [(0, (14, 3)), (1, (12, 12)), (0, (4, 14))] {
            let mut drawn = image.clone();
            let x0 = GRID_ORIGIN.0 + CELL * cell.0 - 4;
            let y0 = GRID_ORIGIN.1 + CELL * cell.1 - 4;
            for (r, row) in CURSOR_FRAMES[frame].iter().enumerate() {
                for (c, ch) in row.chars().enumerate() {
                    if ch == '#' {
                        drawn.put_pixel(x0 + c as u32, y0 + r as u32, CURSOR_WHITE);
                    }
                }
            }
            assert_eq!(detect(&drawn).unwrap().cursor, Some(cell), "frame {frame}");
        }
        assert_eq!(spot_cell("CeruleanCity"), Some((14, 3)));
        assert_eq!(spot_at((12, 12)).map(|s| s.map), Some("FuchsiaCity"));
    }

    #[test]
    fn other_screens_are_not_the_map() {
        for name in [
            "emu-trainer-card.png",
            "emu-pokedex.png",
            "emu-bag-key-items.png",
            "start-menu.png",
            "mart-list.png",
            "mtmoon-1f.png",
            "pokedex-page.png",
        ] {
            let Some(image) = fixture(name) else {
                continue;
            };
            assert!(detect(&image).is_none(), "{name}");
        }
        assert!(detect(&RgbImage::filled(240, 160, SEA[0])).is_none());
    }
}
