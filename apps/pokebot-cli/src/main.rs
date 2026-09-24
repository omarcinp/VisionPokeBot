mod devices;
mod goal;
mod hub;
mod plan;
mod script;
mod serve;
#[cfg(feature = "viewer")]
mod viewer;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use pokebot_agent::checkpoint;
use pokebot_agent::console::bring_up_game;
use pokebot_agent::party::{self, Party};
use pokebot_agent::{
    all_milestones, ContinueTask, Executor, NewGameConfig, NewGameTask, Progress, SaveGameTask,
    Starter, StoryTask, SyncerHandle,
};
use pokebot_core::{CapturedFrame, Error, VideoSource};
use pokebot_replay::Session;
use pokebot_runtime::Runtime;
use pokebot_state::{GameEvent, Gender, Observation, PlayerPose};
use pokebot_telemetry::Telemetry;
use pokebot_video::{detect_viewport, Normalizer, ViewportLocator};
use pokebot_vision::{FireRedPerception, PerceptionSystem};

use crate::devices::{DeviceArgs, ViewportArg};
use crate::script::Step;

/// PokéBot development tools. The bot sees only video and acts only through
/// a controller; these commands wire those devices up.
#[derive(Parser)]
#[command(name = "pokebot", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Args)]
struct OutputArgs {
    /// Serve the web UI (default address 127.0.0.1:8080)
    #[arg(long, num_args = 0..=1, default_missing_value = "127.0.0.1:8080")]
    web: Option<SocketAddr>,
    /// Name of this run in the web UI (`Switch`, `Emulator`); the hub's tabs
    /// highlight the page's own instance by it
    #[arg(long, default_value = "Local")]
    instance_label: String,
    /// Record a replayable session to this directory
    #[arg(long)]
    record: Option<PathBuf>,
    /// Also record full-resolution captured frames
    #[arg(long)]
    record_raw: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bot loop headlessly: observe video, optionally play an input script.
    Run {
        #[command(flatten)]
        devices: DeviceArgs,
        #[command(flatten)]
        output: OutputArgs,
        /// Input script (see apps/pokebot-cli/src/script.rs for the format)
        #[arg(long)]
        script: Option<PathBuf>,
        /// Extra frames to observe after the script
        #[arg(long, default_value_t = 0)]
        frames: u64,
        /// Keep observing after the script until Ctrl-C
        #[arg(long)]
        hold: bool,
        /// Save the final normalized frame here
        #[arg(long)]
        screenshot: Option<PathBuf>,
    },
    /// Start a new game from any state: title, intro, gender, names, until the
    /// player is confirmed in control in their bedroom.
    NewGame {
        #[command(flatten)]
        devices: DeviceArgs,
        #[command(flatten)]
        output: OutputArgs,
        #[arg(long, value_enum, default_value_t = GenderArg::Boy)]
        gender: GenderArg,
        /// Player name (1-7 letters A-Z)
        #[arg(long, default_value = "RED")]
        name: String,
        /// Rival name (GREEN, GARY, KAZ, TORU use the preset list; others are typed)
        #[arg(long, default_value = "GREEN")]
        rival: String,
        /// Skip the initial soft reset (game already at power-on)
        #[arg(long)]
        no_reset: bool,
        /// Keep observing after finishing until Ctrl-C
        #[arg(long)]
        hold: bool,
    },
    /// Play the story: (optionally) a new game, then the opening milestones
    /// (bedroom → Mom → Pallet Town → Oak → starter).
    Story {
        #[command(flatten)]
        devices: DeviceArgs,
        #[command(flatten)]
        output: OutputArgs,
        /// Start with a new game (soft reset, intro, names) first
        #[arg(long)]
        new_game: bool,
        #[arg(long, value_enum, default_value_t = GenderArg::Boy)]
        gender: GenderArg,
        #[arg(long, default_value = "RED")]
        name: String,
        #[arg(long, default_value = "GREEN")]
        rival: String,
        #[arg(long, value_enum, default_value_t = Starter::Bulbasaur)]
        starter: Starter,
        /// World model directory (tools/world/build.sh)
        #[arg(long, default_value = "data/world")]
        world: PathBuf,
        /// Continue the saved game (title → CONTINUE) and resume after the
        /// milestones recorded in the progress file
        #[arg(long, conflicts_with = "new_game")]
        r#continue: bool,
        /// Save in-game at the end and record progress
        #[arg(long)]
        save_game: bool,
        /// The bot's memory of the save file (milestones done, save position)
        #[arg(long, default_value = "saves/progress.json")]
        progress: PathBuf,
        /// Tries per milestone; a failed attempt (stuck, fainted, ...)
        /// reloads the last save and tries again
        #[arg(long, default_value_t = 5)]
        attempts: u32,
        /// Stop after this many milestones (per cycle with --restart)
        #[arg(long)]
        milestones: Option<usize>,
        /// Last milestone to play (e.g. CrossMtMoon); later ones are left
        #[arg(long)]
        until: Option<String>,
        /// Never stop: after the story finishes or fails, wait
        /// --restart-wait seconds (still observing), then soft reset,
        /// CONTINUE the last save and play on. Keeps the Switch in use, so it
        /// never dims or sleeps. Later cycles never start a new game.
        #[arg(long, requires = "save_game")]
        restart: bool,
        /// Seconds between one cycle's end and the next soft reset
        #[arg(long, default_value_t = 240)]
        restart_wait: u64,
        /// Keep observing after finishing until Ctrl-C
        #[arg(long)]
        hold: bool,
        /// Shell command run on failures, retries and the end of the story,
        /// with $POKEBOT_EVENT (retry | failed | finished), $POKEBOT_MESSAGE
        /// and $POKEBOT_BUNDLE (debug bundle directory, if any)
        #[arg(long)]
        notify: Option<String>,
        /// Where debug bundles of failed attempts go
        #[arg(long, default_value = "captures/stuck")]
        bundles: PathBuf,
    },
    /// Open a window showing the video feed, with the keyboard as controller.
    #[cfg(feature = "viewer")]
    Play {
        #[command(flatten)]
        devices: DeviceArgs,
        #[command(flatten)]
        output: OutputArgs,
        /// Window scale factor
        #[arg(long, default_value_t = 3)]
        scale: usize,
    },
    /// Readiness planning: chance to beat a trainer now, and the cheapest
    /// training/catching plan to reach the confidence target.
    Plan(plan::PlanArgs),
    /// Goal planning: the intents that establish a goal predicate from a
    /// checkpoint (`--dry-run` prints them; execution arrives with the goal
    /// loop).
    Goal(goal::GoalArgs),
    /// Serve every instance's web UI under one address: /switch/, /emu/.
    Hub(hub::HubArgs),
    /// Run the emulator as a stand-alone virtual console.
    Emulator {
        #[command(subcommand)]
        command: EmulatorCommand,
    },
    /// Normalize captured images and report what perception sees in each.
    Inspect {
        #[arg(required = true)]
        images: Vec<PathBuf>,
        /// Game viewport as x,y,w,h, or "auto" to detect it
        #[arg(long)]
        viewport: Option<String>,
        /// Save the normalized 240x160 frame here
        #[arg(long)]
        out: Option<PathBuf>,
        /// Also locate the player using this world model (see tools/world/build.sh)
        #[arg(long)]
        world: Option<PathBuf>,
        /// Restrict localization to this map
        #[arg(long)]
        map: Option<String>,
        /// Read text with this font (see tools/world/build.sh); skipped if missing
        #[arg(long, default_value = "data/world/font_normal.json")]
        font: PathBuf,
    },
    /// Verify and summarize a recorded session without any device.
    Replay {
        session: PathBuf,
        #[arg(long, default_value_t = 0)]
        from_frame: u64,
    },
}

