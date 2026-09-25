//! `pokebot goal`: plan a goal predicate from a checkpoint and print the
//! plan (`--dry-run`), or run the execution loop (spec §8) on the console.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use pokebot_agent::checkpoint;
use pokebot_agent::goal::{self, GoalOptions, GoalReport, DEFAULT_MAX_REPLANS};
use pokebot_agent::goal_session::{self, CycleEnd, Runner, Session, Start};
use pokebot_agent::tools::{Intent, Tool, ToolContext, ToolError, ToolOutcome, Toolbox};
use pokebot_agent::{Executor, ExecutorError, NewGameConfig, Progress, Starter};
use pokebot_core::Error;
use pokebot_gamedata::GameData;
use pokebot_planner::{
    load_checkpoint, parse_goal, GoalPredicate, Methods, Obtain, Plan, PlanError, PlanOptions,
    Planner,
};
use pokebot_runtime::Runtime;
use pokebot_state::inference::InferenceRules;
use pokebot_state::{GameEvent, PlayerPose, Priors, SavedKnowledge, RULES_DIR};
use pokebot_telemetry::Telemetry;
use pokebot_vision::FireRedPerception;
use pokebot_world::route::{PlaceGraph, RouteParams};
use pokebot_world::World;

use crate::devices::{self, DeviceArgs};
use crate::{
    attach_outputs, pause, sprite_palettes, write_bundle, GenderArg, OutputArgs, TimingKeeper,
    CONSOLE_SNAPSHOTS,
};

#[derive(Debug, clap::Args)]
pub struct GoalArgs {
    /// `catch SPECIES`, `flag FLAG_NAME`, `badge N`, `at MAP` or `item ITEM N`
    pub goal: String,
    /// Print the plan and stop; no device is opened
    #[arg(long)]
    pub dry_run: bool,
    /// Checkpoint to plan from (`state.json` beside a `progress.json`)
    #[arg(long, default_value = "saves/state.json")]
    pub state: PathBuf,
    /// The bot's memory of the save file (default: `progress.json` beside
    /// `--state`); its `saved_at` is where CONTINUE resumes
    #[arg(long)]
    pub progress: Option<PathBuf>,
    /// Directory with `priors.json`, `inference.json` and `methods.json`
    #[arg(long, default_value = RULES_DIR)]
    pub rules: PathBuf,
    /// A wrong assumption dearer than this many seconds makes a probe mandatory
    #[arg(long, default_value_t = 600.0)]
    pub expensive_secs: f64,
    /// Wall-clock seconds a plan may take before the planner gives up
    /// (Ctrl-C ends it too)
    #[arg(long, default_value_t = 60.0)]
    pub plan_budget_secs: f64,
    /// World model directory (tools/world/build.sh)
    #[arg(long, default_value = "data/world")]
    pub world: PathBuf,
    #[command(flatten)]
    pub devices: DeviceArgs,
    #[command(flatten)]
    pub output: OutputArgs,
    /// Continue the saved game (title → CONTINUE) and restore its checkpoint
    #[arg(long, conflicts_with = "new_game")]
    pub r#continue: bool,
    /// Start with a new game (soft reset, intro, names) and run the goal
    /// from the first overworld frame: the story's opening (Oak, the
    /// starter, the parcel) is planned like everything else. Overwrites the
    /// save.
    #[arg(long, requires = "save_game")]
    pub new_game: bool,
    #[arg(long, value_enum, default_value_t = GenderArg::Boy)]
    pub gender: GenderArg,
    /// The player's name (1–7 letters)
    #[arg(long, default_value = "RED")]
    pub player: String,
    /// The rival's name (GREEN, GARY, KAZ, TORU from the list; else typed)
    #[arg(long, default_value = "GREEN")]
    pub rival: String,
    /// The Pokémon preferred where the story offers a choice (the starter):
    /// the planner prices the script paths giving another species higher
    #[arg(long, value_enum, default_value_t = Starter::Bulbasaur)]
    pub starter: Starter,
    /// Never stop: when the goal run ends (satisfied or not) or fails, wait
    /// --restart-wait seconds (still observing), then soft reset, CONTINUE
    /// the last save and run the goal again. Keeps the Switch in use.
    #[arg(long, requires = "save_game")]
    pub restart: bool,
    /// Seconds between one cycle's end and the next soft reset
    #[arg(long, default_value_t = 240)]
    pub restart_wait: u64,
    /// Save in-game after every step that changed the belief and at the
    /// end; write the checkpoint (`state.json`, `progress.json`)
    #[arg(long)]
    pub save_game: bool,
    /// Replans before the loop gives up
    #[arg(long, default_value_t = DEFAULT_MAX_REPLANS)]
    pub max_replans: u32,
    /// Keep observing after finishing until Ctrl-C
    #[arg(long)]
    pub hold: bool,
    /// Where debug bundles of failed runs go
    #[arg(long, default_value = "captures/stuck")]
    pub bundles: PathBuf,
}

