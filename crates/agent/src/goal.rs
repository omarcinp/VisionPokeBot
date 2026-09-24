//! The execution loop (spec §8): plan, run the intents through the tools,
//! apply what they learned, replan on failure or when the belief
//! contradicts what a step assumed, save after belief-changing steps.
//!
//! Execution monitoring: a `(intent, reason)` failing twice in a row marks
//! the intent infeasible for the session (the planner then takes another
//! branch); three different intents failing at one pose escalate to a
//! re-localisation and probes of the plan's assumptions; a leg that loops
//! on a tile fails in the `Go` tool (`MAX_TILE_VISITS`).
//!
//! Every plan and replan is written to `plan.jsonl` in the session
//! directory (with its reason) and logged through `Runtime::explain`.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use pokebot_planner::{
    GoalBelief, GoalPredicate, Plan, PlanError, PlannedIntent, Planner, ProbeFact, StateBelief,
};
use pokebot_state::{GameEvent, KnowledgeSource, PlayerPose, SavedKnowledge};
use pokebot_world::predicate::{Predicate, Truth};
use serde::Serialize;

use crate::tools::{progress, Intent, ToolContext, ToolError};

/// Replans allowed before the loop gives up (`--max-replans`).
pub const DEFAULT_MAX_REPLANS: u32 = 8;
/// Different intents failing at one pose before re-localisation.
const RELOCALISE_AFTER: usize = 3;
/// The name under which the loop logs `GoalProgress` events.
const GOAL: &str = "Goal";

/// What the loop plans with: the goal planner, or a scripted stand-in in
/// tests.
pub trait GoalPlanner {
    fn plan(
        &self,
        goal: &GoalPredicate,
        knowledge: &SavedKnowledge,
        pose: Option<PlayerPose>,
    ) -> Result<Plan, PlanError>;
}

impl GoalPlanner for Planner<'_> {
    fn plan(
        &self,
        goal: &GoalPredicate,
        knowledge: &SavedKnowledge,
        pose: Option<PlayerPose>,
    ) -> Result<Plan, PlanError> {
        Planner::plan(self, goal, knowledge, pose)
    }
}

