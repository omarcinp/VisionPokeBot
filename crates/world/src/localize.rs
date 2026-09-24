//! Visual localization: where on which map is the player?
//!
//! The camera keeps the player's tile at a fixed screen position, so each
//! candidate `(map, x, y)` corresponds to one 240×160 crop of the map's
//! render. The frame is compared with that crop on a sparse pixel grid,
//! skipping the player sprite and any UI windows (NPC sprites and animated
//! tiles just cost a few percent).

use pokebot_core::RgbImage;
use pokebot_state::{PlayerPose, PoseObservation, Region};

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
        self.neighbours(map)
            .filter_map(|other| self.locate_in(frame, other, None, 0, exclude))
            .max_by_key(|o| o.score)
    }

    /// Searches every map (slow; for recovering when lost).
    pub fn locate_anywhere(&self, frame: &RgbImage, exclude: &[Region]) -> Option<PoseObservation> {
        let mut maps: Vec<&MapData> = self.world.maps().collect();
        maps.sort_by(|a, b| a.name.cmp(&b.name));
        maps.into_iter()
            .filter_map(|m| self.locate_in(frame, m, None, 0, exclude))
            .max_by_key(|o| o.score)
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
