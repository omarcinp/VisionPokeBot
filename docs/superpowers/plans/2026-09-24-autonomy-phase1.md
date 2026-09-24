# Autonomy — Phase 1 implementation plan

Spec: `docs/superpowers/specs/2026-09-24-autonomy-architecture-design.md`.
Branch: `autonomy` (worktree `VisionPokeBot-autonomy`, from `origin/main`).

Phase 1 builds the three foundations that don't depend on each other, so
they run as parallel work streams with **disjoint file ownership**. Phase 2
(planners, tools, execution loop) starts once A1 and B1 land, and the first
emulator runs follow.

Rules for every task:
- TDD: write the failing test, make it pass, refactor. No implementation
  without a test that exercises it.
- `cargo fmt`, `cargo clippy --workspace --all-targets`, `cargo test -p <crate>`
  green before reporting. Run the whole workspace tests once at the end.
- Deterministic: `BTreeMap`/sorted `Vec` everywhere iteration order can reach
  a decision. No randomness.
- Match the code style around you (comment density, naming, error types).
- Don't commit. Report what changed, what was verified and what is open.
- Python only under `tools/`; the runtime is Rust.
- Never touch `saves/`, `roms/`, `captures/fixtures/`.

---

## Stream A1 — Script compiler and world data (Python + loaders)

Owns: `tools/world/compile_events.py` (new), `tools/world/extract_world.py`
(objects only), `tools/world/build.sh`, `crates/world/src/events.rs` (new),
`crates/world/src/dialogue.rs` (new), `crates/world/src/places.rs` (new),
`crates/world/src/lib.rs` (exports + `World::load` loading the new files
when present), `crates/world/tests/events.rs` (new).

### A1.1 Front-end: parse the scripts into a control-flow graph
- Inputs: `data/pret-pokefirered/data/maps/*/scripts.inc`,
  `data/scripts/*.inc`, `data/event_scripts.s`, `include/constants/flags.h`,
  `vars.h`, `items.h`, `species.h`, `data/maps/*/map.json`.
- Parse labels (`Name::`), commands with args, `.string` text blocks.
  Resolve `.equ` aliases per file. Keep every command; the IR pass decides
  what is modelled.
- Test: a fixture `.inc` in `tools/world/tests/` (pytest, run with the
  `.venv`) parses into the expected label → command list.

### A1.2 IR: enumerate paths of conditions and effects
- From each entry label (object scripts, sign scripts, trigger scripts,
  map scripts `on_transition`/`on_frame`/`on_warp`), enumerate paths to
  `end`/`release`/`releaseall`/`return`-at-top. Follow `goto`, inline
  `call`, branch on `goto_if_set/unset`, `goto_if_eq/ne/lt/gt/le/ge`,
  `call_if_*`, `checkitem`+`VAR_RESULT`, `checkpartymove`+`VAR_RESULT`,
  `msgbox … MSGBOX_YESNO` + `VAR_RESULT` (YES/NO), `multichoice` (each
  option), `switch`/`case`. Cut loops after one iteration. Cap paths per
  script at 64 (mark `truncated: true` when hit).
- Effects: `setflag`, `clearflag`, `setvar`, `addvar`, `subvar`,
  `giveitem`/`giveitem_msg`/`additem`, `removeitem`, `givemon`, `giveegg`,
  `trainerbattle_single/double/rematch/no_intro/...` (→ `battle` with
  the trainer id, `intro`/`defeat` text labels; the path continues after the
  win), `warp`/`warpsilent`/`warpdoor`/`warphole`/`warpteleport` (→ `warp`),
  `special HealPlayerParty` (→ `heal`), `setrespawn` (→ `respawn`),
  `setobjectxyperm` (→ `move_object`), `addobject`/`removeobject`,
  `pokemart` (→ `mart` with the item list), `msgbox`/`message` (→ `say`),
  `setworldmapflag` (→ `set`). Unmodelled commands on a path: `opaque` list
  of names; the compiler prints a count per command name at the end.
