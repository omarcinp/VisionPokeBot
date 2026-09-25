//! Object sprites on the field: which visible tiles have a sprite standing
//! on them, and which of the map's objects each one can be.
//!
//! Once the player is located the camera is known, and map renders carry no
//! objects, so a sprite is where the frame differs from the render in the
//! shape of one. People are 16×32 frames whose figure fills rows 11–30
//! (`graphics/object_events/pics/people`: 228 of 255 frames end on row
//! 30, most start on row 11 or 12, and all but a few are centred in their
//! 16 columns); with the feet on the tile, the tile itself holds the face,
//! body and feet, and only the top of the head reaches into the tile above.
//! Item balls and small people are 16×16, drawn on the tile. A tile
//! therefore counts as a sprite's when
//!
//! - enough of it differs, with the face or top of the object in its upper
//!   half (the tile under someone's head differs only near its bottom),
//! - the figure ends near the tile's bottom and its last row is clear (a
//!   sprite walking down already crosses into the next tile),
//! - the difference is centred in its columns (one walking sideways
//!   straddles two tiles), and
//! - it is coloured: the flashing exit arrow on a door mat is only white and
//!   black, speech bubbles mostly white.
//!
//! These keep sprites between tiles from counting on either (only
//! tile-aligned positions are reported), and speech bubbles (drawn above
//! the head, clear of the feet rows) out.
//!
//! Animated tiles (water, flowers) differ from the render too. Every copy
//! of an animated tile shows the same frame, so a changed tile that looks
//! like another copy of its own render tile on screen, with no copy left
//! unchanged, is animation; and an animation matches the render for one of
//! its frames (`tileset_anims.c`: 16 game frames each), so a tile that
//! differs, matches only briefly, and differs again is animated for the
//! rest of the visit (someone stepping off a tile and back leaves it empty
//! for a step and a wait: over 48 frames, `gMovementDelays*` being 32 at
//! least).
//!
//! A sprite is named when only one of the map's objects can stand on its
//! tile ([`reach`]). Which way a person faces shows in where the skin of
//! the face is (`facing`). An object is absent when every tile it can
//! stand on shows the render while nothing on screen is unexplained.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Range, RangeInclusive};

use pokebot_core::RgbImage;
use pokebot_state::{Direction, PlayerPose, Region, SpriteObservation};

use crate::events::Effect;
use crate::localize::{PLAYER_SCREEN_X, PLAYER_SCREEN_Y};
use crate::{behavior, MapData, ObjectEvent, World, BLOCK};

/// Per-channel difference that still matches (JPEG noise on Switch
/// captures; as the localizer).
const TOLERANCE: u8 = 24;
/// Tiles fully on screen around the player's.
const COLUMNS: RangeInclusive<i32> = -7..=7;
const ROWS: RangeInclusive<i32> = -4..=4;
/// Pixels of a 16×16 tile that must differ for a sprite. The top of a head
/// in the tile above measured 34–54 on live frames; bodies 76 (an item
/// ball on Oak's table, partly its colours) to 194.
const BODY_MIN: u32 = 56;
/// Of those, in the tile's upper half (face, torso, top of a ball).
const TOP_MIN: u32 = 16;
/// Of those, neither near-white nor near-black, at least this many and
/// this share (per cent): people and objects measured 49–89 % over three
/// live recordings, the white speech bubble over the player 30 %, the exit
/// arrow 0.
const COLOURED_MIN: u32 = 16;
const COLOURED_SHARE: u32 = 40;
/// The tile's rows that hold a person's face (sprite rows 16–23).
const FACE_ROWS: usize = 8;
/// Skin pixels there for a face (profiles showed 13–23, faces 18–59).
const FACE_MIN: u32 = 12;
/// Per-channel slack for skin colours.
const SKIN_TOLERANCE: u8 = 28;
/// Lowest row with difference: the feet (sprite row 30 is tile row 14;
/// tall grass hides the legs from about row 9).
const FEET_ROW: usize = 10;
const FEET_ROW_IN_GRASS: usize = 6;
/// Differing pixels allowed on the tile's last row (a sprite on its way
/// down fills it).
const LAST_ROW_MAX: u32 = 3;
/// How far the difference's left and right edges may be off centre, in
/// pixels (|left + right − 15|; sprite frames measure 0–2).
const CENTRE_SLACK: i32 = 3;
/// Share (per mille) of a tile's pixels that must be visible for it to be
/// examined at all.
const KNOWN_MIN: u32 = 750;
/// A tile at most this many differing pixels from the render is unchanged.
const UNCHANGED_MAX: u32 = 4;
/// Differing pixels an empty tile may show (JPEG noise reached 16 on a
/// Switch frame; someone mid-step or under a battle wipe shows far more).
const QUIET: u32 = 24;
/// Longest a tile may match the render between two differing sightings
/// and still be an animation (one animation frame is 16 game frames;
/// recordings keep every 10th).
const FLICKER_FRAMES: u64 = 32;