/// What the loop shows while it runs (the web UI's goal panel).
#[derive(Debug, Clone, Default, Serialize)]
pub struct GoalStatus {
    pub goal: String,
    /// 1 for the first plan, +1 per replan.
    pub plan_no: u32,
    /// Why the current plan was made.
    pub reason: String,
    /// Current step (1-based; 0 while planning) of `steps`.
    pub step: usize,
    pub steps: usize,
    pub intent: Option<String>,
    /// `planning`, `running`, `skipped`, `failed`, `done`, `given up`.
    pub phase: String,
    pub detail: String,
    /// The current step's assumptions with what the belief says now.
    pub assumes: Vec<Assumption>,
    /// The whole plan, one line per step.
    pub plan: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Assumption {
    pub predicate: String,
    pub truth: String,
    pub source: String,
}

/// Told about every status change (telemetry).
pub type StatusSink = Box<dyn FnMut(&GoalStatus)>;

pub struct GoalOptions {
    pub max_replans: u32,
    /// Run `Save` after every step that changed the save-relevant belief.
    pub save_game: bool,
    /// Where `plan.jsonl` goes (the recording's session directory).
    pub session_dir: Option<PathBuf>,
    pub on_status: Option<StatusSink>,
}

impl Default for GoalOptions {
    fn default() -> Self {
        GoalOptions {
            max_replans: DEFAULT_MAX_REPLANS,
            save_game: false,
            session_dir: None,
            on_status: None,
        }
    }
}

/// One (re)plan and why it was made.
#[derive(Debug, Clone, Serialize)]
pub struct PlanRecord {
    pub plan_no: u32,
    pub reason: String,
    pub frame_id: u64,
    pub pose: Option<PlayerPose>,
    pub plan: Plan,
}

/// How a goal run ended.
#[derive(Debug, Clone, Serialize)]
pub struct GoalReport {
    pub goal: String,
    pub satisfied: bool,
    /// `goal satisfied`, `out of replans`, `no plan: …`, …
    pub outcome: String,
    /// Every plan made, oldest first (the last is the one in force at the end).
    pub plans: Vec<PlanRecord>,
    pub steps_run: usize,
    pub steps_skipped: usize,
    /// `intent: reason`, oldest first.
    pub failures: Vec<String>,
    /// Everything the tools learned, oldest first.
    pub learned: Vec<GameEvent>,
    /// Intents marked infeasible this session.
    pub infeasible: Vec<String>,
    /// A `Save` followed the last step that ran (nothing changed since).
    pub saved_at_end: bool,
    pub elapsed_s: f64,
}

impl GoalReport {
    pub fn last_plan(&self) -> Option<&Plan> {
        self.plans.last().map(|p| &p.plan)
    }
}

/// The loop's bookkeeping across plans.
struct Run<'g, 'p> {
    goal: &'g GoalPredicate,
    planner: &'p dyn GoalPlanner,
    opts: GoalOptions,
    started: Instant,
    report: GoalReport,
    status: GoalStatus,
    /// The last failure, to spot the same one twice in a row.
    last_failure: Option<(String, String)>,
    /// Intent names that failed at each pose since the last success.
    failed_at: Vec<(PlayerPose, String)>,
}

/// Runs `goal` to completion or until the replans run out (spec §8).
///
/// Only device errors and a stop are `Err`; an unmet goal is a report
/// with `satisfied == false`, the plans made and the facts learned.
pub fn run(
    goal: &GoalPredicate,
    ctx: &mut ToolContext<'_>,
    planner: &dyn GoalPlanner,
    opts: GoalOptions,
) -> Result<GoalReport, ToolError> {
    let mut run = Run {
        goal,
        planner,
        opts,
        started: Instant::now(),
        report: GoalReport {
            goal: goal.to_string(),
            satisfied: false,
            outcome: String::new(),
            plans: Vec::new(),
            steps_run: 0,
            steps_skipped: 0,
            failures: Vec::new(),
            learned: Vec::new(),
            infeasible: Vec::new(),
            saved_at_end: false,
            elapsed_s: 0.0,
        },
        status: GoalStatus {
            goal: goal.to_string(),
            ..GoalStatus::default()
        },
        last_failure: None,
        failed_at: Vec::new(),
    };
    let outcome = run.main(ctx);
    let stopped = matches!(outcome, Err(ToolError::Stopped));
    let report = run.finish(ctx, stopped)?;
    outcome?;
    Ok(report)
}

impl Run<'_, '_> {
    fn main(&mut self, ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
        let mut reason = "start".to_string();
        let mut plan_no = 0u32;
        loop {
            if plan_no > self.opts.max_replans {
                self.report.outcome = format!(
                    "out of replans ({} made, {} allowed)",
                    plan_no - 1,
                    self.opts.max_replans
                );
                return Ok(());
            }
            plan_no += 1;
            let (knowledge, pose) = self.snapshot(ctx)?;
            if self.holds(ctx, &knowledge, pose.clone()) {
                self.report.satisfied = true;
                self.report.outcome = "goal satisfied".into();
                return Ok(());
            }
            self.status.plan_no = plan_no;
            self.status.reason = reason.clone();
            self.set_status("planning", reason.clone(), 0, None, ctx);
            let plan = match self.planner.plan(self.goal, &knowledge, pose.clone()) {
                Ok(plan) => plan,
                Err(e) => {
                    self.report.outcome = format!("no plan: {e}");
                    self.set_status("given up", e.to_string(), 0, None, ctx);
                    return Ok(());
                }
            };
            self.log_plan(ctx, plan_no, &reason, pose, plan.clone())?;
            if plan.intents.is_empty() {
                self.report.outcome =
                    "the planner has nothing to do but the goal is not known to hold".into();
                return Ok(());
            }
            match self.execute(ctx, &plan)? {
                Executed::Satisfied => {
                    self.report.satisfied = true;
                    self.report.outcome = "goal satisfied".into();
                    return Ok(());
                }
                Executed::Replan(why) => reason = why,
                Executed::Completed => {
                    let (knowledge, pose) = self.snapshot(ctx)?;
                    if self.holds(ctx, &knowledge, pose) {
                        self.report.satisfied = true;
                        self.report.outcome = "goal satisfied".into();
                        return Ok(());
                    }
                    reason = "the plan ran through but the goal does not hold".into();
                }
            }
        }
    }

