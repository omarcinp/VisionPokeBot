# VisionPokeBot

Vision-driven Pokémon bot. Plays **Fire Red** using **video + controller input only**: it sees the game the way a capture card does and presses buttons the way a controller does.

Full design: [`docs/spec.md`](docs/spec.md). Current wiring: [`docs/architecture.md`](docs/architecture.md).

## Ground rules

- **Vision-only perception.** The bot never reads emulator memory, save states, or debug/scripting APIs. `adapters/emulator-libretro/tests/no_privileged_access.rs` fails the build if any such entry point appears.
- **Controller-only actuation.** The bot acts only through the `Controller` trait. Today the emulator's joypad backs it; later an ESP32/PABotBase bridge on a Switch will.
- **No ROMs in this repo.** Put your own dump in `roms/` (gitignored) or point `VPB_ROM` at it. Cartridge saves (`.sav`) are fine: they're what a real cartridge keeps.

## Quick start

```bash
# Rust toolchain (once)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Emulator: downloads the mGBA libretro core into emulator/ (gitignored)
tools/fetch-emulator.sh

# Build and test (ROM-backed tests skip themselves if no ROM/core is present)
cargo build --release && cargo test
```

### Hardware-shaped setup (recommended)

The emulator runs as its own process and looks like the real rig from the
outside: video on a virtual webcam, controller on a PABotBase2 serial port.
The bot then uses exactly the adapters it will use with a capture card and an
ESP32.

```bash
# Once per boot: create /dev/video10 (needs sudo)
tools/setup-virtual-camera.sh

# Terminal 1: the "console"
./target/release/pokebot emulator serve            # --no-save to ignore .sav files

# Terminal 2: the bot, with the web UI on http://127.0.0.1:8080
./target/release/pokebot run \
    --video capture-card:/dev/video10 --viewport 0,0,720,480 \
    --controller pabotbase:/tmp/pokebot-esp32 \
    --script tools/scripts/boot-to-new-game.txt --hold --web --record captures/session-1
```

Watch the stream with any V4L2 client too, e.g. `ffplay /dev/video10`.

### Start a new game

The first bot goal: from any state, soft-reset, get through the title, tutorial
and Professor Oak's introduction, choose gender, type the player's name, pick or
type the rival's name, and confirm control in the bedroom (Start menu opens and
closes). Every step is confirmed from video.

```bash
# Against the virtual console (≈100 s at console speed), watch it on the web UI
./target/release/pokebot new-game --video capture-card:/dev/video10 --viewport 0,0,720,480 \
    --controller pabotbase:/tmp/pokebot-esp32 --web 0.0.0.0:8080 --hold

# Headless and deterministic (≈13 s): in-process emulator, no save file
./target/release/pokebot new-game --no-save --gender girl --name LEAF --rival ASH
```

Names are 1–7 letters A–Z. Rival names GREEN, GARY, KAZ and TORU are chosen from
the game's list; anything else is typed on the keyboard.

### Play the story

```bash
# Once: build the world model from the pret/pokefirered decompilation (local, gitignored)
tools/world/build.sh

# New game → bedroom → Mom → Oak → starter → rival battle → heal → Route 1 →
# Viridian Mart (parcel) → back to Oak (Pokédex). ~2 min headless, ~13 min at console speed.
./target/release/pokebot story --new-game --starter bulbasaur --no-save

# Watch it on the LAN web UI; the run stays up afterwards until the next one replaces it
tools/live-run.sh /tmp/story.log story --new-game --no-save

# Checkpoints: save in-game at the end, and resume later from the save
# (title → CONTINUE → next milestone). Progress memory: saves/progress.json.
./target/release/pokebot story --new-game --save-game     # from scratch, then save
./target/release/pokebot story --continue --save-game     # resume, play new milestones, save
```

Milestones so far: new game → Mom → Oak → starter → rival → parcel → Pokédex →
**PrepareForBrock** (the planner decides the training; the bot trains and heals) →
**ReachPewter** (Viridian Forest) → **BeatBrock** (Boulder Badge) →
**StockUpPewter** (buy Poké Balls) → **PrepareForRoute3** → **CrossRoute3** (trainer
battles to Route 4) → **PrepareForMtMoon** → **CrossMtMoon** (wild catches, the Helix
Fossil) → **ReachCerulean** (buy Poké Balls) → **PrepareForMisty** → **BeatMisty**
(Cascade Badge, TM03). The bot saves after each milestone; if a Pokémon faints it
reloads the last save and retries.

