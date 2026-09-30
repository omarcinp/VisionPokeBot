//! `pokebot nugget-farm`: money from the Nugget Bridge grunt, who hands
//! over a NUGGET before his battle and keeps handing them over while he
//! is never beaten ([`pokebot_agent::nugget_farm`]): the contest trainers
//! beaten, every member but the one to lose with stored in the PC, the
//! grunt lost to `--nuggets` times (a white-out each, expected), the
//! NUGGETs sold, the party taken back. Its progress is kept in the farm
//! file, so `--restart` (a reload of the last save) resumes it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use pokebot_agent::checkpoint;
use pokebot_agent::goal_session::{self, CycleEnd, Runner, Session, Start};
use pokebot_agent::ledger::Ledger;
use pokebot_agent::nugget_farm::{self, FarmConfig};
use pokebot_agent::tools::{ToolContext, ToolError, Toolbox};
use pokebot_agent::{Executor, ExecutorError, Progress};
use pokebot_core::Error;
use pokebot_gamedata::GameData;
use pokebot_runtime::Runtime;
use pokebot_state::GameEvent;
use pokebot_vision::FireRedPerception;
use pokebot_world::World;

use crate::devices::{self, DeviceArgs};
use crate::goal::ProgressSave;
use crate::scenario;
use crate::{
    attach_outputs, pause, sprite_palettes, write_bundle, OutputArgs, TimingKeeper,
    CONSOLE_SNAPSHOTS,
};

#[derive(Debug, clap::Args)]
pub struct NuggetArgs {
    /// NUGGETs to farm in all (the farm file counts them across runs)
    #[arg(long, default_value_t = 200)]
    pub nuggets: u32,
    /// White-outs between in-game saves (a reload loses at most these)
    #[arg(long, default_value_t = 10)]
    pub save_every: u32,
    /// The farm's progress: its phase, the NUGGETs saved, the Pokémon
    /// stored (default: `nugget-farm.json` beside the progress file)
    #[arg(long)]
    pub farm_file: Option<PathBuf>,
    /// Checkpoint (`state.json` beside a `progress.json`)
    #[arg(long, default_value = "saves/state.json")]
    pub state: PathBuf,
    /// The bot's memory of the save file (default: `progress.json` beside
    /// `--state`); every save the farm makes updates it
    #[arg(long)]
    pub progress: Option<PathBuf>,
    /// World model directory (tools/world/build.sh)
    #[arg(long, default_value = "data/world")]
    pub world: PathBuf,
    #[command(flatten)]
    pub devices: DeviceArgs,
    #[command(flatten)]
    pub output: OutputArgs,
    /// Continue the saved game (title → CONTINUE) and restore its checkpoint
    #[arg(long)]
    pub r#continue: bool,
    /// Never stop: when the farm ends or fails, wait --restart-wait
    /// seconds, soft reset, CONTINUE the last save and go on from the farm
    /// file. Keeps the Switch in use.
    #[arg(long)]
    pub restart: bool,
    #[arg(long, default_value_t = 240)]
    pub restart_wait: u64,
    /// Keep observing after finishing until Ctrl-C
    #[arg(long)]
    pub hold: bool,
    /// Where debug bundles of failed runs go
    #[arg(long, default_value = "captures/stuck")]
    pub bundles: PathBuf,
    /// Development: start from a recorded scenario (an id in
    /// --scenario-library, or its directory), copied to a scratch
    /// directory with its save and checkpoint. Emulator only.
    #[arg(long, conflicts_with = "continue")]
    pub scenario: Option<String>,
    #[arg(long, default_value = scenario::DEFAULT_DIR)]
    pub scenario_library: PathBuf,
}

impl NuggetArgs {
    fn progress_path(&self) -> PathBuf {
        self.progress
            .clone()
            .unwrap_or_else(|| self.state.with_file_name("progress.json"))
    }

    fn state_path(&self) -> PathBuf {
        self.progress
            .as_deref()
            .map_or_else(|| self.state.clone(), checkpoint::path_for)
    }

    fn farm_path(&self) -> PathBuf {
        self.farm_file
            .clone()
            .unwrap_or_else(|| self.progress_path().with_file_name("nugget-farm.json"))
    }
}