    /// Runs the plan's steps in order.
    fn execute(&mut self, ctx: &mut ToolContext<'_>, plan: &Plan) -> Result<Executed, ToolError> {
        let n = plan.intents.len();
        for (i, step) in plan.intents.iter().enumerate() {
            let k = i + 1;
            let (knowledge, pose) = self.snapshot(ctx)?;
            let belief = StateBelief::new(&knowledge, &ctx.data, pose.clone());
            // "Buy balls if needed": settled by a probe earlier in the plan.
            if !step.unless.is_empty()
                && step
                    .unless
                    .iter()
                    .all(|p| belief.eval_goal(p) == Truth::True)
            {
                self.report.steps_skipped += 1;
                let why = format!(
                    "{}: skipped, already holds: {}",
                    step.intent,
                    list(&step.unless)
                );
                ctx.emit(progress(GOAL, why.clone()))?;
                self.set_status("skipped", why, k, Some(step), ctx);
                continue;
            }
            // Contradiction: what the step assumed is now known false.
            if let Some(p) = step
                .assumes
                .iter()
                .find(|p| belief.eval_goal(p) == Truth::False)
            {
                let why = format!(
                    "belief_contradiction: {} assumed {p}, now false",
                    step.intent
                );
                ctx.runtime.explain(
                    "belief_contradiction",
                    &serde_json::json!({
                        "intent": step.intent.to_string(),
                        "assumed": p.to_string(),
                        "frame_id": frame_id(ctx),
                    }),
                );
                ctx.emit(progress(GOAL, why.clone()))?;
                self.set_status("failed", why.clone(), k, Some(step), ctx);
                return Ok(Executed::Replan(why));
            }
            let intent = match Intent::from_planned(&step.intent, Some(&ctx.world)) {
                Ok(intent) => intent,
                Err(e) => {
                    let why = self.failed(ctx, plan, step, &e.to_string())?;
                    self.set_status("failed", why.clone(), k, Some(step), ctx);
                    return Ok(Executed::Replan(why));
                }
            };
            self.set_status("running", intent.to_string(), k, Some(step), ctx);
            ctx.emit(progress(GOAL, format!("step {k}/{n}: {}", step.intent)))?;
            ctx.runtime.explain(
                "goal_step",
                &serde_json::json!({
                    "plan_no": self.status.plan_no,
                    "step": k,
                    "steps": n,
                    "intent": step.intent.to_string(),
                    "tool_intent": intent.to_string(),
                    "assumes": step.assumes.iter().map(ToString::to_string).collect::<Vec<_>>(),
                }),
            );
            let outcome = ctx.invoke(&intent);
            self.report.learned.extend(outcome.learned.iter().cloned());
            self.report.saved_at_end = false;
            match outcome.result {
                Ok(()) => {
                    self.report.steps_run += 1;
                    self.last_failure = None;
                    self.failed_at.clear();
                    if self.opts.save_game && outcome.learned.iter().any(changes_save) {
                        let saved = ctx.invoke(&Intent::Save);
                        self.report.learned.extend(saved.learned.iter().cloned());
                        self.report.saved_at_end = saved.result.is_ok();
                        if let Err(e) = saved.result {
                            if matches!(e, ToolError::Stopped | ToolError::Device(_)) {
                                return Err(e);
                            }
                            let why = self.failed(ctx, plan, step, &format!("save: {e}"))?;
                            return Ok(Executed::Replan(why));
                        }
                    }
                    let (knowledge, pose) = self.snapshot(ctx)?;
                    if self.holds(ctx, &knowledge, pose) {
                        self.set_status("done", "goal satisfied".into(), k, Some(step), ctx);
                        return Ok(Executed::Satisfied);
                    }
                }
                Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
                Err(e) => {
                    let why = self.failed(ctx, plan, step, &e.to_string())?;
                    self.set_status("failed", why.clone(), k, Some(step), ctx);
                    return Ok(Executed::Replan(why));
                }
            }
        }
        Ok(Executed::Completed)
    }

    /// Books a step's failure and applies the plan-level loop rules; the
    /// replan reason.
    fn failed(
        &mut self,
        ctx: &mut ToolContext<'_>,
        plan: &Plan,
        step: &PlannedIntent,
        reason: &str,
    ) -> Result<String, ToolError> {
        let key = step.intent.to_string();
        let why = format!("{key} failed: {reason}");
        self.report.failures.push(why.clone());
        ctx.emit(progress(GOAL, why.clone()))?;
        // The same failure twice in a row: don't plan this intent again.
        let again = self.last_failure.as_ref() == Some(&(key.clone(), reason.to_owned()));
        if again {
            ctx.emit(GameEvent::IntentInfeasible {
                intent: key.clone(),
            })?;
            ctx.runtime.explain(
                "intent_infeasible",
                &serde_json::json!({ "intent": key, "reason": reason }),
            );
            self.report.infeasible.push(key.clone());
            self.last_failure = None;
        } else {
            self.last_failure = Some((key.clone(), reason.to_owned()));
        }
        // Different intents failing at one pose: the position may be wrong.
        if let Some(pose) = ctx.pose() {
            let name = step.intent.name().to_owned();
            if !self.failed_at.iter().any(|(p, n)| *p == pose && *n == name) {
                self.failed_at.push((pose.clone(), name));
            }
            let here = self.failed_at.iter().filter(|(p, _)| *p == pose).count();
            if here >= RELOCALISE_AFTER {
                self.relocalise(ctx, plan, &pose)?;
                self.failed_at.clear();
            }
        }
        Ok(why)
    }