```bash
# Readiness planning on its own
./target/release/pokebot plan --against LEADER_BROCK --party BULBASAUR:6:TACKLE,GROWL
```

### In-process emulator (fast, deterministic)

```bash
# One emulated frame per read: runs ~10x console speed, bit-for-bit repeatable
./target/release/pokebot run --script tools/scripts/boot-to-new-game.txt --no-save --record captures/s

# Check a recording, or feed it back through the pipeline with the web UI
./target/release/pokebot replay captures/s
./target/release/pokebot run --video replay:captures/s --controller null --hold --web

# Play in a window (arrows, X=A, Z=B, A=L, S=R, Enter=Start, Backspace=Select, P=screenshot)
./target/release/pokebot play --web
```

## Web UI

`--web [addr]` on `run` and `play` serves a live dashboard:

- the normalized 240×160 frame the bot sees (lossless), plus `/stream.mjpg` for VLC/OBS;
- inferred `GameState` with provenance (screen, sync, inputs, dropped frames) and the current observation;
- a filterable log of actions (controller commands) and events (screen changes, dropped frames, errors).

Endpoints: `/frame.png`, `/stream.mjpg`, `/api/snapshot`, `/api/stream` (server-sent events).

`--instance-label <text>` names the run in `/api/snapshot` (default `Local`). The page only uses relative URLs, so it works standalone at `/` and behind the hub.

### The hub on port 8080: Switch and emulator side by side

