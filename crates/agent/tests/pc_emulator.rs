//! `PcSwap` on the stepped emulator: two real swaps at the Cerulean
//! Center's PC, from a copy of a scenario-library save made there
//! (`saves/scenarios/20260928T213755Z-CeruleanCity_PokemonCenter_1F-7-4`:
//! GEODUDE, ZUBAT, CHARMELEON, MANKEY, PARAS, CLEFAIRY in the party, EKANS
//! and RATTATA in BOX1). The save's `state.json` is not restored, so the
//! bot starts knowing neither the party nor the boxes: the first swap
//! reads the party, then BOX1 cell by cell; the second goes by what the
//! first left in the belief.
//!
//! Needs the mGBA core, the ROM, `data/world` and that scenario; skipped
//! otherwise. Run with
//! `cargo test -p pokebot-agent --release --test pc_emulator -- --ignored --nocapture`.
//! `VPB_RECORD=<dir>` records the frames (every 4th).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pokebot_agent::goal_session::continue_game;
use pokebot_agent::tools::{Intent, ToolContext};
use pokebot_agent::{Executor, Progress, Syncer};
use pokebot_emulator_libretro::{launch, EmulatorConfig};
use pokebot_gamedata::GameData;
use pokebot_runtime::{Devices, Runtime};
use pokebot_state::GameEvent;
use pokebot_video::{Normalizer, ViewportLocator};
use pokebot_vision::FireRedPerception;
use pokebot_world::World;

const SCENARIO: &str = "saves/scenarios/20260928T213755Z-CeruleanCity_PokemonCenter_1F-7-4";

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

fn box1(ctx: &ToolContext<'_>) -> Vec<(u8, String)> {
    let mut v: Vec<(u8, String)> = ctx.state().pc.boxes[0]
        .value
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|m| (m.slot, m.species.value.clone().unwrap_or_default()))
        .collect();
    v.sort();
    v
}

#[test]
#[ignore]
fn swap_party_members_with_boxed_pokemon_at_the_cerulean_pc() {
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
    let dir = std::env::temp_dir().join(format!("pokebot-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let save_path = dir.join("game.sav");
    std::fs::write(&save_path, &save).unwrap();

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
    // No state.json: nothing is known but what the screens show.
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

    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&world),
        Arc::clone(&data),
        &stop,
    );

    // PARAS (slot 4) into BOX1, EKANS out: BOX1 is read cell by cell.
    let swap = ctx.invoke(&Intent::PcSwap {
        deposit: "SPECIES_PARAS".into(),
        withdraw: "SPECIES_EKANS".into(),
    });
    eprintln!("PcSwap 1 -> {:?}", swap.result);
    assert!(swap.is_ok(), "{:?}", swap.result);
    let observed = swap.learned.iter().find_map(|e| match e {
        GameEvent::BoxObserved { box_index: 0, mons } => Some(mons.len()),
        _ => None,
    });
    assert_eq!(observed, Some(3), "BOX1 read with PARAS in it");
    assert!(swap.learned.iter().any(|e| matches!(
        e,
        GameEvent::MonDeposited {
            party_slot: 4,
            box_index: 0
        }
    )));
    assert!(swap
        .learned
        .iter()
        .any(|e| matches!(e, GameEvent::MonWithdrawn { box_index: 0, .. })));
    assert_eq!(
        party(&ctx),
        [
            "SPECIES_GEODUDE",
            "SPECIES_ZUBAT",
            "SPECIES_CHARMELEON",
            "SPECIES_MANKEY",
            "SPECIES_CLEFAIRY",
            "SPECIES_EKANS"
        ]
    );
    let boxed = box1(&ctx);
    assert!(
        boxed.iter().any(|(_, s)| s == "SPECIES_PARAS")
            && boxed.iter().any(|(_, s)| s == "SPECIES_RATTATA")
            && !boxed.iter().any(|(_, s)| s == "SPECIES_EKANS"),
        "{boxed:?}"
    );

    // Back again: CLEFAIRY in, PARAS out, by the cell the belief holds.
    let swap = ctx.invoke(&Intent::PcSwap {
        deposit: "SPECIES_CLEFAIRY".into(),
        withdraw: "SPECIES_PARAS".into(),
    });
    eprintln!("PcSwap 2 -> {:?}", swap.result);
    assert!(swap.is_ok(), "{:?}", swap.result);
    assert_eq!(
        party(&ctx),
        [
            "SPECIES_GEODUDE",
            "SPECIES_ZUBAT",
            "SPECIES_CHARMELEON",
            "SPECIES_MANKEY",
            "SPECIES_EKANS",
            "SPECIES_PARAS"
        ]
    );
    let boxed = box1(&ctx);
    assert!(
        boxed.iter().any(|(_, s)| s == "SPECIES_CLEFAIRY")
            && !boxed.iter().any(|(_, s)| s == "SPECIES_PARAS"),
        "{boxed:?}"
    );
    let o = ctx.observe().expect("observe");
    assert!(
        o.player.is_some() && o.menu.is_none() && o.pc_storage.is_none(),
        "back in the overworld"
    );
    drop(ctx);
    let _ = std::fs::remove_dir_all(&dir);
}
