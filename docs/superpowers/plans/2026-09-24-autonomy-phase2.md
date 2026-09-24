# Autonomy — Phase 2 implementation plan

Spec: `docs/superpowers/specs/2026-09-24-autonomy-architecture-design.md`.
Prerequisites: Phase 1 (A1 data + loaders, B1 belief, D1 syncer/walker) on
branch `autonomy`. Same rules as Phase 1 (TDD, deterministic, disjoint
ownership, no commits by the stream agents).

Goal of the phase: `pokebot goal "catch RATTATA" --dry-run` prints a plan
from a checkpoint, and `pokebot goal "catch RATTATA"` executes it on the
emulator through tools, with interrupts handled. Campaign scheduling and the
NPC detector are Phase 3.

---

## Stream C1 — Route planner (crates/world)

Owns: `crates/world/src/route.rs` (new), `crates/world/src/predicate.rs`
(new), `crates/world/tests/route.rs` (new).

### C1.1 Predicates and requirements
```rust
pub enum Predicate { Flag{name, is: bool}, Var{name, op, value}, Visited{map}, HasItem{item, n},
                     PartyHasMove{mv}, Badge{n}, At{map} }
pub enum Truth { True, False, Unknown }
pub trait BeliefView { fn eval(&self, p: &Predicate) -> Truth; }   // implemented in crates/agent over GameState
```
`Requirement = Vec<Predicate>` (conjunction). `crates/world` must not
depend on `crates/state`; the agent adapts `GameState` to `BeliefView`.

