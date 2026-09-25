//! Visual localization: where on which map is the player?
//!
//! The camera keeps the player's tile at a fixed screen position, so each
//! candidate `(map, x, y)` corresponds to one 240×160 crop of the map's
//! render. The frame is compared with that crop on a sparse pixel grid,
//! skipping the player sprite and any UI windows (NPC sprites and animated
//! tiles just cost a few percent).

use pokebot_core::RgbImage;
use pokebot_state::{Direction, PlayerPose, PoseObservation, Region};

use crate::{MapData, World, BLOCK};

/// Screen position of the top-left of the player's tile.
pub const PLAYER_SCREEN_X: i32 = 112;
pub const PLAYER_SCREEN_Y: i32 = 72;
/// Player sprite (16×32 drawn over its tile and the one above), with margin.
pub const PLAYER_SPRITE: Region = Region {
    x: 106,
    y: 50,
    width: 28,
    height: 42,
};
/// Minimum share of matching samples (per mille) to accept a position.
pub const ACCEPT: u32 = 850;
const TOLERANCE: u8 = 24;
const SAMPLE_STEP: u32 = 4;
const COARSE_STEP: u32 = 8;

/// Pixels compared for one search, with excluded regions removed.
pub struct SampleGrid {
    points: Vec<(i32, i32)>,
}

impl SampleGrid {
    pub fn new(exclude: &[Region], step: u32) -> Self {
        let mut points = Vec::new();
        for y in (step / 2..160).step_by(step as usize) {
            for x in (step / 2..240).step_by(step as usize) {
                if !exclude.iter().any(|r| r.contains(x, y)) {
                    points.push((x as i32, y as i32));
                }
            }
        }
        Self { points }
    }
}

/// Brightest channel of a "void" pixel: the black outside a map's walls
/// (MtMoon_B1F is mostly void between its parts) and of screen fades and
/// battle wipes.
const VOID: u8 = 24;
/// Minimum share (per mille) of samples that aren't black on both the frame
/// and the render: black matching black says nothing about where we are.
const MIN_INFORMATIVE: u32 = 100;

/// Score (per mille) of the player standing at `(x, y)` on `map`, or `None`
/// once the score can no longer reach `floor`. Samples black on both the
/// frame and the render are left out: a dark fade or a battle wipe (black
/// sweeping over the view) otherwise matched MtMoon_B1F's void at 850–1000.
pub fn score(
    frame: &RgbImage,
    render: &RgbImage,
    map: &MapData,
    x: i32,
    y: i32,
    grid: &SampleGrid,
    floor: u32,
) -> Option<u32> {
    let ox = (x + map.pad) * BLOCK - PLAYER_SCREEN_X;
    let oy = (y + map.pad) * BLOCK - PLAYER_SCREEN_Y;
    let (rw, rh) = (render.width() as i32, render.height() as i32);
    let total = grid.points.len() as u32;
    if total == 0 {
        return None;
    }
    // Leaving out samples only lowers the score's ceiling: (n - m) / n is at
    // most (total - m) / total, so this bound still ends hopeless searches.
    let allowed_misses = total - (total * floor).div_ceil(1000);
    let mut misses = 0;
    let mut void = 0;
    let (frame_bytes, render_bytes) = (frame.as_bytes(), render.as_bytes());
    let is_void = |b: &[u8], i: usize| b[i..i + 3].iter().all(|&c| c <= VOID);
    for &(sx, sy) in &grid.points {
        let (rx, ry) = (ox + sx, oy + sy);
        let fi = ((sy as u32 * frame.width() + sx as u32) * 3) as usize;
        let inside = rx >= 0 && ry >= 0 && rx < rw && ry < rh;
        let matched = inside && {
            let ri = ((ry * rw + rx) * 3) as usize;
            if is_void(frame_bytes, fi) && is_void(render_bytes, ri) {
                void += 1;
                continue;
            }
            (0..3).all(|c| frame_bytes[fi + c].abs_diff(render_bytes[ri + c]) <= TOLERANCE)
        };
        if !matched {
            misses += 1;
            if misses > allowed_misses {
                return None;
            }
        }
    }
    let informative = total - void;
    if informative * 1000 < total * MIN_INFORMATIVE {
        return None;
    }
    let s = (informative - misses) * 1000 / informative;
    (s >= floor).then_some(s)
}

