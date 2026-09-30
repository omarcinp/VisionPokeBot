//! DIG out of Mt. Moon through `Go` on the stepped emulator, from the
//! save made just out of it (`saves/mtmoon-done.sav`: Route 4 below the
//! east mouth, GEODUDE with three moves). A **copy** of the save gets TM28
//! in the TM CASE (the offline edit of `field_moves_emulator.rs`); the bot
//! teaches it, walks back into Mt. Moon (the sensor records the escape
//! warp outside the mouth), goes to the far end of B2F, and the way back
//! out to Route 4 digs.
//!
//! Needs the mGBA core, the ROM, `data/world` and the mtmoon-done save;
//! skipped otherwise. Run with
//! `cargo test -p pokebot-agent --release --test escape_emulator -- --ignored --nocapture`.
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

static EMULATOR: Mutex<()> = Mutex::new(());

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

const SECTOR: usize = 4096;
const SIGNATURE: u32 = 0x0801_2025;
/// `SaveBlock1.bagPocket_TMHM` and its 58 slots; `SaveBlock2.encryptionKey`.
const TM_POCKET: usize = 0x464;
const TM_SLOTS: usize = 58;
const ENCRYPTION_KEY: usize = 0xF20;
/// `SaveBlock1.flags`.
const FLAGS: usize = 0xEE0;

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Test fixture: adds `items` (item ids, one each) to the TM CASE of a
/// FireRed save image (the newer of its two slots) and sets `flags` (ids
/// below 0x280, which live in the same sector), fixing the sector
/// checksum. Pure file manipulation; nothing the bot uses.
fn give_tm_case_items(save: &mut [u8], items: &[u16], flags: &[u16]) {
    let counter = |slot: usize| {
        (0..14)
            .map(|i| u32_at(save, slot * 14 * SECTOR + i * SECTOR + 0xFFC))
            .max()
            .unwrap_or(0)
    };
    let slot = if counter(0) >= counter(1) { 0 } else { 1 };
    let sector = |id: u16| {
        (0..14)
            .map(|i| slot * 14 * SECTOR + i * SECTOR)
            .find(|&off| u16_at(save, off + 0xFF4) == id && u32_at(save, off + 0xFF8) == SIGNATURE)
            .expect("save sector")
    };
    let (sb2, sb1) = (sector(0), sector(1));
    let key = u16_at(save, sb2 + ENCRYPTION_KEY);
    for &item in items {
        let slots: Vec<u16> = (0..TM_SLOTS)
            .map(|i| u16_at(save, sb1 + TM_POCKET + 4 * i))
            .collect();
        if slots.contains(&item) {
            continue;
        }
        let free = slots
            .iter()
            .position(|&i| i == 0)
            .expect("a free TM CASE slot");
        let at = sb1 + TM_POCKET + 4 * free;
        save[at..at + 2].copy_from_slice(&item.to_le_bytes());
        save[at + 2..at + 4].copy_from_slice(&(1 ^ key).to_le_bytes());
    }
    for &flag in flags {
        let at = sb1 + FLAGS + usize::from(flag / 8);
        save[at] |= 1 << (flag % 8);
    }
    let mut sum: u32 = 0;
    for i in (0..3968).step_by(4) {
        sum = sum.wrapping_add(u32_at(save, sb1 + i));
    }
    let checksum = ((sum >> 16) as u16).wrapping_add(sum as u16);
    save[sb1 + 0xFF6..sb1 + 0xFF8].copy_from_slice(&checksum.to_le_bytes());
}

const ITEM_TM28: u16 = 316;

#[test]
#[ignore]
fn dig_out_of_mt_moon() {
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
    let Ok(mut save) = std::fs::read(root.join("saves/mtmoon-done.sav")) else {
        eprintln!("skipping: no saves/mtmoon-done.sav");
        return;
    };
    let progress = Progress::load(&root.join("saves/progress.mtmoon-done.json")).expect("progress");
    let state_path = root.join("saves/state.mtmoon-done.json");
    let dir = std::env::temp_dir().join(format!("pokebot-escape-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    give_tm_case_items(&mut save, &[ITEM_TM28], &[]);
    let save_path = dir.join("mtmoon-dig.sav");
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
    .expect("continue the mtmoon-done save");

    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&world),
        Arc::clone(&data),
        &stop,
    );
    let teach = ctx.invoke(&Intent::Teach {
        item: "ITEM_TM28".into(),
        member: "SPECIES_GEODUDE".into(),
    });
    eprintln!("Teach -> {:?}", teach.result);
    assert!(teach.is_ok(), "{:?}", teach.result);

    let go = |ctx: &mut ToolContext<'_>, dest: Dest| {
        let out = ctx.invoke(&Intent::Go { dest });
        eprintln!("Go -> {:?} at {:?}", out.result, out.pose);
        assert!(out.is_ok(), "{:?}", out.result);
        out
    };
    go(
        &mut ctx,
        Dest::Map {
            map: "MtMoon_B1F".into(),
        },
    );
    let escape = ctx
        .state()
        .world
        .escape
        .value
        .clone()
        .expect("the escape warp");
    assert_eq!(
        (escape.map.as_str(), escape.x, escape.y),
        ("Route4", 32, 6),
        "{escape:?}"
    );
    go(
        &mut ctx,
        Dest::Tile {
            map: "MtMoon_B2F".into(),
            x: 19,
            y: 3,
        },
    );
    let out = go(
        &mut ctx,
        Dest::Map {
            map: "Route4".into(),
        },
    );
    assert_eq!(out.pose.expect("located").map, "Route4");
    assert!(
        out.learned.iter().any(|e| matches!(
            e,
            GameEvent::GoalProgress { phase, detail, .. }
                if phase == "FieldMove" && detail.starts_with("dig")
        )),
        "dug out: {:?}",
        out.learned
    );
    drop(ctx);
    let _ = std::fs::remove_dir_all(&dir);
}
