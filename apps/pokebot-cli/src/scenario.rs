//! The development scenario library: emulator snapshots taken at every
//! achievement of a development run (`goal --dev-snapshots DIR`), each with
//! the cartridge save and the bot's checkpoint (`state.json`,
//! `progress.json`, `ledger.json`) and the frame, indexed in
//! `DIR/index.jsonl`. A test or a new rule starts right where a situation
//! arises (`goal --scenario ID`) instead of playing up to it.
//!
//! Development only: the bot never restores a snapshot to get something
//! done, and a console run (capture card) refuses the flags. The run that
//! records is a clean run of the bot; the snapshots are taken beside it.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use pokebot_agent::ledger::{fingerprint, Ledger};
use pokebot_agent::tools::ToolContext;
use serde_json::{json, Value};

/// Takes the emulator's snapshot (opaque bytes).
pub type Snapshotter = Arc<dyn Fn() -> pokebot_core::Result<Vec<u8>> + Send + Sync>;

/// Where a run records its snapshots.
pub struct Store {
    pub dir: PathBuf,
    pub take: Snapshotter,
    /// The cartridge save the emulator writes.
    pub sav: Option<PathBuf>,
    pub goal: String,
    pub commit: String,
    /// The plan step running (what the next snapshot achieved).
    pub step: Arc<Mutex<Option<String>>>,
}

