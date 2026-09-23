# Architecture (as built)

The target architecture is in [`spec.md`](spec.md). This file describes what exists today: Phase 0 (interfaces, replay foundation), Phase 1 (devices, normalization), the production device adapters, and the observability UI.

## Topology

Development uses the same process boundaries and device interfaces as the real rig:

```
 ┌──────────── pokebot emulator serve ────────────┐        ┌──────────────── pokebot run ────────────────┐
 │                                                 │        │                                              │
 │ mGBA core ──frames──▶ V4l2Sink ────────────────────▶ /dev/video10 ──▶ CaptureCardVideoSource            │
 │ (real-time)                                     │  v4l2loopback │     │                                 │
 │     ▲                                           │        │      ▼                                        │
 │     │ joypad                                    │        │  Runtime: normalize → perceive → events      │
 │ VirtualDevice (PABotBase2 firmware peer) ◀──────────── /tmp/pokebot-esp32 ◀── PabotBaseController      │
 │                                                 │   pseudo-terminal       │                             │
 └─────────────────────────────────────────────────┘        │  Telemetry ──▶ web UI (HTTP)                 │
                                                            └──────────────────────────────────────────────┘
 Real hardware: Switch ──HDMI──▶ capture card /dev/video0      ESP32 (PABotBase2 firmware) ◀── /dev/ttyUSB0
```

Replacing the emulator with the Switch changes only the two device paths.

For fast, deterministic work the emulator can also run in-process (`--video emulator --controller emulator`), stepping one frame per read.

## Contracts (`crates/core`)

- `VideoSource::next_frame() -> CapturedFrame { frame_id, captured_at, image }`: blocks for the next frame. Gaps in `frame_id` mean frames were dropped.
- `Controller::execute(ControllerCommand) -> ControllerReceipt`: queues input and returns immediately. `is_idle()` reports when the queue has drained. A receipt only confirms that the device accepted the command; only video can confirm what the game did with it.
- Every adapter expands `ControllerCommand`s the same way, via `ControllerCommand::timeline(PressProfile)`.
- Buttons are logical GBA buttons. Adapters map them to the physical controller: Start → Switch `+`, Select → `−`, and so on.

## Bot loop (`crates/runtime`, `state`, `vision`)

```
VideoSource → Normalizer (240×160) → PerceptionSystem → Observation
           → EventExtractor → GameEvent → DefaultReducer → GameState
Controller ◀── commands (also emitted as InputIssued events)
```

- **Perception** (`FireRedPerception`) is rule-based, using colours and geometry measured from the game; no game graphics are stored. A tolerance of ±24 per channel covers capture-card noise.

  | Detector | Rule |
  |---|---|
  | Transition | ≥ 99.5 % of pixels near-black or near-white |
  | Title screen | orange band on rows 0–5, dark-red band on rows 152–159 |
  | Info page | blue header bar (rows 1–6) |
  | Message box | border / inner / white-interior pattern at y 115–156 |
  | Waiting for A | red ▼ whose rows shrink 9-7-5-3-1, inside the text area |
  | Menu | gray ▶ cursor (rows 1-2-3-4-5-4-3-2-1); window found by walking the white interior; rows 16 px apart |
  | Naming keyboard | panel colour; the focused key is the cell whose border isn't panel-coloured; typed count = filled 8 px name slots |
  | YES/NO over a battle | the battle ▶ above the battle panel, inside a white window (same window walk as field menus) |
  | KNOWN MOVES list | tan/blue header bar; the selected row's red frame (rows 28 px apart); five move names read with the font |

  Anything else is `Unknown`, including the overworld for now.
- **Text reading (`vision::text`):** the game's normal font is extracted from the decompilation (`tools/gamedata/extract_font.py` → `data/world/font_normal.json`: bitmap and advance width per character, built locally).
  - Text colour depends on the speaker and the window (blue, red, dark gray, white on battle blue), so the reader tries each prominent non-background colour as ink and keeps the reading that explains the most ink.
  - Lines are found as bands of ink rows. Each band tries every cell top that fits, and glyphs are matched exactly on their ink columns, indexed by the first column. The widest, inkiest match wins, and ties go to the lowest character code.
  - A gap of 4+ blank columns reads as a space; unexplained ink reads as `?`. Lines that glyphs explain worse than they leave unexplained (window frames) are dropped.
  - Dialogue observations carry `lines`, re-read only when the text cells change (about 1 ms per page).
- **Screen events:** a screen change must persist for 2 frames before `ScreenChanged` is emitted, so single-frame flicker never reaches state.
- **Reducer:** pure. A `GameState` can be rebuilt from `events.jsonl`.
- **Provenance:** every state field records where its value came from (`Observed`, `Tracked`, …, `Unknown`).

