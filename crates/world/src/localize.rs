//! Visual localization: where on which map is the player?
//!
//! The camera keeps the player's tile at a fixed screen position, so each
//! candidate `(map, x, y)` corresponds to one 240×160 crop of the map's
//! render (during a step the view scrolls between two such crops, pixel
//! by pixel). The frame is compared with that crop on a sparse pixel grid,
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
const COARSEST_STEP: u32 = 16;
/// A tracking match this good ends the search early (standing or walking
/// frames of the right pose score 960–1000, JPEG-softened Switch ones too).
const CLEAR: u32 = 950;

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
    let (cx, cy) = camera(map, x * BLOCK, y * BLOCK);
    score_at(frame, render, cx, cy, grid, floor)
}

/// Render pixel shown at the screen's top-left when the player's sprite
/// stands at map pixel `(px, py)` (its tile's top-left while standing).
fn camera(map: &MapData, px: i32, py: i32) -> (i32, i32) {
    (
        px + map.pad * BLOCK - PLAYER_SCREEN_X,
        py + map.pad * BLOCK - PLAYER_SCREEN_Y,
    )
}

/// [`score`] with the view's top-left at render pixel `(ox, oy)`.
fn score_at(
    frame: &RgbImage,
    render: &RgbImage,
    ox: i32,
    oy: i32,
    grid: &SampleGrid,
    floor: u32,
) -> Option<u32> {
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

/// The tile reported for the player's sprite at map pixel `(px, py)`.
/// Between two tiles (a step in progress) it is the one nearer the
/// search's `target` (the last located pose: the tile the step started
/// from when walking on from it), else the one the sprite is mostly over.
/// So a pose still changes only once a step has played out, as the
/// executor's timing model expects of the first effect of a walk.
fn step_tile(px: i32, py: i32, target: Option<(i32, i32)>) -> (i32, i32) {
    let from = (px.div_euclid(BLOCK), py.div_euclid(BLOCK));
    let (dx, dy) = (px.rem_euclid(BLOCK), py.rem_euclid(BLOCK));
    if dx == 0 && dy == 0 {
        return from;
    }
    let to = (from.0 + i32::from(dx > 0), from.1 + i32::from(dy > 0));
    let nearer_pixels = if dx + dy <= BLOCK / 2 { from } else { to };
    let Some((tx, ty)) = target else {
        return nearer_pixels;
    };
    let (tx, ty) = (tx.div_euclid(BLOCK), ty.div_euclid(BLOCK));
    let distance = |t: (i32, i32)| (t.0 - tx).abs() + (t.1 - ty).abs();
    match distance(from).cmp(&distance(to)) {
        std::cmp::Ordering::Less => from,
        std::cmp::Ordering::Greater => to,
        std::cmp::Ordering::Equal => nearer_pixels,
    }
}

/// Search windows a tracking search moves through at most (see
/// `Localizer::climb`).
const MAX_WINDOWS: usize = 4;

/// The fine and coarse sample grids of one search.
struct Grids {
    fine: SampleGrid,
    coarse: SampleGrid,
    /// A first, sparser pass: most candidates of a tracking search are
    /// wrong, and this rejects them after a few dozen samples.
    coarsest: SampleGrid,
}

pub struct Localizer<'w> {
    world: &'w World,
}

#[derive(Debug, Clone, Copy)]
struct Best<'m> {
    /// Where the player is (a connected map's tile past an edge).
    map: &'m MapData,
    x: i32,
    y: i32,
    /// The tile the view is at or just past (towards +x/+y) on it.
    grid: (i32, i32),
    score: u32,
    /// Pixels from the hint.
    distance: i32,
}

impl<'w> Localizer<'w> {
    pub fn new(world: &'w World) -> Self {
        Self { world }
    }

    /// Searches `map` for the player, limited to `radius` tiles around
    /// `near` if given. Ties go to the position closest to `near`, then the
    /// top-most, left-most one (deterministic).
    ///
    /// Around `near` the search also covers the views of a step in
    /// progress (the camera scrolls 1–2 px per frame, so a walking player's
    /// frames sit between tiles: Switch Route 3 and Pokémon Center frames
    /// were 3–5 px off the tile grid and scored 350–570 on it) and the
    /// render's padding, where connected maps are drawn (walking across a
    /// map edge). A pose between tiles is reported on the one nearer
    /// `near` (see [`step_tile`]); one past the map's edge on the
    /// connected map's tile.
    pub fn locate_in(
        &self,
        frame: &RgbImage,
        map: &MapData,
        near: Option<(i32, i32)>,
        radius: i32,
        exclude: &[Region],
    ) -> Option<PoseObservation> {
        let render = map.render().ok()?;
        let grids = Grids {
            fine: SampleGrid::new(exclude, SAMPLE_STEP),
            coarse: SampleGrid::new(exclude, COARSE_STEP),
            coarsest: SampleGrid::new(exclude, COARSEST_STEP),
        };
        if featureless(frame, &grids.coarse) {
            return None;
        }
        let best = match near {
            None => {
                let tiles = (0..map.height).flat_map(|y| (0..map.width).map(move |x| (x, y)));
                let candidates = tiles.map(|(x, y)| (x * BLOCK, y * BLOCK)).collect();
                self.best(frame, map, render, &grids, candidates, None)
            }
            Some(near) => self.climb(frame, map, render, &grids, near, radius),
        };
        best.map(|b| PoseObservation {
            pose: PlayerPose {
                map: b.map.name.clone(),
                x: b.x,
                y: b.y,
            },
            score: b.score as u16,
        })
    }