impl GoalArgs {
    fn progress_path(&self) -> PathBuf {
        self.progress
            .clone()
            .unwrap_or_else(|| self.state.with_file_name("progress.json"))
    }

    /// `state.json` beside an explicit `--progress` (a checkpoint's two
    /// files are always written side by side), else `--state`.
    fn state_path(&self) -> PathBuf {
        self.progress
            .as_deref()
            .map_or_else(|| self.state.clone(), checkpoint::path_for)
    }
}

/// Loads the checkpoint and the pose the plan starts from.
fn load_state(args: &GoalArgs) -> Result<(SavedKnowledge, Option<PlayerPose>)> {
    let (mut knowledge, mut pose) = load_checkpoint(&args.state)
        .with_context(|| format!("loading {}", args.state.display()))?;
    let progress = args.progress_path();
    if progress.exists() {
        let p =
            Progress::load(&progress).with_context(|| format!("loading {}", progress.display()))?;
        if p.saved_at.is_some() {
            pose = p.saved_at;
        }
    }
    let inference = args.rules.join("inference.json");
    if inference.exists() {
        let rules = InferenceRules::load(&inference)
            .with_context(|| format!("loading {}", inference.display()))?;
        rules.apply(&mut knowledge.world);
    }
    Ok((knowledge, pose))
}

fn load_optional<T>(
    path: &Path,
    load: impl FnOnce(&Path) -> pokebot_core::Result<T>,
) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    load(path)
        .map(Some)
        .with_context(|| format!("loading {}", path.display()))
}

/// Everything the planner is built from.
struct PlannerData {
    world: Arc<World>,
    data: Arc<GameData>,
    obtain: Option<Obtain>,
    priors: Option<Priors>,
    methods: Methods,
    graph: PlaceGraph,
    options: PlanOptions,
}

impl PlannerData {
    fn load(args: &GoalArgs) -> Result<Self> {
        let world = World::load(&args.world).with_context(|| {
            format!(
                "loading {} (run tools/world/build.sh)",
                args.world.display()
            )
        })?;
        if world.events().is_none() {
            bail!(
                "{} has no events.json (run tools/world/build.sh)",
                args.world.display()
            );
        }
        let data = GameData::load(args.world.join("gamedata.json"))
            .context("loading gamedata.json (run tools/world/build.sh)")?;
        let obtain = load_optional(&args.world.join("obtain.json"), |_| {
            Obtain::load(&args.world)
        })?;
        let priors = load_optional(&args.rules.join("priors.json"), |p| Priors::load(p))?;
        let methods = load_optional(&args.rules.join("methods.json"), |p| Methods::load(p))?
            .unwrap_or_default();
        let graph = PlaceGraph::build(&world, RouteParams::default());
        let options = PlanOptions {
            expensive_secs: args.expensive_secs,
            budget_s: args.plan_budget_secs,
            supported_probes: Some(Toolbox::supported_probes()),
            prefer_species: vec![args.starter.species().to_owned()],
            ..PlanOptions::default()
        };
        Ok(PlannerData {
            world: Arc::new(world),
            data: Arc::new(data),
            obtain,
            priors,
            methods,
            graph,
            options,
        })
    }

