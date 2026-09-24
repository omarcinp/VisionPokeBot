//! `pokebot goal`: plan a goal predicate from a checkpoint and print the
//! plan. Execution arrives with the goal loop.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use pokebot_agent::Progress;
use pokebot_gamedata::GameData;
use pokebot_planner::{load_checkpoint, parse_goal, Methods, Obtain, Plan, PlanOptions, Planner};
use pokebot_state::inference::InferenceRules;
use pokebot_state::{PlayerPose, Priors, SavedKnowledge, RULES_DIR};
use pokebot_world::route::{PlaceGraph, RouteParams};
use pokebot_world::World;

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
    /// Where the game was saved (its `saved_at` overrides the checkpoint's)
    #[arg(long)]
    pub progress: Option<PathBuf>,
    /// Directory with `priors.json`, `inference.json` and `methods.json`
    #[arg(long, default_value = RULES_DIR)]
    pub rules: PathBuf,
    /// A wrong assumption dearer than this many seconds makes a probe mandatory
    #[arg(long, default_value_t = 600.0)]
    pub expensive_secs: f64,
    /// World model directory (tools/world/build.sh)
    #[arg(long, default_value = "data/world")]
    pub world: PathBuf,
}

/// Loads the checkpoint and the pose the plan starts from.
fn load_state(args: &GoalArgs) -> Result<(SavedKnowledge, Option<PlayerPose>)> {
    let (mut knowledge, mut pose) = load_checkpoint(&args.state)
        .with_context(|| format!("loading {}", args.state.display()))?;
    if let Some(progress) = &args.progress {
        let p =
            Progress::load(progress).with_context(|| format!("loading {}", progress.display()))?;
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

pub fn run(args: GoalArgs) -> Result<()> {
    let goal = parse_goal(&args.goal).map_err(|e| anyhow::anyhow!(e))?;
    if !args.dry_run {
        bail!("execution arrives with the goal loop; use --dry-run to see the plan");
    }
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
    let methods =
        load_optional(&args.rules.join("methods.json"), |p| Methods::load(p))?.unwrap_or_default();
    let (knowledge, pose) = load_state(&args)?;
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let options = PlanOptions {
        expensive_secs: args.expensive_secs,
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
    println!("goal: {goal}");
    match &pose {
        Some(p) => println!("from: {p}"),
        None => println!("from: unknown position"),
    }
    let plan = planner
        .plan(&goal, &knowledge, pose)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    print_plan(&plan);
    Ok(())
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
    }
    let minutes = plan.cost_s / 60.0;
    println!(
        "total {:.1}s ({minutes:.1} min) over {} steps; belief {:016x}",
        plan.cost_s,
        plan.intents.len(),
        plan.belief_snapshot
    );
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
