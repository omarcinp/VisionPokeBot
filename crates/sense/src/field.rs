//! The field's sprites across frames. Perception reports tile-aligned
//! sprites on located frames only (walking frames, with the camera between
//! tiles, aren't located), each one frame at a time; here a sprite counts
//! once it has stood on its tile for a while, and stays until it has gone
//! unseen for a step's time or its tile scrolls off screen. Identified NPCs
//! become `NpcSeen` when they come to stand on a tile or turn, and
//! `NpcAbsent` when their whole reach stayed empty.

use std::collections::BTreeMap;

use pokebot_state::{Direction, GameEvent, GameState, Observation, VisibleNpc};

/// Frames an identified sprite must stand on its tile before it counts,
/// and a facing must hold before it does.
const SPRITE_FRAMES: u64 = 8;
/// Frames for a sprite no object can be (someone a script moved, or
/// something else): longer than a flower's animation cycle (80 frames), by
/// the end of which perception knows the tile animates.
const UNIDENTIFIED_FRAMES: u64 = 90;
/// Frames a sprite may go unseen on located frames before it has gone: a
/// step (16 frames on no tile) and the new tile's `SPRITE_FRAMES`, so a
/// step is a move rather than leaving and coming back.
const GONE_FRAMES: u64 = 32;
/// Frames an object's reach must be seen empty, on at least
/// `ABSENT_SIGHTINGS` located frames, before it is absent (a wanderer
/// mid-step is on no tile for up to 16 frames).
const ABSENT_FRAMES: u64 = 60;
const ABSENT_SIGHTINGS: u32 = 3;
/// Tiles around the player's that are (at least half) on screen.
const VISIBLE_X: i32 = 7;
const VISIBLE_Y: i32 = 5;

/// A reading that counts once it held for `SPRITE_FRAMES` over two
/// sightings.
#[derive(Debug, Clone, Copy)]
struct Since<T> {
    value: T,
    first: u64,
    last: u64,
    sightings: u32,
}

impl<T: PartialEq> Since<T> {
    fn new(value: T, frame: u64) -> Self {
        Self {
            value,
            first: frame,
            last: frame,
            sightings: 1,
        }
    }

    /// Adds a sighting of `value`, starting over if it changed.
    fn see(&mut self, value: T, frame: u64) {
        if self.value == value {
            self.last = frame;
            self.sightings += 1;
        } else {
            *self = Self::new(value, frame);
        }
    }

    fn held(&self, span: u64) -> bool {
        self.sightings >= 2 && self.last - self.first >= span
    }
}

#[derive(Debug, Clone)]
struct Track {
    x: i32,
    y: i32,
    /// The object it is, and since when it has read so.
    id: Since<Option<u32>>,
    confirmed: bool,
    /// The facing that counts, and the latest one read.
    facing: Option<Direction>,
    reading: Option<Since<Direction>>,
}

impl Track {
    fn held(&self) -> bool {
        let span = if self.id.value.is_some() {
            SPRITE_FRAMES
        } else {
            UNIDENTIFIED_FRAMES
        };
        self.id.held(span)
    }
}

/// An object's reach seen empty since `since`.
#[derive(Debug, Clone, Copy)]
struct Empty {
    since: u64,
    sightings: u32,
}

#[derive(Debug, Default)]
pub struct Field {
    map: Option<String>,
    tracks: Vec<Track>,
    empty: BTreeMap<u32, Empty>,
    /// The last located frame.
    last: Option<u64>,
}