pub fn run(mut args: NuggetArgs, stop: Arc<AtomicBool>) -> Result<()> {
    let mut restore = None;
    if let Some(id) = &args.scenario {
        let prepared = scenario::prepare(id, &args.scenario_library)?;
        eprintln!(
            "scenario {id}: running from a copy in {}",
            prepared
                .state
                .parent()
                .map_or("?".into(), |p| p.display().to_string())
        );
        args.devices.emulator.save = Some(prepared.sav);
        args.devices.emulator.no_save = false;
        args.state = prepared.state;
        args.progress = Some(prepared.progress);
        restore = Some(prepared.snapshot);
    }
    let progress_path = args.progress_path();
    Progress::load(&progress_path).with_context(|| {
        format!(
            "the farm saves in-game and needs the progress file {} to tie its checkpoint to",
            progress_path.display()
        )
    })?;
    let world = Arc::new(World::load(&args.world).with_context(|| {
        format!(
            "loading {} (run tools/world/build.sh)",
            args.world.display()
        )
    })?);
    let data = Arc::new(
        GameData::load(args.world.join("gamedata.json"))
            .context("loading gamedata.json (run tools/world/build.sh)")?,
    );
    let font = pokebot_vision::text::Font::load(args.world.join("font_normal.json"))
        .context("loading font_normal.json (run tools/world/build.sh)")?;
    let small_font = pokebot_vision::text::Font::load(args.world.join("font_small.json"))
        .context("loading font_small.json (run tools/world/build.sh)")?;
    let start = if args.r#continue {
        Start::Continue
    } else {
        Start::AsIs
    };
    let perception = FireRedPerception::with_world(Arc::clone(&world))
        .with_font(Arc::new(font))
        .with_small_font(Arc::new(small_font))
        .with_palettes(Arc::new(sprite_palettes(&data)))
        .with_global_search(start == Start::AsIs);
    let dev = restore.map(|r| devices::DevSnapshots { restore: Some(r) });
    let (devices, _) = devices::open_with(&args.devices, dev)?;
    let (video_name, controller_name) =
        (devices.video_name.clone(), devices.controller_name.clone());
    let mut runtime = Runtime::with_perception(devices, perception)
        .with_sensor(pokebot_sense::Sensor::new(Arc::clone(&data)).with_world(Arc::clone(&world)));
    let (telemetry, _session_dir) =
        attach_outputs(&mut runtime, &args.output, &video_name, &controller_name)?;
    runtime.echo_events(true);
    let syncer = args.devices.syncer()?;
    let _timing = TimingKeeper::start(
        Arc::clone(&syncer),
        args.devices.timing.clone(),
        telemetry.clone(),
    );
    let executor = Executor {
        max_frames: 60 * 60 * 60 * 3,
        latency_frames: args.devices.latency_frames(),
        syncer: Some(Arc::clone(&syncer)),
        frame_clock: args.devices.frame_clock(),
        ..Executor::default()
    };
    let task_stop = runtime
        .bot_stop_signal()
        .unwrap_or_else(|| Arc::clone(&stop));
    let _ = crate::WEB_BOT_STOP.set(task_stop.clone());
    let mut runner = FarmRunner {
        args: &args,
        world,
        data,
        runtime,
        executor,
        state_path: args.state_path(),
        progress_path,
        stop: &task_stop,
    };
    let mut session = Session {
        start,
        restart: args.restart.then(|| Duration::from_secs(args.restart_wait)),
    };
    let report = loop {
        let report = goal_session::run(session, &mut runner);
        if !runner.runtime.bot_stopped() || stop.load(Ordering::Relaxed) {
            break report;
        }
        runner.runtime.allow_web_resume(true);
        runner
            .runtime
            .info("Bot stopped; viewing and manual control remain available");
        while runner.runtime.bot_stopped() && !stop.load(Ordering::Relaxed) {
            runner.runtime.observe()?;
        }
        runner.runtime.allow_web_resume(false);
        if stop.load(Ordering::Relaxed) {
            break report;
        }
        runner.runtime.clear_pose_hint();
        session.start = Start::AsIs;
    };
    let FarmRunner { mut runtime, .. } = runner;
    if args.hold && !stop.load(Ordering::Relaxed) {
        runtime.info("observing until Ctrl-C");
        while !stop.load(Ordering::Relaxed) {
            match runtime.observe() {
                Ok(_) => {}
                Err(Error::EndOfStream) => break,
                Err(e) => return Err(e.into()),
            }
        }
    }
    runtime.finish()?;
    match report.last {
        CycleEnd::Satisfied(_) => Ok(()),
        CycleEnd::Unsatisfied(outcome) | CycleEnd::Failed(outcome) => {
            bail!("nugget farm: {outcome}")
        }
        CycleEnd::Stopped => bail!("stopped by user"),
    }
}

struct FarmRunner<'a> {
    args: &'a NuggetArgs,
    world: Arc<World>,
    data: Arc<GameData>,
    runtime: Runtime,
    executor: Executor,
    state_path: PathBuf,
    progress_path: PathBuf,
    stop: &'a AtomicBool,
}

