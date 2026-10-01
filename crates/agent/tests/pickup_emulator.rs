//! An item ball picked up on the way, on the stepped emulator: from a
//! scenario-library save of the Geodude hunt on Mt. Moon 1F (18, 34)
//! (`saves/scenarios/20260928T043725Z-MtMoon_1F-18-34`: no ball of the
//! floor taken), a `Go` up the corridor with the scheduler on detours to
//! the TM09 ball at (11, 35), takes it ("RED found a TM09!"), records its
//! hide flag, and walks on.
//!
//! Needs the mGBA core, the ROM, `data/world` and that scenario; skipped
//! otherwise. Run with
//! `cargo test -p pokebot-agent --test pickup_emulator -- --ignored --nocapture`.
//! `VPB_RECORD=<dir>` records the frames (every 4th).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pokebot_agent::goal_session::continue_game;
use pokebot_agent::tools::{Dest, Intent, ToolContext};
use pokebot_agent::{Executor, Progress, Syncer};
use pokebot_emulator_libretro::{launch, EmulatorConfig};
use pokebot_gamedata::GameData;
use pokebot_runtime::{Devices, Runtime};
use pokebot_state::GameEvent;
use pokebot_video::{Normalizer, ViewportLocator};
use pokebot_vision::FireRedPerception;
use pokebot_world::World;

const SCENARIO: &str = "saves/scenarios/20260928T043725Z-MtMoon_1F-18-34";
const FLAG: &str = "FLAG_HIDE_MT_MOON_1F_TM09";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn core_and_rom() -> Option<(PathBuf, PathBuf)> {
    let root = root();
    let core = std::env::var_os("VPB_CORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("emulator/mgba_libretro.so"));
    let rom = std::env::var_os("VPB_ROM").map(PathBuf::from).or_else(|| {
        let mut roms: Vec<PathBuf> = std::fs::read_dir(root.join("roms"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gba")))
            .collect();
        roms.sort();
        roms.into_iter().next()
    })?;
    (core.exists() && rom.exists()).then_some((core, rom))
}

#[test]
#[ignore]
fn a_tm_ball_off_the_way_is_picked_up() {
    let root = root();
    let Some((core, rom)) = core_and_rom() else {
        eprintln!("skipping: emulator core or ROM missing");
        return;
    };
    let world_dir = root.join("data/world");
    let (Ok(world), Ok(data), Ok(font), Ok(small)) = (
        World::load(&world_dir),
        GameData::load(world_dir.join("gamedata.json")),
        pokebot_vision::text::Font::load(world_dir.join("font_normal.json")),
        pokebot_vision::text::Font::load(world_dir.join("font_small.json")),
    ) else {
        eprintln!("skipping: no world data");
        return;
    };
    let scenario = root.join(SCENARIO);
    let (Ok(save), Ok(progress)) = (
        std::fs::read(scenario.join("game.sav")),
        Progress::load(&scenario.join("progress.json")),
    ) else {
        eprintln!("skipping: no {SCENARIO}");
        return;
    };
    let dir = std::env::temp_dir().join(format!("pokebot-pickup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let save_path = dir.join("game.sav");
    std::fs::write(&save_path, &save).unwrap();
    let state_path = dir.join("state.json");
    if let Ok(state) = std::fs::read(scenario.join("state.json")) {
        std::fs::write(&state_path, state).unwrap();
    }

    let world = Arc::new(world);
    let data = Arc::new(data);
    let mut config = EmulatorConfig::new(core, rom);
    config.battery_save = Some(save_path);
    let (video, controller, _) = launch(config).expect("emulator");
    let perception = FireRedPerception::with_world(Arc::clone(&world))
        .with_font(Arc::new(font))
        .with_small_font(Arc::new(small));
    let mut runtime = Runtime::with_perception(
        Devices {
            video: Box::new(video),
            controller: Box::new(controller),
            normalizer: Normalizer::new(ViewportLocator::FullFrame),
            video_name: "emulator".into(),
            controller_name: "emulator".into(),
            persist_save: None,
        },
        perception,
    )
    .with_sensor(pokebot_sense::Sensor::new(Arc::clone(&data)).with_world(Arc::clone(&world)));
    if let Some(rec) = std::env::var_os("VPB_RECORD") {
        runtime
            .record_to(Path::new(&rec), false, 4)
            .expect("record");
    }
    runtime.echo_events(std::env::var_os("VPB_ECHO").is_some());
    let executor = Executor {
        syncer: Some(Arc::new(Mutex::new(Syncer::new("emulator")))),
        ..Executor::default()
    };
    let stop = AtomicBool::new(false);
    continue_game(
        &mut runtime,
        &executor,
        &progress,
        &state_path,
        &data,
        &stop,
        None,
    )
    .expect("continue the scenario save");

    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&world),
        Arc::clone(&data),
        &stop,
    );
    assert_ne!(
        ctx.state().world.flags.get(FLAG).and_then(|k| k.value),
        Some(true),
        "the ball is already taken in this save"
    );
    ctx.scheduler.enabled = true;
    // North up the corridor; the ball is west, round the rock.
    let go = ctx.invoke(&Intent::Go {
        dest: Dest::Tile {
            map: "MtMoon_1F".into(),
            x: 18,
            y: 27,
        },
    });
    eprintln!("Go -> {:?}", go.result);
    let found = go.learned.iter().any(|e| {
        matches!(e, GameEvent::ItemsChanged { item, delta, .. } if item == "ITEM_TM09" && *delta > 0)
    });
    assert!(found, "TM09 found on the way");
    assert_eq!(
        ctx.state().world.flags.get(FLAG).and_then(|k| k.value),
        Some(true),
        "the ball's hide flag recorded"
    );
    assert!(ctx
        .scheduler
        .pickups_tried
        .contains(&("MtMoon_1F".to_owned(), 9)));
    assert!(go.is_ok(), "{:?}", go.result);
    let _ = std::fs::remove_dir_all(&dir);
}
