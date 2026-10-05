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
//! Stuck (an intent infeasible, one reason failing twice across intents,
//! no plan, the replans spent): the ranked recourses of
//! [`crate::recourse`] are tried best first until one changes the belief,
//! which is worth another plan.
//!
//! Every plan and replan is written to `plan.jsonl` in the session
//! directory (with its reason) and logged through `Runtime::explain`.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use pokebot_planner::gauntlet::Gauntlet;
use pokebot_planner::{
    GoalBelief, GoalPredicate, Plan, PlanError, PlannedIntent, Planner, ProbeFact, StateBelief,
};
use pokebot_state::{GameEvent, KnowledgeSource, PlayerPose, SavedKnowledge, ScreenState};
use pokebot_world::predicate::{Predicate, Truth};
use serde::Serialize;

use crate::catch::SideCatch;
use crate::ledger::fingerprint;
use crate::nugget_farm::{self, FarmConfig};
use crate::recourse::{self, Recourse, Stall};
use crate::tools::{progress, Intent, ToolContext, ToolError};

/// Replans allowed before the loop gives up (`--max-replans`).
pub const DEFAULT_MAX_REPLANS: u32 = 8;
/// Different intents failing at one pose before re-localisation.
const RELOCALISE_AFTER: usize = 3;
/// The name under which the loop logs `GoalProgress` events.
const GOAL: &str = "Goal";
/// Recourses one goal run may try in all.
const MAX_RECOURSES: u32 = 24;
/// Recourses tried for one stall before the loop replans or gives up.
const RECOURSES_PER_STALL: usize = 6;

/// What the loop plans with: the goal planner, or a scripted stand-in in
/// tests.
pub trait GoalPlanner {
    fn plan(
        &self,
        goal: &GoalPredicate,
        knowledge: &SavedKnowledge,
        pose: Option<PlayerPose>,
    ) -> Result<Plan, PlanError>;

    /// The win probability a `CanBeat` is planned to (`PlanOptions`): a
    /// step expecting less leaves readiness to be judged again.
    fn confidence(&self) -> f64 {
        pokebot_planner::PlanOptions::default().confidence
    }

    /// The battles a white-out undoes together, when known.
    fn gauntlet(&self) -> Option<&Gauntlet> {
        None
    }
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

    fn confidence(&self) -> f64 {
        self.options.confidence
    }

    fn gauntlet(&self) -> Option<&Gauntlet> {
        Some(&self.gauntlet)
    }
}

/// Readiness judged again at most this many times in a run without
/// counting as a replan.
const MAX_REJUDGES: u32 = 24;

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

/// Steps after one whose target is out of reach looked at for one to run
/// first ([`Run::look_ahead`]).
const LOOK_AHEAD: usize = 3;

/// Told about every status change (telemetry).
pub type StatusSink = Box<dyn FnMut(&GoalStatus)>;

pub struct GoalOptions {
    pub max_replans: u32,
    /// Run `Save` after every step that changed the save-relevant belief.
    pub save_game: bool,
    /// Where `plan.jsonl` goes (the recording's session directory).
    pub session_dir: Option<PathBuf>,
    pub on_status: Option<StatusSink>,
    /// The Nugget Bridge farm, run before its grunt is fought.
    pub nugget_farm: Option<FarmConfig>,
}