    fn planner(&self) -> Planner<'_> {
        Planner::new(
            &self.world,
            &self.graph,
            &self.data,
            self.obtain.as_ref(),
            self.priors.as_ref(),
            &self.methods,
            self.options.clone(),
        )
    }
}

pub fn run(args: GoalArgs, stop: Arc<AtomicBool>) -> Result<()> {
    let goal = parse_goal(&args.goal).map_err(|e| anyhow::anyhow!(e))?;
    let mut pd = PlannerData::load(&args)?;
    // SIGINT and SIGTERM set the process's stop flag; the planner polls it
    // at every node, so a signal during planning ends the search promptly.
    pd.options.stop = Some(Arc::clone(&stop));
    if args.dry_run {
        dry_run(&args, &goal, &pd)
    } else {
        execute(&args, &goal, &pd, &stop)
    }
}

fn dry_run(args: &GoalArgs, goal: &GoalPredicate, pd: &PlannerData) -> Result<()> {
    let planner = pd.planner();
    let (knowledge, pose) = load_state(args)?;
    println!("goal: {goal}");
    match &pose {
        Some(p) => println!("from: {p}"),
        None => println!("from: unknown position"),
    }
    let started = std::time::Instant::now();
    match planner.plan(goal, &knowledge, pose) {
        Ok(plan) => {
            print_plan(&plan);
            println!("planned in {:.1} s", started.elapsed().as_secs_f64());
            Ok(())
        }
        Err(PlanError::Budget {
            goal,
            nodes,
            elapsed_s,
            best_partial,
        }) => {
            println!("planning {goal} stopped after {nodes} nodes and {elapsed_s:.1} s");
            if let Some(plan) = best_partial {
                println!("best partial plan:");
                print_plan(&plan);
            }
            bail!("no plan within the budget")
        }
        Err(e) => bail!("{e}"),
    }
}

/// `Save` that also records the save position in `progress.json`, so the
/// checkpoint the built-in tool writes stays tied to it (`--continue`
/// checks both).
struct ProgressSave {
    progress: Progress,
    path: PathBuf,
}

impl Tool for ProgressSave {
    fn name(&self) -> &str {
        "SaveProgress"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Save)
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let outcome = ctx.invoke(intent);
        let mut result = outcome.result;
        if result.is_ok() {
            let mut progress = self.progress.clone();
            progress.saved_at = ctx.state().player.pose.value.clone();
            match progress.store(&self.path) {
                Ok(()) => ctx.info(format!(
                    "progress written to {} (saved at {})",
                    self.path.display(),
                    progress
                        .saved_at
                        .as_ref()
                        .map_or("?".into(), ToString::to_string)
                )),
                Err(e) => result = Err(e.into()),
            }
        }
        ToolOutcome {
            result,
            // Already emitted by the inner tool.
            learned: Vec::new(),
            pose: outcome.pose,
        }
    }
}

