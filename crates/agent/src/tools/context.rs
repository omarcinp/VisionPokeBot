//! What a tool runs with: the runtime and executor to act through, the
//! world data, the belief, and the other tools. `act` wraps every executor
//! call with the interrupt check (spec §7.3); `drive` runs a small
//! closed-loop step machine (a [`ToolStep`]) the way the executor runs a
//! [`Task`], with the same interrupt check on every frame.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use pokebot_core::NormalizedFrame;
use pokebot_gamedata::GameData;
use pokebot_runtime::{Notice, Origin, Runtime};
use pokebot_state::{GameEvent, GameState, Observation, PlayerPose, ScreenState};
use pokebot_world::World;

use super::{BattlePlan, Intent, ToolError, ToolOutcome, Toolbox};
use crate::executor::OutsideRecovery;
use crate::motion::SyncerHandle;
use crate::nav::{Blocked, Gone};
use crate::{checkpoint, Action, Decision, Executor, Outcome, Task, TaskContext};

/// Quiet frames (no dialogue, menu, battle or transition) after a
/// conversation or cutscene before a tool starts walking or opens a menu;
/// scripts often pause between lines.
pub const SETTLE_FRAMES: u32 = 90;
/// Frames in a row the white-out screen must show before it counts.
pub const WHITEOUT_FRAMES: u32 = 30;
/// A battle that starts this soon after dialogue is a trainer battle (the
/// trainer talks first); wild battles start without any.
const TRAINER_DIALOGUE_WINDOW: u64 = 600;
/// Tools calling tools: deeper than this is a loop.
const MAX_DEPTH: usize = 6;
/// Where unrecognised dialogue frames and their text are saved.
const UNKNOWN_DIR: &str = "captures/unknown";

/// What the running step treats as normal on screen. Anything else is an
/// interrupt: a battle box runs the battle tool, dialogue or a menu runs
/// `Unstick` (dialogue recognition and the generic way out).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Expects {
    pub dialogue: bool,
    pub battle: bool,
    pub menu: bool,
}

impl Expects {
    /// Nothing but the overworld.
    pub const NONE: Expects = Expects {
        dialogue: false,
        battle: false,
        menu: false,
    };
    /// A conversation, questions included.
    pub const DIALOGUE: Expects = Expects {
        dialogue: true,
        battle: false,
        menu: true,
    };
    /// A battle and everything it shows.
    pub const BATTLE: Expects = Expects {
        dialogue: true,
        battle: true,
        menu: true,
    };
    /// Menus without dialogue (the Start menu, the bag).
    pub const MENUS: Expects = Expects {
        dialogue: true,
        battle: false,
        menu: true,
    };
}

/// What a step sees when deciding.
pub struct StepContext<'a> {
    pub observation: &'a Observation,
    pub state: &'a GameState,
    /// Events the step wants recorded.
    pub events: &'a mut Vec<GameEvent>,
    /// Frames without dialogue, menu, battle or transition.
    pub quiet_frames: u32,
    /// The frame behind the observation (for snapshots).
    pub frame: Option<&'a NormalizedFrame>,
    /// Everything learned by the running tool so far.
    pub learned: &'a [GameEvent],
}

/// A small closed-loop machine a tool drives: the next input for the
/// current frame, and the executor's verdict on it.
pub trait ToolStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision;
    fn on_outcome(&mut self, _action: &Action, _outcome: Outcome, _ctx: &mut StepContext<'_>) {}
    /// What this step treats as normal on the current frame.
    fn expects(&self) -> Expects {
        Expects::NONE
    }
}

/// A [`Task`] driven as a step (the save and continue flows).
pub struct AsStep<'t> {
    pub task: &'t mut dyn Task,
    pub expects: Expects,
}

impl ToolStep for AsStep<'_> {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        self.task.next(&mut TaskContext {
            observation: ctx.observation,
            state: ctx.state,
            events: ctx.events,
        })
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        self.task.on_outcome(
            action,
            outcome,
            &mut TaskContext {
                observation: ctx.observation,
                state: ctx.state,
                events: ctx.events,
            },
        );
    }

    fn expects(&self) -> Expects {
        self.expects
    }
}

