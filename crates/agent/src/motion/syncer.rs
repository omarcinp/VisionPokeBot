//! Per-device timing model: how long an input takes to show and how long a
//! unit of it (a tile, a press) lasts, learned from confirmed actions.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// One GBA frame at FireRed's 59.7275 Hz, in milliseconds.
pub const FRAME_MS: f64 = 1000.0 / 59.7275;
/// Weight of a new sample in the running estimates.
const ALPHA: f64 = 0.2;
/// Samples the spread is measured over.
const WINDOW: usize = 16;
/// One walking step: 16 GBA frames, the default tile time.
pub const TILE_MS: u64 = 268;
/// Frames to wait for a step to show up on screen (today's `STEP_TIMEOUT`).
const STEP_TIMEOUT: u64 = 30;
/// Frames to wait for a warp's fade and the new map (today's `WARP_TIMEOUT`).
const WARP_TIMEOUT: u64 = 180;
/// Frames a turn in place may take to show (today's `face` timeout).
const TURN_TIMEOUT: u64 = 6;
/// Frames a menu press may take to show (the common timeout in the tasks).
const MENU_TIMEOUT: u64 = 30;
/// Extra frames actions may take on real hardware (today's CLI default).
const HARDWARE_LATENCY_FRAMES: u64 = 30;

/// A shared timing model: the executor feeds it, the walker reads it.
pub type SyncerHandle = Arc<Mutex<Syncer>>;

/// The kinds of input the model keeps separate estimates for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum InputKind {
    /// A directional press or hold that moves the player (unit: one tile).
    WalkTile,
    /// A directional tap that only turns the player.
    Turn,
    /// A press on a menu, dialogue or list.
    MenuPress,
    /// Taking a warp: fade out, new map located.
    WarpFade,
    /// Entering a battle: transition to the battle screen.
    BattleStart,
}

impl InputKind {
    pub const ALL: [InputKind; 5] = [
        InputKind::WalkTile,
        InputKind::Turn,
        InputKind::MenuPress,
        InputKind::WarpFade,
        InputKind::BattleStart,
    ];

    /// The unit's duration before the model has learned anything.
    fn default_unit_ms(self) -> f64 {
        match self {
            InputKind::WalkTile => TILE_MS as f64,
            // A turn takes 8 frames; a menu press shows the next frame but
            // its animation runs a few; fades and battle transitions are
            // long and only bounded by their timeouts.
            InputKind::Turn => 8.0 * FRAME_MS,
            InputKind::MenuPress => 4.0 * FRAME_MS,
            InputKind::WarpFade | InputKind::BattleStart => WARP_TIMEOUT as f64 * FRAME_MS / 2.0,
        }
    }

    /// Today's timeout (frames after the inputs finish) for `units` of
    /// this kind, before the model has learned anything.
    fn default_timeout_frames(self, units: usize) -> u64 {
        match self {
            // A hold: the last tile finishes after the release, and a
            // moving sprite is located a little late.
            InputKind::WalkTile if units >= 2 => STEP_TIMEOUT + 16 + 2 * units as u64,
            InputKind::WalkTile => STEP_TIMEOUT,
            InputKind::Turn => TURN_TIMEOUT,
            InputKind::MenuPress => MENU_TIMEOUT,
            InputKind::WarpFade | InputKind::BattleStart => WARP_TIMEOUT,
        }
    }

    /// Whether the effect of one unit must play out before the first
    /// visible effect (a step is only seen once the player left the tile).
    fn effect_after_unit(self) -> bool {
        matches!(self, InputKind::WalkTile)
    }
}

/// What the model believes about one input kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    /// From command issue to the first visible effect.
    pub latency_ms: f64,
    /// Per unit (tile, press).
    pub unit_ms: f64,
    /// Mean absolute deviation of the last [`WINDOW`] samples.
    pub spread_ms: f64,
    pub samples: u32,
    /// The samples the spread is measured over.
    #[serde(default)]
    window: VecDeque<f64>,
}

impl Estimate {
    fn default_for(kind: InputKind) -> Self {
        Self {
            latency_ms: 0.0,
            unit_ms: kind.default_unit_ms(),
            spread_ms: 0.0,
            samples: 0,
            window: VecDeque::new(),
        }
    }

    fn push_spread_sample(&mut self, sample: f64) {
        if self.window.len() == WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(sample);
        let mean = self.window.iter().sum::<f64>() / self.window.len() as f64;
        self.spread_ms =
            self.window.iter().map(|s| (s - mean).abs()).sum::<f64>() / self.window.len() as f64;
    }
}

