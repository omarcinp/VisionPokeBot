//! Independent processes are required by libretro's process-global callbacks.
//! Workers share only read-only assets; cartridge saves, progress and logs are private.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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
                let mut command = Command::new(&self.executable);
                command.current_dir(&dir).arg(match task {
                    "observe" => "run",
                    "autonomy" => "goal",
                    _ => task,
                });
                if task == "autonomy" {
                    command.args([
                        "flag FLAG_SYS_GAME_CLEAR",
                        "--plan-budget-secs",
                        "240",
                        "--max-replans",
                        "8",
                        "--restart",
                    ]);
                }
                command
                    .args(["--core"])
                    .arg(&core)
                    .arg("--rom")
                    .arg(&rom)
                    .arg("--save")
                    .arg(dir.join("game.sav"))
                    .args([
                        "--web",
                        "127.0.0.1:0",
                        "--hold",
                        "--telemetry-hz",
                        "5",
                        "--instance-label",
                    ])
                    .arg(&label)
                    .arg("--instance-file")
                    .arg(self.registry.join(format!("{name}.json")))
                    .stdin(Stdio::null())
                    .stdout(log.try_clone()?)
                    .stderr(log);
                if matches!(task, "story" | "autonomy") {
                    command
                        .args(["--new-game", "--save-game", "--progress"])
                        .arg(dir.join("progress.json"))
                        .arg("--world")
                        .arg(data.join("world"));
                }
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
                    if name == "switch"
                        || state.workers.contains_key(name)
                        || entry["alive"] != true
                    {
                        continue;
                    }
                    entry["managed"] = json!(false);
                    entry["status"] = json!("external");
                    workers.push(entry);
                }
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
                for worker in state.workers.values_mut() {
                    worker.stop();
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
            },
            root.join("registry"),
        )
        .unwrap();
        fleet.state.lock().unwrap().executable = executable;
        (fleet, root)
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
        let args = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        assert!(args.starts_with("goal\nflag FLAG_SYS_GAME_CLEAR\n"));
        assert!(args.contains("--new-game\n--save-game\n"));
        assert!(args.contains(dir.join("progress.json").to_str().unwrap()));
        assert!(args.contains(dir.join("game.sav").to_str().unwrap()));
        use clap::Parser;
        crate::Cli::try_parse_from(std::iter::once("pokebot").chain(args.lines())).unwrap();
        drop(fleet);
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