## Agent (`crates/agent`)

- A `Task` looks at the latest `Observation` and `GameState` and returns a `Decision`:
  - act (an `Action`);
  - wait (text printing, animation);
  - done;
  - fail.
- An `Action` has a label, controller commands, an `Expectation` and a timeout. Expectations include:
  - `TextAdvanced` (the text-area luma grid changed, ignoring the bouncing arrow);
  - `MenuCursorAt`, `MenuClosed`, `KeyboardFocus`, `TypedCount`, `NamingClosed`, `ScreenIs`.
- The `Executor` issues the commands and then waits until the controller is idle and the expectation holds on 2 consecutive frames. If that doesn't happen, the action times out.
  - The task decides whether to retry (up to 12 attempts per phase).
  - Waiting more than 45 s without acting counts as stuck.
  - Every decision is logged to telemetry, so the web UI shows the goal, each action and its reason.
- The task records confirmed facts as events (`GoalProgress`, `GenderChosen`, `PlayerNamed`, `RivalNamed`, `ControlConfirmed`). The reducer turns them into `progression` with `Observed` provenance.

**`NewGameTask`** phases:

```
SoftReset → AwaitTitle (Start skips intro) → Title (Start) → AfterTitle
→ Intro (A on ▼) → Gender (2-row menu) → PlayerIntro → PlayerName (keyboard)
→ ConfirmPlayer (YES) → RivalIntro → RivalChoice (5-row list)
→ [RivalName (keyboard)] → ConfirmRival (YES) → Outro (until a transition and 90 quiet frames)
→ OpenStartMenu → CloseStartMenu → Done
```

- The phase only tells the task how to interpret a menu or keyboard it can see. The inputs themselves are always chosen from, and verified against, the screen.
- Typing is closed-loop, one step at a time:
  1. read the focused key;
  2. tap toward the next letter;
  3. confirm the focus moved there;
  4. press A;
  5. confirm the typed count went up;
  6. finish with Start (cursor to OK), then A, and confirm the keyboard closes.
- The keyboard layout is static knowledge from the decompilation (upper-case page: `ABCDEF .` / `GHIJKL ,` / `MNOPQRS ` / `TUVWXYZ `).
- **Not yet covered:** the main menu shown when a save file exists. The task handles it (it picks row 1, NEW GAME), but that path hasn't been exercised yet. Lower-case and symbol names aren't supported either.

## World model, localization and navigation (`crates/world`, `agent::nav`)

- `tools/world/build.sh` pulls the map, layout and tileset data from pret/pokefirered, then `extract_world.py` renders every map. This happens offline and the output stays local and gitignored.
  - Each map is rendered exactly as the game draws it: bottom and top metatile layers, and colours converted the way mGBA outputs them.
  - Renders are padded 8 tiles on every side with the border pattern and any connected maps.
  - A JSON model sits beside each render: collision, elevation, tile behaviour, warps, connections, NPCs, signs and triggers.
- **Localization:** the camera always draws the player's tile at screen (112, 72), so a candidate `(map, x, y)` is simply a crop of that map's render.
  - The frame is compared against the crop on a sparse grid, skipping the player sprite and any open text box or menu.
  - Early exit keeps it at about 0.1 ms near the last pose.
  - The search widens as needed: near the last pose, then the whole map, then connected and warp-linked maps. An optional global search is available but throttled.
  - A pose is confirmed after 2 consecutive frames, producing `PlayerLocated`, `PlayerMoved` or `MapChanged` events.
- **Navigation:** A* per map, where tall grass costs +4 and ledges can only be crossed one way. Between maps, a breadth-first route over fixed warps and map connections.
  - The bot taps one direction per tile and confirms the move by relocating the player.
  - A tap that doesn't move the player counts as a turn; a second one marks the tile blocked.
  - How to take each exit depends on the tile: stand below a door and press Up; stand on an arrow mat and push its arrow; stand on stairs and push sideways; for a map edge, walk to a tile that continues into the neighbour and step across.

## Story (`agent::story`)

- Milestones are lists of declarative `StoryStep`s:
  - `Go(destination)`;
  - `GoUntil { dest, until }`, which walks until a cutscene takes over;
  - `Talk { object, answers }`;
  - `Battle { trigger }`;
  - `Settle { frames }`.
- One `StoryTask` runs them. Battles and dialogue can interrupt any step:
  - A is pressed on ▼, or when the text has been unchanged for 45 frames (the last page shows no arrow);
  - YES/NO questions are answered from the step's script;
  - after any dialogue, the bot waits 1.5 s of quiet before walking again.
- `opening()` covers: new game → bedroom → Mom → leave house → Oak's event → starter → rival battle → heal at home → Viridian Mart (parcel) → deliver the parcel (Pokédex).

