//! Independent processes are required by libretro's process-global callbacks.
//! Workers share only read-only assets; cartridge saves, progress and logs are private.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use pokebot_telemetry::hub_proxy::{self, FleetControl};
use serde_json::{json, Value};

use crate::devices::{self, EmulatorArgs};

const WORKER_MEMORY: u64 = 512 * 1024 * 1024;

#[derive(Debug, clap::Args)]
pub struct FleetArgs {
    /// Maximum simultaneous managed emulators (default: CPU count minus one,
    /// capped by available memory at a conservative 512 MiB per worker)
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    max_emulators: Option<u32>,
    /// Private directories for emulator saves, progress, debug bundles and logs
    #[arg(long, default_value = "saves/emulators")]
    emulator_dir: PathBuf,
    /// Shared read-only data directory (world model, fonts, species graphics)
    #[arg(long, default_value = "data")]
    emulator_data: PathBuf,
    /// libretro core for managed emulators [env: VPB_CORE]
    #[arg(long)]
    emulator_core: Option<PathBuf>,
    /// ROM for managed emulators [env: VPB_ROM]
    #[arg(long)]
    emulator_rom: Option<PathBuf>,
    /// Scenario library autonomy workers record development snapshots
    /// into (`goal --dev-snapshots`), shared by all of them
    #[arg(long, default_value = crate::scenario::DEFAULT_DIR)]
    scenario_library: PathBuf,
}

pub struct Fleet {
    state: Mutex<State>,
    _lock: File,
}

struct State {
    args: FleetArgs,
    executable: PathBuf,
    registry: PathBuf,
    workers: BTreeMap<String, Worker>,
    generation: u128,
    next: u64,
    limit: usize,
    launcher: Launcher,
}

type LaunchRequest = (Command, mpsc::Sender<std::io::Result<Child>>);