- Text: expand `\n`, `\l`, `\p` to line/page breaks as the game prints
  them; placeholders (`{PLAYER}`, `{RIVAL}`, `{STR_VAR_1}`, …) become `*`
  wildcards.
- Output `data/world/events.json` as in spec §2.1 (`rom`, `scripts`,
  `map_scripts`, `triggers`, `objects`), `data/world/dialogue.json`
  (`labels: {label: [lines]}`, `index: {first_line: [labels]}`).
- Tests (pytest): the compiled `PewterCity_Gym_EventScript_Brock` has three
  paths matching spec §2.1; `PalletTown_EventScript_SignLady` has the
  `VAR_MAP_SCENE_PALLET_TOWN_SIGN_LADY` branches; the Cut tree script
  (`EventScript_CutTree`) has a `FLAG_BADGE02_GET` false path with only a
  `say`, and a true path with `checkpartymove` + YES/NO; `Route3`'s
  trainers all appear as `battle` effects.

### A1.3 Objects: movement areas
- `extract_world.py`: add `range_x`, `range_y` (from `movement_range_x/y`),
  keep `movement`. Re-run `tools/world/build.sh` (≈30 s) so `data/world/maps`
  gains the fields. `crates/world` `ObjectEvent` gets `range_x: i32`,
  `range_y: i32` with `#[serde(default)]` and a `fn area(&self) -> (x0, y0, x1, y1)`.
- Test in `crates/world`: PalletTown object 1 has range (1, 4).

### A1.4 Places
- `data/world/places.json`: `heal_spots` (from `src/data/heal_locations.h`
  or the `sHealLocations` table: map, x, y), `fly_spots` (`FLAG_WORLD_MAP_*`
  → map name and landing tile from the same table), `marts` (reuse
  gamedata), `gates`: every tile whose behaviour is a Cut tree / Strength
  boulder / Rock Smash rock object (`OBJ_EVENT_GFX_CUTTABLE_TREE`,
  `_BREAKABLE_ROCK`, `_PUSHABLE_BOULDER`) with map/x/y and the requirement
  (`{"move": "CUT", "badge": "FLAG_BADGE02_GET"}` etc.; the badge map is in
  `include/constants/` or `src/field_control_avatar.c`: Cut = Badge 2,
  Flash = 1, Rock Smash = 6, Strength = 4, Surf = 5, Fly = 3, Waterfall = 7),
  and `water`: per map, the count of water tiles (Surf regions are the
  water tiles themselves; the route planner reads them from the map).
- Test: Pewter's Pokémon Center is a heal spot; `FLAG_WORLD_MAP_PALLET_TOWN`
  lands on PalletTown; the Route 2 Cut tree exists.

### A1.5 Obtain table
- `data/world/obtain.json`: per species, a list of methods
  (`{"method": "wild", "map": …, "slot": "land|water|old_rod|good_rod|super_rod|rock_smash", "rate": …}`,
  `{"method": "gift", "script": …}`, `{"method": "trade", "give": …, "script": …}`
  from `src/data/ingame_trades.h`, `{"method": "evolve", "from": …, "how": "level|item|friendship|trade", "param": …}`
  (`needs_link: true` for trade evolutions), `{"method": "breed", "parents": [egg group ids], "baby_of": …}`
  from `src/data/pokemon/egg_moves.h` / `species_info.h` (egg groups,
  gender ratio, egg cycles), `{"method": "static", "script": …}` for
  `setwildbattle`/`StartLegendaryBattle`, `{"method": "prize", "script": …, "coins": …}`
  for Game Corner, `{"method": "fossil", "script": …}`).
- Species never obtainable here get `reasons: ["other_version" | "event_only" | "needs_link"]`.
- Test: every one of the 151 Kanto species has at least one method or a
  reason; RATTATA has a wild method on Route1; ALAKAZAM is `needs_link`.