impl Default for GoalOptions {
    fn default() -> Self {
        GoalOptions {
            max_replans: DEFAULT_MAX_REPLANS,
            save_game: false,
            session_dir: None,
            on_status: None,
            nugget_farm: None,
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
    /// Later steps already run ahead of one whose target was out of reach
    /// ([`LOOK_AHEAD`]), so each is tried once.
    looked_ahead: BTreeSet<String>,
    /// The last failure's reason, whatever the intent (a plan retrying
    /// one script by its paths fails the same way under other names).
    last_reason: Option<String>,
    /// Recourses tried in this run.
    recourses_run: u32,
    /// Probes a recourse has run in this run.
    probed: BTreeSet<ProbeFact>,
    /// Times readiness was judged again after a step that fell short.
    rejudges: u32,
    /// The save a white-out reloads is inside a gauntlet (the run started
    /// there, and nothing was saved since).
    saved_in_gauntlet: bool,
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
        looked_ahead: BTreeSet::new(),
        last_reason: None,
        recourses_run: 0,
        probed: BTreeSet::new(),
        rejudges: 0,
        saved_in_gauntlet: false,
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
        self.saved_in_gauntlet = self.in_gauntlet(ctx)?;
        loop {
            if plan_no > self.opts.max_replans && self.recover(ctx)? {
                // What the map showed is worth one more plan.
                plan_no -= 1;
            }
            if plan_no > self.opts.max_replans {
                self.report.outcome = format!(
                    "out of replans ({} made, {} allowed)",
                    plan_no - 1,
                    self.opts.max_replans
                );
                return Ok(());
            }
            plan_no += 1;
            // A badge earned since the last plan whose giving path went
            // unread (the leader's opening pages unrecognised) is recorded
            // before planning, not only at the next cycle's audit (fleet
            // worker 4: Brock beaten, Pewter's gym-guide gate still shut,
            // and the cycle's replans spent looking for the way east).
            crate::tools::dialogue::reconcile_badges(ctx)?;
            let (knowledge, pose) = self.snapshot(ctx)?;
            if self.holds(ctx, &knowledge, pose.clone()) {
                self.report.satisfied = true;
                self.report.outcome = "goal satisfied".into();
                return Ok(());
            }
            if knowledge.party.value.is_none() {
                self.report.outcome =
                    "party unknown at start: a party-menu audit is required before planning".into();
                return Ok(());
            }
            self.status.plan_no = plan_no;
            self.status.reason = reason.clone();
            let urgent = urgent_health(&knowledge);
            let recovery_goal = GoalPredicate::Healed { healed: true };
            let goal = if urgent { &recovery_goal } else { self.goal };
            let mut plan_reason = if urgent {
                format!("urgent health: {reason}")
            } else {
                reason.clone()
            };
            // Set when the plan is for a stepping stone, not the goal.
            let mut stone = None;
            self.set_status("planning", plan_reason.clone(), 0, None, ctx);
            // No way to heal (fleet continue-6, the Elite Four: the rooms
            // lock behind, "no plan establishes Healed", and the run gave up
            // in LORELEI's room): the goal is planned as the party stands.
            let mut healing = urgent;
            let planned = match self.planner.plan(goal, &knowledge, pose.clone()) {
                Err(e) if urgent => {
                    plan_reason = format!("{reason}; no way to heal ({e})");
                    healing = false;
                    self.planner.plan(self.goal, &knowledge, pose.clone())
                }
                r => r,
            };
            // Nor do the steps stop to look for a healer (fleet continue-6,
            // after LORELEI: every walk to BRUNO's room failed "urgent
            // recovery: no known safe route to a healer").
            ctx.scheduler.no_healer = urgent && !healing;
            let plan = match planned {
                Ok(plan) => plan,
                Err(e) => {
                    let stepping = (!urgent && matches!(e, PlanError::Budget { .. }))
                        .then(|| self.stepping_stone(ctx, &knowledge, pose.clone()))
                        .flatten();
                    if let Some((next, plan)) = stepping {
                        plan_reason = format!("{plan_reason}; stepping stone {next} ({e})");
                        stone = Some(next);
                        plan
                    } else if self.recover(ctx)? {
                        reason = format!("no plan ({e}); a recourse changed the belief");
                        continue;
                    } else {
                        self.report.outcome = format!("no plan: {e}");
                        self.set_status("given up", e.to_string(), 0, None, ctx);
                        return Ok(());
                    }
                }
            };
            // The tools' routes read the belief as the plan did.
            ctx.inferred = crate::belief_view::Inferred {
                flags: plan.implied.flags.clone(),
                floors: plan.implied.var_floors.clone(),
                values: Default::default(),
            };
            self.log_plan(ctx, plan_no, &plan_reason, pose, plan.clone())?;
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
                Executed::Rejudge(why) => {
                    // The party is what the step made it: progress, so
                    // the replans start over (bounded).
                    reason = why;
                    self.rejudges += 1;
                    if self.rejudges <= MAX_REJUDGES {
                        plan_no = 0;
                    }
                }
                Executed::Fainted(why) => {
                    // The save is inside a gauntlet (fleet continue-6, in
                    // LORELEI's room with LANCE at 0%): a reload loses
                    // there again, with no way out to train. The game has
                    // started the gauntlet over itself and put the player
                    // at the Center: the goal goes on from there.
                    if self.saved_in_gauntlet {
                        // Past the white-out pages and the nurse first
                        // (the next walk began on "RED scurried to a
                        // POKéMON CENTER" and failed "whited out").
                        ctx.drive(&mut crate::whiteout::AfterWhiteOut::default())?;
                        if let Some(g) = self.planner.gauntlet() {
                            for e in &g.undo {
                                ctx.emit(e.clone())?;
                            }
                        }
                        self.saved_in_gauntlet = false;
                        reason = format!(
                            "{why}; the save is inside the gauntlet, where a reload loses again: on from the white-out"
                        );
                        ctx.emit(progress(GOAL, reason.clone()))?;
                        plan_no = 0;
                        continue;
                    }
                    self.report.outcome = why;
                    return Ok(());
                }
                Executed::Completed => {
                    let (knowledge, pose) = self.snapshot(ctx)?;
                    if self.holds(ctx, &knowledge, pose.clone()) {
                        self.report.satisfied = true;
                        self.report.outcome = "goal satisfied".into();
                        return Ok(());
                    }
                    if let Some(stone) = stone.as_ref().filter(|s| {
                        StateBelief::new(&knowledge, &ctx.data, pose.clone()).eval_goal(s)
                            == Truth::True
                    }) {
                        // Progress: the replans start over from here.
                        reason = format!("stepping stone {stone} reached");
                        plan_no = 0;
                        continue;
                    }
                    if healing {
                        if urgent_health(&knowledge) {
                            self.report.outcome =
                                "healed, but party health is still unsafe or unreadable".into();
                            return Ok(());
                        }
                        reason = "urgent health recovered".into();
                    } else {
                        reason = "the plan ran through but the goal does not hold".into();
                    }
                }
            }
        }
    }