pub struct ToolContext<'a> {
    pub runtime: &'a mut Runtime,
    pub executor: &'a Executor,
    pub world: Arc<World>,
    pub data: Arc<GameData>,
    pub stop: &'a AtomicBool,
    /// The device's timing model (the executor's, unless given).
    pub syncer: Option<SyncerHandle>,
    /// Map objects known to be gone (taken items): they don't block.
    pub gone: Gone,
    /// Tiles learnt to be blocked this session, shared by every leg.
    pub blocked: Blocked,
    /// `state.json` and the identity to write it with after a save.
    pub checkpoint: Option<(PathBuf, checkpoint::Identity)>,
    /// Where unrecognised dialogue is saved (frame and text).
    pub unknown_dir: PathBuf,
    pub scheduler: crate::scheduler::Scheduler,
    /// The compiled script `RunScript` is following, if any (a battle it
    /// starts is its battle).
    pub running_script: Option<String>,
    /// Tiles walked, people seen and talked to, recourse outcomes
    /// ([`crate::ledger`]); in memory only unless given a file.
    pub ledger: crate::ledger::Ledger,
    next_need_check: u64,
    toolbox: Toolbox,
    expects: Expects,
    depth: usize,
    learned: Vec<GameEvent>,
    /// Per step being driven (innermost last): interruptions per tile,
    /// counted between its actions and during them ([`count_interrupt`]).
    interrupt_counts: Vec<std::collections::HashMap<pokebot_state::PlayerPose, u32>>,
    quiet_frames: u32,
    last_map: Option<String>,
    last_dialogue_frame: Option<u64>,
    outside: OutsideRecovery,
    /// Frames in a row the white-out screen has shown.
    whiteout_frames: u32,
    /// The world's story passages ([`pokebot_world::gates::derive`]),
    /// derived once.
    gates: std::cell::OnceCell<Arc<pokebot_world::gates::Gates>>,
    /// Facts the runtime's sensor read, not yet handed to the scheduler.
    perceived: Receiver<Notice>,
    /// Every step and sighting the runtime saw (frames the executor reads
    /// for a walk too), for the ledger.
    walked: Receiver<Notice>,
}

/// Sensor facts a tool learns from (not the screen classification, the
/// pose or the view, which the tools read from observations).
fn sensed(event: &GameEvent) -> bool {
    !matches!(
        event,
        GameEvent::ScreenChanged { .. }
            | GameEvent::PlayerLocated { .. }
            | GameEvent::PlayerMoved { .. }
            | GameEvent::MapChanged { .. }
            | GameEvent::FramesDroppedByCard { .. }
            | GameEvent::FramesDroppedByProcessing { .. }
            | GameEvent::ViewObserved { .. }
    )
}

impl<'a> ToolContext<'a> {
    pub fn new(
        runtime: &'a mut Runtime,
        executor: &'a Executor,
        world: Arc<World>,
        data: Arc<GameData>,
        stop: &'a AtomicBool,
    ) -> Self {
        let syncer = executor.syncer.clone();
        // What the belief knows is gone (taken items, removed objects):
        // the session's own discoveries are added as it runs.
        let gone = crate::nav::belief_gone(&world, runtime.state());
        let perceived = runtime.subscribe(|n| {
            matches!(n, Notice::Event { record, origin: Origin::Perception } if sensed(&record.event))
        });
        let walked = runtime.subscribe(|n| {
            matches!(n, Notice::Event { record, .. } if matches!(
                record.event,
                GameEvent::PlayerMoved { .. }
                    | GameEvent::PlayerLocated { .. }
                    | GameEvent::NpcSeen { .. }
            ))
        });
        Self {
            perceived,
            walked,
            runtime,
            executor,
            world,
            data,
            stop,
            syncer,
            gone,
            blocked: Blocked::default(),
            checkpoint: None,
            unknown_dir: PathBuf::from(UNKNOWN_DIR),
            scheduler: crate::scheduler::Scheduler::default(),
            running_script: None,
            ledger: crate::ledger::Ledger::default(),
            next_need_check: 0,
            toolbox: Toolbox::default(),
            expects: Expects::NONE,
            depth: 0,
            learned: Vec::new(),
            interrupt_counts: Vec::new(),
            quiet_frames: 0,
            last_map: None,
            last_dialogue_frame: None,
            outside: OutsideRecovery::default(),
            whiteout_frames: 0,
            gates: std::cell::OnceCell::new(),
        }
    }

    /// The story passages as the belief stands now: what a walk must go
    /// around and may go through.
    pub fn gate_tiles(&self) -> pokebot_world::gates::GateTiles {
        pokebot_world::gates::GateTiles::believed(
            &self.world,
            &self.story_gates(),
            &crate::belief_view::StateBelief(self.runtime.state()),
        )
    }

    /// The trainer `intent` fights: a `Beat`, or a script path that
    /// starts a trainer battle (a gym leader's).
    fn trainer_of(&self, intent: &Intent) -> Option<String> {
        match intent {
            Intent::Beat { trainer, .. } => Some(trainer.clone()),
            Intent::RunScript {
                script,
                path: Some(path),
                ..
            } => self
                .world
                .events()?
                .script(script)?
                .paths
                .get(*path)?
                .does
                .iter()
                .find_map(|e| match e {
                    pokebot_world::events::Effect::Battle { battle, .. } => Some(battle.clone()),
                    _ => None,
                }),
            _ => None,
        }
    }