### A1.6 Rust loaders
- `crates/world/src/events.rs`: serde types for `events.json`
  (`Condition`, `Effect`, `ScriptPath`, `Script`, `Events`) and
  `Events::load(dir)`; `crates/world/src/dialogue.rs`: `Dialogue::load`,
  `Dialogue::identify(lines: &[String]) -> Vec<&str>` (labels whose first
  line matches with `*` wildcards; exact match on later lines narrows);
  `crates/world/src/places.rs`. `World::load` keeps working when the files
  are missing (older data dirs): expose them as `Option` on `World` or as
  separate loaders; pick the simpler and say why.
- Tests: load the real `data/world` (skip when absent, like the existing
  ROM-backed tests), assert the Brock paths and that
  `identify(["This tree looks like it can be CUT", "down!"])` contains
  `Text_TreeCanBeCutDown`.

### A1.7 Build integration
- `tools/world/build.sh` runs `compile_events.py`; `README`/`docs/architecture.md`
  gain a paragraph on the compiled data. Print the unmodelled-command
  report at the end of the build.

---

## Stream B1 — World belief (crates/state)

Owns: `crates/state/src/belief.rs` (new), `crates/state/src/events.rs`
(new variants only), `crates/state/src/reducer.rs` / `reduce_knowledge.rs`
(new arms only), `crates/state/src/state.rs` (`world: WorldBelief` field +
`SavedKnowledge`), `crates/state/src/inference.rs` (new), `data/world/inference.json`
(new), `data/world/priors.json` (new), `crates/agent/src/checkpoint.rs`
(persist/restore the belief; small change).

### B1.1 Model
```rust
pub struct WorldBelief {
    pub flags: BTreeMap<String, Knowledge<bool>>,
    pub vars: BTreeMap<String, Knowledge<u16>>,
    pub visited: BTreeMap<String, Knowledge<bool>>,
    pub respawn: Knowledge<HealSpot>,           // { map, x, y }
    pub npcs: BTreeMap<(String, u32), NpcBelief>, // (map, local_id) → { pos: Knowledge<(i32,i32)>, present: Knowledge<bool> }
}
```
Helper `flag(&self, name) -> Knowledge<bool>` returning `unknown()` when
absent. `WorldBelief` is in `SavedKnowledge` (missing → default) so the
checkpoint round-trips it.

### B1.2 Events
`FlagObserved { flag, value }`, `FlagTracked { flag, value }`,
`VarObserved { var, value }`, `VarTracked { var, value }`,
`MapVisited { map }`, `RespawnSet { map, x, y }`,
`NpcSeen { map, local_id, x, y, facing }`, `NpcAbsent { map, local_id }`,
`ScriptPathRun { script, path: usize }` (the agent ran a compiled path to
completion; the reducer applies nothing itself — the agent emits the
`FlagTracked`/`VarTracked`/`ItemsChanged` that the path's effects imply,
keeping the reducer free of world data), `IntentInfeasible { intent }`
(session-scoped, provenance Derived, cleared by `CheckpointRestored`).
Reducer arms are pure; `Observed` overwrites, `Tracked` sets `Tracked`
unless the current value is `Observed` with the same value at a later
frame. Property: replaying `events.jsonl` rebuilds the belief.

### B1.3 Inference
`data/world/inference.json`: list of `{ "if": {"flag"|"var"|"visited": …, "is"/"eq"/"ge": …}, "then": {...} }`.
`inference::apply(&mut WorldBelief, rules)` runs to a fixed point, only
from `Observed` premises, writing `Derived` conclusions that never overwrite
`Observed`. Seed rules: each `FLAG_BADGE0N_GET` ⇒ `FLAG_DEFEATED_<leader>`;
`FLAG_SYS_POKEDEX_GET` ⇒ `FLAG_GOT_POKEDEX`-style pairs where the scripts
set them together (B1 writes the obvious ones by hand; A2 will generate
more from the compiled paths later).

### B1.4 Priors
`data/world/priors.json`: rules `{ "fact": {"flag": …}, "given": [premises], "p": 0.9 }`
and `priors::probability(belief, fact) -> f64` (most specific matching rule
wins; ties → lowest p; no rule → 0.5). Seed: `party_has_move(FLY)` is not
a belief fact, so priors here are over flags/visited only; seed the badge
→ `visited` rules for the towns on the only route to each badge (write the
list by hand from the map graph: Pallet, Viridian, Pewter for Badge 1;
+Route 4, Cerulean for Badge 2; …).