    /// Runs the plan's steps in order.
    fn execute(&mut self, ctx: &mut ToolContext<'_>, plan: &Plan) -> Result<Executed, ToolError> {
        let n = plan.intents.len();
        let start = Owned::of(&self.snapshot(ctx)?.0);
        for (i, step) in plan.intents.iter().enumerate() {
            let k = i + 1;
            let (knowledge, pose) = self.snapshot(ctx)?;
            let belief = StateBelief::new(&knowledge, &ctx.data, pose.clone());
            let now = Owned::of(&knowledge);
            let earlier = catches_before(&plan.intents[..i], &step.intent);
            if let Some(why) = caught_on_the_side_after(&step.intent, &start, &now, earlier)
                .or_else(|| grass_unneeded(step, &plan.intents[k..], &start, &now))
            {
                self.report.steps_skipped += 1;
                let why = format!("{}: skipped, {why}", step.intent);
                ctx.emit(progress(GOAL, why.clone()))?;
                self.set_status("skipped", why, k, Some(step), ctx);
                // Skipped, it still falls short: readiness is judged again
                // (fleet emu worker 5: the second PIDGEY's catch, planned
                // at an expected 0% against Brock, was skipped for the
                // first one caught; the plan went on to the gym and
                // whited out, every cycle).
                if let Some(why) = short_of(step, self.planner.confidence()) {
                    self.set_status("rejudge", why.clone(), k, Some(step), ctx);
                    return Ok(Executed::Rejudge(why));
                }
                continue;
            }
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
                if let Some(why) = short_of(step, self.planner.confidence()) {
                    self.set_status("rejudge", why.clone(), k, Some(step), ctx);
                    return Ok(Executed::Rejudge(why));
                }
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
            if let (Intent::Beat { trainer, .. }, Some(config)) = (&intent, &self.opts.nugget_farm)
            {
                if trainer == nugget_farm::ROCKET {
                    match nugget_farm::before_the_grunt(ctx, config) {
                        Ok(Some(done)) => {
                            ctx.emit(progress(GOAL, format!("nugget farm: {done}")))?
                        }
                        Ok(None) => {}
                        Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
                        Err(e) => {
                            let why = self.failed(ctx, plan, step, &format!("nugget farm: {e}"))?;
                            self.set_status("failed", why.clone(), k, Some(step), ctx);
                            return Ok(Executed::Replan(why));
                        }
                    }
                }
            }
            ctx.scheduler.assumptions = step.assumes.clone();
            ctx.scheduler.side_catch = wanted_later(self.goal, &plan.intents[k..], &start, &now);
            if !ctx.scheduler.side_catch.is_empty() && matches!(intent, Intent::Train { .. }) {
                ctx.emit(progress(
                    GOAL,
                    format!(
                        "training catches on the side: {}",
                        side_list(&ctx.scheduler.side_catch)
                    ),
                ))?;
            }
            let outcome = ctx.invoke(&intent);
            ctx.scheduler.assumptions.clear();
            ctx.scheduler.side_catch = SideCatch::default();
            if outcome.result.is_ok() {
                // The step establishing what it assumed absent (a trainer
                // beaten, a flag set) is its effect, not a contradiction;
                // the next step's own check covers the rest.
                ctx.scheduler.invalidated = None;
            }
            self.report.learned.extend(outcome.learned.iter().cloned());
            self.report.saved_at_end = false;
            match outcome.result {
                Ok(()) => {
                    self.report.steps_run += 1;
                    // Only this intent succeeding clears its failure: the
                    // walk there succeeding in between doesn't (fleet
                    // continue-6: Go(Route23), then Train on Route 23
                    // failing "no path to (14, 43)", 26 times, never
                    // "twice in a row").
                    if self
                        .last_failure
                        .as_ref()
                        .is_some_and(|(k, _)| *k == step.intent.to_string())
                    {
                        self.last_failure = None;
                    }
                    self.last_reason = None;
                    self.failed_at.clear();
                    // Not inside a gauntlet: a reload there could only
                    // lose again.
                    if self.opts.save_game
                        && outcome.learned.iter().any(changes_save)
                        && !self.in_gauntlet(ctx)?
                    {
                        let saved = ctx.invoke(&Intent::Save);
                        self.report.learned.extend(saved.learned.iter().cloned());
                        self.report.saved_at_end = saved.result.is_ok();
                        self.saved_in_gauntlet &= saved.result.is_err();
                        if let Err(e) = saved.result {
                            if matches!(e, ToolError::Stopped | ToolError::Device(_)) {
                                return Err(e);
                            }
                            // No saving here (the Safari Zone): the step
                            // stands, saved once the player is out.
                            if e.to_string().contains(crate::save::NO_SAVE_HERE) {
                                ctx.info(format!("save after {}: {e}", step.intent));
                            } else {
                                let why = self.failed(ctx, plan, step, &format!("save: {e}"))?;
                                return Ok(Executed::Replan(why));
                            }
                        }
                    }
                    let (knowledge, pose) = self.snapshot(ctx)?;
                    if self.holds(ctx, &knowledge, pose) {
                        self.set_status("done", "goal satisfied".into(), k, Some(step), ctx);
                        return Ok(Executed::Satisfied);
                    }
                    // Readiness planned this step knowing it falls short
                    // ("reaches only …: judged again after"): the plan
                    // doesn't go on to that battle (fleet workers 2 and
                    // 5 caught a second PIDGEY, then fought Brock at an
                    // expected 0% and whited out).
                    if let Some(why) = short_of(step, self.planner.confidence()) {
                        self.set_status("rejudge", why.clone(), k, Some(step), ctx);
                        return Ok(Executed::Rejudge(why));
                    }
                }
                Err(ToolError::Replan(why)) => return Ok(Executed::Replan(why)),
                Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
                Err(e) => {
                    let reason = e.to_string();
                    if let Some(why) = self.fainted(ctx, &reason)? {
                        self.report
                            .failures
                            .push(format!("{} failed: {reason}", step.intent));
                        self.set_status("fainted", why.clone(), k, Some(step), ctx);
                        return Ok(Executed::Fainted(why));
                    }
                    // Its target out of reach: a later step of the plan may
                    // be what opens the way (Switch, Silph Co.: 11F's door
                    // is opened from the far side of the floor, reached past
                    // the 7F rival planned right after it; "no path next to
                    // (5, 16)", in a loop). The next few are tried once.
                    if reason.starts_with("no path") {
                        if let Some(why) = self.look_ahead(ctx, plan, i)? {
                            return Ok(Executed::Replan(why));
                        }
                    }
                    let why = self.failed(ctx, plan, step, &reason)?;
                    self.set_status("failed", why.clone(), k, Some(step), ctx);
                    return Ok(Executed::Replan(why));
                }
            }
        }
        Ok(Executed::Completed)
    }