/// What the field shows around the located player.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldScan {
    /// Sprites on tiles of the map, identified when only one object can be
    /// there.
    pub sprites: Vec<SpriteObservation>,
    /// Objects whose whole reach was examined and showed the render (no
    /// one there, not even between tiles), while every sprite on screen was
    /// identified and no tile looked like someone between tiles (either
    /// could be one of them, moved).
    pub absent: Vec<u32>,
}

/// Finds sprites frame after frame, remembering per map visit what each
/// object's reach is and which tiles animate.
#[derive(Debug, Default)]
pub struct SpriteDetector {
    map: String,
    /// Tiles each object can stand on (objects never drawn left out).
    reaches: Vec<(u32, Vec<(i32, i32)>)>,
    /// Per map tile (row-major): the last frame it differed from the
    /// render, and since when it has matched it.
    history: Vec<(Option<u64>, Option<u64>)>,
    animated: Vec<bool>,
}

impl SpriteDetector {
    /// Sprites around the player at `pose` on `map` (whose render must
    /// load) in frame `frame_id`, skipping pixels in `occluded` (UI
    /// windows). Objects are named from `map.objects`, with trainers' sight
    /// from `world`'s events.
    pub fn scan(
        &mut self,
        frame_id: u64,
        frame: &RgbImage,
        world: &World,
        map: &MapData,
        pose: &PlayerPose,
        occluded: &[Region],
    ) -> FieldScan {
        let Ok(render) = map.render() else {
            return FieldScan::default();
        };
        if self.map != map.name {
            self.enter(world, map);
        }
        let mut cells = cells(frame, render, map, pose, occluded);
        hide_map_name_popup(&mut cells);
        self.remember(frame_id, map, &cells);
        let (found, stray) = self.sprite_tiles(frame, render, map, &mut cells);
        let ids = identify(&found, &self.reaches);
        let sprites: Vec<SpriteObservation> = found
            .iter()
            .zip(&ids)
            .map(|(&(x, y), &local_id)| SpriteObservation {
                x,
                y,
                local_id,
                facing: cells.iter().find(|c| c.tile == (x, y)).and_then(facing),
            })
            .collect();
        let absent = if stray || ids.iter().any(Option::is_none) {
            Vec::new()
        } else {
            let empty: BTreeSet<(i32, i32)> = cells
                .iter()
                .filter(|c| self.empty(map, c))
                .map(|c| c.tile)
                .chain(std::iter::once((pose.x, pose.y)))
                .collect();
            self.reaches
                .iter()
                .filter(|(_, tiles)| tiles.iter().all(|t| empty.contains(t)))
                .map(|(id, _)| *id)
                .collect()
        };
        FieldScan { sprites, absent }
    }