### B1.5 Bootstrap hooks
`GameState::world` reachable from `Party::from_state`-style views; add
`WorldBelief::needs(&self, facts) -> Vec<Fact>` returning the unknown ones,
which the planner will turn into probes. Tests for everything above;
a replay test using an existing `events.jsonl` in `captures/` if one is
present (skip otherwise).

---

## Stream D1 — Motion: syncer and walker (crates/agent)

Owns: `crates/agent/src/motion/` (new: `mod.rs`, `syncer.rs`, `walker.rs`),
`crates/agent/src/nav.rs` (only to delegate hold lengths/timeouts to the
syncer and to consume `Walker` where it walks), `crates/agent/src/executor.rs`
(replace `latency_frames` with a `Syncer` handle; keep behaviour identical
when the syncer has no samples), `apps/pokebot-cli` (a `--timing <file>`
option defaulting to `saves/timing.json`, per device profile), telemetry
(publish the timing model in the status snapshot; `web/index.html` shows
latency/duration per input kind in the existing status area).

### D1.1 Syncer
```rust
pub enum InputKind { WalkTile, Turn, MenuPress, WarpFade, BattleStart }
pub struct Estimate { pub latency_ms: f64, pub unit_ms: f64, pub spread_ms: f64, pub samples: u32 }
pub struct Syncer { profile: String, estimates: BTreeMap<InputKind, Estimate> }
impl Syncer {
    pub fn hold_for(&self, kind: InputKind, units: usize) -> Duration; // latency + n·unit − unit/2 + 2·spread; defaults = today's constants
    pub fn timeout_frames(&self, kind: InputKind, units: usize) -> u64;
    pub fn observe(&mut self, kind: InputKind, issued: Instant, first_effect: Instant, done: Instant, units: usize);
    pub fn load(path, profile) / save(path)
}
```
EWMA α = 0.2; spread = mean absolute deviation over the last 16 samples.
Defaults reproduce `TILE_MS = 268`, `STEP_TIMEOUT`, `WARP_TIMEOUT` exactly
(test). Persisted per profile (`emulator`, `switch`) in one JSON file.

### D1.2 Samples from the executor
The executor already knows the issue frame and the confirmation frame of
every action; give `Action` an optional `timing: Option<(InputKind, usize)>`
and have the executor call `syncer.observe` on `Confirmed`. The walker's
hold actions set it. Use `captured_at` of frames for wall-clock time.

### D1.3 Walker with predictive tracking
Move the hold/tap logic of `Navigator::walk` into `motion::Walker`
(`next(&Observation, &Syncer) -> WalkStep { Hold{dir, tiles}, Tap{dir}, Cancel, Arrived, Blocked{tile} }`).
During a hold the walker predicts `tiles_done` from elapsed time and the
estimate; if the located pose lags the prediction by > 1 tile for longer
than `spread` (min 100 ms) it returns `Cancel` (the caller sends `Neutral`),
then taps to learn the blocker as today. `Navigator` keeps its public API
(`next`, `on_outcome`, `stalled`) so `story.rs` doesn't change.
NPC-aware local replanning is D2 (needs the NPC detector).

### D1.4 Verification
- Unit tests for the estimator (convergence on synthetic samples, defaults,
  persistence round-trip) and the walker (a scripted sequence of
  observations: hold issued, poses arriving late → `Cancel`).
- `crates/agent/tests/routing.rs` and `milestones.rs` still pass.
- Emulator: `tools/live-run.sh --instance emu /tmp/d1.log story --continue --save-game --until CrossRoute3 --record /tmp/d1-run` (against `pokebot emulator serve`; see README). Report the learned estimates from `saves/timing.json` and that the run reached the milestone.

---

## Phase 2 (after A1 + B1): planners, tools, goal loop — separate plan.