#[derive(Subcommand)]
enum EmulatorCommand {
    /// Video out on a V4L2 device, controller in over a PABotBase2 serial port.
    Serve(serve::ServeArgs),
}

fn main() -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || {
            if stop.swap(true, Ordering::Relaxed) {
                std::process::exit(130); // second Ctrl-C: give up on a clean exit
            }
        })
        .context("installing Ctrl-C handler")?;
    }
    match Cli::parse().command {
        Command::Run {
            devices,
            output,
            script,
            frames,
            hold,
            screenshot,
        } => run(&devices, &output, script, frames, hold, screenshot, &stop),
        #[cfg(feature = "viewer")]
        Command::Play {
            mut devices,
            output,
            scale,
        } => {
            devices.realtime = true;
            let (runtime, _) = start_runtime(&devices, &output)?;
            viewer::play(runtime, scale, &stop)
        }
        Command::NewGame {
            devices,
            output,
            gender,
            name,
            rival,
            no_reset,
            hold,
        } => {
            let config = NewGameConfig {
                gender: gender.into(),
                player_name: name.to_ascii_uppercase(),
                rival_name: rival.to_ascii_uppercase(),
                soft_reset: !no_reset,
            };
            new_game(&devices, &output, config, hold, &stop)
        }
        Command::Story {
            devices,
            output,
            new_game,
            gender,
            name,
            rival,
            starter,
            world,
            r#continue,
            save_game,
            progress,
            attempts,
            milestones,
            until,
            restart,
            restart_wait,
            hold,
            notify,
            bundles,
        } => {
            let start = if r#continue {
                StoryStart::Continue
            } else if new_game {
                StoryStart::NewGame(NewGameConfig {
                    gender: gender.into(),
                    player_name: name.to_ascii_uppercase(),
                    rival_name: rival.to_ascii_uppercase(),
                    soft_reset: true,
                })
            } else {
                StoryStart::AsIs
            };
            let options = StoryOptions {
                starter,
                world,
                save_game,
                progress,
                hold,
                attempts,
                milestones,
                until,
                restart: restart.then(|| Duration::from_secs(restart_wait)),
                notify,
                bundles,
            };
            story(&devices, &output, start, &options, &stop)
        }
        Command::Plan(args) => plan::run(args),
        Command::Goal(args) => goal::run(args),
        Command::Hub(args) => hub::run(args, stop),
        Command::Emulator {
            command: EmulatorCommand::Serve(args),
        } => serve::serve(args, stop),
        Command::Inspect {
            images,
            viewport,
            out,
            world,
            map,
            font,
        } => inspect(images, viewport, out, world, map, font),
        Command::Replay {
            session,
            from_frame,
        } => replay(session, from_frame),
    }
}

