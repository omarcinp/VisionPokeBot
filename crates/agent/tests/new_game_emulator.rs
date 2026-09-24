//! Acceptance test: from power-on, the bot starts a new game using only video
//! and controller input, and ends in control of the character.
//!
//! Needs the mGBA core (`tools/fetch-emulator.sh`) and a FireRed ROM in
//! `roms/` (or `$VPB_ROM`); skipped otherwise.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use pokebot_agent::{Executor, NewGameConfig, NewGameTask};
use pokebot_emulator_libretro::{launch, EmulatorConfig};
use pokebot_runtime::{Devices, Runtime};
use pokebot_state::{Gender, KnowledgeSource};
use pokebot_video::{Normalizer, ViewportLocator};

static EMULATOR: Mutex<()> = Mutex::new(());

fn fixtures() -> Option<(PathBuf, PathBuf)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
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
    (core.exists() && rom.exists())
        .then_some((core, rom))
        .or_else(|| {
            eprintln!("skipping: emulator core or ROM missing");
            None
        })
}

fn play(config: NewGameConfig) -> Option<pokebot_state::GameState> {
    let (core, rom) = fixtures()?;
    // No battery save: always a fresh cartridge. Stepped clock: deterministic.
    let (video, controller, _) = launch(EmulatorConfig::new(core, rom)).expect("emulator");
    let mut runtime = Runtime::new(Devices {
        video: Box::new(video),
        controller: Box::new(controller),
        normalizer: Normalizer::new(ViewportLocator::FullFrame),
        video_name: "emulator".into(),
        controller_name: "emulator".into(),
        persist_save: None,
    });
    let mut task = NewGameTask::new(config).unwrap();
    Executor::default()
        .run(&mut runtime, &mut task, &AtomicBool::new(false))
        .expect("new game completes");
    Some(runtime.state().clone())
}

#[test]
fn boy_with_preset_rival() {
    let _guard = EMULATOR.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = play(NewGameConfig::default()) else {
        return;
    };
    let p = &state.progression;
    assert_eq!(p.player_name.value.as_deref(), Some("RED"));
    assert_eq!(p.rival_name.value.as_deref(), Some("GREEN"));
    assert_eq!(p.gender.value, Some(Gender::Boy));
    assert_eq!(p.in_control.value, Some(true));
    assert_eq!(p.in_control.source, KnowledgeSource::Observed);
    assert!(state.active_goal.is_none());
}

#[test]
fn girl_with_typed_names() {
    let _guard = EMULATOR.lock().unwrap_or_else(|e| e.into_inner());
    let config = NewGameConfig {
        gender: Gender::Girl,
        player_name: "LEAF".into(),
        rival_name: "ASHKETC".into(), // 7 letters: max length, not a preset
        soft_reset: true,
    };
    let Some(state) = play(config) else { return };
    let p = &state.progression;
    assert_eq!(p.player_name.value.as_deref(), Some("LEAF"));
    assert_eq!(p.rival_name.value.as_deref(), Some("ASHKETC"));
    assert_eq!(p.gender.value, Some(Gender::Girl));
    assert_eq!(p.in_control.value, Some(true));
}

/// New game through delivering Oak's Parcel: overworld localization, walking,
/// doors, map edges, cutscenes, the rival battle and wild battles on Route 1.
/// Slow (~2 min in release); run with `cargo test --release -- --ignored`.
#[test]
#[ignore]
fn story_from_new_game_to_parcel_delivered() {
    let _guard = EMULATOR.lock().unwrap_or_else(|e| e.into_inner());
    let Some((core, rom)) = fixtures() else {
        return;
    };
    let world_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
    let Ok(world) = pokebot_world::World::load(&world_dir) else {
        eprintln!("skipping: no world model (run tools/world/build.sh)");
        return;
    };
    let world = std::sync::Arc::new(world);
    // Battles need the game data and the fonts, as in `pokebot story`.
    let (Ok(data), Ok(font), Ok(small_font)) = (
        pokebot_gamedata::GameData::load(world_dir.join("gamedata.json")),
        pokebot_vision::text::Font::load(world_dir.join("font_normal.json")),
        pokebot_vision::text::Font::load(world_dir.join("font_small.json")),
    ) else {
        eprintln!("skipping: no game data or fonts (run tools/world/build.sh)");
        return;
    };
    let (video, controller, _) = launch(EmulatorConfig::new(core, rom)).expect("emulator");
    let perception = pokebot_vision::FireRedPerception::with_world(std::sync::Arc::clone(&world))
        .with_font(std::sync::Arc::new(font))
        .with_small_font(std::sync::Arc::new(small_font));
    let data = std::sync::Arc::new(data);
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
    let stop = AtomicBool::new(false);
    let executor = Executor::default();
    executor
        .run(
            &mut runtime,
            &mut NewGameTask::new(NewGameConfig::default()).unwrap(),
            &stop,
        )
        .expect("new game");
    runtime.set_pose_hint(pokebot_state::PlayerPose {
        map: "PalletTown_PlayersHouse_2F".into(),
        x: 6,
        y: 6,
    });
    // As `pokebot story` does: the starter is known to be received at Lv5.
    runtime
        .emit(pokebot_state::GameEvent::PartyMonDerived {
            slot: 0,
            mon: Box::new(pokebot_agent::party::starter_mon(
                &data,
                pokebot_agent::Starter::Bulbasaur.species(),
                5,
            )),
        })
        .unwrap();
    let mut story = pokebot_agent::StoryTask::new(
        world,
        pokebot_agent::opening(pokebot_agent::Starter::Bulbasaur),
    )
    .with_data(data);
    executor
        .run(&mut runtime, &mut story, &stop)
        .expect("story");
    let pose = runtime.state().player.pose.value.clone().expect("located");
    assert_eq!(pose.map, "PalletTown_ProfessorOaksLab");
}