/// Share (per mille) of the pixels of map tile `tile` that look like the
/// map's render there, with the player standing at `pose`: high once an
/// object drawn over the tile (a cut tree, a smashed rock) is gone. `None`
/// when the tile is off screen.
pub fn tile_score(
    frame: &RgbImage,
    render: &RgbImage,
    map: &MapData,
    pose: &PlayerPose,
    tile: (i32, i32),
) -> Option<u32> {
    let sx0 = PLAYER_SCREEN_X + (tile.0 - pose.x) * BLOCK;
    let sy0 = PLAYER_SCREEN_Y + (tile.1 - pose.y) * BLOCK;
    let rx0 = (tile.0 + map.pad) * BLOCK;
    let ry0 = (tile.1 + map.pad) * BLOCK;
    let (fw, fh) = (frame.width() as i32, frame.height() as i32);
    let (rw, rh) = (render.width() as i32, render.height() as i32);
    if sx0 < 0 || sy0 < 0 || sx0 + BLOCK > fw || sy0 + BLOCK > fh {
        return None;
    }
    if rx0 < 0 || ry0 < 0 || rx0 + BLOCK > rw || ry0 + BLOCK > rh {
        return None;
    }
    let mut matched = 0u32;
    for dy in 0..BLOCK {
        for dx in 0..BLOCK {
            let f = frame.pixel((sx0 + dx) as u32, (sy0 + dy) as u32);
            let r = render.pixel((rx0 + dx) as u32, (ry0 + dy) as u32);
            if (0..3).all(|c| f[c].abs_diff(r[c]) <= TOLERANCE) {
                matched += 1;
            }
        }
    }
    Some(matched * 1000 / (BLOCK * BLOCK) as u32)
}

/// Whether the sampled frame is one flat colour (a white flash, a fade):
/// it would match any equally flat stretch of some map (a white frame scored
/// 1000 on NavelRock_Fork).
fn featureless(frame: &RgbImage, grid: &SampleGrid) -> bool {
    let mut points = grid.points.iter();
    let Some(&(x0, y0)) = points.next() else {
        return true;
    };
    let first = frame.pixel(x0 as u32, y0 as u32);
    points.all(|&(x, y)| {
        let p = frame.pixel(x as u32, y as u32);
        (0..3).all(|c| p[c].abs_diff(first[c]) <= TOLERANCE)
    })
}

pub struct Localizer<'w> {
    world: &'w World,
}

#[derive(Debug, Clone, Copy)]
struct Best {
    x: i32,
    y: i32,
    score: u32,
}

impl<'w> Localizer<'w> {
    pub fn new(world: &'w World) -> Self {
        Self { world }
    }

    /// Searches `map` for the player, limited to `radius` tiles around
    /// `near` if given. Ties go to the position closest to `near`, then the
    /// top-most, left-most one (deterministic).
    pub fn locate_in(
        &self,
        frame: &RgbImage,
        map: &MapData,
        near: Option<(i32, i32)>,
        radius: i32,
        exclude: &[Region],
    ) -> Option<PoseObservation> {
        let render = map.render().ok()?;
        let fine = SampleGrid::new(exclude, SAMPLE_STEP);
        let coarse = SampleGrid::new(exclude, COARSE_STEP);
        if featureless(frame, &coarse) {
            return None;
        }
        let (x_range, y_range) = match near {
            Some((nx, ny)) => (
                (nx - radius).max(0)..=(nx + radius).min(map.width - 1),
                (ny - radius).max(0)..=(ny + radius).min(map.height - 1),
            ),
            None => (0..=map.width - 1, 0..=map.height - 1),
        };
        let mut best: Option<Best> = None;
        for y in y_range {
            for x in x_range.clone() {
                // Cheap coarse pass first, then the fine score.
                if score(frame, render, map, x, y, &coarse, ACCEPT - 100).is_none() {
                    continue;
                }
                let Some(s) = score(frame, render, map, x, y, &fine, ACCEPT) else {
                    continue;
                };
                let distance =
                    |b: &Best| near.map_or(0, |(nx, ny)| (b.x - nx).abs() + (b.y - ny).abs());
                let candidate = Best { x, y, score: s };
                let better = match &best {
                    None => true,
                    Some(b) => s > b.score || (s == b.score && distance(&candidate) < distance(b)),
                };
                if better {
                    best = Some(candidate);
                }
            }
        }
        best.map(|b| PoseObservation {
            pose: PlayerPose {
                map: map.name.clone(),
                x: b.x,
                y: b.y,
            },
            score: b.score as u16,
        })
    }