fn execute(
    args: &GoalArgs,
    goal: &GoalPredicate,
    pd: &PlannerData,
    stop: &AtomicBool,
) -> Result<()> {
    let progress_path = args.progress_path();
    let start = if args.new_game {
        Start::NewGame
    } else if args.r#continue {
        Start::Continue
    } else {
        Start::AsIs
    };
    if start == Start::Continue {
        Progress::load(&progress_path).with_context(|| {
            format!(
                "--continue needs the progress file {} (written by --save-game)",
                progress_path.display()
            )
        })?;
    }
    if args.save_game && start == Start::AsIs && Progress::load(&progress_path).is_err() {
        bail!(
            "--save-game needs a progress file to tie the checkpoint to ({}); play the story with --save-game first, or start with --new-game",
            progress_path.display()
        );
    }
    let new_game = NewGameConfig {
        gender: args.gender.into(),
        player_name: args.player.to_ascii_uppercase(),
        rival_name: args.rival.to_ascii_uppercase(),
        soft_reset: true,
    };
    if start == Start::NewGame {
        new_game.validate().map_err(anyhow::Error::msg)?;
    }
    let font_path = args.world.join("font_normal.json");
    let font = pokebot_vision::text::Font::load(&font_path)
        .with_context(|| format!("loading {} (run tools/world/build.sh)", font_path.display()))?;
    let small_font_path = args.world.join("font_small.json");
    let small_font = pokebot_vision::text::Font::load(&small_font_path).with_context(|| {
        format!(
            "loading {} (run tools/world/build.sh)",
            small_font_path.display()
        )
    })?;
    let perception = FireRedPerception::with_world(Arc::clone(&pd.world))
        .with_font(Arc::new(font))
        .with_small_font(Arc::new(small_font))
        .with_palettes(Arc::new(sprite_palettes(&pd.data)))
        // Without CONTINUE or a new game the player is wherever the game
        // was left.
        .with_global_search(start == Start::AsIs);
    let devices = devices::open(&args.devices)?;
    let (video_name, controller_name) =
        (devices.video_name.clone(), devices.controller_name.clone());
    let mut runtime = Runtime::with_perception(devices, perception);
    let (telemetry, session_dir) =
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
    let mut runner = CliRunner {
        args,
        goal,
        pd,
        runtime,
        executor,
        telemetry,
        session_dir,
        state_path: args.state_path(),
        progress_path,
        new_game,
        stop,
    };
    let session = Session {
        start,
        restart: args.restart.then(|| Duration::from_secs(args.restart_wait)),
    };
    let report = goal_session::run(session, &mut runner);
    let CliRunner { mut runtime, .. } = runner;
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
        CycleEnd::Unsatisfied(outcome) => bail!("goal {goal} not reached: {outcome}"),
        CycleEnd::Failed(e) => bail!("{e}"),
        CycleEnd::Stopped => bail!("stopped by user"),
    }
}

/// One goal cycle against the devices: bring the game to where the cycle
/// starts (a new game's menus, CONTINUE, or as it
/// stands), run the goal loop, save at the end.
struct CliRunner<'a> {
    args: &'a GoalArgs,
    goal: &'a GoalPredicate,
    pd: &'a PlannerData,
    runtime: Runtime,
    executor: Executor,
    telemetry: Option<Telemetry>,
    session_dir: Option<PathBuf>,
    progress_path: PathBuf,
    state_path: PathBuf,
    new_game: NewGameConfig,
    stop: &'a AtomicBool,
}

