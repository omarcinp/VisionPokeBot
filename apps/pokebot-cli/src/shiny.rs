//! `pokebot shiny-starter`: hunts a shiny starter from a save made in Oak's
//! lab in front of the chosen Poké Ball (`--prepare` makes it). See
//! `docs/shiny-starter.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use pokebot_agent::nav::Destination;
use pokebot_agent::shiny_plan::{Hunt, HuntConfig, Plan, Sighting, Timing};
use pokebot_agent::shiny_starter::{AttemptResult, HuntFile, ResetKind, StarterAttemptTask};
use pokebot_agent::{
    ContinueTask, Executor, Milestone, SaveGameTask, Starter, StoryStep, StoryTask,
};
use pokebot_gamedata::rng::{Method1Mon, TrainerIds};
use pokebot_runtime::Runtime;
use pokebot_state::ShinyReading;
use pokebot_vision::FireRedPerception;

use crate::devices::{self, DeviceArgs};
use crate::{attach_outputs, sprite_palettes, OutputArgs, TimingKeeper, CONSOLE_SNAPSHOTS};

#[derive(Debug, clap::Args)]
pub struct ShinyArgs {
    #[command(flatten)]
    pub devices: DeviceArgs,
    #[command(flatten)]
    pub output: OutputArgs,
    #[arg(long, value_enum, default_value_t = Starter::Charmander)]
    pub starter: Starter,
    /// World model directory (tools/world/build.sh)
    #[arg(long, default_value = "data/world")]
    pub world: PathBuf,
    /// Continue the save (made in Oak's lab after he tells you to choose),
    /// stand in front of the starter's ball, save there, then hunt
    #[arg(long)]
    pub prepare: bool,
    /// Cartridge save to read the trainer IDs from (default: the emulator's
    /// --save, or the ROM's .sav)
    #[arg(long)]
    pub sav: Option<PathBuf>,
    /// Trainer ID and secret ID, instead of reading the save
    #[arg(long, requires = "sid")]
    pub tid: Option<u16>,
    #[arg(long, requires = "tid")]
    pub sid: Option<u16>,
    /// Ignore the IDs: spread the timings (full odds, 1/8192 per attempt)
    #[arg(long)]
    pub blind: bool,
    /// Hunt state: scouted timings and every attempt, kept across runs
    #[arg(long, default_value = "saves/shiny-starter/hunt.json")]
    pub hunt: PathBuf,
    /// Frames a timed press may land off (default: 0 on the stepped
    /// emulator, 2 on real hardware)
    #[arg(long)]
    pub jitter: Option<u32>,
    /// Frames past the earliest last press the planner may wait (60 s)
    #[arg(long, default_value_t = 3600)]
    pub window: u32,
    #[arg(long, default_value_t = 200)]
    pub max_attempts: u32,
    /// Play these timings instead of the planner's (TITLE:LAST frames, one
    /// per attempt, repeating): repeatability checks and experiments
    #[arg(long, value_parser = parse_timing, value_delimiter = ',')]
    pub fixed: Vec<Timing>,
    /// How attempts start over: `hard` power cycles (the seed then follows
    /// the timing; needs a controller that can, today the emulator),
    /// `soft` uses A+B+Start+Select (its seed depends on the previous
    /// attempt: only good for blind hunting)
    #[arg(long, value_enum, default_value_t = ResetKind::Hard)]
    pub reset: ResetKind,
    /// Keep observing after finishing until Ctrl-C
    #[arg(long)]
    pub hold: bool,
}

fn parse_timing(s: &str) -> Result<Timing, String> {
    let (title, last) = s.split_once(':').ok_or("expected TITLE:LAST")?;
    Ok(Timing {
        title: title.parse().map_err(|e| format!("{e}"))?,
        last: last.parse().map_err(|e| format!("{e}"))?,
    })
}

/// The ball's tile on Oak's table (`PalletTown_ProfessorOaksLab` objects).
fn ball(starter: Starter) -> (i32, i32) {
    match starter {
        Starter::Bulbasaur => (8, 4),
        Starter::Squirtle => (9, 4),
        Starter::Charmander => (10, 4),
    }
}

const LAB: &str = "PalletTown_ProfessorOaksLab";

fn save_path(args: &ShinyArgs) -> Result<PathBuf> {
    if let Some(p) = args
        .sav
        .clone()
        .or_else(|| args.devices.emulator.save.clone())
    {
        return Ok(p);
    }
    let rom = args
        .devices
        .emulator
        .rom
        .clone()
        .or_else(|| std::env::var_os("VPB_ROM").map(PathBuf::from))
        .or_else(|| {
            std::fs::read_dir("roms")
                .ok()?
                .flatten()
                .map(|e| e.path())
                .find(|p| p.extension().is_some_and(|e| e == "gba"))
        })
        .context("no --sav and no ROM to find the .sav next to")?;
    Ok(rom.with_extension("sav"))
}

