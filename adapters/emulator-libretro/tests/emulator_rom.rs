//! End-to-end checks against the real core and a user-supplied ROM.
//!
//! Skipped (with a message) unless both are present:
//! * core: `$VPB_CORE` or `emulator/mgba_libretro.so` (run `tools/fetch-emulator.sh`)
//! * ROM:  `$VPB_ROM` or the first `*.gba` in `roms/`

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use pokebot_core::{
    Button, Controller, ControllerCommand, VideoSource, CANONICAL_HEIGHT, CANONICAL_WIDTH,
};
use pokebot_emulator_libretro::{
    launch, ClockMode, EmulatorConfig, EmulatorController, EmulatorVideoSource,
};

/// Libretro cores are process-global; run these tests one at a time.
static EMULATOR: Mutex<()> = Mutex::new(());

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture_paths() -> Option<(PathBuf, PathBuf)> {
    let core = std::env::var_os("VPB_CORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("emulator/mgba_libretro.so"));
    let rom = std::env::var_os("VPB_ROM").map(PathBuf::from).or_else(|| {
        let mut roms: Vec<PathBuf> = std::fs::read_dir(root().join("roms"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gba")))
            .collect();
        roms.sort();
        roms.into_iter().next()
    })?;
    if core.exists() && rom.exists() {
        Some((core, rom))
    } else {
        eprintln!("skipping: need core at {} and a ROM", core.display());
        None
    }
}

fn start(clock: ClockMode) -> Option<(EmulatorVideoSource, EmulatorController)> {
    let (core, rom) = fixture_paths()?;
    let mut config = EmulatorConfig::new(core, rom);
    config.clock = clock;
    let (video, controller, info) = launch(config).expect("emulator starts");
    assert_eq!(
        (info.width, info.height),
        (CANONICAL_WIDTH, CANONICAL_HEIGHT)
    );
    assert!((info.fps - 59.7275).abs() < 0.01, "fps {}", info.fps);
    Some((video, controller))
}

/// Advances `frames` frames and returns the fingerprint of the last one.
fn run(video: &mut EmulatorVideoSource, frames: u64) -> u64 {
    let mut last = None;
    for _ in 0..frames {
        last = Some(video.next_frame().expect("frame"));
    }
    last.expect("at least one frame").image.fingerprint()
}

/// Boot, then tap Start/A a few times: gets past the intro to the title/menu.
fn scripted_run(with_input: bool) -> Vec<u64> {
    let (mut video, mut controller) = start(ClockMode::Stepped).unwrap();
    let mut prints = vec![run(&mut video, 600)];
    for _ in 0..4 {
        if with_input {
            controller
                .execute(ControllerCommand::Press(Button::Start))
                .unwrap();
        }
        prints.push(run(&mut video, 120));
    }
    prints
}

#[test]
fn stepped_frames_are_canonical_and_sequential() {
    let _guard = EMULATOR.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut video, _controller)) = start(ClockMode::Stepped) else {
        return;
    };
    let mut lit = false;
    for expected_id in 0..400 {
        let frame = video.next_frame().unwrap();
        assert_eq!(frame.frame_id, expected_id);
        assert_eq!((frame.image.width(), frame.image.height()), (240, 160));
        lit |= frame.image.as_bytes().iter().any(|b| *b > 32);
    }
    assert!(lit, "the game never drew anything");
}

#[test]
fn identical_inputs_give_identical_video_and_inputs_matter() {
    let _guard = EMULATOR.lock().unwrap_or_else(|e| e.into_inner());
    if fixture_paths().is_none() {
        return;
    }
    let first = scripted_run(true);
    let second = scripted_run(true);
    let idle = scripted_run(false);
    assert_eq!(first, second, "emulation is not deterministic");
    assert_eq!(first[0], idle[0], "runs diverged before any input");
    assert_ne!(
        first.last(),
        idle.last(),
        "controller input had no visible effect"
    );
}

#[test]
fn controller_reports_idle_after_input_is_consumed() {
    let _guard = EMULATOR.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut video, mut controller)) = start(ClockMode::Stepped) else {
        return;
    };
    assert!(controller.is_idle().unwrap());
    let receipt = controller
        .execute(ControllerCommand::Hold {
            buttons: Button::A.into(),
            duration: Duration::from_millis(100),
        })
        .unwrap();
    assert!(receipt.input_duration >= Duration::from_millis(100));
    assert!(!controller.is_idle().unwrap());
    run(&mut video, 6);
    assert!(controller.is_idle().unwrap());
}

#[test]
fn realtime_clock_runs_near_console_speed() {
    let _guard = EMULATOR.lock().unwrap_or_else(|e| e.into_inner());
    let Some((mut video, _controller)) = start(ClockMode::RealTime) else {
        return;
    };
    let first = video.next_frame().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let later = video.next_frame().unwrap();
    let produced = later.frame_id - first.frame_id;
    assert!((20..=45).contains(&produced), "{produced} frames in ~0.5 s");
}

#[test]
fn only_one_emulator_per_process() {
    let _guard = EMULATOR.lock().unwrap_or_else(|e| e.into_inner());
    let Some((core, rom)) = fixture_paths() else {
        return;
    };
    let _running = launch(EmulatorConfig::new(&core, &rom)).unwrap();
    assert!(launch(EmulatorConfig::new(&core, &rom)).is_err());
}
