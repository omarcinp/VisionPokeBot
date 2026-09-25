//! Acceptance test: from a fresh new game to the Pokédex purely through
//! the goal loop. `NewGameTask` only does the menus (title, intro, names);
//! from the first overworld frame the planner derives the opening from the
//! compiled events (Oak's trigger at the edge of Pallet Town, the lab's
//! starter scene, a ball on the table with YES, the rival battle at the
//! exit, the parcel scene in the Viridian Mart, Oak's parcel path) and the
//! tools carry it out. No `StoryTask` milestone runs.
//!
//! Needs the mGBA core, a FireRed ROM (`roms/` or `$VPB_ROM`) and
//! `data/world`; skipped otherwise. Slow (several minutes in release); run
//! with `cargo test -p pokebot-agent --release --test opening_emulator --
//! --ignored --nocapture`. `VPB_RECORD=<dir>` records every 4th frame,
//! `VPB_ECHO=1` echoes the events.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pokebot_agent::goal::{self, GoalOptions};
use pokebot_agent::goal_session::{bedroom, new_game_knowledge};
use pokebot_agent::tools::ToolContext;
use pokebot_agent::{Executor, NewGameConfig, NewGameTask, Syncer};
use pokebot_emulator_libretro::{launch, EmulatorConfig};
use pokebot_gamedata::GameData;
use pokebot_planner::{parse_goal, Methods, Obtain, PlanOptions, Planner};
use pokebot_runtime::{Devices, Runtime};
use pokebot_state::{GameEvent, Priors};
use pokebot_video::{Normalizer, ViewportLocator};
use pokebot_vision::FireRedPerception;
use pokebot_world::route::{PlaceGraph, RouteParams};
use pokebot_world::World;

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
fn a_new_game_reaches_the_pokedex_through_the_goal_loop() {
    static EMULATOR: Mutex<()> = Mutex::new(());
    let _guard = EMULATOR.lock().unwrap_or_else(|e| e.into_inner());
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
    if world.events().is_none() {
        eprintln!("skipping: no events.json");
        return;
    }
    let obtain = Obtain::load(&world_dir).ok();
    let priors = Priors::load(root.join("data/rules/priors.json")).ok();
    let methods = Methods::load(root.join("data/rules/methods.json")).unwrap_or_default();
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let world = Arc::new(world);
    let data = Arc::new(data);

    // A fresh cartridge (no battery save), stepped clock: deterministic.
    let (video, controller, _) = launch(EmulatorConfig::new(core, rom)).expect("emulator");
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
    );
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

    // Menus only: title, intro, gender, names.
    executor
        .run(
            &mut runtime,
            &mut NewGameTask::new(NewGameConfig::default()).unwrap(),
            &stop,
        )
        .expect("new game");
    runtime.set_pose_hint(bedroom());
    runtime
        .emit(GameEvent::CheckpointRestored {
            knowledge: Box::new(new_game_knowledge(&world)),
        })
        .unwrap();

    let options = PlanOptions {
        supported_probes: Some(pokebot_agent::tools::Toolbox::supported_probes()),
        prefer_species: vec!["SPECIES_BULBASAUR".into()],
        ..PlanOptions::default()
    };
    let planner = Planner::new(
        &world,
        &graph,
        &data,
        obtain.as_ref(),
        priors.as_ref(),
        &methods,
        options,
    );
    let goal = parse_goal("flag FLAG_SYS_POKEDEX_GET").unwrap();
    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&world),
        Arc::clone(&data),
        &stop,
    );
    ctx.scheduler.enabled = true;
    let report = goal::run(
        &goal,
        &mut ctx,
        &planner,
        GoalOptions {
            max_replans: 16,
            ..GoalOptions::default()
        },
    )
    .expect("goal loop");
    for (i, p) in report.plans.iter().enumerate() {
        let steps: Vec<String> = p
            .plan
            .intents
            .iter()
            .map(|s| s.intent.to_string())
            .collect();
        eprintln!("plan {} ({}): {}", i + 1, p.reason, steps.join(" | "));
    }
    for f in &report.failures {
        eprintln!("failed: {f}");
    }
    eprintln!(
        "{}: {} in {:.0} s, {} steps",
        report.goal, report.outcome, report.elapsed_s, report.steps_run
    );
    let state = ctx.state();
    assert!(report.satisfied, "{}", report.outcome);
    assert_eq!(
        state.world.flag("FLAG_SYS_POKEDEX_GET").value,
        Some(true),
        "the Pokédex flag is believed"
    );
    // The scene vars the opening moved on are in the belief a checkpoint
    // would carry.
    let var = |v: &str| state.world.var(v).value;
    assert_eq!(var("VAR_MAP_SCENE_PALLET_TOWN_OAK"), Some(1));
    assert_eq!(var("VAR_MAP_SCENE_VIRIDIAN_CITY_MART"), Some(2));
    assert_eq!(var("VAR_MAP_SCENE_VIRIDIAN_CITY_OLD_MAN"), Some(1));
    let party = state.party.value.as_ref().expect("party known");
    assert_eq!(
        party[0].species.value.as_deref(),
        Some("SPECIES_BULBASAUR"),
        "the preferred starter"
    );
}
