//! `Teach` and a Cut through `Go` on the stepped emulator, from the Cascade
//! Badge save (`saves/cascade.sav`: Cerulean Gym, IVYSAUR with four moves).
//!
//! No save in `saves/` has HM01 yet (it comes from the S.S. Anne), so the
//! test prepares a **copy** of the save with HM01 and HM05 put into the
//! TM CASE and the S.S. Ticket flag set (HM01 implies it), an offline edit
//! of the `.sav` file with the sector checksum recomputed
//! ([`give_tm_case_items`]). The bot never reads the save: it sees the
//! TM CASE on screen like any other.
//!
//! Needs the mGBA core, the ROM, `data/world` and the cascade save;
//! skipped otherwise. Slow (a minute or two in release); run with
//! `cargo test -p pokebot-agent --release --test field_moves_emulator -- --ignored --nocapture`.
//! `VPB_RECORD=<dir>` records the frames (every 4th).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pokebot_agent::goal_session::continue_game;
use pokebot_agent::tools::{Dest, Intent, ProbeFact, ToolContext};
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

const ITEM_HM01: u16 = 339;
const ITEM_HM05: u16 = 343;
/// HM01 comes from the S.S. Anne, which needs the ticket: a save with HM01
/// has this flag. Without it Cerulean's exits stay blocked (the SLOWBRO
/// stands in front of the cut tree, `CeruleanCity_EventScript_BlockExits`).
const FLAG_GOT_SS_TICKET: u16 = 0x234;

#[test]
#[ignore]
fn teach_hm01_then_cut_the_cerulean_tree() {
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
    let Ok(mut save) = std::fs::read(root.join("saves/cascade.sav")) else {
        eprintln!("skipping: no saves/cascade.sav");
        return;
    };
    let progress = Progress::load(&root.join("saves/progress.cascade.json")).expect("progress");
    let state_path = root.join("saves/state.cascade.json");
    let dir = std::env::temp_dir().join(format!("pokebot-field-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    give_tm_case_items(&mut save, &[ITEM_HM01, ITEM_HM05], &[FLAG_GOT_SS_TICKET]);
    let save_path = dir.join("cascade-hm.sav");
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
    continue_game(
        &mut runtime,
        &executor,
        &progress,
        &state_path,
        &data,
        &stop,
        None,
    )
    .expect("continue the cascade save");

    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&world),
        Arc::clone(&data),
        &stop,
    );
    // The badges are facts the Cut route depends on: read them off the
    // Trainer Card like any probe.
    let card = ctx.invoke(&Intent::Probe {
        fact: ProbeFact::TrainerCard,
    });
    assert!(card.is_ok(), "trainer card: {:?}", card.result);

    // Teach HM01 to IVYSAUR (TACKLE, SLEEP POWDER, RAZOR LEAF, VINE WHIP):
    // TACKLE is forgotten.
    let teach = ctx.invoke(&Intent::Teach {
        item: "ITEM_HM01".into(),
        member: "SPECIES_IVYSAUR".into(),
    });
    eprintln!("Teach -> {:?}", teach.result);
    assert!(teach.is_ok(), "{:?}", teach.result);
    let replaced = teach.learned.iter().find_map(|e| match e {
        GameEvent::MoveReplaced { slot, old, new, .. } => Some((*slot, old.clone(), new.clone())),
        _ => None,
    });
    assert_eq!(
        replaced,
        Some((0, "MOVE_TACKLE".to_owned(), "MOVE_CUT".to_owned()))
    );
    let ivysaur = &ctx.state().party.value.as_ref().unwrap()[0];
    let moves: Vec<_> = ivysaur
        .moves
        .iter()
        .flatten()
        .filter_map(|m| m.mv.value.clone())
        .collect();
    assert_eq!(moves[0], "MOVE_CUT", "{moves:?}");
    assert_eq!(ivysaur.moves[0].as_ref().unwrap().pp.value, Some((30, 30)));

    // Walk to the tile south of Cerulean's cut tree: the route goes
    // through the tree, and Go cuts it.
    let go = ctx.invoke(&Intent::Go {
        dest: Dest::Tile {
            map: "CeruleanCity".into(),
            x: 26,
            y: 33,
        },
    });
    eprintln!("Go -> {:?} at {:?}", go.result, go.pose);
    assert!(go.is_ok(), "{:?}", go.result);
    let at = go.pose.expect("located");
    assert_eq!((at.map.as_str(), at.x, at.y), ("CeruleanCity", 26, 33));
    assert!(
        go.learned.iter().any(|e| matches!(
            e,
            GameEvent::GoalProgress { phase, detail, .. }
                if phase == "FieldMove" && detail.contains("CUT used")
        )),
        "the tree was cut: {:?}",
        go.learned
    );

    // HM05 to CLEFAIRY (POUND, GROWL, ENCORE): into the free fourth slot.
    let teach = ctx.invoke(&Intent::Teach {
        item: "ITEM_HM05".into(),
        member: "SPECIES_CLEFAIRY".into(),
    });
    assert!(teach.is_ok(), "{:?}", teach.result);
    assert!(
        teach.learned.iter().any(|e| matches!(
            e,
            GameEvent::MoveLearned { slot: 4, move_slot: 3, mv, max_pp: 20 } if mv == "MOVE_FLASH"
        )),
        "{:?}",
        teach.learned
    );
    // FLASH from the party menu (CLEFAIRY's action window): Cerulean is
    // not dark, so the game refuses; the tool reads that and closes the
    // menus.
    let flash = ctx.invoke(&Intent::FieldMove {
        mv: "MOVE_FLASH".into(),
        at: None,
        push: Vec::new(),
    });
    eprintln!("Flash -> {:?}", flash.result);
    assert!(
        flash.result.is_err(),
        "Flash is refused outside a dark cave"
    );
    let o = ctx.observe().expect("observe");
    assert!(o.party_menu.is_none() && o.menu.is_none(), "menus closed");
    drop(ctx);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The fixture edit: HM01 lands in a free TM CASE slot, with a valid
/// sector checksum.
#[test]
fn the_save_fixture_gets_hm01() {
    let Ok(mut save) = std::fs::read(root().join("saves/cascade.sav")) else {
        return;
    };
    give_tm_case_items(&mut save, &[ITEM_HM01], &[FLAG_GOT_SS_TICKET]);
    let before = save.clone();
    give_tm_case_items(&mut save, &[ITEM_HM01], &[FLAG_GOT_SS_TICKET]);
    assert_eq!(save, before, "adding it twice changes nothing");
}