impl FarmRunner<'_> {
    /// The game at the cycle's start; the progress its saves are tied to.
    fn bring_up(&mut self, start: Start) -> Result<Progress> {
        let progress = Progress::load(&self.progress_path)
            .with_context(|| format!("loading {}", self.progress_path.display()))?;
        match start {
            Start::Continue | Start::NewGame => {
                goal_session::continue_game(
                    &mut self.runtime,
                    &self.executor,
                    &progress,
                    &self.state_path,
                    &self.data,
                    self.stop,
                    Some(Path::new(CONSOLE_SNAPSHOTS)),
                )?;
            }
            Start::AsIs => {
                if self.runtime.frames_seen() > 0 {
                    self.runtime.clear_pose_hint();
                    return Ok(progress);
                }
                match checkpoint::load(&self.state_path) {
                    Ok(Some(c)) => {
                        if let Some(pose) = c.identity.as_ref().and_then(|i| i.saved_at.clone()) {
                            self.runtime.set_pose_hint(pose);
                        }
                        self.runtime.emit(GameEvent::CheckpointRestored {
                            knowledge: Box::new(c.knowledge),
                        })?;
                    }
                    Ok(None) => self.runtime.info(format!(
                        "no checkpoint at {}: going by what the screen shows",
                        self.state_path.display()
                    )),
                    Err(e) => self
                        .runtime
                        .error(format!("checkpoint {}: {e}", self.state_path.display())),
                }
            }
        }
        Ok(progress)
    }

    fn play(&mut self, start: Start) -> Result<String> {
        let progress = self.bring_up(start)?;
        let mut toolbox = Toolbox::default();
        toolbox.prepend(Box::new(ProgressSave {
            progress: progress.clone(),
            path: self.progress_path.clone(),
            snapshots: None,
        }));
        let mut ctx = ToolContext::new(
            &mut self.runtime,
            &self.executor,
            Arc::clone(&self.world),
            Arc::clone(&self.data),
            self.stop,
        )
        .with_checkpoint(self.state_path.clone(), checkpoint::Identity::of(&progress))
        .with_toolbox(toolbox);
        let ledger_path = Ledger::path_for(&self.state_path);
        match Ledger::load(&ledger_path) {
            Ok(ledger) => ctx = ctx.with_ledger(ledger),
            Err(e) => ctx.info(format!(
                "ledger {}: {e}; starting empty",
                ledger_path.display()
            )),
        }
        pokebot_agent::tools::probe::audit_core(&mut ctx)?;
        let config = FarmConfig {
            nuggets: self.args.nuggets,
            save_every: self.args.save_every,
            file: self.args.farm_path(),
        };
        ctx.info(format!(
            "nugget farm: {} NUGGETs, a save every {}, progress in {}",
            config.nuggets,
            config.save_every,
            config.file.display()
        ));
        let result = nugget_farm::run(&mut ctx, &config);
        if let Err(e) = ctx.ledger.store() {
            ctx.info(format!("ledger: {e}"));
        }
        Ok(result?)
    }
}

/// Ctrl-C, from any of the layers a cycle runs through.
fn was_stopped(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ExecutorError>(),
        Some(ExecutorError::Stopped)
    ) || matches!(e.downcast_ref::<ToolError>(), Some(ToolError::Stopped))
}

impl Runner for FarmRunner<'_> {
    fn cycle(&mut self, start: Start, cycle: u64) -> CycleEnd {
        if cycle > 1 {
            self.runtime
                .info(format!("nugget farm cycle {cycle}: {start:?}"));
        }
        match self.play(start) {
            Ok(summary) => {
                self.runtime.info(format!("nugget farm: {summary}"));
                CycleEnd::Satisfied(summary)
            }
            Err(e) => {
                let runtime = &self.runtime;
                if was_stopped(&e) || self.stop.load(Ordering::Relaxed) || runtime.bot_stopped() {
                    runtime.info("nugget farm: stopped by user");
                    return CycleEnd::Stopped;
                }
                runtime.error(format!("nugget farm: {e:#}"));
                if let Ok(dir) =
                    write_bundle(runtime, &self.args.bundles, "nugget-farm", &e.to_string())
                {
                    runtime.error(format!("debug bundle: {}", dir.display()));
                }
                CycleEnd::Failed(format!("{e:#}"))
            }
        }
    }

    fn wait(&mut self, duration: Duration) {
        pause(&mut self.runtime, duration, self.stop);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.runtime.bot_stopped()
    }

    fn has_checkpoint(&self) -> bool {
        Progress::load(&self.progress_path).is_ok()
    }

    fn log(&self, message: &str) {
        self.runtime.error(format!("NOTIFY {message}"));
    }
}
