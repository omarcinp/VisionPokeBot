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
- `SwitchController::execute(SwitchCommand)` has the same contract for devices that emulate a whole Switch controller: all 18 buttons (including Home, Capture, ZL/ZR and the stick clicks) and both analog sticks.
  - `GbaOnSwitch<C: SwitchController>` is the adapter that makes such a device the bot's `Controller`, using the Switch Online GBA layout (`switch::gba_to_switch`).
  - One device therefore serves the bot and full-layout uses, such as navigating the Switch menus.

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
  | Bag / mart list | cream interior (bag and mart use different creams); pocket title plate; the ▶ cursor, or the inactive ▷ (swapped fill/edge colours) while a sub-window has focus; rows read with the font (name normal, price/count small, `O`→`0`) |
  | Caught-ball icon | a 7×7 Poké Ball (outline, highlight, orange/red top, grey/cream bottom) left of the opponent HP bar, present once that species is caught |
  | Shiny matcher | opponent sprite pixels inside the 64×64 sprite box classified against the species' normal/shiny palettes (`vision::shiny`); a species needs enough normal-only and shiny-only pixels to decide |
  | Pokédex page | tan background, upper/lower page split; only detected (to press A past it after a first catch), not read |

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

## Catching (`agent::catch`) and buying (`agent::stock`, `agent::shop`)

