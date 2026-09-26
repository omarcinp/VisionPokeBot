//! Architectural guard: the bot may only see video and press buttons.
//!
//! Scans every Rust source in the workspace for libretro entry points that
//! expose emulator internals (memory, save states, cheats). Two opaque
//! exceptions: battery-save persistence in `cartridge.rs`, which may only
//! reference the save-RAM region, and development snapshots in
//! `snapshot.rs` (save states for a scenario library), which the bot's
//! crates may not name: they are a development tool, never a way for the
//! bot to make progress.

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "retro_get_memory_data",
    "retro_get_memory_size",
    "retro_serialize",
    "retro_unserialize",
    "retro_cheat",
    "RETRO_MEMORY_SYSTEM_RAM",
    "RETRO_MEMORY_VIDEO_RAM",
    "RETRO_MEMORY_RTC",
    "GET_MEMORY_MAPS",
    "SET_MEMORY_MAPS",
];

const CARTRIDGE: &str = "adapters/emulator-libretro/src/cartridge.rs";
const SNAPSHOT: &str = "adapters/emulator-libretro/src/snapshot.rs";
/// What development snapshots are called outside the adapter.
const DEV_SNAPSHOT: &[&str] = &["dev_snapshot", "DevSnapshot"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            // Nested checkouts (git worktrees) are guarded by their own copy
            // of this test.
            let checkout = path.join(".git").exists();
            if name != "target" && name != ".git" && !checkout {
                rust_sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_emulator_internals_are_referenced() {
    let root = workspace_root();
    let this_file = Path::new(file!()).file_name().unwrap();
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    assert!(files.len() > 10, "source scan looks broken: {files:?}");

    let mut violations = Vec::new();
    for file in &files {
        let relative = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if file.file_name() == Some(this_file) {
            continue;
        }
        let text = std::fs::read_to_string(file).unwrap();
        for token in FORBIDDEN {
            let allowed = (relative == CARTRIDGE && token.starts_with("retro_get_memory_"))
                || (relative == SNAPSHOT && token.contains("serialize"));
            if text.contains(token) && !allowed {
                violations.push(format!("{relative}: {token}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "privileged emulator access found:\n{}",
        violations.join("\n")
    );
}

/// The bot (every crate under `crates/`) never names development
/// snapshots: only the adapter and the command line's development tooling
/// do.
#[test]
fn the_bot_never_names_development_snapshots() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_sources(&root.join("crates"), &mut files);
    assert!(files.len() > 10, "source scan looks broken: {files:?}");
    let violations: Vec<String> = files
        .iter()
        .flat_map(|file| {
            let text = std::fs::read_to_string(file).unwrap();
            DEV_SNAPSHOT
                .iter()
                .filter(|t| text.contains(**t))
                .map(|t| format!("{}: {t}", file.display()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn cartridge_only_touches_save_ram() {
    let text = std::fs::read_to_string(workspace_root().join(CARTRIDGE)).unwrap();
    let regions: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|word| word.starts_with("RETRO_MEMORY_"))
        .collect();
    assert!(!regions.is_empty());
    assert!(
        regions.iter().all(|r| *r == "RETRO_MEMORY_SAVE_RAM"),
        "{regions:?}"
    );
}