impl CliRunner<'_> {
    /// The game brought to the cycle's start; the progress to tie saves to.
    fn bring_up(&mut self, start: Start) -> Result<Option<Progress>> {
        let snapshots = Some(Path::new(CONSOLE_SNAPSHOTS));
        match start {
            Start::NewGame => {
                let progress = goal_session::start_new_game(
                    &mut self.runtime,
                    &self.executor,
                    &self.new_game,
                    self.args.starter,
                    &self.pd.world,
                    self.stop,
                    snapshots,
                )?;
                self.runtime
                    .info("new game done: the goal loop plays from the first overworld frame");
                Ok(Some(progress))
            }
            Start::Continue => {
                let previous = Progress::load(&self.progress_path).with_context(|| {
                    format!("loading {} to CONTINUE from", self.progress_path.display())
                })?;
                goal_session::continue_game(
                    &mut self.runtime,
                    &self.executor,
                    &previous,
                    &self.state_path,
                    &self.pd.data,
                    self.stop,
                    snapshots,
                )?;
                Ok(Some(previous))
            }
            Start::AsIs => {
                match checkpoint::load(&self.state_path) {
                    Ok(Some(c)) => {
                        if let Some(pose) = c.identity.as_ref().and_then(|i| i.saved_at.clone()) {
                            self.runtime.set_pose_hint(pose);
                        }
                        self.runtime.emit(GameEvent::CheckpointRestored {
                            knowledge: Box::new(c.knowledge),
                        })?;
                        self.runtime.info(format!(
                            "knowledge restored from {}",
                            self.state_path.display()
                        ));
                    }
                    Ok(None) => self.runtime.info(format!(
                        "no checkpoint at {}: planning from what the screen shows",
                        self.state_path.display()
                    )),
                    Err(e) => self
                        .runtime
                        .error(format!("checkpoint {}: {e}", self.state_path.display())),
                }
                Ok(Progress::load(&self.progress_path).ok())
            }
        }
    }

    fn play(&mut self, start: Start) -> Result<GoalReport> {
        let previous = self.bring_up(start)?;
        if self.args.save_game && previous.is_none() {
            bail!(
                "--save-game needs a progress file to tie the checkpoint to ({})",
                self.progress_path.display()
            );
        }
        let planner = self.pd.planner();
        let mut toolbox = Toolbox::default();
        let mut ctx = ToolContext::new(
            &mut self.runtime,
            &self.executor,
            Arc::clone(&self.pd.world),
            Arc::clone(&self.pd.data),
            self.stop,
        );
        if self.args.save_game {
            let previous = previous.expect("checked above");
            ctx = ctx.with_checkpoint(self.state_path.clone(), checkpoint::Identity::of(&previous));
            toolbox.prepend(Box::new(ProgressSave {
                progress: previous,
                path: self.progress_path.clone(),
            }));
        }
        ctx = ctx.with_toolbox(toolbox);
        if start == Start::NewGame {
            // Nothing to read: a new game's party, bag and money are known
            // (and there is no POKéMON entry in the Start menu yet).
            ctx.scheduler.enabled = true;
        } else {
            pokebot_agent::tools::probe::audit_core(&mut ctx)?;
        }
        let on_status = self.telemetry.clone().map(|t| {
            Box::new(move |status: &goal::GoalStatus| t.publish_plan(status)) as goal::StatusSink
        });
        let opts = GoalOptions {
            max_replans: self.args.max_replans,
            save_game: self.args.save_game,
            session_dir: self.session_dir.clone(),
            on_status,
        };
        ctx.info(format!("goal: {}", self.goal));
        let report = goal::run(self.goal, &mut ctx, &planner, opts)?;
        // The end state is the checkpoint, whether or not the last step
        // changed anything.
        if self.args.save_game
            && report.satisfied
            && !report.saved_at_end
            && !self.stop.load(Ordering::Relaxed)
        {
            ctx.invoke(&Intent::Save).result?;
        }
        Ok(report)
    }
}

/// Ctrl-C, from any of the layers a cycle runs through.
fn was_stopped(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ExecutorError>(),
        Some(ExecutorError::Stopped)
    ) || matches!(e.downcast_ref::<ToolError>(), Some(ToolError::Stopped))
}