/// PR_SET_PDEATHSIG follows the thread that forked, not just its process.
/// Never fork on Tokio's expiring blocking pool: keep the parent thread alive
/// until all workers have been stopped.
struct Launcher {
    tx: Option<mpsc::Sender<LaunchRequest>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Launcher {
    fn new() -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<LaunchRequest>();
        let thread = std::thread::Builder::new()
            .name("emulator-launcher".into())
            .spawn(move || {
                for (mut command, reply) in rx {
                    if let Err(unsent) = reply.send(command.spawn()) {
                        if let Ok(mut child) = unsent.0 {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    fn spawn(&self, command: Command) -> Result<Child> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .as_ref()
            .context("launcher stopped")?
            .send((command, tx))
            .map_err(|_| anyhow::anyhow!("launcher stopped"))?;
        Ok(rx.recv().context("launcher stopped")??)
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What a worker runs, for [`worker_stages`].
struct StageArgs<'a> {
    task: &'a str,
    /// The worker's number (1-based).
    n: u64,
    dir: &'a Path,
    core: &'a Path,
    rom: &'a Path,
    data: &'a Path,
    instance_file: &'a Path,
    label: &'a str,
    scenarios: &'a Path,
}

/// The processes a worker runs in turn, each one's arguments. An autonomy
/// worker starts a new game with a shiny starter: the story up to Oak's
/// "choose!" (saved there), the shiny-starter hunt from it
/// (docs/shiny-starter.md: power cycles and aimed timings, the shiny saved
/// in game), then the goal loop continuing that save. A hunt that ends
/// without one leaves the save in front of the ball, and the goal picks
/// the starter as usual.
fn worker_stages(a: &StageArgs<'_>) -> Vec<Vec<OsString>> {
    let os = |s: &str| OsString::from(s);
    let path = |p: &Path| p.as_os_str().to_owned();
    let output = |hold: bool| {
        let mut v = vec![
            os("--core"),
            path(a.core),
            os("--rom"),
            path(a.rom),
            os("--save"),
            path(&a.dir.join("game.sav")),
            os("--web"),
            os("127.0.0.1:0"),
        ];
        if hold {
            v.push(os("--hold"));
        }
        v.extend([
            os("--telemetry-hz"),
            os("5"),
            os("--instance-label"),
            os(a.label),
            os("--instance-file"),
            path(a.instance_file),
        ]);
        v
    };
    let progress = || {
        vec![
            os("--save-game"),
            os("--progress"),
            path(&a.dir.join("progress.json")),
            os("--world"),
            path(&a.data.join("world")),
        ]
    };
    match a.task {
        "autonomy" => {
            let (starter, fossil) = new_game_choice(a.n);
            // The stepped emulator is deterministic: the same inputs
            // play the same game. A name typed differently shifts
            // the frames the game's random numbers advance by.
            let player =
                ["RED", "LEAF", "ASH", "KRIS", "GOLD", "JADE", "BLUE", "ROSE"][(a.n % 8) as usize];
            let gender = if a.n % 2 == 0 { "boy" } else { "girl" };
            let mut story = vec![
                os("story"),
                os("--new-game"),
                os("--until"),
                os("MeetOak"),
                os("--gender"),
                os(gender),
                os("--starter"),
                os(starter),
            ];
            story.extend(progress());
            story.extend(output(false));
            let mut hunt = vec![
                os("shiny-starter"),
                os("--prepare"),
                os("--starter"),
                os(starter),
                os("--hunt"),
                path(&a.dir.join("hunt.json")),
                os("--world"),
                path(&a.data.join("world")),
            ];
            hunt.extend(output(false));
            let mut goal: Vec<OsString> = [
                "goal",
                "flag FLAG_SYS_GAME_CLEAR",
                "--plan-budget-secs",
                "240",
                "--max-replans",
                "8",
                "--restart",
                "--starter",
                starter,
                "--fossil",
                fossil,
                "--player",
                player,
                "--gender",
                gender,
                "--dev-snapshots",
            ]
            .map(os)
            .into();
            goal.push(path(a.scenarios));
            goal.push(os("--continue"));
            goal.extend(progress());
            goal.extend(output(true));
            vec![story, hunt, goal]
        }
        task => {
            let mut args = vec![os(if task == "observe" { "run" } else { task })];
            args.extend(output(true));
            if task == "story" {
                args.push(os("--new-game"));
                args.extend(progress());
            }
            vec![args]
        }
    }
}

/// One stage runs as the executable itself; several in turn under `sh`,
/// in a process group of their own so a stop (SIGTERM to the shell)
/// reaches the stage running.
fn chain(executable: &Path, stages: &[Vec<OsString>]) -> Command {
    if let [only] = stages {
        let mut command = Command::new(executable);
        command.args(only);
        return command;
    }
    let quote = |s: &OsString| format!("'{}'", s.to_string_lossy().replace('\'', "'\\''"));
    let line = |stage: &Vec<OsString>| {
        std::iter::once(quote(&executable.as_os_str().to_owned()))
            .chain(stage.iter().map(quote))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (last, first) = stages.split_last().expect("stages");
    let (head, middle) = first.split_first().expect("two stages at least");
    let mut script = String::from("trap 'trap - TERM; kill 0' TERM\nrun() { \"$@\" & wait $!; }\n");
    script += &format!("run {} || exit $?\n", line(head));
    for stage in middle {
        script += &format!("run {}\n", line(stage));
    }
    script += &format!("exec {}\n", line(last));
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script).process_group(0);
    command
}

/// Worker `n`'s (1-based) starter and Mt. Moon fossil: every pair in
/// turn, so six parallel new games play six different games (and meet
/// different edge cases).
fn new_game_choice(n: u64) -> (&'static str, &'static str) {
    let i = n.saturating_sub(1) as usize;
    (
        ["bulbasaur", "charmander", "squirtle"][i % 3],
        ["dome", "helix"][(i / 3) % 2],
    )
}

struct Worker {
    child: Child,
    label: String,
    dir: PathBuf,
    task: String,
    started: u64,
    stopping: Option<Instant>,
    outcome: Option<String>,
}

/// Respect both host memory and the common cgroup-v2 container memory limit.
fn available_memory() -> u64 {
    let host = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("MemAvailable:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
        })
        .unwrap_or(0)
        * 1024;
    let group = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("0::").map(str::to_owned))
        })
        .unwrap_or_default();
    let root = PathBuf::from("/sys/fs/cgroup");
    let base = root.join(group.trim_start_matches('/'));
    let base = if base.is_dir() { &base } else { &root };
    let number = |path| {
        std::fs::read_to_string(path)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    };
    let mut available = host;
    // A systemd slice may impose the limit on an ancestor of this process.
    for dir in base.ancestors().take_while(|dir| dir.starts_with(&root)) {
        if let (Some(limit), Some(used)) = (
            number(dir.join("memory.max")),
            number(dir.join("memory.current")),
        ) {
            available = available.min(limit.saturating_sub(used));
        }
    }
    available
}

impl Fleet {
    pub fn new(mut args: FleetArgs, registry: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&registry)?;
        std::fs::create_dir_all(&args.emulator_dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(registry.join("fleet.lock"))?;
        // SAFETY: a valid owned descriptor; advisory lock is released on drop.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("another emulator supervisor owns {}", registry.display());
        }
        args.emulator_dir = args.emulator_dir.canonicalize()?;
        // Assets are checked on launch so the Switch hub still works without a ROM.
        args.emulator_data = std::path::absolute(&args.emulator_data)?;
        args.scenario_library = std::path::absolute(&args.scenario_library)?;
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let automatic = cpus
            .saturating_sub(1)
            .max(1)
            .min((available_memory() / WORKER_MEMORY).max(1) as usize);
        let limit = args.max_emulators.map_or(automatic, |n| n as usize);
        Ok(Self {
            state: Mutex::new(State {
                args,
                executable: std::env::current_exe()?,
                registry: registry.canonicalize()?,
                workers: BTreeMap::new(),
                generation: SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
                next: 0,
                limit,
                launcher: Launcher::new()?,
            }),
            _lock: lock,
        })
    }

    pub fn reap(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for worker in state.workers.values_mut() {
            worker.reap();
        }
    }

    pub fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for worker in state.workers.values_mut() {
            worker.stop();
        }
        // All get the full grace period concurrently, independent of fleet size.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for worker in state.workers.values_mut() {
                worker.reap();
            }
            if state.workers.values().all(|w| w.outcome.is_some()) {
                break;
            }
            if Instant::now() >= deadline {
                for worker in state.workers.values_mut().filter(|w| w.outcome.is_none()) {
                    let _ = worker.child.kill();
                    let _ = worker.child.wait();
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Worker {
    fn reap(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        if self
            .stopping
            .is_some_and(|t| t.elapsed() > Duration::from_secs(5))
        {
            let _ = self.child.kill();
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.outcome = Some(if self.stopping.is_some() {
                    "stopped".into()
                } else if status.success() {
                    "completed".into()
                } else {
                    format!("failed ({status})")
                })
            }
            Ok(None) => {}
            Err(error) => self.outcome = Some(format!("failed ({error})")),
        }
    }
    fn stop(&mut self) {
        self.reap();
        if self.outcome.is_some() || self.stopping.is_some() {
            return;
        }
        // SAFETY: this PID is our own unreaped child, so it cannot be reused.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        self.stopping = Some(Instant::now());
    }
    fn report(&self, name: &str) -> Value {
        let status = self
            .outcome
            .as_deref()
            .unwrap_or(if self.stopping.is_some() {
                "stopping"
            } else {
                "running"
            });
        json!({"name": name, "label": self.label, "task": self.task, "pid": self.child.id(),
            "started_at": self.started, "status": status, "alive": self.outcome.is_none(),
            "managed": true, "path": format!("/{name}/"), "directory": self.dir})
    }
}

impl State {
    fn launch(&mut self, body: &Value) -> Result<Value> {
        let count = body["count"]
            .as_u64()
            .context("count must be a positive integer")?;
        let task = body["task"]
            .as_str()
            .context("task must be autonomy, new-game, story or observe")?;
        if !["autonomy", "new-game", "story", "observe"].contains(&task) {
            bail!("unknown task");
        }
        let running = self
            .workers
            .values()
            .filter(|w| w.outcome.is_none())
            .count();
        if count == 0 || count > self.limit.saturating_sub(running) as u64 {
            bail!("capacity exceeded: {running}/{} workers active", self.limit);
        }
        if count > available_memory() / WORKER_MEMORY {
            bail!("insufficient available memory (budget: 512 MiB per new worker)");
        }
        let config = devices::emulator_config(&EmulatorArgs {
            core: self.args.emulator_core.clone(),
            rom: self.args.emulator_rom.clone(),
            save: None,
            no_save: true,
            link_listen: None,
            link_connect: None,
        })?;
        let core = config.core_path.canonicalize().context("emulator core")?;
        let rom = config.rom_path.canonicalize().context("ROM")?;
        let data = self
            .args
            .emulator_data
            .canonicalize()
            .context("emulator data directory")?;
        if matches!(task, "story" | "autonomy") && !data.join("world/index.json").is_file() {
            bail!("build the world model with tools/world/build.sh first");
        }
        let mut created = Vec::new();
        for _ in 0..count {
            self.next += 1;
            let name = format!("emu-{:x}-{}", self.generation, self.next);
            let label = format!("Emulator {}", self.next);
            let dir = self.args.emulator_dir.join(&name);
            let launch = (|| -> Result<Worker> {
                std::fs::create_dir(&dir)?;
                std::os::unix::fs::symlink(&data, dir.join("data"))?;
                let log = File::create(dir.join("worker.log"))?;
                let stages = worker_stages(&StageArgs {
                    task,
                    n: self.next,
                    dir: &dir,
                    core: &core,
                    rom: &rom,
                    data: &data,
                    instance_file: &self.registry.join(format!("{name}.json")),
                    label: &label,
                    scenarios: &self.args.scenario_library,
                });
                let mut command = chain(&self.executable, &stages);
                command
                    .current_dir(&dir)
                    .stdin(Stdio::null())
                    .stdout(log.try_clone()?)
                    .stderr(log);
                // Linux also stops workers if the supervisor dies unexpectedly.
                let parent = std::process::id();
                // SAFETY: only async-signal-safe libc calls in the forked child.
                unsafe {
                    command.pre_exec(move || {
                        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::getppid() as u32 != parent {
                            return Err(std::io::Error::other("supervisor exited"));
                        }
                        Ok(())
                    });
                }
                Ok(Worker {
                    child: self.launcher.spawn(command)?,
                    label,
                    dir: dir.clone(),
                    task: task.into(),
                    started: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                    stopping: None,
                    outcome: None,
                })
            })();
            match launch {
                Ok(worker) => {
                    self.workers.insert(name.clone(), worker);
                    created.push(name);
                }
                Err(error) => {
                    // A partial batch is visible and stopped; no hidden orphan processes.
                    for id in &created {
                        self.workers.get_mut(id).unwrap().stop();
                    }
                    bail!("launch failed: {error:#}; stopped partial batch {created:?}");
                }
            }
        }
        Ok(json!({"started": created}))
    }
}

impl FleetControl for Fleet {
    fn request(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for worker in state.workers.values_mut() {
            worker.reap();
        }
        match (method, path) {
            ("GET", "/api/emulators") => {
                let mut workers: Vec<_> =
                    state.workers.iter().map(|(id, w)| w.report(id)).collect();
                // Include legacy and externally registered runs without taking ownership.
                for mut entry in hub_proxy::list_instances(&state.registry) {
                    let name = entry["name"].as_str().unwrap_or_default();
                    if name == "switch-device"
                        || state.workers.contains_key(name)
                        || entry["alive"] != true
                    {
                        continue;
                    }
                    entry["kind"] = json!(if name == "switch" {
                        "switch"
                    } else {
                        "emulator"
                    });
                    entry["managed"] = json!(false);
                    entry["status"] = json!("external");
                    workers.push(entry);
                }
                let device = hub_proxy::list_instances(&state.registry)
                    .into_iter()
                    .find(|v| {
                        v["name"] == "switch-device" && v["alive"] == true && v["connected"] == true
                    });
                if device.is_some() && !workers.iter().any(|w| w["name"] == "switch") {
                    workers.insert(
                        0,
                        json!({"name":"switch","label":"Switch","kind":"switch","path":"/switch/",
                        "alive":true,"managed":false,"status":"connected","task":"no bot"}),
                    );
                }
                workers.sort_by_key(|w| w["name"] != "switch");
                (
                    200,
                    json!({"workers": workers, "max_workers": state.limit,
                    "available_memory_mb": available_memory() / 1024 / 1024,
                    "cpus": std::thread::available_parallelism().map_or(1, usize::from)}),
                )
            }
            ("POST", "/api/emulators") => match state.launch(&body) {
                Ok(value) => (201, value),
                Err(error) => (409, json!({"error":format!("{error:#}")})),
            },
            ("POST", "/api/emulators/stop") => {
                let names = if let Some(names) = body.get("names") {
                    let Some(names) = names.as_array() else {
                        return (400, json!({"error":"names must be an array"}));
                    };
                    if names
                        .iter()
                        .any(|n| n.as_str().is_none_or(|n| !state.workers.contains_key(n)))
                    {
                        return (404, json!({"error":"Select only managed emulator sets"}));
                    }
                    Some(
                        names
                            .iter()
                            .map(|n| n.as_str().unwrap().to_owned())
                            .collect::<Vec<_>>(),
                    )
                } else {
                    None
                };
                for (name, worker) in &mut state.workers {
                    if names.as_ref().is_none_or(|names| names.contains(name)) {
                        worker.stop();
                    }
                }
                (200, json!({"stopping": true}))
            }
            _ => {
                let rest = path.strip_prefix("/api/emulators/").unwrap_or_default();
                let Some((name, action)) = rest.split_once('/') else {
                    return (404, json!({"error":"Unknown endpoint"}));
                };
                let Some(worker) = state.workers.get_mut(name) else {
                    return (404, json!({"error":"Unknown managed emulator"}));
                };
                match (method, action) {
                    ("POST", "stop") => {
                        worker.stop();
                        (200, worker.report(name))
                    }
                    ("GET", "log") => {
                        let log = (|| -> std::io::Result<String> {
                            let mut file = File::open(worker.dir.join("worker.log"))?;
                            let start = file.metadata()?.len().saturating_sub(16 * 1024);
                            file.seek(SeekFrom::Start(start))?;
                            let mut bytes = Vec::new();
                            file.take(16 * 1024).read_to_end(&mut bytes)?;
                            Ok(String::from_utf8_lossy(&bytes).into_owned())
                        })()
                        .unwrap_or_else(|e| e.to_string());
                        (200, json!({"log":log}))
                    }
                    _ => (405, json!({"error":"Method not allowed"})),
                }
            }
        }
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (Fleet, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "vpb-fleet-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("core.so"), "test").unwrap();
        std::fs::write(root.join("game.gba"), "test").unwrap();
        let executable = root.join("worker");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > args.txt\nexec sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let fleet = Fleet::new(
            FleetArgs {
                max_emulators: Some(2),
                emulator_dir: root.join("runs"),
                emulator_data: root.join("data"),
                emulator_core: Some(root.join("core.so")),
                emulator_rom: Some(root.join("game.gba")),
                scenario_library: root.join("scenarios"),
            },
            root.join("registry"),
        )
        .unwrap();
        fleet.state.lock().unwrap().executable = executable;
        (fleet, root)
    }

    /// Six workers play every (starter, fossil) pair once.
    #[test]
    fn six_new_games_cover_every_starter_and_fossil() {
        let pairs: std::collections::BTreeSet<_> = (1..=6).map(new_game_choice).collect();
        assert_eq!(pairs.len(), 6);
        assert_eq!(new_game_choice(1), ("bulbasaur", "dome"));
        assert_eq!(new_game_choice(6), ("squirtle", "helix"));
        assert_eq!(new_game_choice(7), new_game_choice(1));
    }

    #[test]
    fn batch_stop_validates_names_before_stopping_anything() {
        if available_memory() < WORKER_MEMORY * 2 {
            return;
        }
        let (fleet, root) = fixture();
        let (_, started) = fleet.request(
            "POST",
            "/api/emulators",
            json!({"count":2,"task":"observe"}),
        );
        let names = started["started"].as_array().unwrap();
        assert_eq!(
            fleet
                .request(
                    "POST",
                    "/api/emulators/stop",
                    json!({"names":[names[0],"switch"]})
                )
                .0,
            404
        );
        assert!(fleet
            .state
            .lock()
            .unwrap()
            .workers
            .values()
            .all(|w| w.stopping.is_none()));
        assert_eq!(
            fleet
                .request("POST", "/api/emulators/stop", json!({"names":[names[0]]}))
                .0,
            200
        );
        let state = fleet.state.lock().unwrap();
        assert!(state.workers[names[0].as_str().unwrap()].stopping.is_some());
        assert!(state.workers[names[1].as_str().unwrap()].stopping.is_none());
        drop(state);
        drop(fleet);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_workers_are_isolated_bounded_and_stopped_independently() {
        if available_memory() < WORKER_MEMORY * 2 {
            return;
        }
        let (fleet, root) = fixture();
        // The HTTP request thread may disappear; its workers must survive it.
        let fleet = std::sync::Arc::new(fleet);
        let requester = std::sync::Arc::clone(&fleet);
        let (code, started) = std::thread::spawn(move || {
            requester.request(
                "POST",
                "/api/emulators",
                json!({"count":2,"task":"new-game"}),
            )
        })
        .join()
        .unwrap();
        assert_eq!(code, 201, "{started}");
        let names = started["started"].as_array().unwrap();
        let first = names[0].as_str().unwrap();
        let second = names[1].as_str().unwrap();
        for name in [first, second] {
            let dir = root.join("runs").join(name);
            let deadline = Instant::now() + Duration::from_secs(3);
            while !dir.join("args.txt").exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let args = std::fs::read_to_string(dir.join("args.txt")).unwrap();
            assert!(args.contains(dir.join("game.sav").to_str().unwrap()));
            assert!(args.contains("127.0.0.1:0"));
            assert!(!args.contains("--realtime"));
            assert_eq!(dir.join("data").canonicalize().unwrap(), root.join("data"));
        }
        assert_eq!(
            fleet
                .request(
                    "POST",
                    "/api/emulators",
                    json!({"count":1,"task":"observe"})
                )
                .0,
            409
        );
        assert_eq!(
            fleet
                .request("POST", "/api/emulators/switch/stop", json!({}))
                .0,
            404
        );
        assert_eq!(
            fleet
                .request("POST", &format!("/api/emulators/{first}/stop"), json!({}))
                .0,
            200
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (_, report) = fleet.request("GET", "/api/emulators", Value::Null);
            let workers = report["workers"].as_array().unwrap();
            assert_eq!(
                workers.iter().find(|w| w["name"] == second).unwrap()["alive"],
                true
            );
            if workers.iter().find(|w| w["name"] == first).unwrap()["status"] == "stopped" {
                break;
            }
            assert!(Instant::now() < deadline, "child failed to stop");
            std::thread::sleep(Duration::from_millis(10));
        }
        let pids: Vec<_> = fleet
            .state
            .lock()
            .unwrap()
            .workers
            .values()
            .map(|w| w.child.id())
            .collect();
        drop(fleet);
        assert!(pids
            .iter()
            .all(|pid| !PathBuf::from(format!("/proc/{pid}")).exists()));
        assert!(
            root.join("runs").join(first).is_dir(),
            "stop retains artifacts"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn autonomy_workers_launch_the_goal_loop_with_private_saves() {
        if available_memory() < WORKER_MEMORY {
            return;
        }
        let (fleet, root) = fixture();
        std::fs::create_dir(root.join("data/world")).unwrap();
        std::fs::write(root.join("data/world/index.json"), "{}").unwrap();
        let (code, started) = fleet.request(
            "POST",
            "/api/emulators",
            json!({"count":1,"task":"autonomy"}),
        );
        assert_eq!(code, 201, "{started}");
        let name = started["started"][0].as_str().unwrap();
        let dir = root.join("runs").join(name);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !dir.join("args.txt").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // The first stage runs: the story up to Oak, on the worker's save.
        let args = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        assert!(
            args.starts_with("story\n--new-game\n--until\nMeetOak\n"),
            "{args}"
        );
        assert!(args.contains(dir.join("progress.json").to_str().unwrap()));
        assert!(args.contains(dir.join("game.sav").to_str().unwrap()));
        drop(fleet);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Every worker starts a new game with a shiny starter (the user's
    /// ask): story to Oak, the hunt, then the goal loop continuing that
    /// save. Each stage is a command the CLI takes, on the worker's own
    /// save, hunt and progress files.
    #[test]
    fn autonomy_hunts_a_shiny_starter_before_the_goal_loop() {
        use clap::Parser;
        let dir = PathBuf::from("/runs/emu-1");
        let stages = worker_stages(&StageArgs {
            task: "autonomy",
            n: 2,
            dir: &dir,
            core: Path::new("/core.so"),
            rom: Path::new("/game.gba"),
            data: Path::new("/data"),
            instance_file: Path::new("/registry/emu-1.json"),
            label: "Emulator 2",
            scenarios: Path::new("/scenarios"),
        });
        let lines: Vec<String> = stages
            .iter()
            .map(|s| {
                s.iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("story\n--new-game\n--until\nMeetOak\n"));
        assert!(lines[1].starts_with("shiny-starter\n--prepare\n--starter\ncharmander\n"));
        assert!(lines[1].contains("/runs/emu-1/hunt.json"));
        assert!(lines[2].starts_with("goal\nflag FLAG_SYS_GAME_CLEAR\n"));
        assert!(lines[2].contains("--starter\ncharmander\n--fossil\ndome\n"));
        assert!(lines[2].contains("--continue\n--save-game\n"));
        assert!(!lines[2].contains("--new-game"));
        for (i, line) in lines.iter().enumerate() {
            assert!(line.contains("/runs/emu-1/game.sav"));
            // Only the last stage keeps observing when done.
            assert_eq!(line.contains("--hold"), i == 2, "{line}");
            crate::Cli::try_parse_from(std::iter::once("pokebot").chain(line.lines()))
                .unwrap_or_else(|e| panic!("stage {i}: {e}"));
        }
    }

    /// The chain runs its stages in turn and a SIGTERM to it stops the one
    /// running, not just the shell.
    #[test]
    fn a_chain_runs_its_stages_in_turn_and_stops_as_a_whole() {
        let root = std::env::temp_dir().join(format!("vpb-chain-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join("stage");
        std::fs::write(
            &exe,
            "#!/bin/sh\necho \"$1\" >> ran.txt\n[ \"$1\" = last ] && exec sleep 60\n[ \"$1\" = \"it's\" ] && sleep 60\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let stages = |names: &[&str]| -> Vec<Vec<OsString>> {
            names.iter().map(|n| vec![OsString::from(*n)]).collect()
        };
        let ran = || std::fs::read_to_string(root.join("ran.txt")).unwrap_or_default();
        let wait_for = |want: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ran().ends_with(want) {
                assert!(Instant::now() < deadline, "{:?}", ran());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let mut child = chain(&exe, &stages(&["first", "second", "last"]))
            .current_dir(&root)
            .spawn()
            .unwrap();
        wait_for("first\nsecond\nlast\n");
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        child.wait().unwrap();
        std::fs::remove_file(root.join("ran.txt")).unwrap();
        // Stopped mid-stage (a quote in an argument, too): nothing is left.
        let mut child = chain(&exe, &stages(&["first", "it's", "last"]))
            .current_dir(&root)
            .spawn()
            .unwrap();
        wait_for("first\nit's\n");
        let group = child.id() as libc::pid_t;
        unsafe { libc::kill(group, libc::SIGTERM) };
        child.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(-group, 0) } == 0 {
            assert!(Instant::now() < deadline, "a stage outlived the stop");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ran(), "first\nit's\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_invalid_batches_and_conflicting_supervisors() {
        let (fleet, root) = fixture();
        for body in [
            json!({"count":0,"task":"observe"}),
            json!({"count":3,"task":"observe"}),
            json!({"count":1,"task":"shell"}),
            json!({"count":-1,"task":"observe"}),
        ] {
            assert_eq!(fleet.request("POST", "/api/emulators", body).0, 409);
        }
        assert!(fleet.state.lock().unwrap().workers.is_empty());
        let lock = File::open(root.join("registry/fleet.lock")).unwrap();
        // SAFETY: valid file descriptor; verifies another supervisor cannot own this registry.
        assert_ne!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        drop(fleet);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn child_startup_failure_is_reported_and_capacity_is_released() {
        if available_memory() < WORKER_MEMORY {
            return;
        }
        let (fleet, root) = fixture();
        fleet.state.lock().unwrap().executable = PathBuf::from("/bin/false");
        let (code, _) = fleet.request(
            "POST",
            "/api/emulators",
            json!({"count":1,"task":"observe"}),
        );
        assert_eq!(code, 201);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (_, report) = fleet.request("GET", "/api/emulators", Value::Null);
            let worker = &report["workers"][0];
            if worker["alive"] == false {
                assert!(worker["status"].as_str().unwrap().starts_with("failed"));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(fleet);
        std::fs::remove_dir_all(root).unwrap();
    }
}