    fn story_gates(&self) -> Arc<pokebot_world::gates::Gates> {
        Arc::clone(self.gates.get_or_init(|| match &self.scheduler.graph {
            Some(g) => Arc::new(g.gates().clone()),
            None => Arc::new(pokebot_world::gates::derive(&self.world)),
        }))
    }

    /// A tile a script draws (a beam, a barrier) that the belief holds
    /// open but a walk found it can't step onto: the belief is wrong, and
    /// each flag it was open by alone is observed the other way (Switch,
    /// Vermilion Gym: a trash can's "second lock opened" path recorded on
    /// the plan's word; the beams stayed on and Lt. Surge was walked to
    /// eight plans in a row). The flags corrected.
    pub fn retract_open_gates(&mut self) -> Result<Vec<String>, ToolError> {
        use pokebot_world::gates::{gate_truth, GateKind};
        use pokebot_world::predicate::{check, Predicate, Truth};
        let gates = self.story_gates();
        let blocked = self.blocked.lock().unwrap_or_else(|e| e.into_inner()).all();
        let mut wrong: Vec<(String, bool)> = Vec::new();
        {
            let belief = crate::belief_view::StateBelief(self.runtime.state());
            for (map, tiles) in &blocked {
                let Some(on) = gates.get(map) else { continue };
                for gate in tiles.iter().filter_map(|t| on.get(t)) {
                    if gate.kind != GateKind::Metatile
                        || !matches!(gate_truth(gate, &belief), Truth::True)
                    {
                        continue;
                    }
                    for way in &gate.ways {
                        let c = check(&belief, way);
                        if let ([Predicate::Flag { name, is }], true) =
                            (way.as_slice(), c.failed.is_empty() && c.unknown.is_empty())
                        {
                            if !wrong.contains(&(name.clone(), *is)) {
                                wrong.push((name.clone(), *is));
                            }
                        }
                    }
                }
            }
        }
        for (flag, is) in &wrong {
            self.emit(GameEvent::FlagObserved {
                flag: flag.clone(),
                value: !is,
            })?;
        }
        Ok(wrong.into_iter().map(|(f, _)| f).collect())
    }

    pub fn with_toolbox(mut self, toolbox: Toolbox) -> Self {
        self.toolbox = toolbox;
        self
    }

    pub fn with_checkpoint(mut self, path: PathBuf, identity: checkpoint::Identity) -> Self {
        self.checkpoint = Some((path, identity));
        self
    }

    pub fn with_ledger(mut self, ledger: crate::ledger::Ledger) -> Self {
        self.ledger = ledger;
        self
    }

    pub fn toolbox(&self) -> &Toolbox {
        &self.toolbox
    }

    /// The last frame's observation.
    pub fn observation(&self) -> Option<&Observation> {
        self.runtime.observation()
    }

    pub fn state(&self) -> &GameState {
        self.runtime.state()
    }

    /// Where the player was last located.
    pub fn pose(&self) -> Option<PlayerPose> {
        self.observation()
            .and_then(|o| o.player.as_ref().map(|p| p.pose.clone()))
            .or_else(|| self.state().player.pose.value.clone())
    }

    /// Frames without dialogue, menu, battle or transition.
    pub fn quiet_frames(&self) -> u32 {
        self.quiet_frames
    }

    /// The last frame dialogue was seen on.
    pub fn last_dialogue_frame(&self) -> Option<u64> {
        self.last_dialogue_frame
    }

    /// Whether a battle starting now follows dialogue closely enough to be
    /// a trainer's.
    pub fn after_dialogue(&self) -> bool {
        let now = self.observation().map_or(0, |o| o.frame_id);
        self.last_dialogue_frame
            .is_some_and(|f| now.saturating_sub(f) < TRAINER_DIALOGUE_WINDOW)
    }

    /// Records an event: through the runtime, and into the running tool's
    /// outcome.
    pub fn emit(&mut self, event: GameEvent) -> Result<(), ToolError> {
        self.runtime.emit(event.clone())?;
        self.learn(event);
        Ok(())
    }

    /// An event into the scheduler's needs and the running tool's outcome.
    fn learn(&mut self, event: GameEvent) {
        if matches!(
            event,
            GameEvent::FlagTracked { .. }
                | GameEvent::FlagObserved { .. }
                | GameEvent::ScriptPathRun { .. }
                | GameEvent::ScriptPathRetracted { .. }
                | GameEvent::CheckpointRestored { .. }
        ) {
            let known = crate::nav::belief_gone(&self.world, self.runtime.state());
            self.gone.extend(known);
        }
        if self
            .scheduler
            .event(&event, self.runtime.state(), &self.data)
        {
            self.runtime.explain(
                "scheduler: needs changed",
                &serde_json::json!({
                    "layer": crate::scheduler::layer(&event), "queue": self.scheduler.queue,
                    "cause": event, "destination": self.scheduler.destination,
                }),
            );
            self.next_need_check = 0;
        }
        self.learned.push(event);
    }