    /// Re-localisation after repeated failures at one pose: observe afresh
    /// and probe every fact the plan assumed, so the next plan starts from
    /// observed facts. (The perception keeps searching from its last pose;
    /// a whole-world search needs a hint reset the runtime does not offer
    /// yet, so this is the probe half of the rule.)
    fn relocalise(
        &mut self,
        ctx: &mut ToolContext<'_>,
        plan: &Plan,
        pose: &PlayerPose,
    ) -> Result<(), ToolError> {
        ctx.runtime.explain(
            "relocalise",
            &serde_json::json!({
                "pose": pose,
                "failed": self.failed_at.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>(),
                "assumes": plan.assumes.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }),
        );
        ctx.emit(progress(
            GOAL,
            format!(
                "{RELOCALISE_AFTER} different intents failed at {pose}: re-localising and probing {}",
                list(&plan.assumes)
            ),
        ))?;
        ctx.observe()?;
        let mut probed: BTreeSet<ProbeFact> = BTreeSet::new();
        for p in &plan.assumes {
            let Some(fact) = ProbeFact::for_predicate(p, &ctx.data) else {
                continue;
            };
            if !probed.insert(fact.clone()) {
                continue;
            }
            let planned = pokebot_planner::Intent::Probe { fact };
            let Ok(intent) = Intent::from_planned(&planned, Some(&ctx.world)) else {
                continue;
            };
            let outcome = ctx.invoke(&intent);
            self.report.learned.extend(outcome.learned.iter().cloned());
            match outcome.result {
                Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
                Err(e) => ctx.info(format!("probe {intent} after re-localisation: {e}")),
                Ok(()) => {}
            }
        }
        Ok(())
    }

    /// The knowledge to plan from (the session's infeasible intents kept)
    /// and the pose, observing a frame when none is known yet.
    fn snapshot(
        &self,
        ctx: &mut ToolContext<'_>,
    ) -> Result<(SavedKnowledge, Option<PlayerPose>), ToolError> {
        let mut pose = ctx.pose();
        if pose.is_none() {
            ctx.observe()?;
            pose = ctx.pose();
        }
        let state = ctx.state();
        let mut knowledge = state.saved_knowledge();
        knowledge.world.infeasible = state.world.infeasible.clone();
        Ok((knowledge, pose))
    }

    fn holds(
        &self,
        ctx: &ToolContext<'_>,
        knowledge: &SavedKnowledge,
        pose: Option<PlayerPose>,
    ) -> bool {
        StateBelief::new(knowledge, &ctx.data, pose).eval_goal(self.goal) == Truth::True
    }

    fn log_plan(
        &mut self,
        ctx: &mut ToolContext<'_>,
        plan_no: u32,
        reason: &str,
        pose: Option<PlayerPose>,
        plan: Plan,
    ) -> Result<(), ToolError> {
        let record = PlanRecord {
            plan_no,
            reason: reason.to_owned(),
            frame_id: frame_id(ctx),
            pose,
            plan,
        };
        let lines: Vec<String> = record
            .plan
            .intents
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {} ({:.0}s)", i + 1, s.intent, s.cost_s))
            .collect();
        ctx.info(format!(
            "plan {plan_no} ({reason}): {} steps, {:.0}s: {}",
            record.plan.intents.len(),
            record.plan.cost_s,
            lines.join("; ")
        ));
        ctx.runtime.explain(
            if plan_no == 1 { "plan" } else { "replan" },
            &serde_json::json!({
                "plan_no": plan_no,
                "reason": reason,
                "cost_s": record.plan.cost_s,
                "steps": lines,
                "assumes": record.plan.assumes.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }),
        );
        ctx.runtime.record("plan", &record)?;
        if let Some(dir) = &self.opts.session_dir {
            let path = dir.join("plan.jsonl");
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| pokebot_core::Error::io(&path, e))?;
            let json = serde_json::to_string(&record)
                .map_err(|e| pokebot_core::Error::InvalidData(e.to_string()))?;
            writeln!(file, "{json}").map_err(|e| pokebot_core::Error::io(&path, e))?;
        }
        self.status.plan = lines;
        self.report.plans.push(record);
        Ok(())
    }