fn start_runtime(
    devices: &DeviceArgs,
    output: &OutputArgs,
) -> Result<(Runtime, Option<Telemetry>)> {
    let devices = devices::open(devices)?;
    let (video_name, controller_name) =
        (devices.video_name.clone(), devices.controller_name.clone());
    let mut runtime = Runtime::new(devices);
    let telemetry = attach_outputs(&mut runtime, output, &video_name, &controller_name)?;
    Ok((runtime, telemetry))
}

/// Attaches the web UI and the recorder; the telemetry hub, if any, so
/// other publishers (the timing model) can reach the UI too.
fn attach_outputs(
    runtime: &mut Runtime,
    output: &OutputArgs,
    video_name: &str,
    controller_name: &str,
) -> Result<Option<Telemetry>> {
    let mut hub = None;
    if let Some(addr) = output.web {
        let telemetry = Telemetry::new(video_name, controller_name);
        let server = pokebot_telemetry::serve(telemetry.clone(), addr, &output.instance_label)?;
        eprintln!("web UI: http://{}", server.addr);
        runtime.attach_telemetry(telemetry.clone());
        hub = Some(telemetry);
    }
    if let Some(dir) = &output.record {
        runtime.record_to(&fresh_record_dir(dir), output.record_raw)?;
    }
    Ok(hub)
}

/// How often the timing model is written to its file while a run lasts.
const TIMING_SAVE_INTERVAL: Duration = Duration::from_secs(60);

/// Keeps the timing model on the web UI and on disk: publishes it whenever
/// it changes, saves it every [`TIMING_SAVE_INTERVAL`] and when dropped
/// (the end of the run).
struct TimingKeeper {
    syncer: SyncerHandle,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TimingKeeper {
    fn start(syncer: SyncerHandle, path: PathBuf, telemetry: Option<Telemetry>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (syncer, path, stop) = (Arc::clone(&syncer), path.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut published = None;
                let mut saved = None;
                let mut last_save = std::time::Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(1));
                    let (revision, report) = {
                        let syncer = syncer.lock().unwrap_or_else(|e| e.into_inner());
                        (syncer.revision(), syncer.report())
                    };
                    if published != Some(revision) {
                        if let Some(telemetry) = &telemetry {
                            telemetry.publish_timing(&report);
                        }
                        published = Some(revision);
                    }
                    if saved != Some(revision) && last_save.elapsed() >= TIMING_SAVE_INTERVAL {
                        Self::save(&syncer, &path);
                        saved = Some(revision);
                        last_save = std::time::Instant::now();
                    }
                }
            })
        };
        Self {
            syncer,
            path,
            stop,
            thread: Some(thread),
        }
    }

    fn save(syncer: &SyncerHandle, path: &Path) {
        let syncer = syncer.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = syncer.save(path) {
            eprintln!("timing model: could not save {}: {e}", path.display());
        }
    }
}

impl Drop for TimingKeeper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        Self::save(&self.syncer, &self.path);
    }
}