impl Store {
    /// Records a snapshot of the game now, `trigger` saying why; its
    /// directory.
    pub fn record(
        &self,
        ctx: &ToolContext<'_>,
        trigger: &str,
        state_path: &Path,
        progress_path: &Path,
    ) -> Result<PathBuf> {
        let bytes = (self.take)().context("taking the emulator snapshot")?;
        let state = ctx.state();
        let pose = state.player.pose.value.clone();
        let created = utc_stamp(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
        let place = pose.as_ref().map_or("unknown".to_owned(), |p| {
            format!("{}-{}-{}", p.map, p.x, p.y)
        });
        let mut id = format!("{created}-{place}");
        let mut n = 1;
        while self.dir.join(&id).exists() {
            n += 1;
            id = format!("{created}-{place}-{n}");
        }
        let dir = self.dir.join(&id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(dir.join("snapshot.bin"), &bytes)?;
        let ledger_path = Ledger::path_for(state_path);
        for (from, to) in [
            (self.sav.as_deref(), "game.sav"),
            (Some(state_path), "state.json"),
            (Some(progress_path), "progress.json"),
            (Some(ledger_path.as_path()), "ledger.json"),
        ] {
            if let Some(from) = from.filter(|p| p.exists()) {
                std::fs::copy(from, dir.join(to))
                    .with_context(|| format!("copying {}", from.display()))?;
            }
        }
        if let Some(frame) = ctx.runtime.last_frame() {
            pokebot_video::png::save(frame.image(), dir.join("frame.png"))?;
        }
        let flags: Vec<&String> = state
            .world
            .flags
            .iter()
            .filter(|(_, k)| k.value == Some(true))
            .map(|(f, _)| f)
            .collect();
        let badges = flags
            .iter()
            .filter(|f| f.starts_with("FLAG_BADGE0") && f.ends_with("_GET"))
            .count();
        let party: Vec<String> = state
            .party
            .value
            .iter()
            .flatten()
            .map(|m| {
                format!(
                    "{} L{}",
                    m.species.value.as_deref().unwrap_or("?"),
                    m.level.value.map_or("?".into(), |l| l.to_string())
                )
            })
            .collect();
        let meta = json!({
            "id": id,
            "created": created,
            "commit": self.commit,
            "goal": self.goal,
            "trigger": trigger,
            "step": self.step.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "pose": pose,
            "badges": badges,
            "flags": flags,
            "party": party,
            "money": state.money.value,
            "frame_id": ctx.observation().map(|o| o.frame_id),
            "fingerprint": fingerprint(state),
        });
        std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
        let index = self.dir.join("index.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index)
            .with_context(|| format!("opening {}", index.display()))?;
        // One write per line: runs appending side by side don't interleave.
        file.write_all(format!("{}\n", serde_json::to_string(&meta)?).as_bytes())?;
        Ok(dir)
    }
}

/// `YYYYMMDDTHHMMSSZ` for Unix seconds `secs`.
fn utc_stamp(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil from days (H. Hinnant's algorithm).
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The commit the binary runs from (`git rev-parse`), `unknown` outside a
/// checkout; `+dirty` with uncommitted changes.
pub fn commit() -> String {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let Some(head) = git(&["rev-parse", "--short", "HEAD"]) else {
        return "unknown".into();
    };
    match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(s) if !s.is_empty() => format!("{head}+dirty"),
        _ => head,
    }
}

#[derive(Debug, clap::Args)]
pub struct ScenarioArgs {
    #[command(subcommand)]
    command: ScenarioCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ScenarioCommand {
    /// List the recorded scenarios (newest last), filtered
    List {
        /// The library (`goal --dev-snapshots`)
        #[arg(long, default_value = DEFAULT_DIR)]
        dir: PathBuf,
        /// Only on this map
        #[arg(long)]
        map: Option<String>,
        /// Only with this flag set (`--flag !FLAG_X`: not set)
        #[arg(long)]
        flag: Vec<String>,
        /// Only with at least this many badges
        #[arg(long)]
        badges: Option<u64>,
        /// Only whose trigger or step contains this text
        #[arg(long)]
        text: Option<String>,
        /// Print the index lines as JSON
        #[arg(long)]
        json: bool,
    },
}

/// Where `goal --dev-snapshots` records when given no directory.
pub const DEFAULT_DIR: &str = "saves/scenarios";

pub fn run(args: ScenarioArgs) -> Result<()> {
    match args.command {
        ScenarioCommand::List {
            dir,
            map,
            flag,
            badges,
            text,
            json,
        } => {
            for meta in index(&dir)? {
                let has = |f: &str| {
                    meta["flags"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|v| v == f))
                };
                let keep = map.as_ref().is_none_or(|m| meta["pose"]["map"] == **m)
                    && flag.iter().all(|f| match f.strip_prefix('!') {
                        Some(f) => !has(f),
                        None => has(f),
                    })
                    && badges.is_none_or(|b| meta["badges"].as_u64().unwrap_or(0) >= b)
                    && text.as_ref().is_none_or(|t| {
                        [&meta["trigger"], &meta["step"]]
                            .iter()
                            .any(|v| v.as_str().is_some_and(|s| s.contains(t.as_str())))
                    });
                if !keep {
                    continue;
                }
                if json {
                    println!("{meta}");
                } else {
                    println!(
                        "{}  badges {}  {}  [{}]  {}",
                        meta["id"].as_str().unwrap_or("?"),
                        meta["badges"],
                        meta["commit"].as_str().unwrap_or("?"),
                        meta["party"]
                            .as_array()
                            .map(|p| p
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", "))
                            .unwrap_or_default(),
                        meta["trigger"].as_str().unwrap_or("")
                    );
                }
            }
            Ok(())
        }
    }
}

/// Every index line of `dir`, oldest first.
fn index(dir: &Path) -> Result<Vec<Value>> {
    let path = dir.join("index.jsonl");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

/// A scenario copied to a scratch directory to run from: the snapshot to
/// restore and the files the run reads and writes.
pub struct Prepared {
    pub snapshot: PathBuf,
    pub sav: PathBuf,
    pub state: PathBuf,
    pub progress: PathBuf,
}

/// `scenario` (a directory, or an id in `library`) copied to a new
/// directory under the system temp dir, so the run leaves the library as
/// it was.
pub fn prepare(scenario: &str, library: &Path) -> Result<Prepared> {
    let dir = if Path::new(scenario).join("snapshot.bin").exists() {
        PathBuf::from(scenario)
    } else {
        library.join(scenario)
    };
    if !dir.join("snapshot.bin").exists() {
        bail!(
            "no scenario {scenario} (a directory with snapshot.bin, or an id in {})",
            library.display()
        );
    }
    let name = dir
        .file_name()
        .map_or("scenario".into(), |n| n.to_string_lossy().into_owned());
    let stamp = utc_stamp(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
    let work = std::env::temp_dir().join(format!("pokebot-scenario-{name}-{stamp}"));
    std::fs::create_dir_all(&work)?;
    for file in [
        "snapshot.bin",
        "game.sav",
        "state.json",
        "progress.json",
        "ledger.json",
    ] {
        if dir.join(file).exists() {
            std::fs::copy(dir.join(file), work.join(file))
                .with_context(|| format!("copying {file} of {}", dir.display()))?;
        }
    }
    Ok(Prepared {
        snapshot: work.join("snapshot.bin"),
        sav: work.join("game.sav"),
        state: work.join("state.json"),
        progress: work.join("progress.json"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_civil_dates() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        // 2026-09-26 21:30:05 UTC
        assert_eq!(utc_stamp(1_790_458_205), "20260926T213005Z");
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
    }
}
