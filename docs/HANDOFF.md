# Handoff prompt — continue PokéBot FireRed toward the full story

Paste everything below the line into a new session (working directory: this repo).

---

You are continuing **VisionPokeBot / PokéBot FireRed**: a Rust bot that plays Pokémon FireRed **using only video frames and controller input**, the same as a real Switch with an HDMI capture card and an ESP32 controller. The full design spec is `docs/spec.md`. The as-built architecture is `docs/architecture.md`; read it first. `README.md` has the commands.

## Non-negotiable rules
- **Vision only.** Never read emulator memory, save states or scripting APIs. The only exception is opaque battery-save persistence in `adapters/emulator-libretro/src/cartridge.rs`, and `tests/no_privileged_access.rs` enforces it. Knowledge comes from pixels, static game data (decompilation) and the bot's own history.
- **Closed loop.** Every action carries an `Expectation` that is checked on screen, plus a timeout. Never treat a sleep as evidence that something happened.
- **Deterministic.** Same observations → same plans. Tie-breaks are explicit. No random sampling.
- **Unknown is a legitimate state.** Wildcards and `Knowledge` provenance instead of guesses.
- **No ROMs or game assets in git.** `roms/`, `emulator/`, `data/pret-pokefirered/`, `data/world/`, `saves/` and `captures/` are gitignored. Data derived from the decompilation is built locally.
- Python only for offline tooling (`tools/`). The runtime is Rust.

## Working agreements with the user (important)
- **Every bot run must be visible on the LAN web UI.** Launch runs through `tools/live-run.sh <log> <pokebot args…>`. It adds `--web 0.0.0.0:8080 --hold` and replaces the previous run. The UI is at http://10.10.100.21:8080. Use a fresh `--record` directory per run.
- Verify claims visually: build contact sheets of recorded frames with ffmpeg (`tile=`) and read them. Don't trust "completed" logs alone.
- The user's rule for battles: **no Pokémon may faint**. On a faint, reload the latest save (`story --continue` handles this via checkpoints) and retry.
- Nothing has been committed yet. At the start, run `git status`; there are many untracked files. Ask the user whether to commit (on a branch) before making large changes.

## Environment
- Rust via `~/.cargo/bin/cargo` (not on PATH by default). Python venv at `.venv` (pillow, numpy), used by `tools/world/build.sh`.
- The emulator core is `emulator/mgba_libretro.so` (`tools/fetch-emulator.sh`). The ROM is `roms/Pokemon - Fire Red Version (U) (V1.1).gba` and its cartridge save is `roms/…(V1.1).sav`.
- The world model and game data live in `data/world/` (`tools/world/build.sh`, about 30 s; it also runs `tools/gamedata/extract_gamedata.py`). The decompilation is a sparse clone in `data/pret-pokefirered/` (maps, layouts, tilesets, `src`, `include`).
- Virtual hardware: `tools/setup-virtual-camera.sh` creates `/dev/video10` (v4l2loopback, needs sudo, once per boot). `pokebot emulator serve` provides the emulator as a console: video on `/dev/video10`, PABotBase2 on `/tmp/pokebot-esp32`.
- Processes that may still be running: `/tmp/pokebot-live emulator serve --no-save` (the virtual console) and `/tmp/pokebot-webrun …` (the last live run, holding port 8080). Kill them by exact name: `pkill -INT -x pokebot-webrun`. Never use `pkill -f` with patterns that match your own shell command.

## Current game state (checkpoint)
- `saves/progress.json` plus the ROM's `.sav`: all milestones through **PrepareForRoute3** are done. The game is saved in the Pewter Pokémon Center at (7,4). There is no `saves/state.json` yet: the next in-game save writes it next to `progress.json` (party/bag/money/PC knowledge with provenance, tied to that `progress.json` by its milestones and save position, restored on `--continue` and on a faint retry — see "Global state" in `docs/architecture.md`). Until then `--continue` migrates the party kept in `progress.json` as stale (`Tracked`) knowledge.
  - Party: IVYSAUR Lv 18 with Tackle, Sleep Powder, Leech Seed, Vine Whip.
  - Player RED (Boy), rival GREEN, Boulder Badge.