    /// Runs the first of the [`LOOK_AHEAD`] steps after step `i` that is a
    /// battle or a script and wasn't run ahead already; why to replan,
    /// when one ran.
    fn look_ahead(
        &mut self,
        ctx: &mut ToolContext<'_>,
        plan: &Plan,
        i: usize,
    ) -> Result<Option<String>, ToolError> {
        // Only on the failed step's map: a later step elsewhere is a trip,
        // not what opens the way (Switch: from Saffron, the Safari Zone's
        // entry ran "first"; walked toward Fuchsia, the Route 11 gatehouse,
        // alike, was taken for the entry and its path recorded unplayed).
        let here = plan.intents.get(i).and_then(|s| planned_map(&s.intent));
        for later in plan.intents.iter().skip(i + 1).take(LOOK_AHEAD) {
            let key = later.intent.to_string();
            let same_map = here.is_some() && planned_map(&later.intent) == here;
            if !same_map || self.looked_ahead.contains(&key) {
                continue;
            }
            self.looked_ahead.insert(key.clone());
            let Ok(intent) = Intent::from_planned(&later.intent, Some(&ctx.world)) else {
                continue;
            };
            ctx.emit(progress(GOAL, format!("out of reach: {key} first")))?;
            let outcome = ctx.invoke(&intent);
            self.report.learned.extend(outcome.learned.iter().cloned());
            return match outcome.result {
                Ok(()) => Ok(Some(format!("ran {key} ahead of a step out of reach"))),
                Err(e @ (ToolError::Stopped | ToolError::Device(_))) => Err(e),
                Err(_) => Ok(None),
            };
        }
        Ok(None)
    }