fn ewma(current: f64, sample: f64) -> f64 {
    current + ALPHA * (sample - current)
}

fn frames(ms: f64) -> u64 {
    (ms / FRAME_MS).ceil().max(0.0) as u64
}

/// The model as published to the web UI.
#[derive(Debug, Clone, Serialize)]
pub struct TimingReport {
    pub profile: String,
    pub base_latency_frames: u64,
    pub estimates: Vec<(InputKind, Estimate)>,
}

/// The persisted file: one set of estimates per device profile.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TimingFile {
    profiles: BTreeMap<String, BTreeMap<InputKind, Estimate>>,
}

/// The timing model of one device profile.
#[derive(Debug, Clone)]
pub struct Syncer {
    profile: String,
    /// Extra frames every action may take to show its effect, before the
    /// model has learned anything (the executor's `latency_frames`).
    base_latency_frames: u64,
    estimates: BTreeMap<InputKind, Estimate>,
    /// Bumped by every observation, so a publisher can tell when to refresh.
    revision: u64,
}

impl Syncer {
    /// An empty model for `profile` (`emulator`, `switch`): every value is
    /// today's constant.
    pub fn new(profile: &str) -> Self {
        Self {
            profile: profile.to_owned(),
            base_latency_frames: if profile == "emulator" {
                0
            } else {
                HARDWARE_LATENCY_FRAMES
            },
            estimates: BTreeMap::new(),
            revision: 0,
        }
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The extra frames every action may take before anything is learned
    /// (the CLI's `--latency-frames`).
    pub fn set_base_latency_frames(&mut self, frames: u64) {
        self.base_latency_frames = frames;
    }

    pub fn base_latency_frames(&self) -> u64 {
        self.base_latency_frames
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The estimate for `kind` (the defaults when nothing was observed).
    pub fn estimate(&self, kind: InputKind) -> Estimate {
        self.estimates
            .get(&kind)
            .cloned()
            .unwrap_or_else(|| Estimate::default_for(kind))
    }

    /// Every kind's estimate, learned or default.
    pub fn estimates(&self) -> Vec<(InputKind, Estimate)> {
        InputKind::ALL
            .iter()
            .map(|&kind| (kind, self.estimate(kind)))
            .collect()
    }

    /// The whole model, for the web UI.
    pub fn report(&self) -> TimingReport {
        TimingReport {
            profile: self.profile.clone(),
            base_latency_frames: self.base_latency_frames,
            estimates: self.estimates(),
        }
    }

    /// How long to hold for `units` of `kind`: release inside the last
    /// unit, which the game then finishes, plus a margin of two spreads.
    /// Latency is not added: a press and its release travel the same path,
    /// so it moves the walk in time without changing how far it goes.
    pub fn hold_for(&self, kind: InputKind, units: usize) -> Duration {
        let e = self.estimate(kind);
        let ms = e.unit_ms * units as f64 - e.unit_ms / 2.0 + 2.0 * e.spread_ms;
        Duration::from_millis(ms.round().max(0.0) as u64)
    }

    /// Frames to wait after the inputs finish for `units` of `kind` to show:
    /// today's constant, extended when the device measured slower.
    pub fn timeout_frames(&self, kind: InputKind, units: usize) -> u64 {
        let default = kind.default_timeout_frames(units);
        let e = self.estimate(kind);
        if e.samples == 0 {
            return default;
        }
        // After the inputs finish: the latency, then the whole unit (a tap
        // walks its tile after the press) or the last half of it (a hold is
        // released inside the last unit).
        let tail = if units >= 2 {
            e.unit_ms / 2.0
        } else {
            e.unit_ms
        };
        default.max(frames(e.latency_ms + tail + 2.0 * e.spread_ms))
    }

    /// Extra frames an action of `kind` may take to show its effect (the
    /// executor's allowance on top of the action's timeout).
    pub fn expect_frames(&self, kind: Option<InputKind>) -> u64 {
        let Some(kind) = kind else {
            return self.base_latency_frames;
        };
        let e = self.estimate(kind);
        if e.samples == 0 {
            return self.base_latency_frames;
        }
        self.base_latency_frames
            .max(frames(e.latency_ms + 2.0 * e.spread_ms))
    }

    /// One confirmed action of `units` of `kind`: issued at `issued`, its
    /// first effect seen at `first_effect`, its expectation met at `done`.
    pub fn observe(
        &mut self,
        kind: InputKind,
        issued: Instant,
        first_effect: Instant,
        done: Instant,
        units: usize,
    ) {
        let to_effect = first_effect.saturating_duration_since(issued).as_secs_f64() * 1000.0;
        let effect_to_done = done.saturating_duration_since(first_effect).as_secs_f64() * 1000.0;
        self.observe_ms(kind, to_effect, effect_to_done, units);
    }

    /// [`Syncer::observe`] with the intervals already in milliseconds
    /// (frame counts × [`FRAME_MS`] when the clock is the frame counter).
    pub fn observe_ms(
        &mut self,
        kind: InputKind,
        to_effect: f64,
        effect_to_done: f64,
        units: usize,
    ) {
        if units == 0 || !to_effect.is_finite() || !effect_to_done.is_finite() {
            return;
        }
        let mut e = self.estimate(kind);
        // The first effect of `units` units is seen once the first unit
        // played out: the player is only located on the next tile.
        let latency = if kind.effect_after_unit() {
            (to_effect - e.unit_ms).max(0.0)
        } else {
            to_effect.max(0.0)
        };
        e.latency_ms = ewma(e.latency_ms, latency);
        if units >= 2 {
            // Located on tile 1 at the first effect, on tile n when done.
            let unit = effect_to_done / (units - 1) as f64;
            e.unit_ms = ewma(e.unit_ms, unit);
            e.push_spread_sample(unit);
        } else if !kind.effect_after_unit() {
            e.push_spread_sample(latency);
        }
        e.samples += 1;
        self.estimates.insert(kind, e);
        self.revision += 1;
    }

    /// The estimates of `profile` from `path`; a missing file or profile
    /// gives an empty model.
    pub fn load(path: &Path, profile: &str) -> std::io::Result<Self> {
        let mut syncer = Self::new(profile);
        let file = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<TimingFile>(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(syncer),
            Err(e) => return Err(e),
        };
        if let Some(estimates) = file.profiles.get(profile) {
            syncer.estimates = estimates.clone();
        }
        Ok(syncer)
    }

    /// Writes this profile's estimates to `path`, keeping the other
    /// profiles in the file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut file = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<TimingFile>(&bytes).unwrap_or_default(),
            Err(_) => TimingFile::default(),
        };
        file.profiles
            .insert(self.profile.clone(), self.estimates.clone());
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_reproduce_todays_constants() {
        let s = Syncer::new("emulator");
        // run_hold: TILE_MS * tiles - TILE_MS / 2.
        assert_eq!(s.hold_for(InputKind::WalkTile, 1).as_millis(), 134);
        assert_eq!(
            s.hold_for(InputKind::WalkTile, 4).as_millis(),
            268 * 4 - 134
        );
        assert_eq!(
            s.hold_for(InputKind::WalkTile, 8).as_millis(),
            268 * 8 - 134
        );
        // STEP_TIMEOUT, the hold timeout, WARP_TIMEOUT, the face timeout.
        assert_eq!(s.timeout_frames(InputKind::WalkTile, 1), 30);
        assert_eq!(s.timeout_frames(InputKind::WalkTile, 4), 30 + 16 + 8);
        assert_eq!(s.timeout_frames(InputKind::WarpFade, 1), 180);
        assert_eq!(s.timeout_frames(InputKind::Turn, 1), 6);
        // The executor's latency_frames: 0 on the emulator, 30 on hardware.
        assert_eq!(s.expect_frames(None), 0);
        assert_eq!(s.expect_frames(Some(InputKind::WalkTile)), 0);
        let hw = Syncer::new("switch");
        assert_eq!(hw.expect_frames(None), 30);
        assert_eq!(hw.expect_frames(Some(InputKind::MenuPress)), 30);
        assert_eq!(
            hw.hold_for(InputKind::WalkTile, 3),
            s.hold_for(InputKind::WalkTile, 3)
        );
    }

    #[test]
    fn converges_on_synthetic_samples() {
        let mut s = Syncer::new("emulator");
        // A device that walks a tile in 300 ms and shows it 50 ms late:
        // holds of 4 tiles, first effect after latency + one tile.
        for _ in 0..40 {
            s.observe_ms(InputKind::WalkTile, 50.0 + 300.0, 3.0 * 300.0, 4);
        }
        let e = s.estimate(InputKind::WalkTile);
        assert!((e.unit_ms - 300.0).abs() < 1.0, "{e:?}");
        assert!((e.latency_ms - 50.0).abs() < 1.0, "{e:?}");
        assert!(e.spread_ms < 1.0, "{e:?}");
        assert_eq!(e.samples, 40);
        // Holds follow: 4 tiles = 4·300 − 150 (+ a negligible spread).
        let hold = s.hold_for(InputKind::WalkTile, 4).as_millis();
        assert!((1049..=1052).contains(&hold), "{hold}");
        // Timeouts only grow: 350 ms after a tap is 21 frames, less than
        // STEP_TIMEOUT, so the constant stays; the executor's allowance is
        // the learned latency.
        assert_eq!(s.timeout_frames(InputKind::WalkTile, 1), 30);
        assert_eq!(
            s.expect_frames(Some(InputKind::WalkTile)),
            frames(e.latency_ms + 2.0 * e.spread_ms)
        );
        // A device twice as slow as modelled: the timeouts follow it.
        let mut slow = Syncer::new("switch");
        for _ in 0..40 {
            slow.observe_ms(InputKind::WalkTile, 200.0 + 600.0, 3.0 * 600.0, 4);
        }
        let e = slow.estimate(InputKind::WalkTile);
        assert_eq!(
            slow.timeout_frames(InputKind::WalkTile, 1),
            frames(e.latency_ms + e.unit_ms + 2.0 * e.spread_ms)
        );
        assert!(slow.timeout_frames(InputKind::WalkTile, 1) > 30);
        assert_eq!(
            slow.timeout_frames(InputKind::WalkTile, 4),
            frames(e.latency_ms + e.unit_ms / 2.0 + 2.0 * e.spread_ms).max(30 + 16 + 8)
        );
        assert!(slow.expect_frames(Some(InputKind::WalkTile)) >= 30);
    }

    #[test]
    fn spread_is_the_mean_absolute_deviation_of_recent_samples() {
        let mut s = Syncer::new("switch");
        // Alternating 250 / 290 per tile: mean 270, deviation 20.
        for i in 0..32 {
            let unit = if i % 2 == 0 { 250.0 } else { 290.0 };
            s.observe_ms(InputKind::WalkTile, 268.0, 2.0 * unit, 3);
        }
        let e = s.estimate(InputKind::WalkTile);
        assert!((e.spread_ms - 20.0).abs() < 0.01, "{e:?}");
        assert!((e.unit_ms - 270.0).abs() < 5.0, "{e:?}");
        // The margin is two spreads on top of the plain hold.
        let plain = e.unit_ms * 3.0 - e.unit_ms / 2.0;
        assert_eq!(
            s.hold_for(InputKind::WalkTile, 3).as_millis(),
            (plain + 40.0).round() as u128
        );
        // Kinds without units learn their latency and its spread.
        for i in 0..16 {
            s.observe_ms(
                InputKind::MenuPress,
                if i % 2 == 0 { 100.0 } else { 140.0 },
                0.0,
                1,
            );
        }
        let m = s.estimate(InputKind::MenuPress);
        assert!((m.spread_ms - 20.0).abs() < 0.01, "{m:?}");
        assert!(m.latency_ms > 100.0 && m.latency_ms < 140.0, "{m:?}");
    }

    #[test]
    fn a_tap_only_teaches_latency() {
        let mut s = Syncer::new("emulator");
        s.observe_ms(InputKind::WalkTile, 268.0 + 40.0, 0.0, 1);
        let e = s.estimate(InputKind::WalkTile);
        assert_eq!(e.unit_ms, 268.0);
        assert!((e.latency_ms - 8.0).abs() < 0.01, "{e:?}");
        assert_eq!(e.spread_ms, 0.0);
        assert_eq!(e.samples, 1);
    }

    #[test]
    fn persistence_round_trip_keeps_other_profiles() {
        let dir = std::env::temp_dir().join(format!("pokebot-timing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("timing.json");
        let mut emu = Syncer::new("emulator");
        for _ in 0..5 {
            emu.observe_ms(InputKind::WalkTile, 300.0, 3.0 * 280.0, 4);
        }
        emu.save(&path).unwrap();
        let mut sw = Syncer::new("switch");
        sw.observe_ms(InputKind::MenuPress, 120.0, 0.0, 1);
        sw.save(&path).unwrap();

        let emu2 = Syncer::load(&path, "emulator").unwrap();
        assert_eq!(emu2.estimates, emu.estimates);
        assert_eq!(
            emu2.hold_for(InputKind::WalkTile, 4),
            emu.hold_for(InputKind::WalkTile, 4)
        );
        let sw2 = Syncer::load(&path, "switch").unwrap();
        assert_eq!(sw2.estimates, sw.estimates);
        assert_eq!(sw2.base_latency_frames(), 30);
        // Unknown profile and missing file: empty models.
        assert!(Syncer::load(&path, "other").unwrap().estimates.is_empty());
        assert!(Syncer::load(&dir.join("none.json"), "emulator")
            .unwrap()
            .estimates
            .is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