`pokebot hub [--listen 0.0.0.0:8080] [--instances-dir /tmp/pokebot-instances]` is the one address for every run (the user's reverse proxy fronts port 8080):

- `/` redirects to `/switch/`, the main page, which always shows the physical Switch;
- `/emu/` shows the emulator run;
- `/api/instances` lists both, with `alive`, from the files `tools/live-run.sh` writes (`$POKEBOT_INSTANCES_DIR` overrides the directory).

Each instance serves its own UI on a loopback port (Switch `127.0.0.1:18080`, emulator `127.0.0.1:18081`) and the hub forwards `/<name>/…` to it with the prefix stripped, piping the bytes so MJPEG and server-sent events stream through. The forwarded request always says `Connection: close`, so each request gets its own connection and its own path rewrite. A stopped instance gives a 503 page with links to the others. The page shows one tab per instance, and the Switch page adds a small live thumbnail of the emulator.

`tools/live-run.sh [--instance switch|emu] <log> <pokebot args…>` starts the hub if needed and replaces only the same instance, so starting one never stops the other (`--instance` defaults to `switch` when an argument starts with `capture-card:`, else `emu`):

```sh
# Switch (main page, http://<host>:8080/switch/): never stops (--restart)
tools/live-run.sh --instance switch /tmp/sw.log story --video capture-card:/dev/video0 --viewport 180,5,1560,1040 --card-controls switch --controller esp32:<ip> --continue --save-game --progress saves/switch/progress.json --until CrossMtMoon --restart --record /tmp/sw-<n>
# Emulator (http://<host>:8080/emu/): in-process emulator on copies of the saves,
# so it never touches roms/*.sav or saves/switch/
tools/live-run.sh --instance emu /tmp/emu.log story --continue --save-game --save /tmp/emu/game.sav --progress /tmp/emu/progress.json --record /tmp/emu-<n>
```

Each process runs in its own systemd user unit (`pokebot-switch`, `pokebot-emu`, `pokebot-hub`, `pokebot-disk-guard`; `systemctl --user status <unit>`), so runs outlive the shell or agent session that started them. `sg video` gives the unit access to the capture card.

### Never stopping on the Switch

An idle Switch dims after 5 minutes and auto-sleeps after an hour or more; asleep, it needs a person to wake it. Three layers keep it busy:

- `story --restart` never ends. After the story finishes or fails, it watches the screen for `--restart-wait` seconds (default 240), then soft resets, CONTINUEs the last save (never a new game) and plays the next milestones. `--until <milestone>` caps every cycle at that milestone; later cycles then only keep the game alive. `--restart` needs `--save-game`.
- The ESP32 firmware nudges the right stick after 4 minutes without input (see `firmware/esp32s3-controller/README.md`).
- If the Switch sleeps or is turned off anyway, the bot waits for it and starts the game again by itself (next section). The ESP32 client reconnects after the board reboots or WiFi drops. `tools/live-run.sh` units restart a bot that exits with an error after 30 s.
- Set the Switch's own Auto-Sleep to Never (System Settings → Sleep Mode).

### Switch off, asleep, on the HOME menu

Before every CONTINUE (and a new game) `story` makes sure the game is on screen (`crates/agent/src/console.rs`). It reads two sources:

- **The capture.** An all-black frame means no HDMI signal. A lit letterbox around the game's viewport is a console screen (lock screen, HOME menu, user picker). Anything else is the game.
- **The controller.** The ESP32 reports whether the Switch has it configured over USB: `Attached`, `Suspended` (the bus is suspended: asleep) or `Detached` (off, or a dock that powers its ports down).

Then:

| What it sees | What it does |
|---|---|
| The game for 3 s | carries on (CONTINUE) |
| Black for 10 s, or black and the link is Detached/Suspended | logs "off or asleep" and waits. While Suspended it presses HOME every 30 s, which asks for a USB remote wakeup. |
| A console screen for 2 s | launch attempt: after a wake-up, A three times (the lock screen's "press the same button three times"); HOME (resumes a suspended game, or puts the cursor on the first software); Right to the slot; A to start; A for the user picker or "close the other software?". It waits 60 s for the game, then retries with the next slot (1, 1, 2, 3, …) and alternates the unlock. |

It never presses anything while the game is on screen, and it retries forever. Every new console screen is saved to `captures/console/` (the 50 newest), to build real detectors from.

### Disk

Recordings grow about 2 GB per hour on the Switch. `tools/disk-guard.sh` lists free space and every recording (size, age, ACTIVE while a bot writes it). `--prune` deletes inactive recordings older than 12 h (`KEEP_HOURS`); while free space is under 100 GB (`MIN_FREE_GB`) it deletes the oldest inactive ones and then trims old frames of active ones. It also drops debug bundles older than 14 days. `tools/live-run.sh` prunes on every launch and keeps the `pokebot-disk-guard` unit pruning every 30 minutes (log `/tmp/pokebot-disk-guard.log`). Copy frames worth keeping to `captures/fixtures/` first.

## Swapping devices

Every command takes the same device flags, and nothing downstream knows which devices are active:

| Flag | Values |
|---|---|
| `--video` | `emulator` · `capture-card:/dev/videoN` · `replay:<session>` · `images:<dir>` |
| `--controller` | `emulator` · `pabotbase:<serial port>` · `null` |
| `--viewport x,y,w,h` | Game area inside the captured frame (`pokebot inspect frame.png --viewport auto` finds it) |
| `--baud` | Serial baud for PABotBase2 (default: try 921600, then 115200) |
| `--realtime` | Run the in-process emulator at console speed instead of one frame per read |

With real hardware the only change is the device paths: `--video capture-card:/dev/video0 --controller pabotbase:/dev/ttyUSB0`.

## Layout

```
crates/core          domain types + VideoSource / Controller traits
crates/video         240×160 normalization, viewport detection, PNG I/O, image-sequence source
crates/controller    frame-clocked input scheduler, no-op controller
crates/replay        session recorder + replay video source
crates/state         Observation, events, reducer, GameState with provenance
crates/vision        perception: title, info pages, message box + ▼ arrow, menus + ▶ cursor, naming keyboard
crates/runtime       the bot loop: video → normalize → perceive → events → state
crates/agent         closed-loop tasks: NewGame, Story (milestones), navigation, battles
crates/world         world model (maps, collision, warps, connections), localization, A*
crates/telemetry     live hub + embedded web UI
adapters/emulator-libretro   mGBA core host: video out + joypad in
adapters/capture-card        V4L2 capture (UVC cards, v4l2loopback): RGB24/BGR24/YUYV/MJPEG
adapters/pabotbase           PABotBase2 protocol, PC client + device-side peer
adapters/virtual-console     emulator → V4L2 output + virtual ESP32 on a pseudo-terminal
apps/pokebot-cli     `pokebot` binary: new-game / run / play / emulator serve / inspect / replay
tools/               emulator download, virtual camera, world model build (tools/world), live-run.sh
```