- Backups: copy `X.sav` over the ROM's `.sav`, `progress.X.json` over `saves/progress.json`, and `state.X.json` over `saves/state.json` when it exists — otherwise **delete** `saves/state.json` (a `state.json` that doesn't match the restored `progress.json` is ignored with a warning anyway, and the party in `progress.X.json` is migrated as stale knowledge). When making a new backup, copy all three files.
  - `saves/route3-ready.sav` + `progress.route3-ready.json` + `state.route3-ready.json`: after PrepareForRoute3 (the state file comes from a run that ended on exactly this save);
  - `saves/brock.sav` + `progress.brock.json`: after BeatBrock;
  - `saves/pre-brock.sav` + `progress.pre-brock.json`: after the Pokédex;
  - `saves/trained.sav` + `progress.trained.json`: Lv 10 at the Viridian Pokémon Center.
- Resume with: `tools/live-run.sh /tmp/x.log story --continue --save-game --record /tmp/run-N`.

## Code map (about 14k lines of Rust)
- `crates/core` holds the `VideoSource`/`Controller` traits and `ControllerCommand`. `crates/video` normalizes to 240×160. `crates/controller` has `InputSchedule`. `crates/replay` handles sessions. `crates/telemetry` is the web UI (`web/index.html`).
- `crates/state`:
  - `Observation`: dialogue, menu, naming, battle HUD, player pose;
  - `GameEvent`, the pure reducer and `GameState`, with `Knowledge` provenance.
- `crates/vision` (`FireRedPerception`), rule-based detectors in `detect/`:
  - `dialogue` (message box, info page, ▼ arrow, settled-text rule), `menu` (▶ cursor), `main_menu`, `naming`, `title`;
  - `battle` (battle text box, battle ▶, HP bars);
  - `hud` (HUD font OCR, glyph table and dictionary resolution).
  - Localization through `pokebot_world::Localizer` with a pose hint.
- `crates/world`: map model, `Localizer` (render crops), `path` (A*, ledges, grass cost), `behavior`.
- `crates/gamedata`: species, moves, type chart, trainers, map trainers, wild tables, marts, items, plus Gen III mechanics (stats, exp, damage rolls, catch formula).
- `crates/planner`:
  - `evaluate` (exact per-matchup win probabilities, chained for trainer battles);
  - `prepare` (cheapest train/catch plan to reach a confidence target);
  - CLI: `pokebot plan --against LEADER_BROCK --party BULBASAUR:6:TACKLE,GROWL`.
- `crates/agent`:
  - `executor` (Task / Decision / Expectation loop);
  - `new_game`, `save` (SaveGame via the Start-menu wrap-around; Continue);
  - `nav` (`Navigator`; tile-level cross-map routing `route_from`; `static_obstacles`; talking across counters);
  - `battle` (menu driving, move choice with the evaluator, PP reserve, RUN rules);
  - `party` (knowledge from the HUD, learnset, PP);
  - `story` (`StoryStep`: Go, GoUntil, Talk, Challenge, Settle, Battle, Heal, Train, Prepare; milestones `opening()` and `to_brock()`; `all_milestones()`);
  - `progress` (`saves/progress.json`).
- `crates/runtime`: the bot loop (observe → events → state → telemetry and recording).
- `adapters/`: `emulator-libretro`, `capture-card` (V4L2), `pabotbase` (the PABotBase2 protocol client, plus a device peer), `virtual-console`.
- `apps/pokebot-cli`: `run`, `play`, `new-game`, `story`, `plan`, `inspect` (perception, HUD and `--world` localization on PNGs), `replay`, `emulator serve`.

## Useful techniques that worked
- **Explore unknown UI deterministically.** The in-process emulator is stepped and deterministic. Convert a recorded `controller.jsonl` into a `run --script` file (see the earlier `/tmp/to_bedroom.txt` approach) to reach any state quickly, then probe with `screenshot` steps.
- **Measure pixels** with `.venv/bin/python` + PIL/numpy (colour counters, `#` masks), then write rule-based detectors with ±20–24 tolerance. Test each new detector with `pokebot inspect`.
- **Learn font glyphs** by auto-labelling from known strings of distinct lengths (see `hud.rs`).
- **Renders must match the game's colours exactly** (the mGBA RGB565 path). Map weather changes palettes: `WEATHER_SHADE` scales 5-bit channels by 13/16. Other weathers are unhandled.