    /// Hands what the sensor read since the last call to the scheduler and
    /// the running tool (it reads every frame, including those the
    /// executor observes while an action is pending).
    fn take_perceived(&mut self) {
        while let Ok(notice) = self.perceived.try_recv() {
            if let Notice::Event { record, .. } = notice {
                self.learn(record.event);
            }
        }
    }

    pub fn info(&self, message: impl Into<String>) {
        self.runtime.info(message);
    }

    /// Reads the next frame and updates the bookkeeping: quiet frames, the
    /// last dialogue frame, and `MapVisited` on every map change.
    pub fn observe(&mut self) -> Result<Observation, ToolError> {
        if self.stop.load(Ordering::Relaxed) {
            let _ = self
                .runtime
                .execute(pokebot_core::ControllerCommand::Neutral);
            return Err(ToolError::Stopped);
        }
        self.runtime.observe()?;
        let o = self
            .runtime
            .observation()
            .cloned()
            .expect("observe() sets the observation");
        self.note_frame(&o)?;
        Ok(o)
    }

    fn note_frame(&mut self, o: &Observation) -> Result<(), ToolError> {
        self.take_perceived();
        let busy = o.dialogue.is_some()
            || o.menu.is_some()
            || o.battle.is_some()
            || o.bag.is_some()
            || o.shop.is_some()
            || o.party_menu.is_some()
            || o.summary.is_some()
            || o.pc_storage.is_some()
            || o.screen.value == ScreenState::Transition
            || self.runtime.outside_game();
        self.quiet_frames = if busy {
            0
        } else {
            self.quiet_frames.saturating_add(1)
        };
        if o.dialogue.is_some() {
            self.last_dialogue_frame = Some(o.frame_id);
        }
        if let Some(p) = &o.player {
            let map = &p.pose.map;
            if self.last_map.as_deref() != Some(map.as_str()) {
                self.ledger.entered(map);
            }
            while let Ok(notice) = self.walked.try_recv() {
                let Notice::Event { record, .. } = notice else {
                    continue;
                };
                match record.event {
                    GameEvent::PlayerMoved { from, to } => self.ledger.walked_between(&from, &to),
                    GameEvent::PlayerLocated { pose } => self.ledger.walked(&pose),
                    GameEvent::NpcSeen {
                        map,
                        local_id,
                        x,
                        y,
                        ..
                    } => self
                        .ledger
                        .seen(&map, &crate::ledger::object_key(local_id), (x, y)),
                    _ => {}
                }
            }
            self.ledger.walked(&p.pose);
            if self.last_map.as_deref() != Some(map.as_str()) {
                self.last_map = Some(map.clone());
                // What the last map showed is kept (a crash loses at most
                // one map's worth).
                if let Err(e) = self.ledger.store() {
                    self.runtime.error(format!("ledger: {e}"));
                }
                // NPCs are back at their data positions on re-entry, and cut
                // trees and smashed rocks have grown back.
                self.blocked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .forget(map);
                if let Some(places) = self.world.places() {
                    self.gone.retain(|(m, id)| {
                        m != map
                            || !places.gates.iter().any(|g| {
                                g.map == *m && g.local_id == *id && super::field::regrows(&g.kind)
                            })
                    });
                }
                self.emit(GameEvent::MapVisited { map: map.clone() })?;
                self.track_entry(map)?;
            }
        }
        Ok(())
    }

    /// Books what arriving on `map` does by itself (its silent entry
    /// scripts, [`super::effects::entry_events`]).
    fn track_entry(&mut self, map: &str) -> Result<(), ToolError> {
        let world = Arc::clone(&self.world);
        let Some(events) = world.events() else {
            return Ok(());
        };
        let map_name = |id: &str| world.name_of(id).map(str::to_owned);
        let learned = {
            let state = self.runtime.state();
            super::effects::entry_events(
                events,
                map,
                &crate::belief_view::StateBelief(state),
                state,
                &self.data,
                world.places(),
                &map_name,
            )
        };
        for event in learned {
            self.emit(event)?;
        }
        Ok(())
    }

    /// Runs one action to its outcome through the executor, then checks
    /// the screen (§7.3): a battle box runs the battle tool, unexpected
    /// dialogue or a menu runs `Unstick`, and the outcome is
    /// `Interrupted` so the tool re-plans from the new observation.
    pub fn act(&mut self, action: Action) -> Result<Outcome, ToolError> {
        let outcome = self.executor.run_action(self.runtime, action, self.stop)?;
        let o = self
            .runtime
            .observation()
            .cloned()
            .expect("run_action observed");
        self.note_frame(&o)?;
        let before = self.pose();
        let learned_before = self.learned.len();
        if self.interrupt(&o)? {
            // Interrupted mid-action (a walk's hold turned back by a
            // scene) counts like between actions (Switch, Vermilion: the
            // ticket check turned a hold Down back for 35 minutes, never
            // counted).
            if let Some(why) = self.count_interrupt_here(before, learned_before) {
                return Err(ToolError::Failed(why));
            }
            return Ok(Outcome::Interrupted);
        }
        Ok(outcome)
    }