    /// A white-out (the screen, the tool's reason, or every member at 0 HP)
    /// ends the run: the user's rule is that the game restarts only when
    /// all the Pokémon have fainted, so the session reloads the last save.
    /// One member fainting is not the end: the battle sends out the next,
    /// and the plan heals after it. On the white-out the belief gets what
    /// the game does: the party healed and the player at the respawn spot.
    fn fainted(
        &mut self,
        ctx: &mut ToolContext<'_>,
        reason: &str,
    ) -> Result<Option<String>, ToolError> {
        let screen = ctx.observation().map(|o| o.screen.value);
        let all_fainted = ctx.state().party.value.as_ref().is_some_and(|p| {
            !p.is_empty() && p.iter().all(|m| m.hp.value.is_some_and(|(hp, _)| hp == 0))
        });
        let whiteout =
            screen == Some(ScreenState::Whiteout) || reason.contains("whited out") || all_fainted;
        if !whiteout {
            return Ok(None);
        }
        let why = format!("fainted: {reason}");
        ctx.emit(progress(GOAL, why.clone()))?;
        ctx.emit(GameEvent::WhitedOut)?;
        let respawn = respawn_pose(ctx);
        ctx.emit(GameEvent::Healed)?;
        ctx.runtime.clear_pose_hint();
        if let Some(pose) = respawn {
            ctx.info(format!("whited out: the game puts the player at {pose}"));
            ctx.emit(GameEvent::PlayerLocated { pose: pose.clone() })?;
            ctx.runtime.set_pose_hint(pose);
        }
        self.report.outcome = why.clone();
        Ok(Some(why))
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
        let retracted = ctx.retract_open_gates()?;
        if !retracted.is_empty() {
            ctx.emit(progress(
                GOAL,
                format!("a passage believed open is blocked: {retracted:?} can't hold"),
            ))?;
        }
        // The same failure twice in a row: don't plan this intent again.
        // Not for a `Go`: a leg fails at one tile (an NPC in the way, a
        // block the world model lacks), which the navigator learns and
        // routes around; marking the whole destination infeasible cut the
        // only way to Mt. Moon B2F for the session (flash-6).
        let again = self.last_failure.as_ref() == Some(&(key.clone(), reason.to_owned()));
        let same_reason = self.last_reason.as_deref() == Some(reason);
        self.last_reason = Some(reason.to_owned());
        if again && step.intent.name() == "Go" {
            let blocked = ctx.blocked.lock().unwrap_or_else(|e| e.into_inner()).all();
            ctx.emit(progress(
                GOAL,
                format!(
                    "{key} failed twice; keeping it plannable, blocked tiles learnt: {blocked:?}"
                ),
            ))?;
            self.last_failure = None;
        } else if again {
            ctx.emit(GameEvent::IntentInfeasible {
                intent: key.clone(),
            })?;
            ctx.runtime.explain(
                "intent_infeasible",
                &serde_json::json!({ "intent": key, "reason": reason }),
            );
            self.report.infeasible.push(key.clone());
            self.last_failure = None;
            self.recover(ctx)?;
        } else {
            self.last_failure = Some((key.clone(), reason.to_owned()));
            if same_reason && step.intent.name() != "Go" {
                // Another intent, the same failure (`RunScript(Bill[9])`,
                // then `Bill[30]`, `Bill[5]`, …): the plan is going round.
                ctx.emit(progress(
                    GOAL,
                    format!("{reason}: failed again under another intent"),
                ))?;
                self.recover(ctx)?;
            }
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

    /// When the goal can't be planned within the budget (the whole game
    /// from a lab with Charmander or Squirtle preferred: 20000 nodes and
    /// 240 s were not enough, while `badge 1` plans in seconds), the next
    /// badge not held and its plan: the goal is planned again once it is
    /// reached.
    fn stepping_stone(
        &self,
        ctx: &ToolContext<'_>,
        knowledge: &SavedKnowledge,
        pose: Option<PlayerPose>,
    ) -> Option<(GoalPredicate, Plan)> {
        let belief = StateBelief::new(knowledge, &ctx.data, pose.clone());
        let stone = (1..=8)
            .map(|n| GoalPredicate::World(Predicate::Badge { n }))
            .find(|g| belief.eval_goal(g) != Truth::True)?;
        if stone == *self.goal {
            return None;
        }
        let plan = self.planner.plan(&stone, knowledge, pose).ok()?;
        (!plan.intents.is_empty()).then_some((stone, plan))
    }

    /// The last resort of a stuck plan (an intent failed twice, one
    /// reason failed under two intents, no plan, the replans spent): the
    /// recourses on offer ([`recourse::offers`]) are tried best first until
    /// one changes what the belief knows, at most [`RECOURSES_PER_STALL`]
    /// (and [`MAX_RECOURSES`] per run). Each outcome is booked in the
    /// ledger, which is what the next ranking is computed from. Whether
    /// the belief changed.
    fn recover(&mut self, ctx: &mut ToolContext<'_>) -> Result<bool, ToolError> {
        let stall = self.stall(ctx);
        let world = std::sync::Arc::clone(&ctx.world);
        ctx.scheduler
            .graph
            .get_or_insert_with(|| crate::scheduler::graph(&world));
        let mut tried: BTreeSet<String> = BTreeSet::new();
        for _ in 0..RECOURSES_PER_STALL {
            if self.recourses_run >= MAX_RECOURSES {
                ctx.emit(progress(
                    GOAL,
                    format!("stuck: the {MAX_RECOURSES} recourses of this run are spent"),
                ))?;
                return Ok(false);
            }
            let Some(pose) = ctx.pose() else {
                return Ok(false);
            };
            let offers: Vec<_> = recourse::offers(
                &ctx.world,
                ctx.scheduler.graph.as_ref(),
                &ctx.ledger,
                ctx.state(),
                &pose,
                &stall,
            )
            .into_iter()
            .filter(|o| !tried.contains(&o.recourse.to_string()))
            .collect();
            ctx.runtime.explain(
                "recourse",
                &serde_json::json!({
                    "pose": pose,
                    "offers": offers.iter().take(6).map(|o| serde_json::json!({
                        "recourse": o.recourse.to_string(),
                        "chance": o.chance,
                        "cost_s": o.cost_s,
                        "priority": o.priority(),
                        "why": o.why,
                    })).collect::<Vec<_>>(),
                }),
            );
            let Some(best) = offers.into_iter().next() else {
                ctx.emit(progress(
                    GOAL,
                    format!("stuck on {}: no recourse left", pose.map),
                ))?;
                return Ok(false);
            };
            tried.insert(best.recourse.to_string());
            self.recourses_run += 1;
            ctx.emit(progress(
                GOAL,
                format!(
                    "stuck on {}: {} ({:.0}% in {:.0} s: {})",
                    pose.map,
                    best.recourse,
                    best.chance * 100.0,
                    best.cost_s,
                    best.why
                ),
            ))?;
            let before = fingerprint(ctx.state());
            let started = Instant::now();
            let result = self.run_recourse(ctx, &best.recourse);
            // `Explore` succeeds only once the belief changed.
            let unblocked = matches!(
                (&best.recourse, &result),
                (Recourse::Explore { .. }, Ok(()))
            ) || fingerprint(ctx.state()) != before;
            ctx.ledger.tried(
                best.recourse.kind(),
                unblocked,
                started.elapsed().as_secs_f64(),
            );
            if let Err(e) = ctx.ledger.store() {
                ctx.info(format!("ledger: {e}"));
            }
            match result {
                Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
                Err(e) => ctx.emit(progress(GOAL, format!("{}: {e}", best.recourse)))?,
                Ok(()) => {}
            }
            if unblocked {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// What the last plan says about the stall: the maps it goes to or
    /// acts on, and the probes settling its assumptions that were not
    /// observed (nor probed by a recourse already).
    fn stall(&self, ctx: &ToolContext<'_>) -> Stall {
        let Some(plan) = self.report.last_plan() else {
            return Stall::default();
        };
        let relevant = plan
            .intents
            .iter()
            .filter_map(|p| Intent::from_planned(&p.intent, Some(&ctx.world)).ok())
            .filter_map(|i| intent_map(&i).map(str::to_owned))
            .collect();
        let knowledge = ctx.state().saved_knowledge();
        let mut probes: Vec<ProbeFact> = Vec::new();
        for p in &plan.assumes {
            if provenance(&knowledge, p) == KnowledgeSource::Observed {
                continue;
            }
            let Some(fact) = ProbeFact::for_predicate(p, &ctx.data) else {
                continue;
            };
            let planned = pokebot_planner::Intent::Probe { fact: fact.clone() };
            if self.probed.contains(&fact)
                || probes.contains(&fact)
                || Intent::from_planned(&planned, None).is_err()
            {
                continue;
            }
            probes.push(fact);
        }
        Stall { relevant, probes }
    }

    fn run_recourse(
        &mut self,
        ctx: &mut ToolContext<'_>,
        recourse: &Recourse,
    ) -> Result<(), ToolError> {
        match recourse {
            Recourse::Explore { map } => {
                let outcome = ctx.invoke(&Intent::Explore {
                    map: Some(map.clone()),
                });
                self.report.learned.extend(outcome.learned.iter().cloned());
                outcome.result
            }
            Recourse::Probe { facts } => {
                for fact in facts {
                    self.probed.insert(fact.clone());
                    let planned = pokebot_planner::Intent::Probe { fact: fact.clone() };
                    let Ok(intent) = Intent::from_planned(&planned, Some(&ctx.world)) else {
                        continue;
                    };
                    let outcome = ctx.invoke(&intent);
                    self.report.learned.extend(outcome.learned.iter().cloned());
                    match outcome.result {
                        Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return Err(e),
                        Err(e) => ctx.info(format!("recourse {intent}: {e}")),
                        Ok(()) => {}
                    }
                }
                Ok(())
            }
        }
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
    /// Whether a gauntlet is under way (in one of its rooms, a battle of it
    /// won): no save then.
    fn in_gauntlet(&self, ctx: &mut ToolContext<'_>) -> Result<bool, ToolError> {
        let Some(g) = self.planner.gauntlet() else {
            return Ok(false);
        };
        let (knowledge, pose) = self.snapshot(ctx)?;
        Ok(g.in_progress(&StateBelief::new(&knowledge, &ctx.data, pose)))
    }

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
        // Trainers lost to since last beaten, from the ledger (it outlives
        // the reload after a white-out): the plan asks for more before
        // meeting them again.
        knowledge.handicaps = ctx.ledger.handicaps();
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
    /// A step readiness knew falls short of a battle ran: plan again
    /// with the party it made.
    Rejudge(String),
    /// The party whited out (every Pokémon fainted): the run ends
    /// (`fainted: …`) for the session to reload the save.
    Fainted(String),
}

/// Why readiness is judged again after `step`: it was planned expecting
/// a `CanBeat` below `confidence`.
fn short_of(step: &PlannedIntent, confidence: f64) -> Option<String> {
    step.expected
        .iter()
        .find(|(p, prob)| matches!(p, GoalPredicate::CanBeat { .. }) && *prob < confidence)
        .map(|(p, prob)| {
            format!(
                "readiness judged again after {}: {p} expected {:.0}%",
                step.intent,
                prob * 100.0
            )
        })
}

/// Where the game puts the player after a white-out: the respawn heal
/// spot's interior (`respawn_map` of `places.json`; the last Pokémon
/// Center healed at, else Mom's house), at a walkable tile near its
/// middle (the exact spot isn't in the data; the localizer's search
/// widens from there).
pub fn respawn_pose(ctx: &ToolContext<'_>) -> Option<PlayerPose> {
    let places = ctx.world.places()?;
    let known = ctx.state().world.respawn.value.as_ref();
    let spot = known
        .and_then(|r| {
            places
                .heal_spots
                .iter()
                .find(|h| h.map == r.map && h.x == r.x && h.y == r.y)
        })
        .or_else(|| places.heal_spot("HEAL_LOCATION_PALLET_TOWN"))?;
    // The decomp's tile (Switch-era guess: the open tile nearest the
    // middle, which at the Indigo Plateau is the nurse's, behind the
    // counter; fleet continue-6 found no way out from there).
    if let Some((x, y)) = spot.respawn {
        return Some(PlayerPose {
            map: spot.respawn_map.clone(),
            x,
            y,
        });
    }
    let map = ctx.world.map(&spot.respawn_map)?;
    let (cx, cy) = (map.width / 2, map.height / 2);
    let mut best: Option<(i32, (i32, i32))> = None;
    for y in 0..map.height {
        for x in 0..map.width {
            if map.tile(x, y).is_some_and(|t| t.collision == 0) {
                let d = (x - cx).abs() + (y - cy).abs();
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, (x, y)));
                }
            }
        }
    }
    best.map(|(_, (x, y))| PlayerPose {
        map: spot.respawn_map.clone(),
        x,
        y,
    })
}

/// The map an intent walks to or acts on.
/// The map a planned battle or script happens on.
fn planned_map(intent: &pokebot_planner::Intent) -> Option<&str> {
    match intent {
        pokebot_planner::Intent::RunScript { map, .. }
        | pokebot_planner::Intent::Beat { map, .. } => Some(map),
        _ => None,
    }
}

fn intent_map(intent: &Intent) -> Option<&str> {
    match intent {
        Intent::Go { dest } => Some(dest.map()),
        Intent::Talk { map, .. } | Intent::Beat { map, .. } | Intent::Train { map, .. } => {
            Some(map)
        }
        Intent::Catch { map, .. } | Intent::Explore { map } => map.as_deref(),
        Intent::Heal { center } => center.as_deref(),
        _ => None,
    }
}

fn frame_id(ctx: &ToolContext<'_>) -> u64 {
    ctx.observation().map_or(0, |o| o.frame_id)
}

/// What the Pokédex marks caught and how many of each species the party
/// holds: taken when a plan starts running and before each step.
#[derive(Debug, Clone, Default, PartialEq)]
struct Owned {
    marked: BTreeSet<String>,
    party: std::collections::BTreeMap<String, usize>,
}

impl Owned {
    fn of(knowledge: &SavedKnowledge) -> Owned {
        let marked = knowledge
            .pokedex
            .caught
            .iter()
            .filter(|(_, k)| k.value == Some(true))
            .map(|(s, _)| s.clone())
            .collect();
        let mut party = std::collections::BTreeMap::new();
        for m in knowledge.party.value.iter().flatten() {
            if let Some(s) = &m.species.value {
                *party.entry(s.clone()).or_insert(0) += 1;
            }
        }
        Owned { marked, party }
    }
}

/// Why a plan's `Catch` needn't run: an earlier step of the plan caught
/// its species on the side (a training hunt, a walk's catch). A catch for
/// the Pokédex (the species not marked when the plan began) is done once
/// it is marked; one for the party (marked already: readiness wants it as
/// a member, fleet worker 2's second PIDGEY) once the party holds more of
/// it than when the plan began.
fn caught_on_the_side(
    intent: &pokebot_planner::Intent,
    start: &Owned,
    now: &Owned,
) -> Option<String> {
    let pokebot_planner::Intent::Catch { species, .. } = intent else {
        return None;
    };
    if !start.marked.contains(species) {
        return now
            .marked
            .contains(species)
            .then(|| format!("{species} caught on the side since the plan began"));
    }
    let held = |o: &Owned| o.party.get(species).copied().unwrap_or(0);
    (held(now) > held(start))
        .then(|| format!("{species} caught into the party on the side since the plan began"))
}

/// The `Catch`es of `intent`'s species among `before` (the plan's steps
/// ahead of it).
fn catches_before(before: &[PlannedIntent], intent: &pokebot_planner::Intent) -> usize {
    let pokebot_planner::Intent::Catch { species, .. } = intent else {
        return 0;
    };
    before
        .iter()
        .filter(|s| matches!(&s.intent, pokebot_planner::Intent::Catch { species: o, .. } if o == species))
        .count()
}

/// [`caught_on_the_side`] for a plan's `Catch` with `earlier` catches of
/// its species ahead of it: done on the side only once the party holds
/// more of it than those earlier catches added (fleet emu worker 5: two
/// PIDGEY catches, the second for readiness against Brock, skipped for
/// the first one caught, and Brock fought at 0%).
fn caught_on_the_side_after(
    intent: &pokebot_planner::Intent,
    start: &Owned,
    now: &Owned,
    earlier: usize,
) -> Option<String> {
    if earlier == 0 {
        return caught_on_the_side(intent, start, now);
    }
    let pokebot_planner::Intent::Catch { species, .. } = intent else {
        return None;
    };
    let held = |o: &Owned| o.party.get(species).copied().unwrap_or(0);
    (held(now) > held(start) + earlier)
        .then(|| format!("{species} caught into the party on the side since the plan began"))
}

/// Why a catch's walk to the grass needn't run: the `Catch`es it leads
/// to (those right after it, on its map) were all caught on the side.
fn grass_unneeded(
    step: &PlannedIntent,
    rest: &[PlannedIntent],
    start: &Owned,
    now: &Owned,
) -> Option<String> {
    let pokebot_planner::Intent::Go { dest } = &step.intent else {
        return None;
    };
    if !step
        .note
        .as_deref()
        .is_some_and(|n| n.starts_with("to the grass"))
    {
        return None;
    }
    let catches: Vec<&PlannedIntent> = rest
        .iter()
        .take_while(
            |s| matches!(&s.intent, pokebot_planner::Intent::Catch { map, .. } if map == dest),
        )
        .collect();
    (!catches.is_empty()
        && catches
            .iter()
            .all(|s| caught_on_the_side(&s.intent, start, now).is_some()))
    .then(|| "the catches it leads to were made on the side".to_string())
}

/// What the steps after the running one (`rest`) and the goal want caught,
/// but for what was caught on the side already: a hunt catches it when
/// met ([`SideCatch`]).
fn wanted_later(
    goal: &GoalPredicate,
    rest: &[PlannedIntent],
    start: &Owned,
    now: &Owned,
) -> SideCatch {
    let mut side = SideCatch::default();
    for step in rest {
        if let pokebot_planner::Intent::Catch { species, .. } = &step.intent {
            if caught_on_the_side(&step.intent, start, now).is_none() {
                side.species.insert(species.clone());
            }
        }
    }
    match goal {
        GoalPredicate::Caught { caught } if !now.marked.contains(caught) => {
            side.species.insert(caught.clone());
        }
        GoalPredicate::PokedexCaught { .. } => side.any_new = true,
        _ => {}
    }
    side
}

fn side_list(side: &SideCatch) -> String {
    let mut out: Vec<String> = side.species.iter().cloned().collect();
    if side.any_new {
        out.push("any species not caught yet".into());
    }
    out.join(", ")
}

fn list(ps: &[GoalPredicate]) -> String {
    ps.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A damaged or unreadable lead cannot safely be sent into a walking or
/// training plan. A full party has more margin than its last battler.
fn urgent_health(knowledge: &SavedKnowledge) -> bool {
    let Some(party) = knowledge.party.value.as_ref() else {
        return true;
    };
    // No Pokémon yet (a new game): nothing can faint or be healed.
    if party.is_empty() {
        return false;
    }
    let usable = party
        .iter()
        .filter(|mon| {
            mon.hp.value.is_some_and(|hp| {
                mon.level.value.is_some_and(|level| {
                    crate::party::plausible_hp_for(hp, level, mon.species.value.as_deref())
                }) && hp.0 > 0
            })
        })
        .count();
    let Some(lead) = party.first() else {
        return true;
    };
    let Some((hp, max)) = lead.hp.value.filter(|hp| {
        lead.level.value.is_some_and(|level| {
            crate::party::plausible_hp_for(*hp, level, lead.species.value.as_deref())
        })
    }) else {
        return true;
    };
    let threshold = if usable <= 1 { 50 } else { 35 };
    u32::from(hp) * 100 < u32::from(max) * threshold
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
            | GameEvent::PocketRowsObserved { .. }
            | GameEvent::PartyAudited { .. }
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
        | GoalPredicate::Healed { .. }
        | GoalPredicate::LeadHp { .. } => knowledge.party.source,
        GoalPredicate::Caught { caught } => knowledge
            .pokedex
            .caught
            .get(caught)
            .map_or(KnowledgeSource::Unknown, |k| k.source),
        GoalPredicate::Money { .. } => knowledge.money.source,
        GoalPredicate::PokedexCaught { .. } | GoalPredicate::PokedexSeen { .. } => {
            knowledge.pokedex.counts.source
        }
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;
    use pokebot_state::{Knowledge, PartyMon};

    fn party(hps: &[Option<(u16, u16)>]) -> SavedKnowledge {
        SavedKnowledge {
            party: Knowledge::observed(
                hps.iter()
                    .map(|hp| PartyMon {
                        level: Knowledge::observed(6, 1),
                        hp: hp.map_or_else(Knowledge::unknown, |v| Knowledge::observed(v, 1)),
                        ..PartyMon::default()
                    })
                    .collect(),
                1,
            ),
            ..Default::default()
        }
    }

    /// Fleet workers 2 and 5: readiness caught a second PIDGEY knowing
    /// the party still reached 0% against Brock ("judged again after"),
    /// and the plan went on to fight him. A step expecting a `CanBeat`
    /// short of the confidence is judged again; one expecting it met, or
    /// only a catch, isn't.
    #[test]
    fn a_step_readiness_knew_falls_short_is_judged_again() {
        let step = |expected: Vec<(GoalPredicate, f64)>| PlannedIntent {
            intent: pokebot_planner::Intent::Catch {
                species: "SPECIES_PIDGEY".into(),
                map: "Route1".into(),
                slot: "land".into(),
                balls: 7,
            },
            cost_s: 80.0,
            assumes: Vec::new(),
            unless: Vec::new(),
            note: None,
            route: Vec::new(),
            expected,
        };
        let brock = GoalPredicate::can_beat("TRAINER_LEADER_BROCK");
        let why = short_of(&step(vec![(brock.clone(), 0.0)]), 0.9).expect("judged again");
        assert!(why.contains("TRAINER_LEADER_BROCK"), "{why}");
        assert_eq!(short_of(&step(vec![(brock, 0.95)]), 0.9), None);
        assert_eq!(
            short_of(
                &step(vec![(GoalPredicate::caught("SPECIES_PIDGEY"), 0.5)]),
                0.9
            ),
            None
        );
    }

    fn catch(species: &str, map: &str) -> PlannedIntent {
        PlannedIntent {
            intent: pokebot_planner::Intent::Catch {
                species: species.into(),
                map: map.into(),
                slot: "land".into(),
                balls: 7,
            },
            cost_s: 80.0,
            assumes: Vec::new(),
            unless: Vec::new(),
            note: None,
            route: Vec::new(),
            expected: Vec::new(),
        }
    }

    fn owned(marked: &[&str], party: &[&str]) -> Owned {
        let mut o = Owned {
            marked: marked.iter().map(|s| s.to_string()).collect(),
            ..Owned::default()
        };
        for s in party {
            *o.party.entry(s.to_string()).or_insert(0) += 1;
        }
        o
    }

    /// A `Train` before a `Catch(ODDISH)` on the same grass catches the
    /// ODDISH it meets, and the `Catch` (and its walk to the grass) is
    /// skipped: for the Pokédex once ODDISH is marked caught, for the
    /// party (PIDGEY marked long before, fleet worker 2) once the party
    /// holds one more. What is still wanted goes to the hunt.
    /// Fleet emu worker 5: a plan with two PIDGEY catches, the second
    /// for readiness against Brock; the first one caught made the second
    /// look done, and Brock was fought at 0%. The second is done only once
    /// a second PIDGEY is in the party.
    #[test]
    fn a_second_catch_of_a_species_wants_a_second_one() {
        let first = catch("SPECIES_PIDGEY", "Route1");
        let second = catch("SPECIES_PIDGEY", "Route1");
        let plan = [first.clone(), second.clone()];
        assert_eq!(catches_before(&plan[..1], &second.intent), 1);
        let start = owned(&[], &["SPECIES_CHARMANDER"]);
        let one = owned(
            &["SPECIES_PIDGEY"],
            &["SPECIES_CHARMANDER", "SPECIES_PIDGEY"],
        );
        assert!(caught_on_the_side_after(&first.intent, &start, &one, 0).is_some());
        assert_eq!(
            caught_on_the_side_after(&second.intent, &start, &one, 1),
            None
        );
        let two = owned(
            &["SPECIES_PIDGEY"],
            &["SPECIES_CHARMANDER", "SPECIES_PIDGEY", "SPECIES_PIDGEY"],
        );
        assert!(caught_on_the_side_after(&second.intent, &start, &two, 1).is_some());
    }

    #[test]
    fn a_catch_made_on_the_side_skips_the_later_catch() {
        let oddish = catch("SPECIES_ODDISH", "Route24");
        let pidgey = catch("SPECIES_PIDGEY", "Route24");
        let start = owned(&["SPECIES_PIDGEY"], &["SPECIES_IVYSAUR", "SPECIES_PIDGEY"]);
        // Nothing caught yet: both run, and both are wanted on the side.
        assert_eq!(caught_on_the_side(&oddish.intent, &start, &start), None);
        assert_eq!(caught_on_the_side(&pidgey.intent, &start, &start), None);
        let goal = GoalPredicate::badge(3);
        let side = wanted_later(&goal, &[oddish.clone(), pidgey.clone()], &start, &start);
        assert_eq!(
            side.species.iter().map(String::as_str).collect::<Vec<_>>(),
            ["SPECIES_ODDISH", "SPECIES_PIDGEY"]
        );
        assert!(!side.any_new);
        // ODDISH caught while training: marked in the Pokédex.
        let now = owned(
            &["SPECIES_PIDGEY", "SPECIES_ODDISH"],
            &["SPECIES_IVYSAUR", "SPECIES_PIDGEY", "SPECIES_ODDISH"],
        );
        assert!(caught_on_the_side(&oddish.intent, &start, &now).is_some());
        // The PIDGEY mark was there before: only one more in the party counts.
        assert_eq!(caught_on_the_side(&pidgey.intent, &start, &now), None);
        let side = wanted_later(&goal, &[oddish.clone(), pidgey.clone()], &start, &now);
        assert_eq!(
            side.species.iter().map(String::as_str).collect::<Vec<_>>(),
            ["SPECIES_PIDGEY"]
        );
        let more = owned(
            &["SPECIES_PIDGEY", "SPECIES_ODDISH"],
            &["SPECIES_IVYSAUR", "SPECIES_PIDGEY", "SPECIES_PIDGEY"],
        );
        assert!(caught_on_the_side(&pidgey.intent, &start, &more).is_some());
        // The walk to the grass goes only while one of its catches is left.
        let mut go = PlannedIntent {
            intent: pokebot_planner::Intent::Go {
                dest: "Route24".into(),
            },
            ..oddish.clone()
        };
        go.note = Some("to the grass at (10, 5)".into());
        let rest = [oddish.clone(), pidgey.clone()];
        assert_eq!(grass_unneeded(&go, &rest, &start, &now), None);
        assert!(grass_unneeded(&go, &rest[..1], &start, &now).is_some());
        // A Pokédex goal wants any new species; a `Caught` goal its own.
        assert!(wanted_later(&GoalPredicate::pokedex_caught(30), &[], &start, &now).any_new);
        let side = wanted_later(&GoalPredicate::caught("SPECIES_ABRA"), &[], &start, &now);
        assert!(side.species.contains("SPECIES_ABRA"));
    }

    #[test]
    fn urgency_rises_when_the_last_usable_member_is_wounded_or_unreadable() {
        assert!(urgent_health(&party(&[Some((10, 22))])));
        assert!(urgent_health(&party(&[Some((9, 2))])));
        assert!(urgent_health(&party(&[None])));
        assert!(!urgent_health(&party(&[Some((10, 22)), Some((22, 22))])));
        assert!(urgent_health(&party(&[Some((7, 22)), Some((22, 22))])));
    }
}