/// `dir`, or `dir.2`, `dir.3`, … if it already holds a session: a bot that
/// systemd restarts after a crash reuses its command line.
fn fresh_record_dir(dir: &Path) -> PathBuf {
    let taken = |d: &Path| d.join("metadata.json").exists();
    if !taken(dir) {
        return dir.to_path_buf();
    }
    let mut n = 2;
    loop {
        let mut name = dir.as_os_str().to_owned();
        name.push(format!(".{n}"));
        let candidate = PathBuf::from(name);
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum GenderArg {
    Boy,
    Girl,
}

impl From<GenderArg> for Gender {
    fn from(g: GenderArg) -> Self {
        match g {
            GenderArg::Boy => Gender::Boy,
            GenderArg::Girl => Gender::Girl,
        }
    }
}

fn new_game(
    args: &DeviceArgs,
    output: &OutputArgs,
    config: NewGameConfig,
    hold: bool,
    stop: &AtomicBool,
) -> Result<()> {
    let mut task = NewGameTask::new(config).map_err(anyhow::Error::msg)?;
    let (mut runtime, telemetry) = start_runtime(args, output)?;
    runtime.echo_events(true);
    let started = std::time::Instant::now();
    let syncer = args.syncer()?;
    let _timing = TimingKeeper::start(Arc::clone(&syncer), args.timing.clone(), telemetry);
    let executor = Executor {
        latency_frames: args.latency_frames(),
        syncer: Some(syncer),
        frame_clock: args.frame_clock(),
        ..Executor::default()
    };
    let result = executor.run(&mut runtime, &mut task, stop);
    match &result {
        Ok(summary) => runtime.info(format!(
            "NewGame finished in {:.1} s ({} frames): {summary}",
            started.elapsed().as_secs_f64(),
            runtime.frames_seen()
        )),
        Err(e) => runtime.error(format!("NewGame: {e}")),
    }
    if hold && !stop.load(Ordering::Relaxed) {
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
    result.map(|_| ()).map_err(Into::into)
}

/// Captures of the Switch's own screens (lock screen, HOME menu), kept to
/// build detectors from.
const CONSOLE_SNAPSHOTS: &str = "captures/console";

enum StoryStart {
    NewGame(NewGameConfig),
    Continue,
    /// Already in the overworld; locate the player anywhere.
    AsIs,
}

struct StoryOptions {
    starter: Starter,
    world: PathBuf,
    save_game: bool,
    progress: PathBuf,
    hold: bool,
    /// Tries per milestone (a faint reloads the last save).
    attempts: u32,
    /// Stop after this many milestones.
    milestones: Option<usize>,
    /// Last milestone to play.
    until: Option<String>,
    /// Never stop: the pause before each restart (soft reset, CONTINUE).
    restart: Option<Duration>,
    notify: Option<String>,
    bundles: PathBuf,
}

/// Tells the operator: a `NOTIFY` log line (watchable) and the `--notify`
/// command, if any.
fn notify(
    runtime: &Runtime,
    options: &StoryOptions,
    event: &str,
    message: &str,
    bundle: Option<&Path>,
) {
    runtime.error(format!("NOTIFY {event}: {message}"));
    let Some(command) = &options.notify else {
        return;
    };
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .env("POKEBOT_EVENT", event)
        .env("POKEBOT_MESSAGE", message);
    if let Some(bundle) = bundle {
        cmd.env("POKEBOT_BUNDLE", bundle);
    }
    // Don't wait: a slow notifier must not stall the bot.
    if let Err(e) = cmd.spawn() {
        runtime.error(format!("--notify: {e}"));
    }
}

/// Saves what the bot saw when an attempt failed: the captured frame (full
/// resolution), the normalized frame, the observation and the reason.
fn write_bundle(runtime: &Runtime, dir: &Path, milestone: &str, reason: &str) -> Result<PathBuf> {
    let frame = runtime.last_frame().map_or(0, |f| f.frame_id);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let dir = dir.join(format!("{stamp}-{milestone}-f{frame}"));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("reason.txt"), format!("{milestone}: {reason}\n"))?;
    if let Some(captured) = runtime.last_captured() {
        pokebot_video::png::save(&captured.image, dir.join("captured.png"))?;
    }
    if let Some(normalized) = runtime.last_frame() {
        pokebot_video::png::save(normalized.image(), dir.join("normalized.png"))?;
    }
    if let Some(observation) = runtime.observation() {
        std::fs::write(
            dir.join("observation.json"),
            serde_json::to_string_pretty(observation)?,
        )?;
    }
    std::fs::write(
        dir.join("state.json"),
        serde_json::to_string_pretty(runtime.state())?,
    )?;
    Ok(dir)
}

/// Restores the knowledge that goes with the save just loaded.
fn restore_checkpoint(
    runtime: &mut Runtime,
    state_path: &Path,
    progress: &Progress,
    data: &pokebot_gamedata::GameData,
) -> Result<()> {
    let restored = checkpoint::restore(state_path, progress, data)?;
    if let Some(warning) = &restored.warning {
        runtime.error(format!("checkpoint: {warning}"));
    }
    runtime.emit(GameEvent::CheckpointRestored {
        knowledge: Box::new(restored.knowledge),
    })?;
    runtime.info(format!(
        "Checkpoint knowledge restored from {}",
        restored.source
    ));
    Ok(())
}

fn story(
    args: &DeviceArgs,
    output: &OutputArgs,
    start: StoryStart,
    options: &StoryOptions,
    stop: &AtomicBool,
) -> Result<()> {
    if let Some(until) = &options.until {
        let known = all_milestones(options.starter);
        if !known.iter().any(|m| &m.name == until) {
            bail!(
                "--until {until}: no such milestone (known: {})",
                known
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let world = Arc::new(pokebot_world::World::load(&options.world).with_context(|| {
        format!(
            "loading {} (run tools/world/build.sh)",
            options.world.display()
        )
    })?);
    let previous = match &start {
        StoryStart::Continue => Some(Progress::load(&options.progress).with_context(|| {
            format!(
                "--continue needs the progress file {} (written by --save-game)",
                options.progress.display()
            )
        })?),
        _ => None,
    };
    let font_path = options.world.join("font_normal.json");
    let font = pokebot_vision::text::Font::load(&font_path)
        .with_context(|| format!("loading {} (run tools/world/build.sh)", font_path.display()))?;
    let small_font_path = options.world.join("font_small.json");
    let small_font = pokebot_vision::text::Font::load(&small_font_path).with_context(|| {
        format!(
            "loading {} (run tools/world/build.sh)",
            small_font_path.display()
        )
    })?;
    let data = Arc::new(
        pokebot_gamedata::GameData::load(options.world.join("gamedata.json"))
            .context("loading gamedata.json (run tools/world/build.sh)")?,
    );
    let perception = FireRedPerception::with_world(Arc::clone(&world))
        .with_font(Arc::new(font))
        .with_small_font(Arc::new(small_font))
        .with_palettes(Arc::new(sprite_palettes(&data)))
        .with_global_search(matches!(start, StoryStart::AsIs));
    let devices = devices::open(args)?;
    let (video_name, controller_name) =
        (devices.video_name.clone(), devices.controller_name.clone());
    let mut runtime = Runtime::with_perception(devices, perception);
    let telemetry = attach_outputs(&mut runtime, output, &video_name, &controller_name)?;
    runtime.echo_events(true);
    let syncer = args.syncer()?;
    let _timing = TimingKeeper::start(Arc::clone(&syncer), args.timing.clone(), telemetry);
    // Milestones include long training plans; stuck detection still ends an
    // attempt that stops making progress.
    let executor = Executor {
        max_frames: 60 * 60 * 60 * 3,
        latency_frames: args.latency_frames(),
        syncer: Some(Arc::clone(&syncer)),
        frame_clock: args.frame_clock(),
        ..Executor::default()
    };
    let state_path = checkpoint::path_for(&options.progress);
    let (mut start, mut previous) = (start, previous);
    let mut cycle = 1u64;
    let result = loop {
        let started = std::time::Instant::now();
        let result = (|| -> Result<String> {
            let mut progress = match (&start, previous.take()) {
                (StoryStart::NewGame(config), _) => {
                    bring_up_game(&mut runtime, stop, Some(Path::new(CONSOLE_SNAPSHOTS)))?;
                    executor.run(
                        &mut runtime,
                        &mut NewGameTask::new(config.clone()).map_err(anyhow::Error::msg)?,
                        stop,
                    )?;
                    // A new game always starts in the bedroom, next to the bed.
                    runtime.set_pose_hint(PlayerPose {
                        map: "PalletTown_PlayersHouse_2F".into(),
                        x: 6,
                        y: 6,
                    });
                    // Known from the story: the starter was received at level 5.
                    runtime.emit(GameEvent::PartyMonDerived {
                        slot: 0,
                        mon: Box::new(party::starter_mon(&data, options.starter.species(), 5)),
                    })?;
                    Progress {
                        player_name: config.player_name.clone(),
                        rival_name: config.rival_name.clone(),
                        gender: config.gender,
                        starter: options.starter,
                        milestones: Vec::new(),
                        saved_at: None,
                        party: Party::default(),
                    }
                }
                (StoryStart::Continue, Some(previous)) => {
                    if let Some(pose) = &previous.saved_at {
                        runtime.set_pose_hint(pose.clone());
                    }
                    bring_up_game(&mut runtime, stop, Some(Path::new(CONSOLE_SNAPSHOTS)))?;
                    executor.run(&mut runtime, &mut ContinueTask::default(), stop)?;
                    runtime.info(format!(
                        "continuing after: {}",
                        previous.milestones.join(", ")
                    ));
                    restore_checkpoint(&mut runtime, &state_path, &previous, &data)?;
                    previous
                }
                _ => {
                    let progress = Progress {
                        player_name: "?".into(),
                        rival_name: "?".into(),
                        gender: Gender::Boy,
                        starter: options.starter,
                        milestones: Vec::new(),
                        saved_at: None,
                        party: Party::default(),
                    };
                    if runtime.state().party.value.is_none() {
                        runtime.emit(GameEvent::PartyMonDerived {
                            slot: 0,
                            mon: Box::new(party::starter_mon(&data, progress.starter.species(), 5)),
                        })?;
                    }
                    progress
                }
            };
            let mut all = all_milestones(progress.starter);
            if let Some(until) = &options.until {
                if let Some(last) = all.iter().position(|m| &m.name == until) {
                    all.truncate(last + 1);
                }
            }
            let remaining: Vec<_> = all
                .into_iter()
                .filter(|m| !progress.milestones.contains(&m.name))
                .take(options.milestones.unwrap_or(usize::MAX))
                .collect();
            if remaining.is_empty() {
                return Ok("no milestones left".into());
            }
            // One milestone at a time: save after each; if a Pokémon faints,
            // reload the last save and retry that milestone.
            for milestone in remaining {
                let mut attempt = 1;
                loop {
                    let mut task = StoryTask::new(Arc::clone(&world), vec![milestone.clone()])
                        .with_data(Arc::clone(&data))
                        .with_syncer(Arc::clone(&syncer));
                    match executor.run(&mut runtime, &mut task, stop) {
                        Ok(_) => {
                            progress.milestones.push(milestone.name.clone());
                            break;
                        }
                        Err(pokebot_agent::ExecutorError::Stopped) => {
                            return Err(pokebot_agent::ExecutorError::Stopped.into())
                        }
                        Err(e) => {
                            let bundle = write_bundle(
                                &runtime,
                                &options.bundles,
                                &milestone.name,
                                &e.to_string(),
                            )
                            .map_err(|b| runtime.error(format!("debug bundle: {b:#}")))
                            .ok();
                            let saved = Progress::load(&options.progress).ok();
                            if attempt >= options.attempts || saved.is_none() {
                                let why = if saved.is_none() {
                                    "no save to reload (use --save-game)"
                                } else {
                                    "out of attempts"
                                };
                                notify(
                                    &runtime,
                                    options,
                                    "failed",
                                    &format!("{}: {e} ({why})", milestone.name),
                                    bundle.as_deref(),
                                );
                                return Err(e.into());
                            }
                            let saved = saved.expect("checked");
                            attempt += 1;
                            notify(
                                &runtime,
                                options,
                                "retry",
                                &format!(
                                    "{}: {e} — reloading the last save (attempt {attempt}/{})",
                                    milestone.name, options.attempts
                                ),
                                bundle.as_deref(),
                            );
                            if let Some(pose) = &saved.saved_at {
                                runtime.set_pose_hint(pose.clone());
                            }
                            bring_up_game(&mut runtime, stop, Some(Path::new(CONSOLE_SNAPSHOTS)))?;
                            executor.run(&mut runtime, &mut ContinueTask::default(), stop)?;
                            restore_checkpoint(&mut runtime, &state_path, &saved, &data)?;
                            progress = saved;
                        }
                    }
                }
                if options.save_game {
                    executor.run(&mut runtime, &mut SaveGameTask::default(), stop)?;
                    runtime.persist_save()?;
                    progress.saved_at = runtime.state().player.pose.value.clone();
                    if let Some(dir) = options.progress.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    // state.json first: if progress.json is not written after
                    // it, their identities differ and state.json is ignored.
                    checkpoint::store(
                        &state_path,
                        &checkpoint::Identity::of(&progress),
                        &runtime.state().saved_knowledge(),
                    )?;
                    progress.store(&options.progress)?;
                    let party = Party::from_state(runtime.state());
                    runtime.info(format!(
                        "checkpoint after {}: saved in-game at {}; party {}",
                        milestone.name,
                        progress
                            .saved_at
                            .as_ref()
                            .map_or("?".into(), |p| p.to_string()),
                        party
                            .members
                            .iter()
                            .map(|m| format!("{} Lv{}", m.display_name(), m.level))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            Ok(format!("completed {}", progress.milestones.join(", ")))
        })();
        let stopped = matches!(
            result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<pokebot_agent::ExecutorError>()),
            Some(pokebot_agent::ExecutorError::Stopped)
        );
        report_story(&runtime, options, &result, started);
        let Some(wait) = options.restart else {
            break result;
        };
        if stopped || stop.load(Ordering::Relaxed) {
            break result;
        }
        // Never stop: pause (watching, so the web UI stays live), then load the
        // last save and play on.
        match Progress::load(&options.progress) {
            Ok(saved) => {
                cycle += 1;
                notify(
                    &runtime,
                    options,
                    "restart",
                    &format!(
                        "restarting in {} s (cycle {cycle}): soft reset and CONTINUE after {}",
                        wait.as_secs(),
                        saved.milestones.last().map_or("the start", String::as_str)
                    ),
                    None,
                );
                pause(&mut runtime, wait, stop);
                if stop.load(Ordering::Relaxed) {
                    break result;
                }
                start = StoryStart::Continue;
                previous = Some(saved);
            }
            Err(e) => {
                // Nothing to continue from: wait and look again.
                runtime.error(format!(
                    "restart: no progress file to continue from ({e:#}); waiting"
                ));
                pause(&mut runtime, wait, stop);
                if stop.load(Ordering::Relaxed) {
                    break result;
                }
            }
        }
    };
    if options.hold && !stop.load(Ordering::Relaxed) {
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
    result.map(|_| ())
}

/// Logs how a story cycle ended and tells the operator.
fn report_story(
    runtime: &Runtime,
    options: &StoryOptions,
    result: &Result<String>,
    started: std::time::Instant,
) {
    match result {
        Ok(summary) => {
            let message = format!(
                "Story finished in {:.1} s ({} frames): {summary}",
                started.elapsed().as_secs_f64(),
                runtime.frames_seen()
            );
            runtime.info(&message);
            notify(runtime, options, "finished", &message, None);
        }
        Err(e) => {
            runtime.error(format!("Story: {e:#}"));
            let stopped = matches!(
                e.downcast_ref::<pokebot_agent::ExecutorError>(),
                Some(pokebot_agent::ExecutorError::Stopped)
            );
            if !stopped {
                notify(runtime, options, "failed", &format!("{e:#}"), None);
            }
        }
    }
}

/// Watches the screen for `duration` (the web UI stays live). A video error
/// (the Switch off, the capture unplugged) is waited out, not fatal.
fn pause(runtime: &mut Runtime, duration: Duration, stop: &AtomicBool) {
    let until = std::time::Instant::now() + duration;
    while std::time::Instant::now() < until && !stop.load(Ordering::Relaxed) {
        if runtime.observe().is_err() {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn run(
    args: &DeviceArgs,
    output: &OutputArgs,
    script: Option<PathBuf>,
    frames: u64,
    hold: bool,
    screenshot: Option<PathBuf>,
    stop: &AtomicBool,
) -> Result<()> {
    let steps = match &script {
        Some(path) => script::parse(
            &std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?,
        )?,
        None => Vec::new(),
    };
    if steps.is_empty() && frames == 0 && !hold {
        bail!("nothing to do: pass --script, --frames and/or --hold");
    }
    let (mut runtime, _) = start_runtime(args, output)?;
    let result = run_steps(&mut runtime, steps, frames, hold, stop);
    if let Err(e) = &result {
        runtime.error(format!("{e:#}"));
    }
    if let Some(path) = screenshot {
        save_last(&runtime, &path)?;
    }
    if let Some(frame) = runtime.last_frame() {
        println!(
            "observed {} frames; last frame #{} fingerprint {:016x}",
            runtime.frames_seen(),
            frame.frame_id,
            frame.image().fingerprint()
        );
    }
    // Keep the web UI up after a finished/failed run until Ctrl-C.
    if output.web.is_some() && hold && !stop.load(Ordering::Relaxed) {
        eprintln!("run finished; web UI stays up until Ctrl-C");
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    runtime.finish()?;
    result
}

fn run_steps(
    runtime: &mut Runtime,
    steps: Vec<Step>,
    frames: u64,
    hold: bool,
    stop: &AtomicBool,
) -> Result<()> {
    let stopped = || stop.load(Ordering::Relaxed);
    for step in steps.into_iter().chain(std::iter::once(Step::Wait(frames))) {
        match step {
            Step::Wait(n) => {
                for _ in 0..n {
                    if stopped() {
                        return Ok(());
                    }
                    runtime.observe()?;
                }
            }
            Step::Command(command) => {
                runtime.execute(command)?;
            }
            Step::UntilIdle => {
                while !runtime.is_idle()? && !stopped() {
                    runtime.observe()?;
                }
            }
            Step::Screenshot(path) => save_last(runtime, &path)?,
        }
    }
    if hold {
        runtime.info("script done; observing until Ctrl-C");
        while !stopped() {
            match runtime.observe() {
                Ok(_) => {}
                Err(Error::EndOfStream) => {
                    runtime.info("video source ended");
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn save_last(runtime: &Runtime, path: &Path) -> Result<()> {
    let Some(frame) = runtime.last_frame() else {
        bail!("screenshot requested before any frame was observed");
    };
    pokebot_video::png::save(frame.image(), path)?;
    runtime.info(format!(
        "saved frame #{} to {}",
        frame.frame_id,
        path.display()
    ));
    Ok(())
}

fn inspect(
    paths: Vec<PathBuf>,
    viewport: Option<String>,
    out: Option<PathBuf>,
    world: Option<PathBuf>,
    map: Option<String>,
    font: PathBuf,
) -> Result<()> {
    if out.is_some() && paths.len() > 1 {
        bail!("--out needs a single image");
    }
    let world = world.map(pokebot_world::World::load).transpose()?;
    let small_font_path = font.with_file_name("font_small.json");
    let font = font
        .exists()
        .then(|| pokebot_vision::text::Font::load(&font).map(Arc::new))
        .transpose()?;
    let small_font = small_font_path
        .exists()
        .then(|| pokebot_vision::text::Font::load(&small_font_path).map(Arc::new))
        .transpose()?;
    let gamedata_path = small_font_path.with_file_name("gamedata.json");
    let palettes = gamedata_path
        .exists()
        .then(|| pokebot_gamedata::GameData::load(&gamedata_path))
        .transpose()?
        .map(|data| Arc::new(sprite_palettes(&data)));
    for path in paths {
        let image = pokebot_video::png::load(&path)?;
        let locator = match viewport.as_deref() {
            None => ViewportLocator::FullFrame,
            Some("auto") => {
                let rect = detect_viewport(&image, 16)
                    .context("no game viewport found (frame is black)")?;
                println!(
                    "{}: detected viewport {},{},{},{}",
                    path.display(),
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height
                );
                ViewportLocator::Fixed(rect)
            }
            Some(spec) => {
                ViewportLocator::Fixed(spec.parse::<ViewportArg>().map_err(anyhow::Error::msg)?.0)
            }
        };
        let (width, height) = (image.width(), image.height());
        let captured = CapturedFrame {
            frame_id: 0,
            captured_at: std::time::Instant::now(),
            image,
        };
        let normalized = Normalizer::new(locator).normalize(&captured)?;
        let mut perception = FireRedPerception::default();
        if let Some(font) = &font {
            perception = perception.with_font(Arc::clone(font));
        }
        if let Some(small_font) = &small_font {
            perception = perception.with_small_font(Arc::clone(small_font));
        }
        if let Some(palettes) = &palettes {
            perception = perception.with_palettes(Arc::clone(palettes));
        }
        let observation = perception.observe(&normalized);
        println!(
            "{} ({width}x{height}): {}",
            path.display(),
            describe_observation(&observation)
        );
        if let Some(world) = &world {
            let started = std::time::Instant::now();
            let localizer = pokebot_world::Localizer::new(world);
            let exclude = [pokebot_world::localize::PLAYER_SPRITE];
            let found = match &map {
                Some(name) => {
                    let data = world
                        .map(name)
                        .with_context(|| format!("unknown map {name}"))?;
                    localizer.locate_in(normalized.image(), data, None, 0, &exclude)
                }
                None => localizer.locate_anywhere(normalized.image(), &exclude),
            };
            match found {
                Some(p) => println!(
                    "  player at {} (score {}) in {:?}",
                    p.pose,
                    p.score,
                    started.elapsed()
                ),
                None => println!("  player not located ({:?})", started.elapsed()),
            }
        }
        if let Some(out) = &out {
            pokebot_video::png::save(normalized.image(), out)?;
            println!("wrote {}", out.display());
        }
    }
    Ok(())
}

/// Species sprite palettes keyed by the name the game prints
/// (`pokebot_gamedata::printed_name`: `MR. MIME`, `FARFETCH'D`). NIDORAN ♀/♂
/// are left out: the HUD reads the name without its coloured gender sign,
/// so it can't tell their palettes apart. Any other name shared by several
/// species is dropped for the same reason.
fn sprite_palettes(data: &pokebot_gamedata::GameData) -> pokebot_vision::shiny::SpritePalettes {
    let mut species: Vec<_> = data.species.iter().collect();
    species.sort_by(|a, b| a.0.cmp(b.0));
    let mut palettes = pokebot_vision::shiny::SpritePalettes::new();
    let mut ambiguous = std::collections::BTreeSet::new();
    for (constant, s) in species {
        let Some(p) = &s.palettes else { continue };
        let (Ok(normal), Ok(shiny)) = (p.normal.clone().try_into(), p.shiny.clone().try_into())
        else {
            continue;
        };
        let name = pokebot_gamedata::printed_name(constant);
        if name.ends_with(['♀', '♂']) {
            continue;
        }
        if palettes.insert(name.clone(), (normal, shiny)).is_some() {
            ambiguous.insert(name);
        }
    }
    for name in ambiguous {
        palettes.remove(&name);
    }
    palettes
}

fn describe_observation(o: &Observation) -> String {
    let mut parts = vec![format!("{:?}", o.screen.value)];
    if let Some(d) = &o.dialogue {
        parts.push(format!(
            "{:?}{}",
            d.kind,
            if d.waiting_for_input {
                " waiting▼"
            } else {
                ""
            }
        ));
    }
    if let Some(d) = o.dialogue.as_ref().filter(|d| !d.lines.is_empty()) {
        parts.push(format!("text {:?}", d.lines));
    }
    if let Some(m) = &o.menu {
        parts.push(format!(
            "menu {} rows, cursor {} at ({},{}) y {}",
            m.rows, m.cursor_row, m.window.x, m.window.y, m.cursor_y
        ));
    }
    if !o.menu_lines.is_empty() {
        parts.push(format!("menu text {:?}", o.menu_lines));
    }
    if let Some(b) = &o.battle {
        parts.push(format!(
            "battle {:?} PP {:?} moves {:?}, us {:?} Lv{:?} HP {:?} ({:?}‰) vs {:?} Lv{:?} ({:?}‰)",
            b.menu,
            b.move_pp,
            b.move_names,
            b.player_name,
            b.player_level,
            b.player_hp_numbers,
            b.player_hp,
            b.opponent_name,
            b.opponent_level,
            b.opponent_hp
        ));
        if b.opponent_caught.is_some() || b.opponent_shiny.is_some() {
            parts.push(format!(
                "opponent caught {:?} shiny {:?}",
                b.opponent_caught, b.opponent_shiny
            ));
        }
    }
    if o.pokedex_page {
        parts.push("pokedex page".into());
    }
    if let Some(l) = &o.move_list {
        parts.push(format!("moves {:?}, selected {:?}", l.moves, l.selected));
    }
    if let Some(b) = &o.bag {
        let mut s = format!("bag: {} {:?} ▶{:?}", b.pocket, b.rows, b.cursor);
        if let Some((opts, row)) = &b.prompt {
            s.push_str(&format!(" prompt {opts:?} ▶{row}"));
        }
        parts.push(s);
    }
    if let Some(s) = &o.shop {
        parts.push(format!(
            "shop: money {:?} {:?} ▶{:?} qty {:?}",
            s.money, s.items, s.cursor, s.quantity
        ));
    }
    if let Some(n) = &o.naming {
        parts.push(format!("naming {:?}, {} typed", n.focus, n.typed));
    }
    parts.join(" | ")
}

fn replay(dir: PathBuf, from_frame: u64) -> Result<()> {
    let session = Session::open(&dir)?;
    println!(
        "session {} (video: {}, controller: {}): {} frames, {} commands",
        dir.display(),
        session.metadata.video_source,
        session.metadata.controller,
        session.frames.len(),
        session.commands.len()
    );
    let mut source = session.video_source(from_frame);
    let mut checked = 0;
    for record in session.frames.iter().filter(|f| f.frame_id >= from_frame) {
        let frame = source.next_frame()?;
        let actual = format!("{:016x}", frame.image.fingerprint());
        if frame.frame_id != record.frame_id || actual != record.fingerprint {
            bail!(
                "frame {} does not match its record (fingerprint {actual}, expected {})",
                record.frame_id,
                record.fingerprint
            );
        }
        checked += 1;
    }
    println!("verified {checked} frames from #{from_frame}");
    for command in session
        .commands
        .iter()
        .filter(|c| c.after_frame_id.unwrap_or(0) >= from_frame)
    {
        let after = command
            .after_frame_id
            .map_or("-".into(), |id| id.to_string());
        println!(
            "  after frame {after:>6}: #{} {:?}",
            command.command_id, command.command
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_palettes_are_keyed_by_printed_names() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(data) = pokebot_gamedata::GameData::load(root.join("data/world/gamedata.json"))
        else {
            return;
        };
        let palettes = sprite_palettes(&data);
        for name in ["RATTATA", "MR. MIME", "FARFETCH'D"] {
            assert!(palettes.contains_key(name), "{name}");
        }
        assert!(!palettes.keys().any(|k| k.starts_with("NIDORAN")));
        assert!(!palettes.contains_key("MR MIME"));
    }
}