impl Runner for CliRunner<'_> {
    fn cycle(&mut self, start: Start, cycle: u64) -> CycleEnd {
        let started = std::time::Instant::now();
        if cycle > 1 {
            self.runtime.info(format!("goal cycle {cycle}: {start:?}"));
        }
        let result = self.play(start);
        let runtime = &self.runtime;
        match result {
            Ok(report) => {
                let summary = format!(
                    "Goal {}: {} in {:.1} s ({} plans, {} steps run, {} skipped, {} failures, {} facts learned)",
                    report.goal,
                    report.outcome,
                    started.elapsed().as_secs_f64(),
                    report.plans.len(),
                    report.steps_run,
                    report.steps_skipped,
                    report.failures.len(),
                    report.learned.len()
                );
                if report.satisfied {
                    runtime.info(&summary);
                    return CycleEnd::Satisfied(summary);
                }
                runtime.error(&summary);
                for f in &report.failures {
                    runtime.error(format!("  failed: {f}"));
                }
                if let Some(plan) = report.last_plan() {
                    runtime.error("last plan:");
                    print_plan(plan);
                }
                if let Ok(dir) = write_bundle(runtime, &self.args.bundles, "goal", &report.outcome)
                {
                    runtime.error(format!("debug bundle: {}", dir.display()));
                }
                if report.outcome == "stopped" || self.stop.load(Ordering::Relaxed) {
                    return CycleEnd::Stopped;
                }
                CycleEnd::Unsatisfied(report.outcome)
            }
            Err(e) => {
                if was_stopped(&e) || self.stop.load(Ordering::Relaxed) {
                    runtime.info("Goal: stopped by user");
                    return CycleEnd::Stopped;
                }
                runtime.error(format!("Goal: {e:#}"));
                if let Ok(dir) = write_bundle(runtime, &self.args.bundles, "goal", &e.to_string()) {
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
        self.stop.load(Ordering::Relaxed)
    }

    fn has_checkpoint(&self) -> bool {
        Progress::load(&self.progress_path).is_ok()
    }

    fn log(&self, message: &str) {
        self.runtime.error(format!("NOTIFY {message}"));
    }
}

/// What a step is expected to yield on its own (spec §4.6.2), e.g.
/// "expects +1.3 new species (ZUBAT 0.9, PARAS 0.4)".
fn expected_note(step: &pokebot_planner::PlannedIntent) -> String {
    let species: Vec<String> = step
        .expected
        .iter()
        .filter_map(|(p, prob)| match p {
            GoalPredicate::Caught { caught } => Some(format!(
                "{} {prob:.2}",
                caught.trim_start_matches("SPECIES_")
            )),
            _ => None,
        })
        .collect();
    format!(
        "expects +{:.2} new species ({})",
        step.expected_new_species(),
        species.join(", ")
    )
}

/// The plan as a table: step, intent, cost, what it assumes or is
/// conditional on.
pub fn print_plan(plan: &Plan) {
    if plan.intents.is_empty() {
        println!("nothing to do: the goal already holds");
        return;
    }
    let width = plan
        .intents
        .iter()
        .map(|s| s.intent.to_string().chars().count())
        .max()
        .unwrap_or(0)
        .clamp(20, 90);
    println!(
        "{:>4}  {:<width$}  {:>8}  assumes",
        "step", "intent", "cost"
    );
    for (i, step) in plan.intents.iter().enumerate() {
        let mut notes: Vec<String> = step.assumes.iter().map(|p| p.to_string()).collect();
        for u in &step.unless {
            notes.push(format!("unless {u}"));
        }
        notes.extend(step.note.iter().cloned());
        if step
            .expected
            .iter()
            .any(|(p, _)| matches!(p, GoalPredicate::Caught { .. }))
        {
            notes.push(expected_note(step));
        }
        for (p, prob) in step
            .expected
            .iter()
            .filter(|(p, _)| !matches!(p, GoalPredicate::Caught { .. }))
        {
            notes.push(format!("expects {p} ({:.0}%)", prob * 100.0));
        }
        let mut name = step.intent.to_string();
        if name.chars().count() > width {
            name = name.chars().take(width - 1).collect::<String>() + "…";
        }
        println!(
            "{:>4}  {:<width$}  {:>7.1}s  {}",
            i + 1,
            name,
            step.cost_s,
            notes.join(" & ")
        );
        for leg in &step.route {
            println!("{:>4}    {leg}", "");
        }
    }
    let minutes = plan.cost_s / 60.0;
    println!(
        "total {:.1}s ({minutes:.1} min) over {} steps; belief {:016x}",
        plan.cost_s,
        plan.intents.len(),
        plan.belief_snapshot
    );
    let expected = plan.expected_new_species();
    if expected > 0.0 {
        println!("expects +{expected:.1} new species on the way");
    }
    if !plan.assumes.is_empty() {
        let a: Vec<String> = plan.assumes.iter().map(|p| p.to_string()).collect();
        println!("assumes: {}", a.join(", "));
    }
    let blocked = plan.blocked();
    if !blocked.is_empty() {
        println!("blocked:");
        for b in blocked {
            println!("  - {b}");
        }
    }
}