    fn enter(&mut self, world: &World, map: &MapData) {
        self.map = map.name.clone();
        let placed = placements(world, map);
        self.reaches = map
            .objects
            .iter()
            .map(|o| {
                let elsewhere = placed.get(&o.local_id).map_or(&[][..], Vec::as_slice);
                (o.local_id, reach(o, sight(world, map, o), elsewhere, map))
            })
            .filter(|(_, r)| !r.is_empty())
            .collect();
        let tiles = (map.width * map.height).max(0) as usize;
        self.history = vec![(None, None); tiles];
        self.animated = vec![false; tiles];
    }

    fn animates(&self, map: &MapData, tile: (i32, i32)) -> bool {
        self.animated
            .get((tile.1 * map.width + tile.0) as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Whether nobody stands on the cell: it shows the render (up to JPEG
    /// noise) or an animation.
    fn empty(&self, map: &MapData, c: &Cell) -> bool {
        let rows = if c.above_player { 0..8 } else { 0..16 };
        c.examined && (c.diff.count(rows) <= QUIET || self.animates(map, c.tile))
    }

    /// Records which examined tiles differ and which match the render, and
    /// marks tiles that matched only briefly between differing sightings.
    fn remember(&mut self, frame_id: u64, map: &MapData, cells: &[Cell]) {
        for c in cells.iter().filter(|c| c.examined && !c.above_player) {
            let i = (c.tile.1 * map.width + c.tile.0) as usize;
            let (last_differed, matched_since) = &mut self.history[i];
            let n = c.diff.count(0..16);
            if n <= UNCHANGED_MAX {
                if last_differed.is_some() && matched_since.is_none() {
                    *matched_since = Some(frame_id);
                }
            } else if n >= BODY_MIN / 2 {
                if let (Some(differed), Some(_)) = (*last_differed, *matched_since) {
                    if frame_id.saturating_sub(differed) <= FLICKER_FRAMES {
                        self.animated[i] = true;
                    }
                }
                *last_differed = Some(frame_id);
                *matched_since = None;
            }
        }
    }

    /// Tiles (map coordinates) with a sprite standing on them, and whether
    /// some other tile changed as much as a sprite would (someone between
    /// tiles, a speech bubble). A tile under a sprite holds that sprite's
    /// head in its lower rows: it isn't examined when it differs there
    /// (anyone standing on it is hidden).
    fn sprite_tiles(
        &self,
        frame: &RgbImage,
        render: &RgbImage,
        map: &MapData,
        cells: &mut [Cell],
    ) -> (Vec<(i32, i32)>, bool) {
        let mut stray = false;
        let mut sprite = vec![false; cells.len()];
        // Bottom-up, so the tile below is decided first.
        let mut order: Vec<usize> = (0..cells.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(cells[i].screen.1));
        let mut at = std::collections::HashMap::new();
        for (i, c) in cells.iter().enumerate() {
            at.insert(c.tile, i);
        }
        for i in order {
            if !cells[i].examined {
                continue;
            }
            let c = &cells[i];
            let grass = map
                .tile(c.tile.0, c.tile.1)
                .is_some_and(|t| t.behavior == behavior::TALL_GRASS);
            let like = looks_like_sprite(c, grass);
            let busy = c.diff.count(0..16) >= BODY_MIN && !c.above_player;
            let animated = (like || busy)
                && (self.animates(map, c.tile) || animated_copy(frame, render, cells, i));
            sprite[i] = like && !animated;
            stray |= busy && !sprite[i] && !animated;
            let below = at
                .get(&(c.tile.0, c.tile.1 + 1))
                .is_some_and(|&j| sprite[j]);
            if !sprite[i] && below && !c.above_player && c.diff.count(11..16) > UNCHANGED_MAX {
                // A head over its last rows: whoever stands here is hidden
                // (live, Route 4's Pokémon Center: a gentleman over a
                // clipboard still showed; two people wouldn't).
                cells[i].examined = false;
            }
        }
        let found = cells
            .iter()
            .zip(&sprite)
            .filter(|(_, &s)| s)
            .map(|(c, _)| c.tile)
            .collect();
        (found, stray)
    }
}

/// Differing, coloured, skin-coloured (face rows only) and visible pixels
/// of one tile, a bit per column per row.
#[derive(Debug, Clone, Copy, Default)]
struct TileDiff {
    diff: [u16; 16],
    coloured: [u16; 16],
    skin: [u16; 16],
    known: [u16; 16],
}

impl TileDiff {
    fn count(&self, rows: Range<usize>) -> u32 {
        rows.map(|r| self.diff[r].count_ones()).sum()
    }

    fn coloured(&self) -> u32 {
        self.coloured.iter().map(|r| r.count_ones()).sum()
    }

    fn known_count(&self) -> u32 {
        self.known.iter().map(|r| r.count_ones()).sum()
    }

    /// Lowest row with at least two differing pixels in the middle columns.
    fn lowest(&self) -> Option<usize> {
        const MIDDLE: u16 = 0b0011_1111_1111_1100;
        (0..16)
            .rev()
            .find(|&r| (self.diff[r] & MIDDLE).count_ones() >= 2)
    }

    /// Whether the differing columns (two or more pixels each) of `rows`
    /// are centred.
    fn centred(&self, rows: Range<usize>) -> bool {
        let mut columns = [0u8; 16];
        for r in rows {
            for (c, n) in columns.iter_mut().enumerate() {
                *n += (self.diff[r] >> c & 1) as u8;
            }
        }
        let left = columns.iter().position(|&n| n >= 2);
        let right = columns.iter().rposition(|&n| n >= 2);
        match (left, right) {
            (Some(l), Some(r)) => (l as i32 + r as i32 - 15).abs() <= CENTRE_SLACK,
            _ => false,
        }
    }
}

/// One tile of the map on screen.
struct Cell {
    tile: (i32, i32),
    /// Top-left on screen and in the render.
    screen: (i32, i32),
    render: (i32, i32),
    diff: TileDiff,
    /// Only the rows above the player's head can be seen.
    above_player: bool,
    examined: bool,
}

impl Cell {
    /// The tile's rows on screen (the half rows at the top and bottom edges
    /// show only half).
    fn known_rows(&self) -> Range<usize> {
        let known = &self.diff.known;
        let first = known.iter().position(|&r| r != 0).unwrap_or(16);
        let last = known.iter().rposition(|&r| r != 0).map_or(0, |r| r + 1);
        first..last.max(first)
    }
}

fn cells(
    frame: &RgbImage,
    render: &RgbImage,
    map: &MapData,
    pose: &PlayerPose,
    occluded: &[Region],
) -> Vec<Cell> {
    let (fw, fh) = (frame.width() as i32, frame.height() as i32);
    let (rw, rh) = (render.width() as i32, render.height() as i32);
    let (fb, rb) = (frame.as_bytes(), render.as_bytes());
    let mut out = Vec::with_capacity(165);
    // One more row each way, half on screen: never examined, but copies of
    // an animated tile for `animated_copy`.
    for ky in ROWS.start() - 1..=ROWS.end() + 1 {
        for kx in COLUMNS {
            let tile = (pose.x + kx, pose.y + ky);
            if (kx, ky) == (0, 0) || !map.in_bounds(tile.0, tile.1) {
                continue;
            }
            let (sx, sy) = (PLAYER_SCREEN_X + kx * BLOCK, PLAYER_SCREEN_Y + ky * BLOCK);
            let (rx, ry) = ((tile.0 + map.pad) * BLOCK, (tile.1 + map.pad) * BLOCK);
            if rx < 0 || ry < 0 || rx + BLOCK > rw || ry + BLOCK > rh || sx < 0 || sx + BLOCK > fw {
                continue;
            }
            let above_player = (kx, ky) == (0, -1);
            let mut known = [0u16; 16];
            for (dy, row) in known.iter_mut().enumerate() {
                let y = sy + dy as i32;
                // The player's head covers the lower half of the tile above.
                if y >= 0 && y < fh && !(above_player && dy >= 8) {
                    *row = u16::MAX;
                }
            }
            for r in occluded {
                let (x0, x1) = (
                    (r.x as i32 - sx).max(0),
                    (r.x as i32 + r.width as i32 - sx).min(BLOCK),
                );
                let (y0, y1) = (
                    (r.y as i32 - sy).max(0),
                    (r.y as i32 + r.height as i32 - sy).min(BLOCK),
                );
                if x0 >= x1 || y0 >= y1 {
                    continue;
                }
                let bits = (((1u32 << (x1 - x0)) - 1) << x0) as u16;
                for row in &mut known[y0 as usize..y1 as usize] {
                    *row &= !bits;
                }
            }
            let mut diff = TileDiff {
                known,
                ..TileDiff::default()
            };
            for (dy, &known) in known.iter().enumerate() {
                if known == 0 {
                    continue;
                }
                let fi = (((sy + dy as i32) * fw + sx) * 3) as usize;
                let ri = (((ry + dy as i32) * rw + rx) * 3) as usize;
                let (f, r) = (&fb[fi..fi + 48], &rb[ri..ri + 48]);
                let (mut bits, mut coloured, mut skin) = (0u16, 0u16, 0u16);
                for (dx, (p, q)) in f.chunks_exact(3).zip(r.chunks_exact(3)).enumerate() {
                    if (0..3).any(|c| p[c].abs_diff(q[c]) > TOLERANCE) {
                        bits |= 1 << dx;
                        let (lo, hi) = (p.iter().min().unwrap(), p.iter().max().unwrap());
                        if (48..216).contains(hi) || hi - lo >= 48 {
                            coloured |= 1 << dx;
                        }
                        if dy < FACE_ROWS && is_skin(p) {
                            skin |= 1 << dx;
                        }
                    }
                }
                diff.diff[dy] = bits & known;
                diff.coloured[dy] = coloured & known;
                diff.skin[dy] = skin & known;
            }
            let full = sy >= 0 && sy + BLOCK <= fh;
            let needed = if above_player { 128 } else { 256 };
            let examined = full && diff.known_count() * 1000 >= needed * KNOWN_MIN;
            out.push(Cell {
                tile,
                screen: (sx, sy),
                render: (rx, ry),
                diff,
                above_player,
                examined,
            });
        }
    }
    out
}

/// The map-name popup slides down over the top 24 rows on entering a map
/// (`map_name_popup.c`: a window from x 0, 14–22 tiles wide): when the
/// first examined row's top line differs across the left half of the
/// screen, that row isn't examined.
fn hide_map_name_popup(cells: &mut [Cell]) {
    let top = PLAYER_SCREEN_Y + ROWS.start() * BLOCK;
    let (differing, width) = cells
        .iter()
        .filter(|c| c.screen.1 == top && c.screen.0 < 120)
        .fold((0, 0), |(d, w), c| {
            (d + c.diff.diff[0].count_ones(), w + BLOCK as u32)
        });
    if width > 0 && differing * 10 >= width * 7 {
        for c in cells.iter_mut().filter(|c| c.screen.1 == top) {
            c.examined = false;
        }
    }
}

/// People's skin: entries 1–3 of the four NPC palettes (npc_blue, pink,
/// green, white: every `graphics/object_events/pics/people` sheet carries
/// them), within the capture tolerance.
fn is_skin(p: &[u8]) -> bool {
    const SKIN: [[u8; 3]; 3] = [[255, 213, 180], [246, 189, 148], [222, 148, 115]];
    SKIN.iter()
        .any(|s| (0..3).all(|c| p[c].abs_diff(s[c]) <= SKIN_TOLERANCE))
}

/// Which way a person faces, from the skin in the tile's top rows (the
/// face): a face turned down spans the head, one in profile sits on the
/// side it looks to, and from behind only the hands' edges show. Measured
/// on live frames (emulator and Switch), the skin's mean column sat within
/// ±0.5 of the centre facing down, +1.1–1.7 right, −1.2–1.8 left. Objects
/// without skin (item balls, the Pokédex) and anything unclear have none.
fn facing(c: &Cell) -> Option<Direction> {
    let mut columns = [0u32; 16];
    for row in &c.diff.skin[..FACE_ROWS] {
        for (x, n) in columns.iter_mut().enumerate() {
            *n += u32::from(row >> x & 1);
        }
    }
    let total: u32 = columns.iter().sum();
    let left = columns.iter().position(|&n| n > 0)?;
    let right = columns.iter().rposition(|&n| n > 0)?;
    // Mean column minus 7.5, in tenths.
    let offset = (columns
        .iter()
        .enumerate()
        .map(|(x, &n)| x as u32 * n)
        .sum::<u32>()
        * 10
        / total) as i32
        - 75;
    if total <= 6 {
        let hands = columns
            .iter()
            .enumerate()
            .all(|(x, &n)| n == 0 || !(4..12).contains(&x));
        return hands.then_some(Direction::Up);
    }
    if total < FACE_MIN {
        return None;
    }
    match offset {
        -5..=5 if left <= 3 && right >= 12 => Some(Direction::Down),
        8.. if left >= 4 => Some(Direction::Right),
        ..=-8 if right <= 11 => Some(Direction::Left),
        _ => None,
    }
}

fn looks_like_sprite(c: &Cell, grass: bool) -> bool {
    let d = &c.diff;
    let top = d.count(0..8);
    let coloured = d.coloured();
    if coloured < COLOURED_MIN || coloured * 100 < d.count(0..16) * COLOURED_SHARE {
        return false;
    }
    if c.above_player {
        // Only the face and torso rows show over the player's head.
        return top >= BODY_MIN / 2 && d.centred(0..8);
    }
    let feet = if grass { FEET_ROW_IN_GRASS } else { FEET_ROW };
    d.count(0..16) >= BODY_MIN
        && top >= TOP_MIN
        && d.count(15..16) <= LAST_ROW_MAX
        && d.lowest().is_some_and(|r| r >= feet)
        && d.centred(0..15)
}

/// Whether cell `i`'s difference is a tile animation: another copy of the
/// same render tile on screen shows the same frame, and no copy is
/// unchanged (all copies of an animated tile change together; two item
/// balls on plain grass leave the rest of the grass unchanged).
fn animated_copy(frame: &RgbImage, render: &RgbImage, cells: &[Cell], i: usize) -> bool {
    let c = &cells[i];
    let mut same_frame = false;
    for (j, o) in cells.iter().enumerate() {
        if j == i || o.above_player {
            continue;
        }
        let rows = o.known_rows();
        if rows.len() < 4 || !same_pixels(render, c.render, o.render, rows.clone(), 0) {
            continue;
        }
        if o.diff.count(rows.clone()) <= UNCHANGED_MAX {
            return false;
        }
        if same_pixels(frame, c.screen, o.screen, rows, TOLERANCE) {
            same_frame = true;
        }
    }
    same_frame
}

/// Whether rows `rows` of two 16×16 patches of `image` match: exactly, or
/// within `tolerance` on all but a tenth of the pixels (two copies of one
/// tile differed by 9 of 256 pixels beyond 24 on a Switch frame).
fn same_pixels(
    image: &RgbImage,
    a: (i32, i32),
    b: (i32, i32),
    rows: Range<usize>,
    tolerance: u8,
) -> bool {
    let (w, bytes) = (image.width() as i32, image.as_bytes());
    let allowed = if tolerance == 0 {
        0
    } else {
        rows.len() * BLOCK as usize / 10
    };
    let mut misses = 0;
    for dy in rows {
        let dy = dy as i32;
        let ia = (((a.1 + dy) * w + a.0) * 3) as usize;
        let ib = (((b.1 + dy) * w + b.0) * 3) as usize;
        let (ra, rb) = (&bytes[ia..ia + 48], &bytes[ib..ib + 48]);
        if tolerance == 0 {
            if ra != rb {
                return false;
            }
            continue;
        }
        for (p, q) in ra.chunks_exact(3).zip(rb.chunks_exact(3)) {
            if (0..3).any(|c| p[c].abs_diff(q[c]) > tolerance) {
                misses += 1;
                if misses > allowed {
                    return false;
                }
            }
        }
    }
    true
}

/// A trainer's sight (tiles), from the compiled events.
fn sight(world: &World, map: &MapData, o: &ObjectEvent) -> i32 {
    let trainer = o
        .trainer_type
        .as_deref()
        .is_some_and(|t| t != "TRAINER_TYPE_NONE");
    if !trainer {
        return 0;
    }
    world
        .events()
        .and_then(|e| {
            e.objects
                .iter()
                .find(|r| r.map == map.name && r.local_id == o.local_id)
        })
        .map_or(0, |r| r.sight)
}

/// Where the map's entry scripts put objects (`setobjectxyperm`), by local
/// id: Pallet Town's sign lady stands at (5, 15) or (12, 2) rather than her
/// spawn tile, depending on the story.
fn placements(world: &World, map: &MapData) -> BTreeMap<u32, Vec<(i32, i32)>> {
    let mut out: BTreeMap<u32, Vec<(i32, i32)>> = BTreeMap::new();
    let Some(events) = world.events() else {
        return out;
    };
    for paths in crate::gates::entry_scripts(events, &map.name) {
        for e in paths.iter().flat_map(|p| &p.does) {
            if let Effect::MoveObject { move_object, x, y } = e {
                let id = move_object.as_int().and_then(|i| u32::try_from(i).ok());
                if let (Some(id), Some(x), Some(y)) = (id, x.as_int(), y.as_int()) {
                    out.entry(id).or_default().push((x as i32, y as i32));
                }
            }
        }
    }
    out
}

/// In-bounds tiles where object `o` can be seen standing: a wanderer's
/// area; else its spawn tile, plus a trainer's lines of sight (a trainer
/// walks up to the player it spots and stays there until the map reloads;
/// live, Route 3's lass #7 stood at (18, 9) and (16, 9) after spotting the
/// player from (19, 9)); plus the tiles entry scripts move it to
/// (`elsewhere`). Empty for objects never drawn (`INVISIBLE`) or without a
/// position.
pub fn reach(
    o: &ObjectEvent,
    sight: i32,
    elsewhere: &[(i32, i32)],
    map: &MapData,
) -> Vec<(i32, i32)> {
    let (Some(x), Some(y)) = (o.x, o.y) else {
        return Vec::new();
    };
    let movement = o.movement.as_deref().unwrap_or("");
    if movement.contains("INVISIBLE") {
        return Vec::new();
    }
    let mut tiles = BTreeSet::new();
    if crate::obstacles::is_wanderer(o) {
        let (x0, y0, x1, y1) = o.area();
        for ty in y0..=y1 {
            for tx in x0..=x1 {
                tiles.insert((tx, ty));
            }
        }
    }
    tiles.insert((x, y));
    tiles.extend(elsewhere.iter().copied());
    let all = movement.contains("LOOK_AROUND") || movement.contains("ROTATE");
    for dir in Direction::ALL {
        let name = match dir {
            Direction::Up => "UP",
            Direction::Down => "DOWN",
            Direction::Left => "LEFT",
            Direction::Right => "RIGHT",
        };
        if !(all || movement.contains(name)) {
            continue;
        }
        let (dx, dy) = dir.delta();
        for step in 1..sight {
            tiles.insert((x + dx * step, y + dy * step));
        }
    }
    tiles
        .into_iter()
        .filter(|&(tx, ty)| map.in_bounds(tx, ty))
        .collect()
}

/// The object each sprite can be: the only one whose reach holds its tile,
/// once objects already placed are set aside. An object claimed by two
/// sprites names neither.
fn identify(found: &[(i32, i32)], reaches: &[(u32, Vec<(i32, i32)>)]) -> Vec<Option<u32>> {
    let mut candidates: Vec<Vec<u32>> = found
        .iter()
        .map(|t| {
            reaches
                .iter()
                .filter(|(_, tiles)| tiles.contains(t))
                .map(|(id, _)| *id)
                .collect()
        })
        .collect();
    loop {
        let placed: Vec<u32> = candidates
            .iter()
            .filter(|c| c.len() == 1)
            .map(|c| c[0])
            .collect();
        let mut changed = false;
        for c in candidates.iter_mut().filter(|c| c.len() > 1) {
            let before = c.len();
            c.retain(|id| !placed.contains(id));
            changed |= c.len() != before;
        }
        if !changed {
            break;
        }
    }
    candidates
        .iter()
        .map(|c| match c.as_slice() {
            [id] if candidates.iter().filter(|o| o.as_slice() == [*id]).count() == 1 => Some(*id),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tile whose pixels in `rows` × `columns` differ (and are coloured
    /// when `coloured`).
    fn cell(rows: Range<usize>, columns: RangeInclusive<u32>, coloured: bool) -> Cell {
        let bits = columns.map(|c| 1u16 << c).fold(0, |a, b| a | b);
        let mut diff = TileDiff {
            known: [u16::MAX; 16],
            ..TileDiff::default()
        };
        for r in rows {
            diff.diff[r] = bits;
            if coloured {
                diff.coloured[r] = bits;
            }
        }
        Cell {
            tile: (0, 0),
            screen: (0, 0),
            render: (0, 0),
            diff,
            above_player: false,
            examined: true,
        }
    }

    #[test]
    fn a_standing_figure_is_a_sprite() {
        assert!(looks_like_sprite(&cell(0..15, 3..=12, true), false));
    }

    #[test]
    fn a_figure_between_tiles_is_not() {
        // Five pixels into its step right: off centre.
        assert!(!looks_like_sprite(&cell(0..15, 8..=15, true), false));
        // One pixel into its step down: the last row is covered.
        assert!(!looks_like_sprite(&cell(1..16, 3..=12, true), false));
        // Most of the way down into this tile: no face or torso on top.
        assert!(!looks_like_sprite(&cell(9..15, 3..=12, true), false));
    }

    #[test]
    fn a_head_from_below_or_a_white_arrow_is_not() {
        assert!(!looks_like_sprite(&cell(11..16, 4..=11, true), false));
        assert!(!looks_like_sprite(&cell(4..15, 3..=12, false), false));
    }

    #[test]
    fn tall_grass_hides_the_legs() {
        let upper_body = cell(0..8, 3..=12, true);
        assert!(!looks_like_sprite(&upper_body, false));
        assert!(looks_like_sprite(&upper_body, true));
    }

    #[test]
    fn two_sprites_claiming_one_object_name_neither() {
        let reaches = vec![(1, vec![(3, 3), (3, 4)]), (2, vec![(5, 5)])];
        assert_eq!(
            identify(&[(3, 3), (3, 4), (5, 5)], &reaches),
            vec![None, None, Some(2)]
        );
    }

    #[test]
    fn overlapping_areas_resolve_once_one_is_placed() {
        // Object 1 can only be the sprite at (3, 3); the one at (3, 4) is
        // then 2, unless a third object can stand there too.
        let reaches = vec![(1, vec![(3, 3), (3, 4)]), (2, vec![(3, 4), (3, 5)])];
        assert_eq!(
            identify(&[(3, 3), (3, 4)], &reaches),
            vec![Some(1), Some(2)]
        );
        let reaches = vec![(1, vec![(3, 3)]), (2, vec![(3, 4)]), (3, vec![(3, 4)])];
        assert_eq!(identify(&[(3, 3), (3, 4)], &reaches), vec![Some(1), None]);
    }
}