- **Catch policy** (`agent::catch::plan_catch`): a non-shiny catch is only attempted when the lead is at ≥ 75 % HP with no major status (a shiny ignores this gate, and also the risk gate and the ball reserve; it never RUNs for risk, but the ordinary low-HP rule — under 35 % HP — may still RUN). The plan:
  - picks an opener — a sleep or paralysis move only (sleep first), used once on a foe without a status — and a **crit-safe weakening move**: among the moves with PP whose critical-hit maximum roll (our iv 31 vs. the foe's iv 0) cannot faint the foe at the lowest HP its bar allows, the one with the highest normal maximum roll (ties: slot order);
  - estimates throws at the HP weakening can actually reach — `max(max_hp × 250‰, the crit-safe floor)` — not a fixed 25 %;
  - stops attempting when the estimated risk of a faint over the remaining turns exceeds **`RISK_LIMIT` = 2 %**;
  - picks the **best ball** by the catch formula, but the **Master Ball is never auto-used**;
  - declines when the known ball stock is at or below the shiny reserve (5) plus the expected throws, unless the target is a shiny;
  - during a non-shiny attempt, stops (RUN in a wild battle when allowed, otherwise fight on) as soon as the balls held are at or below the reserve: the count tracked from the attempt's start (one less per ball that broke free), and the battle bag's own count read before each throw.
- **Ball stock** (`agent::stock`): a reserve of **5** balls is kept for shinies only; a mart visit restocks toward a target of **15**; buying never spends the money needed for **2 Potions** (`SHINY_RESERVE`, `TARGET_STOCK`, `POTIONS_KEPT`). `must_buy` (stock below the reserve) and `should_buy` (stock unknown or below the target) gate a `StockUp` story step.
- **Heal-before-battle policy**: before a planned `Battle`/`Challenge` step, the lead heals when `P(win)` at full HP with every move (`p_full`) is ≥ 0.9 and higher than `P(win)` now with only PP-having moves (`p_now`), or when the lead has a major status. Both are the lead's P(win) alone, with the planner's IV assumption (`pokebot_planner::OUR_IV`). When `p_full < 0.9` healing can't help, so the step is fought anyway (a `GoalProgress{phase: "Warning"}` is logged) rather than looping. Walking steps (Go, Train, Battle, Challenge, a Talk's approach) also heal a known major status first.
- **Heal cap**: status and pre-battle heals are capped at **3 per story step** (`MAX_HEALS_PER_STEP`; the Heal steps spliced in don't reset the count, the next story step does). Past the cap, a pre-battle heal is replaced by: fight when the lead's `p_now` is ≥ 0.9 (one warning), otherwise the step fails with the reason; a status heal is skipped with one warning.
- **Which mart**: a `Buy` with no mart goes to the nearest mart selling the item **that can be walked to** — a tile-level search across maps from the player's tile to the tiles the clerk is talked from (the same search navigation plans with), ties by map name. From Route 4's west end or Mt. Moon 1F that is Pewter, not Cerulean (fewer maps away, but only through Mt. Moon). The mart is stored on the `Buy` step (`StockUp { mart }` passes it on), so a heal spliced in on the way keeps it.
- **Budget**: every purchase, explicit counts included, keeps the price of 2 Potions (`stock::affordable`).
- **Bag and mart perception** (`vision::detect::bag`, `vision::detect::shop`): the field bag and the battle bag share the same layout, colours and regions (the battle bag differs only in its bottom panel: USE/CANCEL instead of the description). The bag remembers the last pocket and row across field and battle. Prices, counts, money and the Pokédex number use the small font, where `0` and `O` share a bitmap (`O` → `0`).
- **Events**: `PocketObserved` (an audit), `ItemsChanged` (`"bought"`, `"thrown"`, `"obtained"`, `"received"`), `MoneyObserved`/`MoneyChanged`, `SpeciesSeen`, `SpeciesCaught`, `ShinySeen`.
- **Live results (emulator, Task 11–13):** `StockUpPewter` audited 5 Poké Balls and bought 10 (to the target of 15). Four species were caught in Mt. Moon (GEODUDE, ZUBAT, CLEFAIRY, PARAS), each with the full sequence (throw, `ItemsChanged(-1, "thrown")`, "Gotcha!", the Pokédex page, "nickname: NO", `SpeciesCaught`); the Helix Fossil was obtained. Misty was beaten with no faint; the lead's lowest HP was 61/72.

## New moves and evolution (`agent::learn`, `agent::moves`)

- Every finished page is parsed for facts: "X is trying to learn M.", "X forgot M.", "X learned M!", "X did not learn M.", "X evolved into S!". Move names map to constants through the game's own names (`move_names.h`, with `?` wildcards).
- Party knowledge only changes on those confirmations: nothing is inferred from the learnset, even while a slot is free. "X learned M!" fills the next free slot (`MoveLearned`); with four moves it replaces the slot of the move the same Pokémon forgot on the page before (`MoveReplaced`). A forgotten move is tied to that Pokémon and cleared by the next offer, so a missed page never swaps the wrong move; if the "forgot" page was missed, nothing changes until the KNOWN MOVES list or the move menu shows the moves.
- **Which move to forget** (`moves::choose`): each candidate set (the current four, or the new move in slot k) is ranked by, in order:
  1. total evaluator win probability against the trainers the last plan prepared for;
  2. damage value (power × accuracy, STAB), because the evaluator ignores PP, so a damaging move is never traded for a status move unless that wins battles;
  3. status utility (sleep > paralysis > poison / Leech Seed > stat drops);
  4. keeping the current moves, then the earliest slot.
- "Delete a move to make room for M?" gets YES if a slot was chosen, otherwise NO. "Stop learning M?" gets the opposite. On the KNOWN MOVES list the four moves shown replace party knowledge (and the choice is recomputed if they differ). The frame is then moved row by row, each step verified, to the chosen slot (or to the new move, to skip it), and A is pressed.
- In battle, any other YES/NO question fails the attempt instead of being answered, because A would mean YES.
- Evolution is never assumed. The species changes on "X evolved into S!" (S matched uniquely with `GameData::species_named`) or when the HUD shows an evolved name (any branch of the evolution chain). With a single party member, a HUD name that is neither its species nor an evolution but matches exactly one species replaces the species (e.g. knowledge restored from another save).
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
  - Only the lead fights (switching is out of scope; "change POKéMON?" is answered NO), so readiness is planned for **the lead alone**: other party members (catches) never count toward P(win). A `Prepare` that needs nothing logs "already ready (lead alone)" with the lead's P(win) per target.
  - A `PlanStep::Catch` becomes lead-only training at the catch's area, to the level a lead-only training plan needs (the step fails if the lead alone can't reach the confidence). Catches themselves happen opportunistically in any wild battle (see "Catching").
- **Party knowledge** (`agent::party`): species, level, moves in learning order (that order gives the menu slots), HP and PP used.
  - It's updated from the battle HUD: names, levels and HP are read from its font (the letter shapes were learned from recorded battles), and names are matched against the species dictionary with wildcards.
  - Moves come only from the screen (learn/forget text, the KNOWN MOVES list, the battle move menu). The knowledge is `GameState.party`, persisted in `state.json` at each checkpoint (see "Global state").
