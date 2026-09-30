//! The Nugget Bridge farm on the stepped emulator, from a copy of a
//! scenario-library save made at the Cerulean Center with the five
//! contest trainers beaten and the grunt not
//! (`saves/scenarios/20260928T154356Z-CeruleanCity_PokemonCenter_1F-7-4`:
//! GEODUDE Lv19, CLEFAIRY Lv9, ZUBAT Lv10, IVYSAUR Lv24, PARAS Lv6; ¥8084,
//! no NUGGET). The farm keeps PARAS, stores the others, takes two NUGGETs
//! losing to the grunt (saving after each), sells them, and takes the
//! others back.
//!
//! Needs the mGBA core, the ROM, `data/world` and that scenario; skipped
//! otherwise. Run with
//! `cargo test -p pokebot-agent --release --test nugget_farm_emulator -- --ignored --nocapture`.
//! `VPB_RECORD=<dir>` records the frames (every 4th).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pokebot_agent::goal_session::continue_game;
use pokebot_agent::nugget_farm::{self, FarmConfig, FarmFile, FarmPhase};
use pokebot_agent::tools::ToolContext;
use pokebot_agent::{Executor, Progress, Syncer};
use pokebot_emulator_libretro::{launch, EmulatorConfig};
use pokebot_gamedata::GameData;
use pokebot_runtime::{Devices, Runtime};
use pokebot_video::{Normalizer, ViewportLocator};
use pokebot_vision::FireRedPerception;
use pokebot_world::World;

const SCENARIO: &str = "saves/scenarios/20260928T154356Z-CeruleanCity_PokemonCenter_1F-7-4";

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

fn party(ctx: &ToolContext<'_>) -> Vec<String> {
    ctx.state()
        .party
        .value
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|m| m.species.value.clone().unwrap_or_default())
        .collect()
}

#[test]
#[ignore]
fn farm_two_nuggets_sell_them_and_take_the_party_back() {
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
    let dir = std::env::temp_dir().join(format!("pokebot-farm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let save_path = dir.join("game.sav");
    std::fs::write(&save_path, &save).unwrap();
    std::fs::copy(scenario.join("state.json"), dir.join("state.json")).unwrap();

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
        &dir.join("state.json"),
        &data,
        &stop,
        None,
    )
    .expect("continue the scenario save");
    let money_before = runtime.state().money.value.expect("money known");

    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&world),
        Arc::clone(&data),
        &stop,
    );
    let before = party(&ctx);
    assert_eq!(before.len(), 5, "{before:?}");
    let config = FarmConfig {
        nuggets: 2,
        save_every: 1,
        file: dir.join("farm.json"),
    };
    let done = nugget_farm::run(&mut ctx, &config);
    eprintln!("farm -> {done:?}");
    let file = FarmFile::load(&config.file).unwrap();
    eprintln!("farm file: {file:?}");
    assert!(done.is_ok(), "{done:?}");
    assert_eq!(file.phase, FarmPhase::Done);
    assert_eq!(file.farmed, 2);
    assert_eq!(file.sold, 2);
    assert_eq!(file.earned, 10_000);
    assert_eq!(file.keeper.as_deref(), Some("SPECIES_PARAS"));
    // The party is whole again, whatever its order.
    let mut after = party(&ctx);
    let mut want = before.clone();
    after.sort();
    want.sort();
    assert_eq!(after, want);
    assert_eq!(nugget_farm::nuggets_held(ctx.state()), Some(0));
    let money = ctx.state().money.value.expect("money known");
    eprintln!(
        "money ¥{money_before} → ¥{money} (white-outs cost ¥{})",
        file.lost
    );
    assert!(money > money_before + 9_000, "¥{money_before} → ¥{money}");
    drop(ctx);
    let _ = std::fs::remove_dir_all(&dir);
}