## Battles (`vision::detect::battle`, `agent::battle`)

- **Detection:**
  - the gold-framed dark-blue text box (only its left half is checked, because level-up panels cover the right);
  - the near-black battle ▶ (rows 2-3-4-5-5-5-4-3-2), which gives the 2×2 command and move menus;
  - the HP bars (48 px each, framed, green/yellow/red), read as per mille.
- **Policy (placeholder):** FIGHT with move slot 1. RUN when HP is under 35 %, trying at most 3 times per battle so trainer battles fall back to fighting.

## New moves and evolution (`agent::learn`, `agent::moves`)

- Every finished page is parsed for facts: "X is trying to learn M.", "X forgot M.", "X learned M!", "X did not learn M.", "X evolved into S!". Move names map to constants through the game's own names (`move_names.h`, with `?` wildcards).
- Party knowledge only changes on those confirmations. Level-ups add learnset moves by inference only while a slot is free.
- **Which move to forget** (`moves::choose`): each candidate set (the current four, or the new move in slot k) is ranked by, in order:
  1. total evaluator win probability against the trainers the last plan prepared for;
  2. damage value (power × accuracy, STAB), because the evaluator ignores PP, so a damaging move is never traded for a status move unless that wins battles;
  3. status utility (sleep > paralysis > poison / Leech Seed > stat drops);
  4. keeping the current moves, then the earliest slot.
- "Delete a move to make room for M?" gets YES if a slot was chosen, otherwise NO. "Stop learning M?" gets the opposite. On the KNOWN MOVES list the four moves shown replace party knowledge (and the choice is recomputed if they differ). The frame is then moved row by row, each step verified, to the chosen slot (or to the new move, to skip it), and A is pressed.
- In battle, any other YES/NO question fails the attempt instead of being answered, because A would mean YES.
- Evolution is never assumed. The species changes on "X evolved into S!" or when the HUD shows an evolved name (any branch of the evolution chain).
- PP comes from the screen. The move menu prints `PP a/b` for the move under the ▶, which sets that move's PP. Wild battles spend damaging moves' PP above a 5-PP trainer reserve, then the reserve, then RUN. Training heals when fewer than 6 attack PP are left to spend, as well as below 50% HP.

## Readiness planning (`crates/gamedata`, `crates/planner`)

