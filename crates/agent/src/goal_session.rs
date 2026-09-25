//! The long-lived wrapper around a goal run: how it starts (a new game,
//! CONTINUE from the checkpoint, or the game as it stands) and how it
//! cycles with `--restart` (finish or fail → wait → soft reset → CONTINUE
//! → run the goal again, forever), the way `story --restart` keeps the
//! Switch busy.
//!
//! The cycle logic ([`run`]) is pure: it drives a [`Runner`] and never
//! touches a device, so it is tested with a fake. A new game
//! ([`start_new_game`]) is only the menus (title, intro, names): from the
//! first overworld frame the goal loop plays, the story included, with
//! the belief a new game is known to start with ([`new_game_knowledge`]).

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use pokebot_core::Error;
use pokebot_gamedata::GameData;
use pokebot_runtime::Runtime;
use pokebot_state::{
    GameEvent, Knowledge, PlayerPose, Pocket, PokedexCounts, SavedKnowledge, WorldBelief,
};
use pokebot_world::events::{Condition, Effect};
use pokebot_world::gates::{is_local_flag, is_local_var};
use pokebot_world::World;

use crate::console::bring_up_game;
use crate::party::Party;
use crate::{
    checkpoint, ContinueTask, Executor, ExecutorError, NewGameConfig, NewGameTask, Progress,
    SaveGameTask, Starter,
};

/// Where a cycle starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Soft reset, new game, the opening milestones, then the goal.
    NewGame,
    /// Soft reset, CONTINUE the last save, restore its checkpoint, then the
    /// goal.
    Continue,
    /// The game as it stands on screen.
    AsIs,
}

/// How one cycle ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleEnd {
    /// The goal holds (summary).
    Satisfied(String),
    /// The loop gave up (outcome; `fainted: …` when a Pokémon fainted, and
    /// the save is reloaded at once).
    Unsatisfied(String),
    /// The cycle broke off: a device error, a failed task (reason).
    Failed(String),
    /// Ctrl-C / SIGTERM.
    Stopped,
}

impl CycleEnd {
    pub fn is_stopped(&self) -> bool {
        matches!(self, CycleEnd::Stopped)
    }

    /// A faint: the goal loop's outcome `fainted: …`.
    pub fn is_faint(&self) -> bool {
        matches!(self, CycleEnd::Unsatisfied(outcome) if outcome.starts_with("fainted"))
    }
}

