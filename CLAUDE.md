# Rules for agents working on VisionPokeBot

The target is the physical Nintendo Switch (HDMI capture card + ESP32-S3
controller); the emulator is only the fast development loop. Background and
commands: `docs/HANDOFF.md`, `docs/architecture.md`, `README.md`.

## 1. Work only in this folder

The repository, the live bots and their data are all in one folder:
`/home/omar/repositories/github/omarcinp/VisionPokeBot`. The `pokebot-hub`,
`pokebot-switch` and `pokebot-disk-guard` units run from it, and it holds
the live `saves/` (Switch progress, `saves/web-control/` pause flags),
`roms/`, `emulator/` and `data/world/`.

- **Read, edit, build and run only inside this folder.** Don't create git
  worktrees, clones or copies of the repository, whether next to it, in
  `/tmp`, or under `.claude/worktrees/`. Don't give agents
  `isolation: "worktree"`. On 2026-09-26, 17 stray worktrees had to be
  cleaned up; the uncommitted work in some of them was lost, and a deploy
  briefly ran against stale saves in an old copy.
- **Other agent sessions share this folder.** Stay on `main`, don't switch
  branches, and commit or push only your own files (`git commit <paths>`).
  Never revert, stash, reset, `checkout --` or `git clean` changes you didn't
  make. If you need a branch, create it, merge it into `main` and delete it
  (local and remote) in the same task.
- `target/release/pokebot` is shared too, and `tools/live-run.sh` deploys
  whatever it holds. Don't deploy a build that contains another session's
  uncommitted changes. When deploying, say which commit you built.

## 2. Switch capture stays available

An idle Switch dims after 5 minutes and sleeps after its Auto-Sleep time
(TV mode: 1 hour or more). Once asleep, the capture goes black, the
controller's USB is cut (`usb_mounted: false`), and only a person at the
console can wake it. So:

- **Keep the Switch device session available.** The hub owns capture independently
  of bot execution. A user may stop its bot or take manual control from **Game
  instances**; never restart a bot merely to override that choice. The ESP32's
  keepalive prevents idle sleep while no bot is enabled.
- To deploy a bot fix, use `tools/live-run.sh` to replace the run. Keep the capture
  hub up during the replacement, and preserve the user's current control mode.
- **Only `tools/live-run.sh` starts long-lived processes** (the bot, the hub,
  the disk guard). It runs each one in its own systemd user unit
  (`pokebot-switch`, `pokebot-emu`, `pokebot-hub`, `pokebot-disk-guard`),
  so it outlives the agent session. Never `setsid nohup …&` a bot: it stays in
  the agent host's cgroup and dies with it. On 2026-09-24 a restart of the
  agent host killed the Switch run, and the Switch went to sleep.
- **The ESP32 firmware keepalive is the second line of defence.** After 4
  minutes without input it nudges the right stick (FireRed ignores it). Leave
  it on (`GET/POST http://<esp32>/api/keepalive`). Also turn Auto-Sleep off
  on the Switch itself (System Settings → Sleep Mode).
- **Check that the Switch is alive** when you start and while you monitor.
  Run `curl http://10.10.100.185/api/status` (`usb_mounted` must be true),
  and the web UI (http://10.10.100.21:8080/switch/) must show a picture. A
  black capture with `usb_mounted: false` means the Switch is asleep or off.
  Tell the user at once; the `--restart` run resumes by itself once the
  Switch is awake again.
- **A run survives the Switch sleeping, turning off, or showing the HOME
  menu.** Before each CONTINUE it waits for a picture, unlocks the lock
  screen, and starts the software from the HOME menu (`crates/agent/src/
  console.rs`; its log lines start with `console:`). Console screens it sees
  are saved to `captures/console/`: use them to build real detectors.
- When monitoring a run, look for stuck moments and unhandled situations.
  Fix them, then relaunch (still `--restart`).

## 3. Disk: keep track, delete what has been analyzed

Recordings (`--record`) grow about 2 GB per hour of Switch play. `/tmp` once
filled the whole 581 GB disk.

- `tools/disk-guard.sh` reports free space and every recording (size, age,
  ACTIVE if a running bot writes it). Run it when a session starts and about
  hourly while monitoring. `tools/live-run.sh` also starts the
  `pokebot-disk-guard` unit, which runs `--prune` every 30 minutes. It
  deletes inactive recordings older than 12 h (or the oldest ones, when free
  space drops under 100 GB), trims old frames of active recordings if still
  short, and deletes debug bundles older than 14 days.
- **Once a recording is analyzed, delete it** (`rm -rf /tmp/<run>`); don't
  keep recordings "just in case". Analyzed means: failures looked at, contact
  sheets read, and any frame worth a regression test copied to
  `captures/fixtures/`. Also clean up your own scratch files.
- Never delete `saves/`, `captures/fixtures/`, the recording of the running
  bot, or anything outside what the bot and you produced.
- Use a new `--record` directory for every run.

## 4. Working agreements

- Every run shows on the LAN web UI: `tools/live-run.sh [--instance
  switch|emu] <log> <pokebot args…>`. The Switch is `/switch/`, the emulator
  `/emu/`, and both can run at once.
- Verify with evidence: contact sheets of recorded frames, `pokebot inspect`
  on stuck frames, and tests. Don't trust a "completed" log line alone.
- A failure seen on the Switch gets a regression test with its frame as a
  fixture (`captures/fixtures/switch-*.png`).
- Keep `cargo fmt`, `cargo clippy --workspace --all-targets` and
  `cargo test --workspace` green.
- Never `pkill -f` a pattern that can match your own shell's command line.
  Stop processes by exact name (`pkill -x`) or by unit.