impl Field {
    /// Updates the tracks with `o`; returns the NPC facts it confirms.
    pub fn observe(&mut self, o: &Observation, state: &GameState) -> Vec<GameEvent> {
        let mut events = Vec::new();
        if o.battle.is_some() {
            // The field is gone; whoever is there is seen again after.
            self.tracks.clear();
            self.empty.clear();
            return events;
        }
        let Some(player) = &o.player else {
            // Walking frames and full-screen menus show nothing new.
            return events;
        };
        let (map, f) = (&player.pose.map, o.frame_id);
        if self.map.as_ref() != Some(map) {
            self.map = Some(map.clone());
            self.tracks.clear();
            self.empty.clear();
        }
        self.last = Some(f);
        let (px, py) = (player.pose.x, player.pose.y);
        self.tracks
            .retain(|t| (t.x - px).abs() <= VISIBLE_X && (t.y - py).abs() <= VISIBLE_Y);
        let mut turned = Vec::new();
        for s in &o.sprites {
            let at = self.tracks.iter().position(|t| (t.x, t.y) == (s.x, s.y));
            let t = match at {
                Some(i) if self.tracks[i].id.value == s.local_id => {
                    self.tracks[i].id.see(s.local_id, f);
                    &mut self.tracks[i]
                }
                // Named differently now: start over.
                Some(i) => {
                    self.tracks[i] = track(s.x, s.y, s.local_id, f);
                    &mut self.tracks[i]
                }
                None => {
                    self.tracks.push(track(s.x, s.y, s.local_id, f));
                    self.tracks.last_mut().expect("just pushed")
                }
            };
            let Some(facing) = s.facing else { continue };
            match &mut t.reading {
                Some(r) => r.see(facing, f),
                None => t.reading = Some(Since::new(facing, f)),
            }
            if let Some(r) = t.reading.filter(|r| r.held(SPRITE_FRAMES)) {
                if t.facing != Some(r.value) {
                    t.facing = Some(r.value);
                    if t.confirmed {
                        turned.push((t.x, t.y));
                    }
                }
            }
        }
        self.tracks.retain(|t| f - t.id.last <= GONE_FRAMES);
        // A sprite that has just come to stand replaces the one it walked
        // away from: the same object, or an unnamed one a step away.
        let arrived: Vec<(i32, i32, Option<u32>)> = self
            .tracks
            .iter()
            .filter(|t| !t.confirmed && t.held())
            .map(|t| (t.x, t.y, t.id.value))
            .collect();
        for &(x, y, id) in &arrived {
            self.tracks.retain(|t| {
                let stale = t.confirmed && t.id.last < f;
                let same = id.is_some() && t.id.value == id;
                let step =
                    id.is_none() && t.id.value.is_none() && (t.x - x).abs() + (t.y - y).abs() == 1;
                !(stale && (same || step))
            });
            if let Some(t) = self.tracks.iter_mut().find(|t| (t.x, t.y) == (x, y)) {
                t.confirmed = true;
            }
        }
        for t in self.tracks.iter().filter(|t| t.confirmed) {
            let Some(id) = t.id.value else { continue };
            let new = arrived.iter().any(|&(x, y, _)| (x, y) == (t.x, t.y));
            if !new && !turned.contains(&(t.x, t.y)) {
                continue;
            }
            let known = state.world.npc(map, id);
            let stale = known.is_none_or(|n| {
                n.pos.value != Some((t.x, t.y))
                    || n.present.value != Some(true)
                    || (t.facing.is_some() && n.facing.value != t.facing)
            });
            if stale {
                events.push(GameEvent::NpcSeen {
                    map: map.clone(),
                    local_id: id,
                    x: t.x,
                    y: t.y,
                    facing: t.facing,
                });
            }
        }
        self.empty.retain(|id, _| o.objects_absent.contains(id));
        for &id in &o.objects_absent {
            let e = self.empty.entry(id).or_insert(Empty {
                since: f,
                sightings: 0,
            });
            e.sightings += 1;
            let sustained = e.sightings >= ABSENT_SIGHTINGS && f - e.since >= ABSENT_FRAMES;
            let known = state.world.npc(map, id).and_then(|n| n.present.value);
            if sustained && known != Some(false) {
                events.push(GameEvent::NpcAbsent {
                    map: map.clone(),
                    local_id: id,
                });
            }
        }
        events
    }

    /// Starts the absence evidence over.
    pub fn forget_absence(&mut self) {
        self.empty.clear();
    }

    /// The map and the objects whose reach has stayed empty for `frames`
    /// (at least as long as counts as absent), as of the last located
    /// frame.
    pub fn absent_for(&self, frames: u64) -> Option<(&str, Vec<u32>)> {
        let frames = frames.max(ABSENT_FRAMES);
        let map = self.map.as_deref()?;
        let ids = self
            .empty
            .iter()
            .filter(|(_, e)| e.sightings >= ABSENT_SIGHTINGS)
            .filter(|(_, e)| self.last.is_some_and(|f| f - e.since >= frames))
            .map(|(&id, _)| id)
            .collect();
        Some((map, ids))
    }

    /// The confirmed sprites, top to bottom, left to right.
    pub fn visible(&self) -> Vec<VisibleNpc> {
        let Some(map) = &self.map else {
            return Vec::new();
        };
        let mut npcs: Vec<VisibleNpc> = self
            .tracks
            .iter()
            .filter(|t| t.confirmed)
            .map(|t| VisibleNpc {
                map: map.clone(),
                x: t.x,
                y: t.y,
                local_id: t.id.value,
                facing: t.facing,
            })
            .collect();
        npcs.sort_by_key(|n| (n.y, n.x));
        npcs
    }
}

fn track(x: i32, y: i32, local_id: Option<u32>, frame: u64) -> Track {
    Track {
        x,
        y,
        id: Since::new(local_id, frame),
        confirmed: false,
        facing: None,
        reading: None,
    }
}