    /// Counts an interruption at `pose` for the step being driven, with
    /// what was learnt since `learned_before`; the failure when the tile
    /// has turned the step back too often.
    fn count_interrupt_here(
        &mut self,
        pose: Option<pokebot_state::PlayerPose>,
        learned_before: usize,
    ) -> Option<String> {
        let counts = self.interrupt_counts.last_mut()?;
        let learned = self.learned.get(learned_before..).unwrap_or_default();
        count_interrupt(counts, pose, learned)
    }

    /// Handles what `o` shows that the running step doesn't expect. `true`
    /// when something was handled (the caller observes afresh).
    fn interrupt(&mut self, o: &Observation) -> Result<bool, ToolError> {
        if self.runtime.outside_game() {
            return Ok(false);
        }
        // The white-out screens (seen for a while: a battle wipe's black
        // frames can look like one for an instant): nothing to handle
        // here; the goal loop ends the run and the session reloads the
        // save.
        if o.screen.value == ScreenState::Whiteout {
            self.whiteout_frames += 1;
            if self.whiteout_frames >= WHITEOUT_FRAMES {
                return Err(ToolError::Failed("whited out".into()));
            }
            return Ok(false);
        }
        self.whiteout_frames = 0;
        let expects = self.expects;
        if o.battle.is_some() {
            if expects.battle {
                // The step plays its own battle, text and menus included.
                return Ok(false);
            }
            let outcome = self.invoke(&Intent::Battle {
                policy: BattlePlan::Auto,
            });
            return outcome.result.map(|()| true);
        }
        let dialogue = o.dialogue.is_some() && !expects.dialogue;
        let menu = o.menu.is_some() && o.dialogue.is_none() && !expects.menu;
        // No step expects a naming keyboard (the new game's own task runs
        // outside the tools): one on screen came from a stray YES.
        let naming = o.naming.is_some();
        if dialogue || menu || naming {
            let outcome = self.invoke(&Intent::Unstick);
            return outcome.result.map(|()| true);
        }
        // A conversation or scene being followed is not an overworld
        // boundary: its gaps (people walking off) are not the time to
        // leave for a heal (the rival's scene was left half-recorded).
        // With the location unconfirmed no frame is located, and that need
        // is served from right there.
        let unconfirmed = !self.runtime.state().player.candidates.is_empty();
        if (o.player.is_some() || unconfirmed)
            && !expects.dialogue
            && o.dialogue.is_none()
            && o.menu.is_none()
            && o.battle.is_none()
            && o.party_menu.is_none()
            && o.summary.is_none()
            && o.pc_storage.is_none()
            && self.quiet_frames >= SETTLE_FRAMES
            && o.frame_id >= self.next_need_check
        {
            self.next_need_check = o.frame_id + 180;
            if self.service_needs()? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Suspend only at an overworld boundary; nested recovery tools cannot
    /// trigger another recovery. The caller's task stays on the stack.
    pub fn service_needs(&mut self) -> Result<bool, ToolError> {
        use crate::scheduler::{self, Need};
        if !self.scheduler.enabled || self.scheduler.recovering {
            return Ok(false);
        }
        if let Some(why) = self.scheduler.invalidated.take() {
            return Err(ToolError::Replan(why));
        }
        let Some(need) = self.scheduler.queue.front().copied() else {
            return Ok(false);
        };
        self.scheduler.recovering = true;
        let result = (|| {
            if need == Need::ConfirmLocation {
                self.info("scheduler: suspend task, confirm the location");
                self.invoke(&Intent::ConfirmLocation).result?;
                self.scheduler.queue.retain(|n| *n != Need::ConfirmLocation);
                return Ok(true);
            }
            if need == Need::AuditParty {
                self.info("scheduler: suspend task, audit party facts");
                self.invoke(&Intent::Probe {
                    fact: super::ProbeFact::Party,
                })
                .result?;
                return Ok(true);
            }
            let pose = self
                .pose()
                .ok_or_else(|| ToolError::Failed("recovery: player not located".into()))?;
            let urgent = need == Need::HealUrgent;
            let graph = self
                .scheduler
                .graph
                .get_or_insert_with(|| scheduler::graph(&self.world));
            let candidate = scheduler::recovery(
                &self.world,
                graph,
                self.runtime.state(),
                &self.data,
                &pose,
                self.scheduler.destination.as_deref(),
                urgent,
            );
            let Some(candidate) = candidate else {
                return if urgent {
                    Err(ToolError::Failed(
                        "urgent recovery: no known safe route to a healer".into(),
                    ))
                } else {
                    Ok(false)
                };
            };
            if !urgent
                && candidate.added_travel_s + candidate.encounter_risk_s + candidate.item_cost_s
                    > 20.0
            {
                return Ok(false);
            }
            self.runtime
                .explain("scheduler: suspend task for recovery", &candidate);
            if let Some(item) = candidate.medicine {
                super::medicine::use_item(self, &item)?;
            } else {
                self.invoke(&Intent::Heal {
                    center: Some(candidate.map),
                })
                .result?;
            }
            if matches!(
                scheduler::health(self.state().party.value.as_deref(), &self.data),
                Some(Need::HealUrgent | Need::AuditParty)
            ) {
                return Err(ToolError::Failed(
                    "recovery did not establish safe party health".into(),
                ));
            }
            self.info("scheduler: recovery verified, resume suspended task");
            Ok(true)
        })();
        self.scheduler.recovering = false;
        result
    }

    /// Runs `step` until it is done or fails, one frame per decision, with
    /// the executor's stuck rule and the interrupt check on every frame.
    pub fn drive(&mut self, step: &mut dyn ToolStep) -> Result<String, ToolError> {
        let mut waiting_since: Option<(u64, String)> = None;
        let mut nudged = false;
        let mut repeats = ActRepeats::default();
        self.interrupt_counts.push(std::collections::HashMap::new());
        let saved = self.expects;
        let result = self.drive_inner(step, &mut waiting_since, &mut nudged, &mut repeats);
        self.expects = saved;
        self.interrupt_counts.pop();
        result
    }

    fn drive_inner(
        &mut self,
        step: &mut dyn ToolStep,
        waiting_since: &mut Option<(u64, String)>,
        nudged: &mut bool,
        repeats: &mut ActRepeats,
    ) -> Result<String, ToolError> {
        loop {
            let o = self.observe()?;
            if self
                .executor
                .recover_outside(self.runtime, &mut self.outside, "tool")?
            {
                continue;
            }
            self.expects = step.expects();
            let before = self.pose();
            let learned_before = self.learned.len();
            if self.interrupt(&o)? {
                if let Some(why) = self.count_interrupt_here(before, learned_before) {
                    return Err(ToolError::Failed(why));
                }
                // The interrupt ran another tool (a trainer battle can take
                // minutes): the step's wait starts over from the screen it
                // sees next (flash-2: Unstick failed "stuck waiting:
                // conversation ending" the frame after a 95 s battle).
                *waiting_since = None;
                *nudged = false;
                continue;
            }
            let frame = o.frame_id;
            let mut events = Vec::new();
            let decision = {
                let state = self.runtime.state().clone();
                step.next(&mut StepContext {
                    observation: &o,
                    state: &state,
                    events: &mut events,
                    quiet_frames: self.quiet_frames,
                    frame: self.runtime.last_frame(),
                    learned: &self.learned,
                })
            };
            for event in events {
                self.emit(event)?;
            }
            match decision {
                Decision::Act(action) => {
                    *waiting_since = None;
                    *nudged = false;
                    if let Some(why) = repeats.note(&action, &o) {
                        return Err(ToolError::Failed(why));
                    }
                    let outcome = self.act(action.clone())?;
                    let o = self.runtime.observation().cloned().expect("act observed");
                    let state = self.runtime.state().clone();
                    let mut events = Vec::new();
                    step.on_outcome(
                        &action,
                        outcome,
                        &mut StepContext {
                            observation: &o,
                            state: &state,
                            events: &mut events,
                            quiet_frames: self.quiet_frames,
                            frame: self.runtime.last_frame(),
                            learned: &self.learned,
                        },
                    );
                    for event in events {
                        self.emit(event)?;
                    }
                }
                Decision::Wait(reason) => {
                    let (since, _) = waiting_since.get_or_insert((frame, reason.clone()));
                    let waited = frame - *since;
                    let limit = self.executor.max_wait_frames;
                    if !*nudged && waited > limit / 2 {
                        *nudged = true;
                        self.executor.nudge(self.runtime, "tool", &reason)?;
                        continue;
                    }
                    if waited > limit {
                        return Err(ToolError::Failed(format!("stuck waiting: {reason}")));
                    }
                }
                Decision::Done(summary) => return Ok(summary),
                Decision::Fail(reason) => return Err(ToolError::Failed(reason)),
            }
        }
    }

    /// Runs the tool serving `intent`. Its learned events are emitted and
    /// returned in the outcome.
    pub fn invoke(&mut self, intent: &Intent) -> ToolOutcome {
        if self.depth >= MAX_DEPTH {
            return ToolOutcome::failed(format!("tools nested too deep at {intent}"));
        }
        // Tools walk to their target with the navigator: a way there
        // through a Cut tree or water is walked first.
        if let Some(map) = intent.target_map().map(str::to_owned) {
            self.depth += 1;
            let reached = super::go::reach_map(self, &map);
            self.depth -= 1;
            if let Err(e) = reached {
                return e.into();
            }
        }
        // A trainer battle is fought by the lead, and no Pokémon may
        // faint: the member best able to win it alone leads.
        let fights = self.trainer_of(intent);
        if let Some(trainer) = fights.clone() {
            self.depth += 1;
            let led = super::party_order::lead_for(self, &trainer);
            self.depth -= 1;
            match led {
                Err(e @ (ToolError::Stopped | ToolError::Device(_))) => return e.into(),
                Err(e) => self.info(format!("warning: leading for {trainer}: {e}")),
                Ok(()) => {}
            }
        }
        let (index, mut tool) = match self.toolbox.take(intent) {
            Ok(t) => t,
            Err(e) => return e.into(),
        };
        self.runtime.info(format!("tool {}: {intent}", tool.name()));
        let recovering = self.scheduler.recovering;
        if matches!(intent, Intent::Heal { .. } | Intent::Probe { .. }) {
            self.scheduler.recovering = true;
        }
        let destination = self.scheduler.destination.clone();
        if self.depth == 0 {
            match intent {
                Intent::Go { dest } => self.scheduler.destination = Some(dest.map().to_owned()),
                Intent::Train { map, .. } | Intent::Beat { map, .. } => {
                    self.scheduler.destination = Some(map.clone())
                }
                Intent::Catch { map: Some(map), .. } => {
                    self.scheduler.destination = Some(map.clone())
                }
                _ => {}
            }
        }
        let expects = self.expects;
        let learned = std::mem::take(&mut self.learned);
        self.depth += 1;
        let mut outcome = tool.run(intent, self);
        self.depth -= 1;
        self.scheduler.recovering = recovering;
        self.scheduler.destination = destination;
        self.toolbox.put_back(index, tool);
        // Events the tool returned on top of what it emitted.
        for event in outcome.learned.drain(..) {
            if let Err(e) = self.emit(event) {
                if outcome.result.is_ok() {
                    outcome.result = Err(e);
                }
            }
        }
        let mine = std::mem::replace(&mut self.learned, learned);
        // The caller learns what its callees did.
        self.learned.extend(mine.iter().cloned());
        outcome.learned = mine;
        self.expects = expects;
        if outcome.pose.is_none() {
            outcome.pose = self.pose();
        }
        match &outcome.result {
            Ok(()) => self.runtime.info(format!("tool {intent}: done")),
            Err(e) => self.runtime.error(format!("tool {intent}: {e}")),
        }
        // A trainer battle's result is kept across reloads: a loss makes
        // the next plan for that trainer ask for more (the ledger's
        // handicap), a win clears it.
        if let Some(trainer) = fights {
            match &outcome.result {
                Ok(()) => self.ledger.beat(&trainer),
                Err(ToolError::Failed(why)) if why.contains("whited out") => {
                    self.ledger.lost_to(&trainer);
                    self.runtime
                        .info(format!("lost to {trainer}: the next plan asks for more"));
                }
                Err(_) => {}
            }
            if let Err(e) = self.ledger.store() {
                self.runtime.error(format!("ledger: {e}"));
            }
        }
        outcome
    }
}
/// Counts an interruption at `pose` that learned `learned`; the failure
/// once the same tile has turned the step back [`MAX_SAME_INTERRUPTS`]
/// times. A scene at one spot every time the step comes by (a ticket gate
/// turning the walk back) shuts the way (fleet worker 5, Pewter Museum:
/// hours of "Come again!"). A trainer beaten is the way forward, not a
/// turn back, and clears the tile (Switch, Nugget Bridge: Lass Ali's
/// approach, her battle and a move offer at one tile failed the walk).
/// Nor is a wild battle: training in grass meets one at the same tile
/// again and again (Switch, Route 4: three encounters at (84, 12) failed
/// the Train, and the plan went on to lose to the rival).
fn count_interrupt(
    interrupted: &mut std::collections::HashMap<pokebot_state::PlayerPose, u32>,
    pose: Option<pokebot_state::PlayerPose>,
    learned: &[GameEvent],
) -> Option<String> {
    let pose = pose?;
    if learned.iter().any(super::dialogue::trainer_won) {
        interrupted.remove(&pose);
        return None;
    }
    if learned.iter().any(|e| matches!(e, GameEvent::BattleEnded)) {
        return None;
    }
    let n = interrupted.entry(pose.clone()).or_default();
    *n += 1;
    (*n >= MAX_SAME_INTERRUPTS)
        .then(|| format!("interrupted at {pose} {n} times: something there turns the step back"))
}

/// Interruptions (a scene, a conversation) at the same tile within one
/// step before it fails.
pub const MAX_SAME_INTERRUPTS: u32 = 3;

/// The same action sent this many times in a row on an unchanged screen
/// makes no progress: the step fails and the plan goes on without it
/// (fleet workers: "cursor to move 3 (BUBBLE)" pressed Down 300 000 times
/// with a two-move RATTATA out; "select bag item" pressed Up 400 000 times
/// for a POTION the pocket didn't hold, each for hours).
pub const MAX_SAME_ACTS: u32 = 40;

/// Consecutive sends of one action on one screen.
#[derive(Debug, Default)]
pub(crate) struct ActRepeats {
    last: Option<(String, String)>,
    count: u32,
}

impl ActRepeats {
    /// Notes `action` about to be sent on `o`; why the step has failed
    /// when it is the [`MAX_SAME_ACTS`]th send in a row on the same
    /// screen. Actions that wait for something to happen (spinning for
    /// an encounter) are not counted.
    pub(crate) fn note(&mut self, action: &Action, o: &Observation) -> Option<String> {
        if action.interruptible {
            *self = Self::default();
            return None;
        }
        let key = (action.label.clone(), screen_fingerprint(o));
        if self.last.as_ref() == Some(&key) {
            self.count += 1;
        } else {
            self.last = Some(key);
            self.count = 1;
        }
        (self.count >= MAX_SAME_ACTS).then(|| {
            format!(
                "no progress: \"{}\" sent {} times on an unchanged screen",
                action.label, self.count
            )
        })
    }
}

/// What a screen shows that an action is meant to change (not frame
/// counters or match scores).
fn screen_fingerprint(o: &Observation) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        o.screen.value,
        o.dialogue.as_ref().map(|d| &d.lines),
        o.menu,
        o.battle.as_ref().map(|b| (
            b.menu,
            &b.move_names,
            b.move_pp,
            b.player_hp_numbers,
            b.opponent_hp
        )),
        o.bag,
        o.player.as_ref().map(|p| &p.pose),
        o.party_menu,
        o.summary.as_ref().map(|s| (s.page, &s.nickname)),
        o.move_list,
        o.pc_storage,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Expectation;
    use pokebot_core::{Button, ControllerCommand};
    use pokebot_state::{Observed, ScreenState};

    /// The museum's ticket gate turns the walk back at one tile: the third
    /// time fails. Nugget Bridge's trainers interrupt at one tile too, but
    /// each is beaten: that clears the tile.
    #[test]
    fn a_tile_turning_the_step_back_fails_it_but_a_trainer_beaten_does_not() {
        let at = || {
            Some(pokebot_state::PlayerPose {
                map: "Route24".into(),
                x: 11,
                y: 28,
            })
        };
        let won = [GameEvent::MoneyChanged {
            delta: 120,
            reason: "won a battle".into(),
        }];
        let mut seen = std::collections::HashMap::new();
        assert_eq!(count_interrupt(&mut seen, at(), &[]), None);
        assert_eq!(count_interrupt(&mut seen, at(), &[]), None);
        assert_eq!(count_interrupt(&mut seen, at(), &won), None);
        assert_eq!(count_interrupt(&mut seen, at(), &[]), None);
        assert_eq!(count_interrupt(&mut seen, at(), &[]), None);
        assert!(count_interrupt(&mut seen, at(), &[]).is_some_and(|why| why.contains("3 times")));
        assert_eq!(count_interrupt(&mut seen, None, &[]), None);
        // Wild battles at one grass tile, however many: training.
        let mut grass = std::collections::HashMap::new();
        for _ in 0..10 {
            assert_eq!(
                count_interrupt(&mut grass, at(), &[GameEvent::BattleEnded]),
                None
            );
        }
    }

    fn screen(state: ScreenState) -> Observation {
        Observation::bare(
            1,
            Observed {
                value: state,
                detector: "test".into(),
            },
            Default::default(),
        )
    }

    fn press(label: &str) -> Action {
        Action::new(
            label,
            vec![ControllerCommand::Press(Button::Up)],
            Expectation::InputsDone,
            10,
        )
    }

    #[test]
    fn the_same_act_on_an_unchanged_screen_fails_after_the_limit() {
        let bag = screen(ScreenState::Bag);
        let mut repeats = ActRepeats::default();
        for _ in 1..MAX_SAME_ACTS {
            assert_eq!(repeats.note(&press("select bag item"), &bag), None);
        }
        let why = repeats
            .note(&press("select bag item"), &bag)
            .expect("fails");
        assert!(why.contains("select bag item"), "{why}");
        // A changed screen, or another action, starts the count over.
        let mut repeats = ActRepeats::default();
        for i in 0..3 * MAX_SAME_ACTS {
            let o = if i % 10 == 0 {
                screen(ScreenState::Menu)
            } else {
                bag.clone()
            };
            assert_eq!(repeats.note(&press("select bag item"), &o), None);
        }
        // Waiting for an encounter repeats by design.
        let spin = press("spin in the grass").interruptible();
        let field = screen(ScreenState::Overworld);
        let mut repeats = ActRepeats::default();
        assert!((0..3 * MAX_SAME_ACTS).all(|_| repeats.note(&spin, &field).is_none()));
    }
}