## Known gaps / what blocks the full story (prioritized)
1. **Text OCR is done for dialogue and battle text** (`vision::text`, font from the decompilation), along with move learning and evolution. Still to do: menu text (shops, bag, party, PC); see the global-state spec in `docs/superpowers/specs/`.
2. **Menu flows:**
   - party menu (switch, summary, use a field move);
   - bag (use items, teach TM/HM);
   - shop (buy N);
   - PC (deposit/withdraw);
   - move-learning prompt (choose which move to forget, via planner value);
   - evolution (allow).
3. **Catching:** BAG → Poké Ball in battle, weakening first (a status move or a false-swipe equivalent), tracking the party after a catch, and buying balls. The planner already produces `PlanStep::Catch`, but `StoryTask::plan` rejects it today.
4. **HMs and field moves:**
   - Cut (from the S.S. Anne), Flash (Rock Tunnel is dark, so localization needs Flash or a dark-mode matcher), Surf, Strength, optionally Fly;
   - an "HM user" in the party (a catch plus teaching);
   - using them: face the obstacle, then party menu or A → YES.
5. **Dynamic world state:** remove cut trees and boulders once cleared, doors unlocked by flags, and NPCs appearing or disappearing by story flags (`objects[].flag` in the map JSON). Add an event-sourced overlay on the static world, used by the navigator and the localizer.
6. **Puzzles:** Rocket Hideout spin tiles, Silph Co teleport pads, Seafoam boulders and currents, Victory Road boulders, Cinnabar Mansion switches, Vermilion Gym trash cans (random second can), Saffron Gym pads, Fuchsia Gym invisible walls. Each needs a small planner plus closed-loop verification.
7. **Story milestone data:**
   - Mt. Moon (fossil), Cerulean (Nugget Bridge, Bill, the rival), the S.S. Anne (Cut);
   - Vermilion, Rock Tunnel, Lavender (Pokémon Tower needs the Silph Scope from the Celadon Rocket Hideout), the Poké Flute, Snorlax;
   - Fuchsia (Safari Zone for Surf and Strength), Saffron (Silph Co, Tea for the gate guards), Cinnabar (Secret Key);
   - the FireRed One Island trip with Bill (verify against the decompilation's scripts whether it gates anything);
   - Viridian Gym, Victory Road, the Elite Four.
   - Use `data/pret-pokefirered/data/maps/*/scripts.inc` to derive triggers and flags. Derive "areas reachable" from story progress instead of listing them per milestone.
8. **Battles:**
   - switching, status moves in the evaluator, in-battle items (Potions/Revives), PP across long trainer chains;
   - multi-member parties (the evaluator already chains; the executor only uses the lead);
   - the Elite Four needs team planning plus money for items.
9. **Localization edge cases:** caves without Flash, fog/rain overlays, surfing/biking sprites, ice. Also a proper test that retry-on-faint actually works (it has never triggered in a successful run).

## Suggested next milestones (smallest useful vertical slices)
1. Dialogue-font OCR plus the move-learning prompt (BULBASAUR learns Poison Powder/Sleep Powder at 15, so this is needed soon).
2. Catching in the executor plus Poké Ball shopping, so `Prepare` can run catch plans. Then build a team for Misty (the planner will likely pick Vine Whip/levels or a catch).
3. Milestones: Route 3 → Mt. Moon (trainers, Zubat caves: check localization in `MtMoon_*`) → Cerulean → Misty. Make derived reachability part of this.
4. Dynamic world overlay, then Cut (S.S. Anne) → Lt. Surge (trash-can puzzle).
5. Global-state Phase 1 own audits (their own plans, not covered by the just-finished global-state task): the bag menu (use items, teach TM/HM) and its `ItemsChanged`/`PocketObserved` events; the party menu and Pokémon summary screen (switch, use a field move) feeding `PartyObserved`; money read from the overworld/mart HUD (`MoneyObserved`) rather than only from battle-win text; and the PC box UI (`BoxObserved`, `PcItemsObserved`, deposit/withdraw). Also the caught-icon and shiny detectors that emit `SpeciesCaught`/`ShinySeen` (the reducer already supports them).

Keep the working style: small vertical slices, test with deterministic replays and `pokebot inspect`, run live via `tools/live-run.sh`, verify with contact sheets, keep `cargo fmt`, `clippy` and the tests green, and update `docs/architecture.md`.