fn trainer_ids(args: &ShinyArgs) -> Result<Option<TrainerIds>> {
    if args.blind {
        return Ok(None);
    }
    if let (Some(tid), Some(sid)) = (args.tid, args.sid) {
        return Ok(Some(TrainerIds { tid, sid }));
    }
    let path = save_path(args)?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let ids = pokebot_gamedata::sav::trainer_ids(&bytes)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("trainer IDs from {}", path.display()))?;
    Ok(Some(ids))
}

/// Stand in front of the ball facing it, and save there.
fn prepare(
    runtime: &mut Runtime,
    executor: &Executor,
    world: &Arc<pokebot_world::World>,
    starter: Starter,
    stop: &AtomicBool,
) -> Result<Option<pokebot_state::PlayerPose>> {
    executor.run(runtime, &mut ContinueTask::default(), stop)?;
    let (x, y) = ball(starter);
    let mut task = StoryTask::new(
        Arc::clone(world),
        vec![Milestone::new(
            "FaceStarterBall",
            "stand in front of the starter's Poké Ball, facing it",
            vec![
                StoryStep::Settle { frames: 90 },
                StoryStep::Go(Destination::Facing {
                    map: LAB.into(),
                    x,
                    y,
                }),
            ],
        )],
    );
    // The walk ends facing the ball (from whichever side is open).
    executor.run(runtime, &mut task, stop)?;
    executor.run(runtime, &mut SaveGameTask::default(), stop)?;
    runtime.persist_save()?;
    let pose = runtime.state().player.pose.value.clone();
    runtime.info(format!(
        "saved in front of the {starter:?} ball at {pose:?}"
    ));
    Ok(pose)
}

fn attempt(
    runtime: &mut Runtime,
    executor: &Executor,
    reset: ResetKind,
    mut task: StarterAttemptTask,
    stop: &AtomicBool,
) -> Result<AttemptResult> {
    if reset == ResetKind::Hard && !runtime.power_cycle()? {
        bail!("this controller can't power cycle: use --reset soft (blind hunting)");
    }
    executor.run(runtime, &mut task, stop)?;
    task.result().context("the attempt ended without a result")
}

pub fn run(args: ShinyArgs, stop: Arc<AtomicBool>) -> Result<()> {
    let world = Arc::new(pokebot_world::World::load(&args.world).with_context(|| {
        format!(
            "loading {} (run tools/world/build.sh)",
            args.world.display()
        )
    })?);
    let font = pokebot_vision::text::Font::load(args.world.join("font_normal.json"))
        .context("loading font_normal.json (run tools/world/build.sh)")?;
    let small_font = pokebot_vision::text::Font::load(args.world.join("font_small.json"))
        .context("loading font_small.json (run tools/world/build.sh)")?;
    let data = Arc::new(
        pokebot_gamedata::GameData::load(args.world.join("gamedata.json"))
            .context("loading gamedata.json (run tools/world/build.sh)")?,
    );
    let species = args.starter.species();
    let base = data
        .species
        .get(species)
        .with_context(|| format!("{species} not in gamedata"))?
        .base;
    let perception = FireRedPerception::with_world(Arc::clone(&world))
        .with_font(Arc::new(font))
        .with_small_font(Arc::new(small_font))
        .with_palettes(Arc::new(sprite_palettes(&data)))
        // Where the save stands isn't recorded anywhere but in the game.
        .with_global_search(true);
    let opened = devices::open(&args.devices)?;
    let (video_name, controller_name) = (opened.video_name.clone(), opened.controller_name.clone());
    let mut runtime = Runtime::with_perception(opened, perception)
        .with_sensor(pokebot_sense::Sensor::new(Arc::clone(&data)).with_world(Arc::clone(&world)));
    let (telemetry, _) = attach_outputs(&mut runtime, &args.output, &video_name, &controller_name)?;
    runtime.echo_events(true);
    let syncer = args.devices.syncer()?;
    let _timing = TimingKeeper::start(Arc::clone(&syncer), args.devices.timing.clone(), telemetry);
    let frame_clock = args.devices.frame_clock();
    let executor = Executor {
        latency_frames: args.devices.latency_frames(),
        syncer: Some(Arc::clone(&syncer)),
        frame_clock,
        ..Executor::default()
    };
    let result = hunt(
        &args,
        &mut runtime,
        &executor,
        &world,
        base,
        frame_clock,
        &stop,
    );
    match &result {
        Ok(summary) => runtime.info(format!("shiny-starter: {summary}")),
        Err(e) => runtime.error(format!("shiny-starter: {e:#}")),
    }
    runtime.task_finished(result.is_ok());
    if args.hold && !stop.load(Ordering::Relaxed) {
        runtime.info("observing until Ctrl-C");
        while !stop.load(Ordering::Relaxed) {
            if runtime.observe().is_err() {
                break;
            }
        }
    }
    runtime.finish()?;
    result.map(|_| ())
}