- **Battle move choice:** the opponent is identified from its HUD name and level, and the evaluator's best move is chosen. In wild battles, strong moves keep 5 PP in reserve for trainers. A battle counts as a trainer battle when dialogue preceded it (so no RUN).
- **Checkpoints and retry:** `story --continue --save-game` runs one milestone at a time and saves after each. If our Pokémon's HP reads 0, the attempt fails; the bot reloads the last save (soft reset, then CONTINUE) and retries that milestone, up to 5 times.
- **Route 3 preparation:** from the Boulder Badge save, the planner chose Lv 18 on Route 22 (Youngster Calvin's Spearow was 0% at Lv 14). Training took 68 battles. The bot swapped Growl for Poison Powder, then Poison Powder for Sleep Powder, evolved into IVYSAUR at 16, healed twice, and saved in Pewter. No Pokémon fainted.
- **Result:** from the Pokédex save, the planner chose "Lv 10 on Route 22". Training took 12 battles. The bot then crossed Viridian Forest (7 battles), healed and saved in Pewter, beat Camper Liam and Brock (lowest HP 27/38) and received the Boulder Badge. No Pokémon fainted.
- **Cascade Badge segment (live, emulator):** `StockUpPewter` (audited 5 Poké Balls, bought 10 → 15) and `CrossRoute3` (four trainer battles, no faint) reached Route 4. `PrepareForMtMoon` said "already ready" (no training). `CrossMtMoon` took several fixes (cross-map routing, localization, the DISABLE and status/heal policies) before an attempt with no faint: four wild catches (GEODUDE, ZUBAT, CLEFAIRY, PARAS) and the Helix Fossil. `ReachCerulean` bought 4 more balls (11 → 15). `PrepareForMisty` said "already ready". `BeatMisty` succeeded on the first attempt with no faint, lowest HP 61/72; IVYSAUR alone beat Swimmer Luis and Misty (STARYU, STARMIE), earning the Cascade Badge and TM03 (Water Pulse). Checkpoints: `saves/route4.*`, `saves/mtmoon-done.*`, `saves/cascade.*`.

## Global state (`crates/state`, `agent::party`, `agent::checkpoint`)

- **Model:** `GameState` carries typed knowledge beyond the HUD-derived `agent::party::Party` used for battles — `party: Knowledge<Vec<PartyMon>>` (`crates/state/party.rs`) and bag/money/PC (`crates/state/inventory.rs`: `Pocket`, items, money, boxes, Pokédex caught/seen flags). Every field is a `Knowledge<T>` (`value`, `source`, `last_verified_frame`); `source` is one of `Observed`, `Derived` (worked out from static game data, e.g. a move's max PP when learned), `Tracked` (changed by an event since the last observation), `Assumed`, `UserProvided` or `Unknown`. `Knowledge::is_stale()` is true for `Tracked`/`Assumed` — staleness is about provenance, never time.
- **Events and reducer:** `GameEvent` (`crates/state/events.rs`) adds party/bag/money facts to the existing screen/goal/navigation events — `PartyMonDerived`, `PartyObserved`, `MovesObserved`, `MovePpObserved`, `MoveUsed`, `MoveLearned`, `MoveReplaced`, `MoveOutOfPp`, `Evolved`, `ItemsChanged`, `PocketObserved`, `MoneyObserved`, `MoneyChanged`, `BoxObserved`, `PcItemsObserved`, `SentToPc`, `MonDeposited`, `MonWithdrawn`, `SpeciesSeen`, `SpeciesCaught`, `ShinySeen`, `CheckpointRestored`. `crates/state/reduce_knowledge.rs` applies them purely: an `Observed`/`Derived` fact overwrites; a `Tracked` delta (e.g. `MoveUsed` spending one PP, `MoneyChanged`) applied to an `Unknown` value leaves it `Unknown` rather than guessing a baseline.
- **Passive sources** feed events without the bot acting on them:
  - the battle HUD (species/level/HP) and the move menu's small font (move names and `PP a/b` under the ▶) — `agent::party::battle_events`, which only emits `MovesObserved`/`MovePpObserved` when the read disagrees with what's already derived, so a party that's fully in sync with its tracked PP produces no redundant events;
  - dialogue text facts — `agent::track::TextTracker`: "X got ¥Y for winning!" → `MoneyChanged` (trainer wins only; wild battles never print this, so a training run against wild Pokémon legitimately produces zero money events), "X found/received/obtained an ITEM!" → `ItemsChanged`, "We've restored your POKéMON..." → `Healed`, "no PP left for this move!" → `MoveOutOfPp`;
  - move learning and evolution text — `agent::learn` (`MoveLearned`, `MoveReplaced`, `Evolved`).
- **`agent::Party` is now a view**, not its own store: `Party::from_state(&GameState)` (`crates/agent/party.rs`) projects the current `GameState.party` into the flat shape the battle policy and planner use, instead of the party being tracked independently.
- **Checkpoints:** every in-game save writes `state.json` next to the progress file (`agent::checkpoint::store`) with the full `SavedKnowledge` (party + bag/money/PC; missing fields load as unknown) and the **identity** of the `progress.json` written with it (its milestones and `saved_at`). `state.json` is written first, then `progress.json`, each through a `.tmp` file renamed into place. On `story --continue` and on a faint-triggered retry, `checkpoint::restore` reloads it only if its identity matches the `progress.json` being loaded, emits `CheckpointRestored` and logs `Checkpoint knowledge restored from <source>`. A missing `state.json`, one without an identity (written before identities existed) or one for another save falls back to the legacy migration, with an `error: checkpoint: … ignored` warning for the last two: the party kept in an older `progress.json` becomes `Tracked` (stale) knowledge (`checkpoint::legacy_knowledge`); no party there means an unknown party.
- **Healing:** the nurse's "restored your POKéMON" page emits `Healed`. If a Heal step's conversation ends without that page having been read, `Healed` is inferred (log: "heal inferred: the nurse's text was not read"), because the nurse always heals.
- **Web UI:** the telemetry page (`crates/telemetry/web/index.html`) renders party (species, level, HP, moves/PP), money and bag from `GameState`, each value annotated with its provenance, so a stale (`Tracked`) fact reads differently from an `Observed` one.

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

**ESP32-S3 WiFi controller (`crates/remote`, `adapters/esp32-wifi`, `firmware/esp32s3-controller`)**
- Our own firmware: the ESP32-S3 shows up on its native USB port as a wired HORI Pokkén pad (VID 0x0F0D, PID 0x0092), and the bot drives it over WiFi. The Switch needs no pairing.
- Protocol v2: newline-delimited JSON over TCP port 7878. The client sends `SwitchCommand`s. The device replies `accepted` (with the input duration), then `finished` once the last timed input has elapsed. HTTP on port 80 serves `/api/{info,status,command,neutral}` and a manual control page with the full Switch layout.
- The firmware only knows the Switch layout. `Esp32WifiController` implements `SwitchController`, and the CLI hands the bot `GbaOnSwitch(Esp32WifiController)`.
- The device expands commands with `SwitchCommand::timeline` and times every input itself (1 ms tick). WiFi latency therefore delays when a command starts, but never changes how long buttons are held. `Neutral` cancels the queue.
- `pokebot-remote` holds all of the device logic: queue, HID report and descriptor, TCP and HTTP servers. It uses plain `std`, so the same code runs in three places:
  - the firmware (ESP-IDF std, TinyUSB via a small C component);
  - `pokebot-remote-sim` (prints HID reports);
  - `pokebot emulator serve` (presses the emulator's joypad, `--controller esp32:127.0.0.1`).
- The firmware also runs in Espressif QEMU (`--features qemu`), using emulated OpenCores Ethernet and logging HID reports instead of sending them over USB.
- **Not yet verified on real hardware.** Tested against the simulator and against the firmware under QEMU.

**Virtual console (`adapters/virtual-console`)**
- Writes emulator frames to a V4L2 output device (RGB24, integer-upscaled, default 3× = 720×480).
- Serves PABotBase2 on a pseudo-terminal, symlinked to `/tmp/pokebot-esp32`.
- Serves the ESP32-S3 WiFi controller protocol on 127.0.0.1:7878 (HTTP on 8078).
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

- `events.jsonl` is flushed on every write and `frames.jsonl` about once a second; `controller.jsonl` is buffered and flushed only by `SessionRecorder::flush()` (at the end of a run) or when the recorder is dropped.
- SIGINT and SIGTERM shut the bot down cleanly.

## Next

- **Switch acceptance of the Route 3 → Cascade Badge milestones.** All live runs so far are on the emulator; the physical Switch is the target. Acceptance needs `PrepareForRoute3` first (the Switch save is at `BeatBrock`), and it is pending because the console's video was black (the controller is mounted but there's no HDMI image).
- Nugget Bridge and Bill.
- A Paralyze Heal / Potion policy, to cut the round trips a paralyzed lead currently makes to a Pokémon Center mid-dungeon.
- A HUD status badge reader (PAR/PSN/SLP/BRN/FRZ), so status doesn't come from battle text alone.
- `SentToPc`, not yet seen live (the party reached 5 members in Mt. Moon; a sixth catch fills the last slot, and the one after that goes to the PC).
- Other deferred items from the SDD ledger (`.superpowers/sdd/2026-09-24-cascade-badge-and-catching/progress.md`): badges and TM Case not persisted in `state.json`, dead-reckoning or ambiguity-reporting for featureless corridors, `badge_received` covering gyms after Misty, and the rest of the minors listed per task.
- The Town Map from Daisy (optional side goal).
- `pokebot run --hold --record` and `pokebot play --record` capture fixtures for new detectors.