- **Game data:** `tools/gamedata/extract_gamedata.py` pulls from the decompilation:
  - species (base stats, types, catch rate, exp yield, growth rate, learnset, evolutions);
  - moves and the type chart;
  - 743 trainers with their parties;
  - which map objects are trainers (with sight range; route trainers' scripts live in `data/scripts/trainers.inc`);
  - FireRed wild encounter tables, marts and item prices.
- **Mechanics:** Gen III stats, experience curves and gains, the damage formula (16 rolls, crits, STAB, type multipliers, physical/special by type), and the shake-check catch formula.
- **Evaluator:** exact probability that one Pokémon beats another. It tracks the full HP distribution over hit/miss, crits and damage rolls, and uses speed order. Opponents are assumed to always use their most damaging move (pessimistic). Trainer battles chain matchups in party order with expected HP carried over (approximate).
- **Planner:** searches every team composition (the party, or the party plus one species catchable in a reachable area) against every combination of level targets, up to +14 levels per member.
  - New moves and evolutions come from the learnsets.
  - Training time is priced from the area's encounter table: exp per battle, battle length from the matchup, steps between encounters, and healing trips.
  - Catch time uses encounter share and the catch formula (target weakened to half HP), limited by money for Poké Balls.
  - It returns the cheapest plan meeting the confidence target (default 90%), plus the other options that are better on either time or confidence. Results are deterministic.
- **`pokebot plan --against LEADER_BROCK --party BULBASAUR:6:TACKLE,GROWL`** → train to Lv 10 on Route 22 (Vine Whip), 93%.
  - Charmander → catch a Mankey on Route 22 and train it to 13 (Low Kick / Karate Chop).
  - Squirtle → Lv 13 on Route 1 (Water Gun).
- **Plan execution:** a `Prepare` story step runs the planner on the current party knowledge and replaces itself with `Train { map, level }` steps.
  - Training paces between two neighbouring tall-grass tiles and fights whatever appears.
  - It heals at the nearest Pokémon Center when HP drops below 50% (talking to the nurse across the counter, answering YES).
  - It stops once the level read from the battle HUD reaches the target.
  - Catch steps aren't executed yet.
- **Party knowledge** (`agent::party`): species, level, moves in learning order (that order gives the menu slots), HP and PP used.
  - It's updated from the battle HUD: names, levels and HP are read from its font (the letter shapes were learned from recorded battles), and names are matched against the species dictionary with wildcards.
  - Moves come from the learnset as levels rise. The knowledge is persisted in `saves/progress.json`.
- **Battle move choice:** the opponent is identified from its HUD name and level, and the evaluator's best move is chosen. In wild battles, strong moves keep 5 PP in reserve for trainers. A battle counts as a trainer battle when dialogue preceded it (so no RUN).
- **Checkpoints and retry:** `story --continue --save-game` runs one milestone at a time and saves after each. If our Pokémon's HP reads 0, the attempt fails; the bot reloads the last save (soft reset, then CONTINUE) and retries that milestone, up to 5 times.
- **Route 3 preparation:** from the Boulder Badge save, the planner chose Lv 18 on Route 22 (Youngster Calvin's Spearow was 0% at Lv 14). Training took 68 battles. The bot swapped Growl for Poison Powder, then Poison Powder for Sleep Powder, evolved into IVYSAUR at 16, healed twice, and saved in Pewter. No Pokémon fainted.
- **Result:** from the Pokédex save, the planner chose "Lv 10 on Route 22". Training took 12 battles. The bot then crossed Viridian Forest (7 battles), healed and saved in Pewter, beat Camper Liam and Brock (lowest HP 27/38) and received the Boulder Badge. No Pokémon fainted.

## Devices

**Emulator (`adapters/emulator-libretro`)**
- Hosts the mGBA libretro core on its own thread and binds only the run, video, input and lifecycle entry points.
- Two clocks:
  - *Stepped:* one frame per read. Deterministic; tested bit-for-bit.
  - *Real-time:* runs at 59.7275 Hz; readers get the newest frame.
- `InputSchedule` turns commands into per-frame button states and tracks which commands have completed.
- Battery saves: `cartridge.rs` is the only code that touches core memory, and only save RAM, copied opaquely. A guard test fails the build otherwise.

**Capture card (`adapters/capture-card`)**
- V4L2 mmap streaming on a background thread that keeps only the newest frame.
- Decodes RGB24, BGR24, YUYV (BT.601 limited range) and MJPEG.
- Frame ids follow the driver's sequence counter, so dropped frames are visible.
- Skips the initial ring of buffers, which can hold stale frames.

**PABotBase2 (`adapters/pabotbase`)**
- The protocol spoken by current PokemonAutomation ESP32 and RP2040 firmware, implemented from the upstream wire format (MIT).
- Link layer: framed packets with CRC32C seeded by the session ID, 8-bit sequence numbers, retransmission, and a reliable byte stream.
- Message layer on top of the stream: queries and the command queue.
- The PC client performs the reference handshake, then selects the wireless Pro Controller (or the wired controller).
- Each command is sent as a button report with a duration in milliseconds. Commands are paced by the device's queue capacity and complete on `CQ_COMMAND_FINISHED`.
- `device::VirtualDevice` is the device-side peer used by the virtual console.
- **Not yet verified against real firmware.** Both ends are implemented here from the upstream source.

**Virtual console (`adapters/virtual-console`)**
- Writes emulator frames to a V4L2 output device (RGB24, integer-upscaled, default 3× = 720×480).
- Serves PABotBase2 on a pseudo-terminal, symlinked to `/tmp/pokebot-esp32`.
- Reported command completion follows the emulator's actual frame consumption.

## Telemetry and web UI (`crates/telemetry`)

- The runtime publishes frames, `GameState`, observations and log entries to a `Telemetry` hub. Telemetry is write-only: nothing flows back into the bot.
- An embedded axum server serves:
  - the page;
  - `/frame.png` (lossless);
  - `/stream.mjpg`;
  - `/api/snapshot`;
  - `/api/stream` (server-sent events: `status` at 10 Hz, `frame` at 30 Hz, and `log` entries).

## Replay sessions (`crates/replay`)

```
session/
├── metadata.json
├── frames.jsonl       frame_id, elapsed_us, fingerprint, file
├── controller.jsonl   command_id, elapsed_us, after_frame_id, command
├── events.jsonl       semantic events (frame_id + event)
├── frames/*.png       normalized 240×160 frames
└── raw/*.png          optional full-resolution captures (--record-raw)
```

- `controller.jsonl` and `events.jsonl` are flushed on every write, and `frames.jsonl` about once a second.
- SIGINT and SIGTERM shut the bot down cleanly.

## Next

- Menu text: read YES/NO and list menus with the font (shops, bag, party).
- Catching (BAG → Poké Ball) and buying at marts, so plans with catches can run.
- Route 3 / Mt. Moon toward Misty, with story gating (which areas are reachable) derived rather than listed.
- The Town Map from Daisy (optional side goal).
- `pokebot run --hold --record` and `pokebot play --record` capture fixtures for new detectors.