    fn set_status(
        &mut self,
        phase: &str,
        detail: String,
        step: usize,
        planned: Option<&PlannedIntent>,
        ctx: &ToolContext<'_>,
    ) {
        self.status.phase = phase.into();
        self.status.detail = detail;
        self.status.step = step;
        self.status.steps = self.status.plan.len();
        self.status.intent = planned.map(|p| p.intent.to_string());
        self.status.assumes = planned
            .map(|p| {
                let state = ctx.state();
                let knowledge = state.saved_knowledge();
                let belief = StateBelief::new(&knowledge, &ctx.data, ctx.pose());
                p.assumes
                    .iter()
                    .map(|a| Assumption {
                        predicate: a.to_string(),
                        truth: format!("{:?}", belief.eval_goal(a)),
                        source: format!("{:?}", provenance(&knowledge, a)),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(on_status) = &mut self.opts.on_status {
            on_status(&self.status);
        }
    }

    fn finish(
        &mut self,
        ctx: &mut ToolContext<'_>,
        stopped: bool,
    ) -> Result<GoalReport, ToolError> {
        let mut report = self.report.clone();
        if stopped {
            report.outcome = "stopped".into();
        }
        report.elapsed_s = self.started.elapsed().as_secs_f64();
        let phase = if report.satisfied { "done" } else { "given up" };
        self.set_status(phase, report.outcome.clone(), self.status.step, None, ctx);
        if !stopped {
            ctx.emit(GameEvent::GoalFinished {
                goal: report.goal.clone(),
                success: report.satisfied,
                detail: report.outcome.clone(),
            })?;
        }
        Ok(report)
    }
}

enum Executed {
    Satisfied,
    Completed,
    Replan(String),
}

fn frame_id(ctx: &ToolContext<'_>) -> u64 {
    ctx.observation().map_or(0, |o| o.frame_id)
}

fn list(ps: &[GoalPredicate]) -> String {
    ps.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Events that change what the save file holds (flags, items, party,
/// money, Pokédex): the step is followed by a `Save` with `--save-game`.
pub fn changes_save(event: &GameEvent) -> bool {
    matches!(
        event,
        GameEvent::FlagObserved { .. }
            | GameEvent::FlagTracked { .. }
            | GameEvent::VarObserved { .. }
            | GameEvent::VarTracked { .. }
            | GameEvent::ItemsChanged { .. }
            | GameEvent::PocketObserved { .. }
            | GameEvent::PartyObserved { .. }
            | GameEvent::PartyMonDerived { .. }
            | GameEvent::MovesObserved { .. }
            | GameEvent::MoveLearned { .. }
            | GameEvent::MoveReplaced { .. }
            | GameEvent::Evolved { .. }
            | GameEvent::Healed
            | GameEvent::MoneyObserved { .. }
            | GameEvent::MoneyChanged { .. }
            | GameEvent::SentToPc { .. }
            | GameEvent::MonDeposited { .. }
            | GameEvent::MonWithdrawn { .. }
            | GameEvent::SpeciesCaught { .. }
            | GameEvent::BadgeEarned { .. }
            | GameEvent::ScriptPathRun { .. }
            | GameEvent::RespawnSet { .. }
    )
}

/// Where the belief's word on `p` comes from.
pub fn provenance(knowledge: &SavedKnowledge, p: &GoalPredicate) -> KnowledgeSource {
    match p {
        GoalPredicate::World(Predicate::Flag { name, .. }) => knowledge.world.flag(name).source,
        GoalPredicate::World(Predicate::Badge { n }) => {
            knowledge.world.flag(&Predicate::badge_flag(*n)).source
        }
        GoalPredicate::World(Predicate::Var { name, .. }) => knowledge.world.var(name).source,
        GoalPredicate::World(Predicate::Visited { map }) => knowledge.world.visited(map).source,
        GoalPredicate::World(Predicate::HasItem { item, .. }) => knowledge
            .bag
            .pockets
            .values()
            .find(|k| k.value.iter().flatten().any(|(name, _)| name == item))
            .map_or(KnowledgeSource::Unknown, |k| k.source),
        GoalPredicate::World(Predicate::At { .. }) => KnowledgeSource::Observed,
        GoalPredicate::World(Predicate::PartyHasMove { .. })
        | GoalPredicate::CanBeat { .. }
        | GoalPredicate::Healed { .. } => knowledge.party.source,
        GoalPredicate::Caught { caught } => knowledge
            .pokedex
            .caught
            .get(caught)
            .map_or(KnowledgeSource::Unknown, |k| k.source),
        GoalPredicate::Money { .. } => knowledge.money.source,
    }
}