### C1.2 Place graph
Nodes: `Place { map, x, y, kind }` for warps, connection crossings, fly
landings, heal spots, gates (Cut/Strength/Rock Smash objects from
`places.json`), and caller-supplied tiles. Intra-map edges: A\* distance
from `path.rs` with static obstacles (`nav::static_obstacles` moves here
as `world::obstacles`), cached per `(map, from, to)`; NPC wander areas
(`ObjectEvent::area`) cost +2 per tile. Inter-map edges with requirements
and costs (spec §5.1), read from map data, `places.json` and `events.json`
(script warps: requirement = the path's `when`). Fly edges from any outdoor
place to each fly spot: `[PartyHasMove(FLY), Badge(3), Visited(dest)]`.
Surf: water tiles walkable when `[PartyHasMove(SURF), Badge(5)]`.

### C1.3 Search
`route(world, graph, belief, from: Pose, to: Place, policy: UnknownPolicy) -> RouteResult`
where `UnknownPolicy` is `Pessimistic | Optimistic { penalty_of: fn(&Predicate)->f64 }`.
A\* over places; edges whose requirement is `False` are skipped but the best
blocked alternative is returned: `RouteResult { legs: Vec<Leg>, cost_s: f64,
assumes: Vec<Predicate>, blocked: Vec<(Vec<Predicate>, f64)> }`. Tie-break
by (cost, place name, edge kind). Costs in seconds: tiles × 0.268 (the
syncer's estimate injected as a parameter), warp fade 1.5 s, Fly 12 s.

### C1.4 Tests
- Pewter PC → Route 2 south entrance: legs are walk/warp/connection only.
- Cinnabar → Pallet with Fly available and Pallet visited: first leg is Fly.
- Same with `Visited(PalletTown) = Unknown`, `Optimistic`: Fly chosen with
  `assumes = [Visited(PalletTown)]`; `Pessimistic`: Surf route, Fly listed
  in `blocked`.
- A Cut tree between two tiles is not crossed without Badge 2 + CUT and is
  listed in `blocked` with that requirement.

---

## Stream C2 — Goal planner and `pokebot goal --dry-run` (crates/planner, CLI)

Owns: `crates/planner/src/goals.rs`, `intents.rs`, `methods.rs` (new),
`data/rules/methods.json` (new), `apps/pokebot-cli/src/goal.rs` (new) and
the `Goal` subcommand in `main.rs`.

### C2.1 Intents (spec §4.1)
`Intent` enum with `preconditions() -> Vec<Predicate>`, `effects() -> Vec<Effect>`,
`cost_s(ctx) -> f64`. Sources: `Events` (RunScript per compiled path),
`obtain.json` (Catch/Gift/Trade/Evolve/Breed), marts (Buy), heal spots
(Heal), trainers (Beat, using `planner::evaluate` for P(win); below 0.9
expands via `prepare.rs` into Train/Catch), `Probe { fact }` (cost table:
trainer card 8 s, bag pocket 10 s, fly map 8 s, pokedex 15 s).

### C2.2 Search
Backward-chaining A\* over open predicates; `At{map}` priced by C1
`route(...)`. Unknown preconditions per spec §4.3 with `priors::probability`
and `--expensive-secs` (600). Depth limit 6, node budget 20 000. Methods
(`methods.json`) tried first for `GetHM`, `GetBadge`, `TeachHM`,
`StockBalls`. Output `Plan { intents, assumes, cost_s, belief_snapshot }`,
serialisable; identical output for identical input (test by planning twice).

### C2.3 CLI
`pokebot goal <goal> [--dry-run] [--state saves/state.json] [--progress …] [--rules data/rules] [--expensive-secs N]`.
Goal grammar (small, explicit): `catch SPECIES`, `flag FLAG_NAME`,
`badge N`, `at MAP`, `item ITEM N`. `--dry-run` prints the plan as a table
and the assumptions; no devices opened. Tests: the §9.1 Rattata plan from
a fixture `state.json` (write one under `crates/planner/tests/fixtures/`
with IVYSAUR Lv 18, Pewter, balls unknown) yields
`[Probe(balls), Go(Route2 grass) …, Catch]` or `[… Buy …]` depending on the
fixture's money; "win the League" (`flag FLAG_SYS_GAME_CLEAR`) expands to
the eight gyms with HM subgoals in a deterministic order.

---

## Stream D2 — Tools, interrupts, dialogue recognition, goal loop (crates/agent)

Owns: `crates/agent/src/tools/` (new: `mod.rs`, `context.rs`, `go.rs`,
`talk.rs`, `dialogue.rs`, `heal.rs`, `battle.rs`, `catch.rs`, `buy.rs`,
`probe.rs`, `save.rs`, `unstick.rs`), `crates/agent/src/goal.rs` (new:
the §8 loop), `crates/agent/src/belief_view.rs` (new: `GameState` →
`BeliefView`), `apps/pokebot-cli/src/goal.rs` (execution half). Reuses
`story.rs` internals by extracting shared pieces into functions where
needed — but `story.rs` keeps working (the milestones are the regression
suite).

### D2.1 Tool contract (spec §7.1) and `ToolContext`
`ToolContext` wraps the runtime + executor + syncer + world data + belief;
`act(action) -> Outcome` runs one action through the executor's single-step
path (factor the executor loop so one action can be run to its outcome;
keep `Executor::run` for tasks). `invoke(intent) -> ToolOutcome` dispatches
to the tool that `serves` it. Every `ToolOutcome.learned` event goes
through `runtime.emit`.

### D2.2 Interrupt wrapper (spec §7.3)
`ctx.act` checks the observation after each outcome: battle box → run the
battle tool (wild: existing catch policy → fight/flee; trainer: fight),
return `Interrupted`; unexpected dialogue → dialogue tool with recognition;
outside game → existing recovery. Tools mark actions `interruptible: false`
inside menus.

### D2.3 Dialogue recognition and `Unstick` (spec §7.4)
`tools::dialogue::identify(observation, &Dialogue) -> Vec<label>` via
`Dialogue::identify`; map label → script paths through `Events`; answer
YES/NO from the plan's `answers` or the branch policy in §7.4 step 2;
generic fallback in step 4; log `unknown_dialogue` with the frame to
`captures/unknown/`.

### D2.4 Tools
Port in this order, each verified on the emulator via the corresponding
`StoryStep` still passing: `Go` (Navigator + Walker), `Talk`, `Dialogue`,
`Heal`, `Battle`, `Catch`, `Buy`, `Probe` (bag pocket audit exists;
trainer card and fly map readers are B2 — stub as `Unsupported` for now),
`Save`, `Unstick`.

### D2.5 Goal loop (spec §8)
`goal::run(goal, ctx) -> Result`: plan → run intents → apply learned →
replan on failure/contradiction → save after belief-changing intents;
execution-monitoring rules (contradiction, plan-level loops, motion loop,
progress audit). `plan.jsonl` in the session dir; telemetry shows the
current intent/leg.

### D2.6 Emulator verification
From the Pewter checkpoint: `tools/live-run.sh --instance emu /tmp/goal.log goal "catch RATTATA" --video capture-card:/dev/video10 --viewport 0,0,720,480 --controller pabotbase:/tmp/pokebot-esp32 --state saves/… --record /tmp/goal-run`.
Pass = `SpeciesCaught { RATTATA }` observed and `state.json` written.
Then `goal "at CeruleanCity"` from the same checkpoint (Route 3 → Mt. Moon
→ Route 4), which exercises interrupts (trainers, wild battles).

---

## Stream B2 — Screen readers for probes (crates/vision + agent)

Owns: `crates/vision/src/detect/trainer_card.rs`, `fly_map.rs`,
`pokedex.rs` (new), fixtures under `captures/fixtures/`, and the
`Probe` handlers in `crates/agent/src/tools/probe.rs` for those three.
Fixtures: capture with the emulator (`pokebot run --hold --record` and the
menu), one frame each; readers use `vision::text` and colour rules like the
existing detectors. Emits `FlagObserved(FLAG_BADGE0N_GET)`,
`MapVisited`/`visited=false`, `SpeciesCaught`.

---

## Phase 3: campaign scheduler, NPC detector, Puzzle tool,
party/PC tools, Archipelago oracle test — separate plan.