fn hunt(
    args: &ShinyArgs,
    runtime: &mut Runtime,
    executor: &Executor,
    world: &Arc<pokebot_world::World>,
    base: [u16; 6],
    frame_clock: bool,
    stop: &AtomicBool,
) -> Result<String> {
    pokebot_agent::console::bring_up_game(runtime, stop, Some(Path::new(CONSOLE_SNAPSHOTS)))?;
    let mut file = HuntFile::load(&args.hunt, args.starter).map_err(anyhow::Error::msg)?;
    if args.prepare {
        file.stand = prepare(runtime, executor, world, args.starter, stop)?;
        file.store(&args.hunt)?;
    }
    let ids = trainer_ids(args)?;
    match ids {
        Some(ids) => runtime.info(format!(
            "trainer IDs: TID {:05} SID {:05}",
            ids.tid, ids.sid
        )),
        None => runtime.info("no trainer IDs: blind timings at full odds"),
    }
    let scouted = match file.scouted.clone().filter(|s| s.reset == args.reset) {
        Some(s) => s,
        None => {
            runtime.info("scouting the screens' timings (closed loop)");
            if let Some(pose) = &file.stand {
                runtime.set_pose_hint(pose.clone());
            }
            let task = StarterAttemptTask::scout(frame_clock, args.reset);
            let r = attempt(runtime, executor, args.reset, task, stop)?;
            let s = r.scouted.context("the scout measured nothing")?;
            runtime.info(format!(
                "scouted {s:?}; its starter: {:?} shiny {:?}",
                r.reading, r.shiny
            ));
            file.scouted = Some(s.clone());
            file.store(&args.hunt)?;
            s
        }
    };
    let jitter = args.jitter.unwrap_or(if frame_clock { 0 } else { 2 });
    let mut hunt = file.hunt.take().unwrap_or_else(|| {
        Hunt::new(HuntConfig {
            ids,
            base,
            level: 5,
            jitter,
            title_min: scouted.title_min(),
            title_max: scouted.title_max(),
            last_min: scouted.last_min(),
            last_max: scouted.last_min() + args.window,
        })
    });
    hunt.config.ids = ids;
    let mut desyncs = 0;
    for n in 1..=args.max_attempts {
        if stop.load(Ordering::Relaxed) {
            bail!("stopped");
        }
        let plan = match args.fixed.get((n as usize - 1) % args.fixed.len().max(1)) {
            Some(&timing) => Plan {
                timing,
                why: "fixed timing (--fixed)".into(),
                target: None,
            },
            None => hunt.next_plan(),
        };
        runtime.info(format!(
            "attempt {n}: title {} last {} — {}",
            plan.timing.title, plan.timing.last, plan.why
        ));
        if let Some(pose) = &file.stand {
            // Every attempt continues the save in front of the ball.
            runtime.set_pose_hint(pose.clone());
        }
        let task = StarterAttemptTask::scripted(frame_clock, scouted.clone(), plan.timing)
            .map_err(anyhow::Error::msg)?
            .leave_to_field();
        let r = attempt(runtime, executor, args.reset, task, stop)?;
        if r.desynced {
            desyncs += 1;
            runtime.error(format!("attempt {n} desynced ({desyncs} in a row)"));
            if desyncs >= 3 {
                file.scouted = None;
                file.hunt = Some(hunt);
                file.store(&args.hunt)?;
                bail!("3 desynced attempts in a row: the scouted timings are off (cleared; run again to re-scout)");
            }
            continue;
        }
        desyncs = 0;
        let shiny = r.shiny == Some(ShinyReading::Shiny);
        let hit = hunt.record(Sighting {
            timing: r.timing,
            reading: r.reading,
            shiny,
            hit: None,
        });
        let predicted = plan
            .target
            .map(|t| (t, Method1Mon::at(t.seed, u64::from(t.advance))));
        runtime.info(format!(
            "attempt {n}: {} {:?}, stats {:?}, shiny {:?}; hit {} (aimed at {})",
            r.reading
                .map_or("?", |r| pokebot_gamedata::training::NATURES
                    [usize::from(r.nature)]),
            r.reading.map(|r| r.nature),
            r.reading.map(|r| r.stats),
            r.shiny,
            hit.map_or("unidentified".into(), |h| format!(
                "seed {:04X} advance {}",
                h.seed, h.advance
            )),
            predicted.map_or("nothing".into(), |(t, m)| format!(
                "seed {:04X} advance {} PID {:08X}",
                t.seed, t.advance, m.pid
            )),
        ));
        file.hunt = Some(hunt.clone());
        file.store(&args.hunt)?;
        if r.shiny == Some(ShinyReading::Unclear) {
            runtime.error("the shiny star and the picture's palette disagree: stopping to look");
            return Ok(format!(
                "unclear shiny reading on attempt {n}; left on its summary"
            ));
        }
        if shiny {
            executor.run(runtime, &mut SaveGameTask::default(), stop)?;
            runtime.persist_save()?;
            return Ok(format!(
                "SHINY {:?} on attempt {n}, saved in game",
                args.starter
            ));
        }
    }
    bail!("no shiny in {} attempts", args.max_attempts)
}