/// What the session drives: one cycle at a time, the pause between them,
/// and whether there is a save to CONTINUE from.
pub trait Runner {
    /// Plays one cycle from `start` (`cycle` counts from 1).
    fn cycle(&mut self, start: Start, cycle: u64) -> CycleEnd;
    /// Waits (watching the screen) between cycles.
    fn wait(&mut self, duration: Duration);
    /// The stop flag.
    fn stopped(&self) -> bool;
    /// A progress file exists to CONTINUE from.
    fn has_checkpoint(&self) -> bool;
    fn log(&self, message: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    pub start: Start,
    /// Never stop: the pause before each restart (soft reset, CONTINUE).
    pub restart: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReport {
    /// Cycles played.
    pub cycles: u64,
    /// How the last cycle ended.
    pub last: CycleEnd,
}

/// Runs cycles until stopped, or once without `restart`. Every end of a
/// cycle (satisfied, given up or failed) is followed by the pause and a
/// restart: soft reset → CONTINUE → the goal again, so the console never
/// idles. A faint reloads the last save at once (story's rule: no Pokémon
/// faints; the whited-out game is not played on). A new game that failed
/// before its first save is started again; without any save to continue
/// from, the session waits and looks again.
pub fn run(session: Session, runner: &mut dyn Runner) -> SessionReport {
    let mut start = session.start;
    let mut cycle = 1u64;
    loop {
        let end = runner.cycle(start, cycle);
        let stopped = end.is_stopped() || runner.stopped();
        let Some(wait) = session.restart else {
            return SessionReport {
                cycles: cycle,
                last: end,
            };
        };
        if stopped {
            return SessionReport {
                cycles: cycle,
                last: end,
            };
        }
        if end.is_faint() && runner.has_checkpoint() {
            runner.log(&format!(
                "restart: cycle {}: a Pokémon fainted; soft reset and CONTINUE the last save now",
                cycle + 1
            ));
            start = Start::Continue;
            cycle += 1;
            continue;
        }
        // The pause, then the next start; without any save (and no new
        // game to redo) only look again after another pause.
        start = loop {
            let next = if runner.has_checkpoint() {
                Some(Start::Continue)
            } else if session.start == Start::NewGame {
                Some(Start::NewGame)
            } else {
                None
            };
            match next {
                Some(Start::Continue) => runner.log(&format!(
                    "restart: cycle {} in {} s: soft reset and CONTINUE the last save, then the goal again",
                    cycle + 1,
                    wait.as_secs()
                )),
                Some(_) => runner.log(&format!(
                    "restart: cycle {} in {} s: no save yet, so a new game again",
                    cycle + 1,
                    wait.as_secs()
                )),
                None => runner.log(&format!(
                    "restart: no progress file to continue from; looking again in {} s",
                    wait.as_secs()
                )),
            }
            runner.wait(wait);
            if runner.stopped() {
                return SessionReport {
                    cycles: cycle,
                    last: end,
                };
            }
            if let Some(next) = next {
                break next;
            }
        };
        cycle += 1;
    }
}

/// Starting position after a new game: the bedroom, next to the bed.
pub fn bedroom() -> PlayerPose {
    PlayerPose {
        map: "PalletTown_PlayersHouse_2F".into(),
        x: 6,
        y: 6,
    }
}

/// Money a new game starts with (`src/new_game.c`: `SetMoney(…, 3000)`).
pub const NEW_GAME_MONEY: u32 = 3000;

/// What a new game is known to hold, from the game's rules and the
/// compiled events alone: no Pokémon, an empty bag, empty PC boxes, ¥3000,
/// an empty Pokédex, and every story flag and var the scripts name at its
/// start value (the flags `EventScript_ResetAllMapFlags` sets on, every
/// other flag off, every var 0), as `Derived` facts. What the scripts do
/// next is tracked as they run.
pub fn new_game_knowledge(world: &World) -> SavedKnowledge {
    let mut k = SavedKnowledge {
        party: Knowledge::derived(Vec::new(), 0),
        money: Knowledge::derived(NEW_GAME_MONEY, 0),
        ..SavedKnowledge::default()
    };
    for pocket in Pocket::ALL {
        k.bag
            .pockets
            .insert(pocket, Knowledge::derived(Vec::new(), 0));
    }
    for b in k.pc.boxes.iter_mut() {
        *b = Knowledge::derived(Vec::new(), 0);
    }
    k.pokedex.counts = Knowledge::derived(
        PokedexCounts {
            seen: Some(0),
            caught: 0,
        },
        0,
    );
    k.world = story_start(world);
    k
}

/// Every story flag and var the compiled events name, at its new-game
/// value.
fn story_start(world: &World) -> WorldBelief {
    let mut belief = WorldBelief::default();
    let Some(events) = world.events() else {
        return belief;
    };
    let initial = &events.initial.set;
    let mut flag = |name: &str| {
        if !is_local_flag(name) && name.starts_with("FLAG_") {
            let on = initial.iter().any(|f| f == name);
            belief
                .flags
                .entry(name.to_owned())
                .or_insert_with(|| Knowledge::derived(on, 0));
        }
    };
    let mut vars: Vec<String> = Vec::new();
    let mut var = |name: &str| {
        if !is_local_var(name) && name.starts_with("VAR_") {
            vars.push(name.to_owned());
        }
    };
    for script in events.scripts.values() {
        for path in &script.paths {
            for c in &path.when {
                match c {
                    Condition::Flag { flag: f, .. } => flag(f),
                    Condition::Var { var: v, .. } => var(v),
                    _ => {}
                }
            }
            for e in &path.does {
                match e {
                    Effect::Set { set: f } | Effect::Clear { clear: f } => flag(f),
                    Effect::Var { var: v, .. } => var(v),
                    _ => {}
                }
            }
        }
    }
    for t in &events.triggers {
        for c in &t.when {
            if let Condition::Var { var: v, .. } = c {
                var(v);
            }
        }
    }
    for ms in events.map_scripts.values() {
        for f in ms.on_frame.iter().chain(&ms.on_warp) {
            var(&f.var);
        }
    }
    for o in &events.objects {
        if let Some(f) = &o.hidden_by {
            flag(f);
        }
    }
    for v in vars {
        belief
            .vars
            .entry(v)
            .or_insert_with(|| Knowledge::derived(0, 0));
    }
    belief
}

/// Soft reset, title, intro and names (`NewGameTask`, menu input only),
/// then the belief a new game starts with ([`new_game_knowledge`]); the
/// fresh progress (unsaved). Nothing of the story is played here.
pub fn start_new_game(
    runtime: &mut Runtime,
    executor: &Executor,
    config: &NewGameConfig,
    starter: Starter,
    world: &World,
    stop: &AtomicBool,
    snapshots: Option<&Path>,
) -> Result<Progress, ExecutorError> {
    bring_up_game(runtime, stop, snapshots)?;
    let mut task = NewGameTask::new(config.clone()).map_err(|e| ExecutorError::TaskFailed {
        task: "NewGame".into(),
        reason: e,
    })?;
    executor.run(runtime, &mut task, stop)?;
    runtime.set_pose_hint(bedroom());
    runtime.emit(GameEvent::CheckpointRestored {
        knowledge: Box::new(new_game_knowledge(world)),
    })?;
    Ok(Progress {
        player_name: config.player_name.clone(),
        rival_name: config.rival_name.clone(),
        gender: config.gender,
        starter,
        milestones: Vec::new(),
        saved_at: None,
        party: Party::default(),
    })
}

/// Restores the knowledge that goes with the save just loaded.
pub fn restore_checkpoint(
    runtime: &mut Runtime,
    state_path: &Path,
    progress: &Progress,
    data: &GameData,
) -> Result<(), ExecutorError> {
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

/// Soft reset, CONTINUE and the checkpoint: the game as `progress` left it.
pub fn continue_game(
    runtime: &mut Runtime,
    executor: &Executor,
    progress: &Progress,
    state_path: &Path,
    data: &GameData,
    stop: &AtomicBool,
    snapshots: Option<&Path>,
) -> Result<(), ExecutorError> {
    if let Some(pose) = &progress.saved_at {
        runtime.set_pose_hint(pose.clone());
    }
    bring_up_game(runtime, stop, snapshots)?;
    executor.run(runtime, &mut ContinueTask::default(), stop)?;
    runtime.info(format!(
        "continuing after: {}",
        progress.milestones.join(", ")
    ));
    restore_checkpoint(runtime, state_path, progress, data)
}

/// Saves in-game and writes the checkpoint (`state.json` first, then
/// `progress.json`: if the second is not written, their identities differ
/// and `state.json` is ignored).
pub fn save_checkpoint(
    runtime: &mut Runtime,
    executor: &Executor,
    progress: &mut Progress,
    progress_path: &Path,
    stop: &AtomicBool,
) -> Result<(), ExecutorError> {
    executor.run(runtime, &mut SaveGameTask::default(), stop)?;
    runtime.persist_save()?;
    progress.saved_at = runtime.state().player.pose.value.clone();
    if let Some(dir) = progress_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    checkpoint::store(
        &checkpoint::path_for(progress_path),
        &checkpoint::Identity::of(progress),
        &runtime.state().saved_knowledge(),
    )?;
    progress.store(progress_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    /// What a new game starts with, from the rules and the compiled events:
    /// nothing held, ¥3000, the story at its start (the flags the new-game
    /// script sets on, the rest off, every var 0).
    #[test]
    fn a_new_game_starts_known() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let Ok(world) = World::load(&dir) else { return };
        let Some(events) = world.events() else { return };
        if events.initial.set.is_empty() {
            return;
        }
        let k = new_game_knowledge(&world);
        assert_eq!(k.party.value.as_deref(), Some(&[][..]));
        assert_eq!(k.money.value, Some(NEW_GAME_MONEY));
        assert!(k
            .bag
            .pockets
            .values()
            .all(|p| p.value.as_deref() == Some(&[][..])));
        let w = &k.world;
        assert_eq!(w.var("VAR_MAP_SCENE_PALLET_TOWN_OAK").value, Some(0));
        assert_eq!(w.flag("FLAG_SYS_POKEDEX_GET").value, Some(false));
        assert_eq!(w.flag("FLAG_HIDE_OAK_IN_HIS_LAB").value, Some(true));
        assert_eq!(w.flag("FLAG_BADGE01_GET").value, Some(false));
        // Script-local state is not the save's.
        assert!(!w.vars.keys().any(|v| v.starts_with("VAR_TEMP_")));
        assert!(!w.flags.keys().any(|f| f.starts_with("FLAG_TEMP_")));
    }

    /// A scripted runner: the ends of its cycles in order, whether a
    /// checkpoint exists after each, and everything it was asked to do.
    #[derive(Default)]
    struct Fake {
        ends: VecDeque<CycleEnd>,
        checkpoint: bool,
        /// Cycles after which a checkpoint appears (a first save).
        checkpoint_after: Option<u64>,
        /// Stop during the wait after this many waits.
        stop_on_wait: Option<usize>,
        stopped: bool,
        starts: Vec<(Start, u64)>,
        waits: Vec<Duration>,
        logs: RefCell<Vec<String>>,
    }

    impl Runner for Fake {
        fn cycle(&mut self, start: Start, cycle: u64) -> CycleEnd {
            self.starts.push((start, cycle));
            if self.checkpoint_after == Some(cycle) {
                self.checkpoint = true;
            }
            self.ends.pop_front().unwrap_or(CycleEnd::Stopped)
        }

        fn wait(&mut self, duration: Duration) {
            self.waits.push(duration);
            if self.stop_on_wait == Some(self.waits.len()) {
                self.stopped = true;
            }
        }

        fn stopped(&self) -> bool {
            self.stopped
        }

        fn has_checkpoint(&self) -> bool {
            self.checkpoint
        }

        fn log(&self, message: &str) {
            self.logs.borrow_mut().push(message.to_owned());
        }
    }

    const WAIT: Duration = Duration::from_secs(240);

    #[test]
    fn without_restart_one_cycle_is_played() {
        for end in [
            CycleEnd::Satisfied("ok".into()),
            CycleEnd::Unsatisfied("out of replans".into()),
            CycleEnd::Failed("device".into()),
        ] {
            let mut fake = Fake {
                ends: VecDeque::from([end.clone()]),
                checkpoint: true,
                ..Fake::default()
            };
            let report = run(
                Session {
                    start: Start::Continue,
                    restart: None,
                },
                &mut fake,
            );
            assert_eq!(
                report,
                SessionReport {
                    cycles: 1,
                    last: end
                }
            );
            assert_eq!(fake.starts, vec![(Start::Continue, 1)]);
            assert!(fake.waits.is_empty());
        }
    }

    #[test]
    fn every_end_is_followed_by_a_wait_and_a_continue() {
        let mut fake = Fake {
            ends: VecDeque::from([
                CycleEnd::Unsatisfied("out of replans".into()),
                CycleEnd::Failed("video: no signal".into()),
                CycleEnd::Satisfied("goal satisfied".into()),
                CycleEnd::Satisfied("goal satisfied".into()),
                CycleEnd::Stopped,
            ]),
            checkpoint: true,
            ..Fake::default()
        };
        let report = run(
            Session {
                start: Start::Continue,
                restart: Some(WAIT),
            },
            &mut fake,
        );
        assert_eq!(report.cycles, 5);
        assert_eq!(report.last, CycleEnd::Stopped);
        // Satisfied goals keep cycling too: the console must stay busy.
        assert_eq!(
            fake.starts,
            vec![
                (Start::Continue, 1),
                (Start::Continue, 2),
                (Start::Continue, 3),
                (Start::Continue, 4),
                (Start::Continue, 5),
            ]
        );
        assert_eq!(fake.waits, vec![WAIT; 4]);
        let logs = fake.logs.borrow();
        assert_eq!(logs.len(), 4);
        assert!(logs[0].contains("cycle 2 in 240 s"), "{logs:?}");
        assert!(logs[0].contains("CONTINUE"), "{logs:?}");
    }

    /// Switch goal run: a white-out on Route 1 was followed by the 240 s
    /// pause like any other end. A faint reloads the save at once.
    #[test]
    fn a_faint_continues_the_last_save_without_the_pause() {
        let mut fake = Fake {
            ends: VecDeque::from([
                CycleEnd::Unsatisfied("fainted: our Pokémon fainted (BULBASAUR)".into()),
                CycleEnd::Unsatisfied("fainted: whited out".into()),
                CycleEnd::Unsatisfied("out of replans".into()),
                CycleEnd::Stopped,
            ]),
            checkpoint: true,
            ..Fake::default()
        };
        let report = run(
            Session {
                start: Start::AsIs,
                restart: Some(WAIT),
            },
            &mut fake,
        );
        assert_eq!(report.cycles, 4);
        assert_eq!(
            fake.starts,
            vec![
                (Start::AsIs, 1),
                (Start::Continue, 2),
                (Start::Continue, 3),
                (Start::Continue, 4)
            ]
        );
        // Only the third end (not a faint) waited.
        assert_eq!(fake.waits, vec![WAIT]);
        let logs = fake.logs.borrow();
        assert!(
            logs[0].contains("fainted") && logs[0].contains("now"),
            "{logs:?}"
        );
        // Without a save to reload, a faint waits like any other end.
        let mut fake = Fake {
            ends: VecDeque::from([
                CycleEnd::Unsatisfied("fainted: whited out".into()),
                CycleEnd::Stopped,
            ]),
            checkpoint: false,
            stop_on_wait: Some(1),
            ..Fake::default()
        };
        run(
            Session {
                start: Start::AsIs,
                restart: Some(WAIT),
            },
            &mut fake,
        );
        assert_eq!(fake.waits.len(), 1);
    }

    #[test]
    fn a_new_game_is_only_played_once_it_has_saved() {
        let mut fake = Fake {
            ends: VecDeque::from([
                CycleEnd::Failed("NewGame failed: stuck".into()),
                CycleEnd::Unsatisfied("out of replans".into()),
                CycleEnd::Stopped,
            ]),
            checkpoint_after: Some(2),
            ..Fake::default()
        };
        let report = run(
            Session {
                start: Start::NewGame,
                restart: Some(WAIT),
            },
            &mut fake,
        );
        assert_eq!(report.cycles, 3);
        // No save after the first cycle: a new game again; after the second
        // (which saved): CONTINUE, never a new game.
        assert_eq!(
            fake.starts,
            vec![
                (Start::NewGame, 1),
                (Start::NewGame, 2),
                (Start::Continue, 3)
            ]
        );
        assert!(fake.logs.borrow()[0].contains("new game again"));
    }

    #[test]
    fn nothing_to_continue_from_waits_and_looks_again() {
        let mut fake = Fake {
            ends: VecDeque::from([CycleEnd::Failed("device".into())]),
            stop_on_wait: Some(3),
            ..Fake::default()
        };
        let report = run(
            Session {
                start: Start::AsIs,
                restart: Some(WAIT),
            },
            &mut fake,
        );
        assert_eq!(report.cycles, 1);
        assert_eq!(fake.starts.len(), 1);
        assert_eq!(fake.waits.len(), 3);
        assert!(fake.logs.borrow()[0].contains("no progress file"));
    }

    #[test]
    fn a_stop_during_the_wait_ends_the_session() {
        let mut fake = Fake {
            ends: VecDeque::from([CycleEnd::Satisfied("goal satisfied".into())]),
            checkpoint: true,
            stop_on_wait: Some(1),
            ..Fake::default()
        };
        let report = run(
            Session {
                start: Start::Continue,
                restart: Some(WAIT),
            },
            &mut fake,
        );
        assert_eq!(report.cycles, 1);
        assert_eq!(report.last, CycleEnd::Satisfied("goal satisfied".into()));
        assert_eq!(fake.starts.len(), 1);
    }

    #[test]
    fn a_stopped_cycle_ends_without_waiting() {
        let mut fake = Fake {
            ends: VecDeque::from([CycleEnd::Stopped]),
            checkpoint: true,
            ..Fake::default()
        };
        let report = run(
            Session {
                start: Start::Continue,
                restart: Some(WAIT),
            },
            &mut fake,
        );
        assert_eq!(report.cycles, 1);
        assert!(fake.waits.is_empty());
    }
}