    /// The best pose in the window of `radius` tiles around `near`, moved
    /// on while the best sits on the window's edge and the next window
    /// scores higher: along a repeating wall the pose a few tiles short of
    /// the true one still scores above [`ACCEPT`] (Mt. Moon B2F: 899 three
    /// tiles off, 997 at the true tile just outside the window).
    ///
    /// Views mid-step multiply the candidates by 31, so the cheap searches
    /// go first and a clear match ends the search: the tiles alone (the
    /// player standing), then the steps next to the hint (walking on from
    /// the last located frame).
    fn climb<'m>(
        &'m self,
        frame: &RgbImage,
        map: &'m MapData,
        render: &RgbImage,
        grids: &Grids,
        near: (i32, i32),
        radius: i32,
    ) -> Option<Best<'m>> {
        let target = (near.0 * BLOCK, near.1 * BLOCK);
        // The later windows cover these, so a weaker match found here is
        // judged again among all of them.
        for (r, steps) in [(radius, false), (radius.min(1), true)] {
            let window = self.window(map, near, r, steps);
            if let Some(found) = self.best(frame, map, render, grids, window, Some(target)) {
                if found.score >= CLEAR {
                    return Some(found);
                }
            }
        }
        let mut best: Option<Best> = None;
        let mut centre = near;
        for _ in 0..MAX_WINDOWS {
            let window = self.window(map, centre, radius, true);
            let Some(found) = self.best(frame, map, render, grids, window, Some(target)) else {
                break;
            };
            if best.is_some_and(|b| found.score <= b.score) {
                break;
            }
            best = Some(found);
            let edge = (found.grid.0 - centre.0).abs() >= radius
                || (found.grid.1 - centre.1).abs() >= radius;
            if !edge {
                break;
            }
            centre = found.grid;
        }
        best
    }

    /// Views around `centre`: every tile within `radius` (into the render's
    /// padding) and, with `steps`, every step in progress between two of
    /// them, as the map pixel the player's sprite is at.
    fn window(
        &self,
        map: &MapData,
        centre: (i32, i32),
        radius: i32,
        steps: bool,
    ) -> Vec<(i32, i32)> {
        let (lo, hi) = (-map.pad, map.pad - 1);
        let xs = (centre.0 - radius).max(lo)..=(centre.0 + radius).min(map.width + hi);
        let ys = (centre.1 - radius).max(lo)..=(centre.1 + radius).min(map.height + hi);
        let mut out = Vec::new();
        for y in ys {
            for x in xs.clone() {
                out.push((x * BLOCK, y * BLOCK));
                for d in 1..if steps { BLOCK } else { 1 } {
                    out.push((x * BLOCK + d, y * BLOCK));
                    out.push((x * BLOCK, y * BLOCK + d));
                }
            }
        }
        out
    }

    /// The best scoring of `candidates` (sprite positions in map pixels)
    /// at or above [`ACCEPT`]. Ties go to the one closest to `target`,
    /// then the top-most, left-most one. A tie between tiles apart is no
    /// answer when nothing vouches for one of them: the view repeats (a
    /// long uniform corridor scored 1000 at every tile of Mt. Moon B2F's)
    /// and a guess among them is a false localization. With a target a
    /// clear match keeps the tile nearest it (tracking along the corridor),
    /// a weak one doesn't (B2F, running, audited 40 frames apart: 850 at
    /// three tiles in a row, the true pose ten tiles on).
    fn best<'m>(
        &'m self,
        frame: &RgbImage,
        map: &'m MapData,
        render: &RgbImage,
        grids: &Grids,
        mut candidates: Vec<(i32, i32)>,
        target: Option<(i32, i32)>,
    ) -> Option<Best<'m>> {
        candidates.sort_unstable_by_key(|&(px, py)| (py, px));
        let mut best: Option<Best> = None;
        // Tiles (on the grid) that reach the best score: min and max corner.
        let mut span = ((0, 0), (0, 0));
        for (px, py) in candidates {
            let tile = step_tile(px, py, target);
            let Some((m, x, y)) = self.resolve(map, tile.0, tile.1) else {
                continue;
            };
            let (cx, cy) = camera(map, px, py);
            // Cheap coarse passes first, then the fine score.
            if score_at(frame, render, cx, cy, &grids.coarsest, ACCEPT - 200).is_none()
                || score_at(frame, render, cx, cy, &grids.coarse, ACCEPT - 100).is_none()
            {
                continue;
            }
            let Some(s) = score_at(frame, render, cx, cy, &grids.fine, ACCEPT) else {
                continue;
            };
            let distance = target.map_or(0, |(tx, ty)| (px - tx).abs() + (py - ty).abs());
            let better = match &best {
                None => true,
                Some(b) => s > b.score || (s == b.score && distance < b.distance),
            };
            let grid = (px.div_euclid(BLOCK), py.div_euclid(BLOCK));
            if best.as_ref().is_some_and(|b| s == b.score) {
                let ((x0, y0), (x1, y1)) = span;
                span = (
                    (x0.min(grid.0), y0.min(grid.1)),
                    (x1.max(grid.0), y1.max(grid.1)),
                );
            } else if better {
                span = (grid, grid);
            }
            if better {
                best = Some(Best {
                    map: m,
                    x,
                    y,
                    grid,
                    score: s,
                    distance,
                });
            }
        }
        let ((x0, y0), (x1, y1)) = span;
        let apart = if target.is_some() && best.is_some_and(|b| b.score < CLEAR) {
            x1 - x0 > 1 || y1 - y0 > 1
        } else {
            target.is_none() && (x1 > x0 || y1 > y0)
        };
        if apart {
            return None;
        }
        best
    }

    /// The map and tile the player is on when the camera centres `map`'s
    /// tile `(x, y)`: itself when in bounds, past an edge the connected
    /// map's tile (the render's padding shows it there), else nowhere.
    fn resolve<'m>(&'m self, map: &'m MapData, x: i32, y: i32) -> Option<(&'m MapData, i32, i32)> {
        if map.in_bounds(x, y) {
            return Some((map, x, y));
        }
        map.connections.iter().find_map(|c| {
            let other = self.world.map(self.world.name_of(&c.map)?)?;
            let (ox, oy) = match c.direction()? {
                Direction::Up if y < 0 => (x - c.offset, other.height + y),
                Direction::Down if y >= map.height => (x - c.offset, y - map.height),
                Direction::Left if x < 0 => (other.width + x, y - c.offset),
                Direction::Right if x >= map.width => (x - map.width, y - c.offset),
                _ => return None,
            };
            other.in_bounds(ox, oy).then_some((other, ox, oy))
        })
    }

    /// Tracks from a known pose: near it first, then the whole map, then the
    /// maps reachable through its warps and connections (near where the
    /// warps from here arrive, then the whole map).
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
                // Through a door the player often walks on as soon as the
                // fade ends: those frames are mid-step, which only a
                // tracked search finds.
                let found = self
                    .arrivals(map, &other.name)
                    .filter_map(|at| self.locate_in(frame, other, Some(at), 2, exclude))
                    .max_by_key(|o| o.score)
                    .or_else(|| self.locate_in(frame, other, None, 0, exclude))?;
                Some((found, self.distance_to(map, hint, &other.name)))
            })
            .max_by_key(|(o, distance)| (o.score, std::cmp::Reverse(*distance)))
            .map(|(o, _)| o)
    }

    /// Tiles on `other` where the warps from `map` put the player.
    fn arrivals<'a>(
        &'a self,
        map: &'a MapData,
        other: &'a str,
    ) -> impl Iterator<Item = (i32, i32)> + 'a {
        map.warps
            .iter()
            .filter(move |w| self.world.name_of(&w.dest_map) == Some(other))
            .filter_map(|w| self.world.warp_destination(w))
            .map(|(_, x, y)| (x, y))
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
        self.search_all(frame, exclude)
            .into_iter()
            .max_by_key(|o| o.score)
    }

    /// [`Localizer::locate_anywhere`], but no answer when another map
    /// matches as well: maps that share a layout (every Pokémon Center,
    /// the Viridian Forest gates) score the same, and nothing in the frame
    /// tells them apart.
    pub fn locate_anywhere_unambiguous(
        &self,
        frame: &RgbImage,
        exclude: &[Region],
    ) -> Option<PoseObservation> {
        let found = self.search_all(frame, exclude);
        let best = found.iter().map(|o| o.score).max()?;
        let mut top = found.into_iter().filter(|o| o.score == best);
        let first = top.next()?;
        top.next().is_none().then_some(first)
    }

    /// Every map's best pose, in map name order.
    fn search_all(&self, frame: &RgbImage, exclude: &[Region]) -> Vec<PoseObservation> {
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
        found.into_iter().flatten().collect()
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
