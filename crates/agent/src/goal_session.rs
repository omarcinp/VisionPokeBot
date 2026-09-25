//! The long-lived wrapper around a goal run: how it starts (a new game
//! through the opening milestones, CONTINUE from the checkpoint, or the
//! game as it stands) and how it cycles with `--restart` (finish or fail →
//! wait → soft reset → CONTINUE → run the goal again, forever), the way
//! `story --restart` keeps the Switch busy.
//!
//! The cycle logic ([`run`]) is pure: it drives a [`Runner`] and never
//! touches a device, so it is tested with a fake. The device-bound halves
//! ([`start_new_game`], [`play_milestones`]) run the proven `NewGameTask`
//! and `StoryTask` milestones with the same save/retry rules as `story`.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use pokebot_core::Error;
use pokebot_gamedata::GameData;
use pokebot_runtime::Runtime;
use pokebot_state::{GameEvent, PlayerPose};
use pokebot_world::World;

use crate::console::bring_up_game;
use crate::motion::SyncerHandle;
use crate::party::{self, Party};
use crate::{
    checkpoint, ContinueTask, Executor, ExecutorError, Milestone, NewGameConfig, NewGameTask,
    Progress, SaveGameTask, Starter, StoryTask,
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

/// Soft reset, title, intro and names (`NewGameTask`) as `story --new-game`
/// does; the fresh progress (no milestones, unsaved). The starter is
/// derived at level 5, as the story will hand it over.
pub fn start_new_game(
    runtime: &mut Runtime,
    executor: &Executor,
    config: &NewGameConfig,
    starter: Starter,
    data: &GameData,
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
    runtime.emit(GameEvent::PartyMonDerived {
        slot: 0,
        mon: Box::new(party::starter_mon(data, starter.species(), 5)),
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

/// What [`play_milestones`] runs with.
pub struct MilestoneOptions<'a> {
    pub world: Arc<World>,
    pub data: Arc<GameData>,
    pub syncer: SyncerHandle,
    /// Save after every milestone and write the checkpoint.
    pub save_game: bool,
    pub progress_path: &'a Path,
    /// Tries per milestone; a failed attempt reloads the last save.
    pub attempts: u32,
    pub console_snapshots: Option<&'a Path>,
    /// Told about every failed attempt (milestone, reason): debug bundles.
    pub on_failure: &'a mut dyn FnMut(&Runtime, &str, &str),
}

/// Plays `milestones` one at a time through `StoryTask`, the way `story`
/// does: save after each, and on a failed attempt (stuck, fainted, …)
/// reload the last save and retry that milestone. Milestones already in
/// `progress` are skipped.
pub fn play_milestones(
    runtime: &mut Runtime,
    executor: &Executor,
    opts: &mut MilestoneOptions<'_>,
    milestones: Vec<Milestone>,
    progress: &mut Progress,
    stop: &AtomicBool,
) -> Result<(), ExecutorError> {
    let state_path = checkpoint::path_for(opts.progress_path);
    for milestone in milestones {
        if progress.milestones.contains(&milestone.name) {
            continue;
        }
        let mut attempt = 1;
        loop {
            let mut task = StoryTask::new(Arc::clone(&opts.world), vec![milestone.clone()])
                .with_data(Arc::clone(&opts.data))
                .with_syncer(Arc::clone(&opts.syncer));
            match executor.run(runtime, &mut task, stop) {
                Ok(_) => {
                    progress.milestones.push(milestone.name.clone());
                    break;
                }
                Err(ExecutorError::Stopped) => return Err(ExecutorError::Stopped),
                Err(e) => {
                    (opts.on_failure)(runtime, &milestone.name, &e.to_string());
                    let saved = Progress::load(opts.progress_path).ok();
                    let Some(saved) = saved.filter(|_| attempt < opts.attempts) else {
                        let why = if attempt >= opts.attempts {
                            "out of attempts"
                        } else {
                            "no save to reload"
                        };
                        return Err(ExecutorError::TaskFailed {
                            task: milestone.name.clone(),
                            reason: format!("{e} ({why})"),
                        });
                    };
                    attempt += 1;
                    runtime.error(format!(
                        "{}: {e} — reloading the last save (attempt {attempt}/{})",
                        milestone.name, opts.attempts
                    ));
                    continue_game(
                        runtime,
                        executor,
                        &saved,
                        &state_path,
                        &opts.data,
                        stop,
                        opts.console_snapshots,
                    )?;
                    *progress = saved;
                }
            }
        }
        if opts.save_game {
            save_checkpoint(runtime, executor, progress, opts.progress_path, stop)?;
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

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