    /// Tracks from a known pose: near it first, then the whole map, then the
    /// maps reachable through its warps and connections.
    pub fn locate_from(
        &self,
        frame: &RgbImage,
        hint: &PlayerPose,
        exclude: &[Region],
    ) -> Option<PoseObservation> {
        let map = self.world.map(&hint.map)?;
        if let Some(found) = self.locate_in(frame, map, Some((hint.x, hint.y)), 3, exclude) {
            return Some(found);
        }
        if let Some(found) = self.locate_in(frame, map, None, 0, exclude) {
            return Some(found);
        }
        // Maps that look the same (the two Viridian Forest gates, every
        // Pokémon Center) score the same: the one whose way in is nearest
        // the player wins.
        self.neighbours(map)
            .filter_map(|other| {
                let found = self.locate_in(frame, other, None, 0, exclude)?;
                Some((found, self.distance_to(map, hint, &other.name)))
            })
            .max_by_key(|(o, distance)| (o.score, std::cmp::Reverse(*distance)))
            .map(|(o, _)| o)
    }

    /// Tiles from the hint to the nearest warp or map edge leading to `other`.
    fn distance_to(&self, map: &MapData, hint: &PlayerPose, other: &str) -> i32 {
        let warps = map
            .warps
            .iter()
            .filter(|w| self.world.name_of(&w.dest_map) == Some(other))
            .map(|w| (w.x - hint.x).abs() + (w.y - hint.y).abs());
        let edges = map
            .connections
            .iter()
            .filter(|c| self.world.name_of(&c.map) == Some(other))
            .filter_map(|c| {
                Some(match c.direction()? {
                    Direction::Up => hint.y,
                    Direction::Down => map.height - 1 - hint.y,
                    Direction::Left => hint.x,
                    Direction::Right => map.width - 1 - hint.x,
                })
            });
        warps.chain(edges).min().unwrap_or(i32::MAX)
    }

    /// Searches every map, split over the machine's cores (for recovering
    /// when lost: ~150 ms on one core, ~10 ms on 32). The result doesn't
    /// depend on the split: ties go to the last map by name, as sequentially.
    pub fn locate_anywhere(&self, frame: &RgbImage, exclude: &[Region]) -> Option<PoseObservation> {
        let mut maps: Vec<&MapData> = self.world.maps().collect();
        maps.sort_by(|a, b| a.name.cmp(&b.name));
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let chunk = maps.len().div_ceil(threads).max(1);
        let found: Vec<Option<PoseObservation>> = std::thread::scope(|scope| {
            let workers: Vec<_> = maps
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        part.iter()
                            .map(|m| self.locate_in(frame, m, None, 0, exclude))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().expect("localizer worker panicked"))
                .collect()
        });
        found.into_iter().flatten().max_by_key(|o| o.score)
    }

    fn neighbours<'a>(&'a self, map: &'a MapData) -> impl Iterator<Item = &'w MapData> + 'a {
        let mut names: Vec<&str> = map
            .warps
            .iter()
            .filter_map(|w| self.world.name_of(&w.dest_map))
            .chain(
                map.connections
                    .iter()
                    .filter_map(|c| self.world.name_of(&c.map)),
            )
            .collect();
        names.sort();
        names.dedup();
        names.into_iter().filter_map(|n| self.world.map(n))
    }
}
