# Autonomy architecture — design

Status: draft for review, 2026-09-24. Builds on the global-state design
(`2026-09-23-global-state-design.md`) and the catching design
(`2026-09-24-cascade-badge-and-catching-design.md`).

## Goal

Give the bot a goal ("catch a RATTATA", "win the League") and let it work out
the rest: what has to happen first, where to go, how to get there with what
it has (walking, Fly, Surf, Escape Rope, …), how to fix a wrong assumption
when the game says no, and how to keep walking accurately while the game's
timing drifts and NPCs move. The hand-written milestones in `story.rs`
become regression scripts; the bot stops depending on them.

The ground rules do not change: vision and controller only, every action
closed-loop with an expectation, deterministic decisions, `Unknown` is a
legitimate value, no ROM data in git.

## Decisions taken

| Question | Decision |
|---|---|
| Where the world/event/dialogue tables come from | Compiled offline from the decompilation's map scripts into JSON; a hand-edited overlay patches errors and version differences; both hot-reload |
| Delivery | One design, four work streams in parallel (§10) |
| Today's `StoryStep` milestones | Plans compile into the tool layer, which subsumes the steps. The milestones stay as regression scripts, then retire |
| Story flags the bot can't read | Modelled as a **belief** with the existing `Knowledge` provenance; the planner reasons over the belief and buys certainty with probes |
| Battle safety | Unchanged: P(win) ≥ 90 % per trainer battle; no Pokémon may faint |

## 0. Prior art and what it says about this design

Checked on 2026-09-24 against the published Pokémon agents and the game-AI
planning literature (links in §12). The findings that shaped the spec:

| Source | Finding | Consequence here |
|---|---|---|
| PokéAgent Challenge, NeurIPS 2025 (Emerald speedrun track, 100+ teams) | Raw frontier VLMs and CLI-agent harnesses scored ~0 %. Every finisher used deterministic tools (A\*, scripted battle handlers, dialogue detection, NPC avoidance) with the model only decomposing milestones. Agents "struggle with basic localization, action-distance estimation and objective detection" and "make erroneous assumptions about their game progress". The winners read coordinates and flags from RAM. | Confirms tools-first with no model in the loop. Our render-based localizer and the syncer's distance model address the two named weaknesses. The belief layer (§3) is our substitute for the RAM flags the winners had. |
| Gemini Plays Pokémon (harness write-up) | The harness, not the model, carried the run. Hallucinated facts (phantom PP, wrong item effects) were only fixed by injecting ground truth. A fixed primary/secondary/tertiary goal stack stopped "goal flip-flopping". Deterministic mechanics (spinner tiles) had to be precomputed into the map. Boulder puzzles and mazes needed a dedicated solver. | Belief provenance with `Observed` overriding memory (§3). Goal hysteresis in the campaign (§4.5). Spinners, currents and Cycling Road as precomputed edges, and a Sokoban solver tool (§5.1, §7.2). |
| Claude Plays Pokémon analyses | The bottleneck is "pixels → spatial map"; 24–78 h stuck in Mt. Moon; beliefs kept despite contradicting evidence; loops not recognised as loops. | Localization against renders is the core asset. Contradiction rule and plan-level loop detection (§8). |
| PokeRL / Pokémon Red via RL | Five failure modes of unconstrained agents: action loops, reward exploits, button spam, spinning in place, revisiting explored areas. | All are execution-monitoring problems; §8 handles them in the executor and campaign rather than in a policy. |
| Continual Harness (PokéAgent track 2) | Navigation skills converge to a Dijkstra oracle; the residual gap is "dialogue-heavy areas and multi-turn battles". | Exactly where compiled dialogue (§2.1, §7.4) and the exact evaluator give us a structural edge. |
| Archipelago's FireRed/LeafGreen randomizer world | A hand-maintained reachability model of this exact game: ~400 KB of region JSON, entrance rules (`CanSurf`, `Fly Unlock (town)`, island passes, Card Key, Seafoam currents, Route 12 boulders, Mansion switches), events modelled as items ("Defeat Brock"), evolutions as locations gated by gym count. It proves seeds beatable, so it is complete. | Direct validation of the predicate-gated place graph (§5). Also a **test oracle**: our compiled graph must reproduce its reachability under the vanilla order (§10). Its sibling worlds (Emerald, HG/SS, Platinum) share the rule shapes: the engine is portable, the graph is per game. |
| Speedrun routing (RouteThree/RouteThreeFour; "Automating Speedrun Routing") | Community tools only *evaluate* hand-written routes; full routing is NP-hard (knapsack + feedback arc set generalisations). | Greedy campaign scheduling with unlock value (§4.5) is the right compromise; community routes serve as time-estimate priors and benchmarks. |
| Game AI planning (GOAP in F.E.A.R., HTN in Killzone 2 / Decima / Dying Light) | Industry moved from flat backward-chaining (GOAP) to HTN for predictability and tractability; HTN with backtracking over preconditions and execution monitoring is the mature form. | Hybrid: backward chaining over predicates for goals (§4), HTN-style fixed decompositions inside tools (§7.1), and HTN **methods** for the common compound intents to bound the search (§4.2). |
| Contingent / belief-space planning; dead reckoning in networked games | Sensing actions priced by expected cost; execution over belief states; predict-then-correct for latency. | §4.3 and §6.2 are standard forms of these; nothing novel is needed. |
| Voyager | Skills as code plus an automatic curriculum beat every prompt-only baseline by an order of magnitude. | Our tools are the skill library (hand-written, deterministic) and the campaign is the curriculum. |

No published system plays vision-only with a symbolic belief over the
game's own flags compiled from the decompilation; the closest are the
PokéAgent winners (RAM flags, VLM perception) and Archipelago (hand-written
graph, no agent). The combination is the contribution, and the risks are
the two pieces nobody has validated: the script compiler's coverage (§2.1)
and belief inference from screen evidence (§3.2). Both have oracles here:
Archipelago's graph for the first, the recorded sessions for the second.

## 1. The layers

```
                    ┌───────────────────────────── DATA (hot-loaded JSON) ─────────────────────────────┐
                    │ maps, tiles, warps, connections      (existing, data/world/maps)                  │
                    │ events.json    scripts as condition → effect graphs, per NPC/sign/trigger/map     │
                    │ dialogue.json  every text string → (script, line index, branch)                   │
                    │ places.json    fly spots, heal spots, marts, gates (Cut trees, boulders, water)    │
                    │ gamedata.json  species, moves, trainers, wild tables, items (existing)             │
                    │ overrides/*.json  hand patches, keyed by ROM sha1                                  │
                    └──────────────────────────────────────────────────────────────────────────────────┘
                                                        │
   ┌─────────── BELIEF (crates/state) ───────────┐      │
   │ GameState + WorldBelief:                     │◀─────┘  static facts
   │   flags, vars, visited maps, party, bag,     │
   │   money, PC, pose — each Knowledge<T>         │◀──── events from tools, dialogue, HUD
   └──────────────────────────────────────────────┘
                     │ read                          ▲ learned facts (also from failures)
                     ▼                               │
   ┌── GOAL PLANNER ──┐  intents   ┌── ROUTE PLANNER ──┐  legs   ┌── TOOLS ──┐  actions  ┌── MOTION + SYNCER ──┐
   │ symbolic A* over │──────────▶│ place graph with   │────────▶│ Talk, Go, │─────────▶│ holds/taps, latency  │
   │ predicates:      │           │ requirement-gated  │         │ Fly, Surf,│          │ model, predictive     │
   │ "caught(RATTATA)"│◀──────────│ edges (walk, warp, │◀────────│ Buy, Heal,│◀─────────│ tracking, local       │
   │ backward-chains  │  subgoals │ fly, surf, rope…)  │  facts  │ Catch, …  │  outcome │ replanning            │
   └──────────────────┘           └────────────────────┘         └───────────┘          └──────────────────────┘
                                                                        │
                                                          ┌── INTERRUPTS (wrapper on every tool) ──┐
                                                          │ wild/trainer battle, unexpected dialogue,│
                                                          │ HOME menu, level-up, evolution           │
                                                          └──────────────────────────────────────────┘
```

What changed compared with the premise in the brief:

- **Tables are compiled, not authored.** The decompilation already holds the
  full event graph (2 692 `msgbox`, 342 `setflag`, 848 `setvar`, ~300
  conditional jumps, 412 trainer battles, 21 script warps). The overlay is
  for corrections, not for content.
- **A belief layer sits between data and planners.** The bot never reads
  flags, so every planner precondition is evaluated against `Knowledge<bool>`,
  and `Unknown` is a first-class answer that costs something to resolve.
- **Tools return facts, not just success/failure.** "Fly to Cerulean" failing
  because the town is dark on the map is an observation
  (`FLAG_WORLD_MAP_CERULEAN_CITY = false`, `Observed`), and it is what makes
  the planner replan through Surf. This closes the loop the brief asks for.
- **Interrupts are one wrapper, not a concern of every plan.** A trainer
  spotting the player mid-leg is handled once, below the tools.
- **Dialogue recognition replaces most "unknown conversation" handling.**
  With every string extracted, reading a page with the font identifies the
  script and the branch; the generic unstick tool is only for what's left.

## 2. Data

### 2.1 Script compiler (`tools/world/compile_events.py`)

Input: `data/maps/*/scripts.inc`, `data/scripts/*.inc`,
`data/event_scripts.s`, `data/maps/*/map.json`, `include/constants/{flags,vars,items,species}.h`.

Method: parse each script label into a control-flow graph of the commands
we model (below), then **enumerate the paths** from the entry label to
`end`/`release`, collecting on each path the conditions tested
(`goto_if_set/unset`, `goto_if_eq/ne/lt/ge` on vars, `checkitem`,
`checkpartymove`, `VAR_RESULT` from a YES/NO box) and the effects
(`setflag`, `clearflag`, `setvar`, `addvar`, `giveitem`, `removeitem`,
`trainerbattle*`, `warp*`, `special HealPlayerParty`, `setobjectxyperm`,
`addobject`/`removeobject`, `givemon`). Every `msgbox` on the path is kept
in order with its branch position. Paths through unmodelled commands are
kept with an `opaque: true` marker so the planner can still see the visible
branches and the overlay can complete them.

Loops are cut after one iteration; `call`/`return` are inlined; `goto` to a
shared label is followed. `trainerbattle_single T, intro, defeat, after` is
expanded into: precondition `not defeated(T)`, effect `defeated(T)` plus the
battle itself, then the `after` label. `set_gym_trainers`, `famechecker`,
`questlog` commands are ignored.

Output (`data/world/events.json`):

```json
{
  "rom": "firered_rev1",
  "scripts": {
    "PewterCity_Gym_EventScript_Brock": {
      "kind": "object", "map": "PewterCity_Gym", "local_id": 1,
      "paths": [
        { "when": [{"flag": "FLAG_DEFEATED_BROCK", "is": false}],
          "does": [
            {"battle": "TRAINER_LEADER_BROCK", "intro": "PewterCity_Gym_Text_BrockIntro"},
            {"set": "FLAG_DEFEATED_BROCK"}, {"set": "FLAG_BADGE01_GET"},
            {"var": "VAR_MAP_SCENE_PEWTER_CITY", "eq": 1},
            {"set": "FLAG_HIDE_PEWTER_CITY_GYM_GUIDE"},
            {"clear": "FLAG_HIDE_PEWTER_CITY_RUNNING_SHOES_GUY"},
            {"say": "PewterCity_Gym_Text_TakeThisWithYou"},
            {"give": "ITEM_TM39", "count": 1}, {"set": "FLAG_GOT_TM39_FROM_BROCK"},
            {"say": "PewterCity_Gym_Text_ExplainTM39"}
          ]},
        { "when": [{"flag": "FLAG_DEFEATED_BROCK", "is": true},
                   {"flag": "FLAG_GOT_TM39_FROM_BROCK", "is": false}],
          "does": [ "...give TM39..." ] },
        { "when": [{"flag": "FLAG_DEFEATED_BROCK", "is": true},
                   {"flag": "FLAG_GOT_TM39_FROM_BROCK", "is": true}],
          "does": [{"say": "PewterCity_Gym_Text_BrockPostBattle"}] }
      ]
    }
  },
  "map_scripts": { "PalletTown": { "on_transition": [...], "on_frame": [...] } },
  "triggers":     [ {"map": "PalletTown", "x": 12, "y": 1, "when": [...], "script": "..."} ],
  "objects":      [ {"map": "PalletTown", "local_id": 1, "hidden_by": "FLAG_...", "moves": "WANDER_AROUND", "script": "..."} ]
}
```

`dialogue.json` maps every `Text_*` label to its lines as the game prints
them (after `\n`, `\p`, `\l`, placeholders such as `{PLAYER}` kept as
wildcards), plus an inverted index from the first line's text to the labels
that start with it. `places.json` lists heal spots (from `sHealLocations` /
`setrespawn`), fly destinations (`FLAG_WORLD_MAP_*` → map and landing tile),
marts (existing), and **gates**: every Cut tree, Strength boulder, Rock
Smash rock, water tile region, and one-way ledge, with the requirement to
pass it.

`objects` entries also carry each NPC's **movement area**: `map.json` gives
`movement_type` and `movement_range_x/y`, so a wanderer's reachable
rectangle is static data (224 wandering NPCs in the game; most NPCs have a
one-tile area). The route planner treats the whole area as "may be
occupied"; the motion controller (§6.3) resolves it on arrival.

`obtain.json` lists, per species, every way to obtain it on this ROM, each
with its preconditions, compiled from the same sources:

| Method | Compiled from |
|---|---|
| wild: land, water (Surf), Old/Good/Super Rod, Rock Smash | `wild_encounters.json`, all slot types |
| gift (`givemon`, `giveegg`), fossil revival | scripts |
| in-game trade | `data/ingame_trades.json` |
| Game Corner prize | prize scripts and coin prices |
| evolution: level, stone, happiness, trade (marked `needs_link`) | species evolutions |
| breeding: egg groups, gender ratio, egg cycles, baby forms | `pokemon.c` tables |
| static / legendary encounter | `StartLegendaryBattle` and `setwildbattle` in scripts |

Methods whose preconditions can never hold on one console (`needs_link`,
event tickets, the other version's exclusives) are kept in the table with
that reason, so the campaign report (§4.5) can name it.

### 2.1.1 Portability: one IR, one front-end per game

The compiler is split into a **front-end** that understands one
decompilation's script dialect (pret's `pokefirered`; `pokeemerald` and
`pokeruby` use the same command set, `pokecrystal` and `pokered` a
different but equivalent one) and a game-agnostic **intermediate
representation**: the `paths` of `when`/`does` above, the place graph, the
dialogue index and the obtain table. Everything downstream (belief,
planners, campaign, tools' contracts) consumes only the IR. Moving to
another game means a new front-end, new vision detectors (fonts, colours,
HUD geometry), a new `gamedata` mechanics module for its generation, and a
new overlay; the planners and the executor do not change. Archipelago's
per-game worlds show the same split working in practice.

### 2.2 Overlay and hot reload

`data/overrides/<rom>.json` uses the same shapes and is applied by key
(script name, map/x/y for triggers) with `replace` or `patch` semantics. A
`WorldData` handle reloads all JSON when any file's mtime changes, between
tool invocations (never mid-action), and logs which keys changed. The
planners hold an `Arc<WorldData>` snapshot per plan; a reload invalidates
the current plan and triggers a replan.

### 2.3 Versioning

Every compiled file carries the ROM's sha1. Loading data compiled for a
different ROM than the overlay targets is an error, not a warning.

## 3. Belief

### 3.1 Model (`crates/state`)

```rust
GameState {
    // …existing party/bag/money/pc/pokedex/pose…
    world: WorldBelief,
}
WorldBelief {
    flags:   BTreeMap<FlagId, Knowledge<bool>>,   // absent = Unknown
    vars:    BTreeMap<VarId,  Knowledge<u16>>,
    visited: BTreeMap<MapId,  Knowledge<bool>>,   // FLAG_WORLD_MAP_*, kept separately for Fly
    respawn: Knowledge<HealSpot>,                 // where a blackout/Teleport lands
    repel:   Knowledge<u16>,                       // steps left
}
```

### 3.2 Sources of belief

| Source | Provenance | Example |
|---|---|---|
| A tool ran a script path to completion (dialogue matched to the path's lines) | `Tracked` for effects, upgraded to `Observed` when a later screen shows them | Brock's post-battle text → `FLAG_BADGE01_GET` |
| Dialogue recognised on screen whose branch condition implies flag values | `Observed` | Gym guide says "you're champ material" → `FLAG_DEFEATED_BROCK = true` |
| Trainer card badges | `Observed` | `FLAG_BADGE0N_GET` |
| Fly map: lit towns | `Observed` | `visited[CeruleanCity] = false` |
| NPC present/absent at its scripted tile | `Observed` | `FLAG_HIDE_*` |
| Map transition seen | `Observed` | `visited[map] = true` |
| Checkpoint `state.json` | as saved | restored with its identity check |
| Nothing | `Unknown` | everything else |

Rules of inference (belief → belief) are declarative and live in
`data/world/inference.json` so they can be extended without code:
`{"if": {"flag": "FLAG_BADGE01_GET", "is": true}, "then": {"flag": "FLAG_DEFEATED_BROCK", "is": true}}`.
Only `Observed` facts propagate; derived facts are marked `Derived`.

### 3.3 State capture at start

`pokebot goal … --continue` runs a **bootstrap** sequence before planning,
each step a tool with its own expectations:

1. Locate the player (no menu).
2. Restore `state.json` if its identity matches `progress.json`.
3. If flags needed by the goal are `Unknown`, and only then: open the
   Trainer Card (badges), the party menu (species/level/HP), the Bag pockets
   that matter, and the Fly map if Fly is available. Each is a probe (§4.3),
   so the planner decides which are worth it; nothing is opened "just in
   case".

Every event since then updates the belief, and every in-game save writes
`state.json` as today.

## 4. Goal planner (`crates/planner::goals`)

### 4.1 Vocabulary

A **predicate** is a test on the belief: `flag(F) = v`, `var(V) op n`,
`has_item(I, ≥ n)`, `party_has_move(M)`, `party_can_beat(T, p ≥ 0.9)`,
`caught(S)`, `at(map)`, `money ≥ n`, `badge_count ≥ n`.

An **intent** is something a tool can carry out, with a precondition set, an
effect set, and a cost estimate in seconds:

| Intent | Comes from | Precondition | Effect |
|---|---|---|---|
| `RunScript { script, path }` | `events.json` | the path's `when` + `at(map)` reachable | the path's `does` |
| `Beat { trainer }` | trainer battles in scripts | `party_can_beat(trainer)` | `defeated(trainer)` |
| `Catch { species }` | wild tables | at an area with the species, balls ≥ n, risk ≤ 2 % | `caught(species)` |
| `Train { area, level }` | readiness planner | at area | party level ≥ level |
| `Buy { item, n }` | marts | at mart, money ≥ price·n | `has_item` |
| `Heal` | places | at heal spot | party HP full |
| `Teach { hm, mon }` | items | has HM, mon can learn | `party_has_move` |
| `Probe { fact }` | §4.3 | — | `fact` becomes `Observed` |

### 4.2 Search

Backward chaining with A\*: the open set holds partial plans; the state is
the belief plus the plan's expected effects; the heuristic is the sum of the
cheapest known intent for each unmet predicate. Ties break on intent name,
then map name, so plans are deterministic.

The planner is **recursive through the route planner**: reaching `at(map)`
is priced by the route planner (§5), whose edges may themselves demand
predicates (`party_has_move(SURF)`, `FLAG_BADGE05_GET`). An unmet edge
requirement is turned into a subgoal and planned the same way, up to a depth
limit (default 6) and a cost budget. This is how "win the League" expands
into badges → gyms → HMs → the Pokémon that can learn them.

Battle predicates use the existing evaluator; a `Beat` whose probability is
below the threshold expands into `Train`/`Catch` through the existing
readiness planner (`prepare.rs`), which already prices training and
catching.

To keep the search bounded, common compound intents have **methods**
(HTN-style fixed decompositions, in `data/world/methods.json`): `GetHM(m)`,
`GetBadge(n)`, `ReachNationalDex`, `TeachHM(m)`, `StockBalls`. The planner
tries a method's decomposition before searching from primitives, and
only falls back to the general search when the method's preconditions can't
be met. Methods are data, so a new game brings its own.

### 4.3 Unknowns, priors and probes

A precondition on an `Unknown` fact is not a coin toss: the belief usually
holds evidence about it. The planner first asks the **prior model** for
`P(fact)` given what is `Observed`, then decides how to spend on certainty.

**Priors from evidence** (`data/world/priors.json`, declarative like the
inference rules; each rule gives a probability, and the most specific rule
that matches wins):

| Evidence | Prior |
|---|---|
| A flag that the event graph says is set on the *only* path to something already observed (e.g. we are on an island → Surf or the ferry was used) | 1.0 (this is inference, §3.2, not a prior) |
| Badge count 0 | `party_has_move(FLY)` = 0 (Fly can't be used without Badge 3) |
| Badge count ≥ 3 and Fly in the party | `visited[town]` = share of towns reachable before that badge that are normally visited on the way (from the graph: a town on the only route to a badge we hold is 1.0) |
| The Pokédex shows no species that can learn Flash | `has_item(HM05)` irrelevant: `party_has_move(FLASH)` = 0 |
| A trainer seen defeated on a route | trainers before it on the same route: 0.9 |
| No evidence | 0.5 |

**Choosing between assuming and probing.** For an `Unknown` fact with prior
`p`, alternative-plan cost `A` (what it costs to recover if the assumption
is wrong) and probe cost `C`:

- if `(1 − p) · A ≤ C` → **assume**, mark the intent `assumes: [fact]`, and
  add `(1 − p) · A` to its cost;
- otherwise → **probe** first (`Probe { fact }`);
- if `A` exceeds the **expensive threshold** (`--expensive-secs`, default
  600 s: a wrong guess that costs more than ten minutes) the probe is
  mandatory whenever one exists, regardless of `p`;
- if no probe exists, the plan proceeds on the assumption and the intent
  records what a failure would teach.

A tool that runs an intent with `assumes` and finds the assumption false
reports the fact `Observed` and fails the intent; the executor replans from
the updated belief. The Fly example in §9.1 goes through both branches.

Probe costs are measured, not guessed: each probe tool records its duration
into the syncer's timing model (§6.1), so the trade-off tightens over time.

### 4.4 Output

A `Plan` is the ordered list of intents with, for each, the predicates it
expects to establish and the belief snapshot id it was planned against.
The plan is shown in the web UI and written to the session as
`plan.jsonl` (one line per replan, with the reason).

### 4.5 Campaigns: many goals, scarce resources, irreversible choices

A **campaign** is a set of goal predicates ("every obtainable species is
caught", "all eight badges"). A single A\* over the whole set would be a
travelling-salesman problem, so campaigns are scheduled greedily above the
goal planner:

1. **Reachable set first.** Before moving, every goal is classified
   `reachable` (a plan exists), `blocked` (a plan exists once some
   predicate holds that another goal establishes) or `unreachable` (no
   intent can establish a required predicate on this console: link trades,
   event-only species, version exclusives, the other starter). The report
   is printed and shown in the UI before the first action, with the unmet
   predicate for every unreachable goal.
2. **Pick the next goal** by `cost / (1 + unlock_value)`, where
   `unlock_value` is the number of blocked goals the plan's effects unblock
   (the National Dex unblocks dozens of species; Surf unblocks half the
   map), with a **locality** bonus for goals whose plan starts in the
   current map or its neighbours. Ties break on goal name.
3. **Scarce resources.** Intents consuming an item the game gives once
   (Master Ball, the single Eevee, one fossil, one Dojo Pokémon) are not
   scheduled greedily. The campaign reserves each such resource for the
   goal whose plan needs it most: the one whose best alternative without
   it is most expensive or violates a safety rule (the Master Ball goes to
   the legendary with the worst catch odds under the 2 % risk limit).
4. **Irreversible choices.** An effect that makes another goal unreachable
   (`setflag FLAG_GOT_FOSSIL` hides the other fossil; the compiled events
   show this) costs the full value of that goal. The campaign then chooses
   the branch that keeps the reachable set largest, and notices when the
   choice is free: Tyrogue from breeding reaches both Hitmonlee and
   Hitmonchan, so the Dojo choice costs nothing.
5. **Progress** is the campaign's own predicate set, re-evaluated from the
   belief after every goal; the Pokédex probe is the ground truth when the
   belief is stale (e.g. resuming after a long break).
6. **Hysteresis.** The current goal is kept until it completes or fails;
   a re-score only replaces it when another goal is cheaper by a margin
   (default 25 %) or the current one became unreachable. Gemini's run
   showed that re-choosing the goal on every tick produces flip-flopping.

Each goal still runs through the §8 loop; the campaign only chooses which
goal is next and which resources it may spend.

## 5. Route planner (`crates/world::route`)

### 5.1 Place graph

Nodes are **places**: every warp tile, connection crossing, fly landing,
heal spot, gate tile, and any tile the caller asks for. Within a map, the
distance between two places is the A\* path length from `path.rs` (cached
per map and obstacle set).

Edges, each with a **requirement** (a predicate list, possibly empty) and a
cost in seconds (`TILE_MS` per tile, measured by the syncer over time):

| Edge | Requirement | Note |
|---|---|---|
| Walk | none | one-way over ledges (existing) |
| Warp / door / stairs / arrow mat | none, or a flag if the script gates it | existing |
| Connection | none | existing |
| Script warp (event teleport) | the script path's `when` | e.g. the Gym guy "take me to the top" |
| Fly | `party_has_move(FLY)`, `FLAG_BADGE03_GET`, `visited[dest]`, outdoors | cost ≈ 12 s + menu |
| Surf | `party_has_move(SURF)`, `FLAG_BADGE05_GET` | water tiles become walkable |
| Cut / Strength / Rock Smash gate | move + badge | opens the tile behind |
| Escape Rope / Dig | item or move; in a dungeon | to the dungeon entrance |
| Teleport | move; outdoors | to `respawn` |
| Blackout | none (last resort, costs a faint) | never chosen: violates the no-faint rule |
| NPC blocker | `hidden_by` flag true, or a script path that moves it | from `objects` |
| Forced movement: spinner tiles (Rocket Hideout), currents (Seafoam), Cycling Road slope, ice | none | endpoint precomputed offline from tile behaviours: the edge goes from the entry tile straight to where the game leaves the player |
| Teleporter pads (Silph Co.) | none | warps in the map data; kept as warps |
| Boulder puzzle (Victory Road, Seafoam) | `party_has_move(STRENGTH)`, `FLAG_BADGE04_GET` | the edge's cost is the push sequence found offline by the Sokoban solver (§7.2); the tool replays it, verifying every push |
| Ferry (Seagallop), S.S. Anne | ticket / island pass items, story flags | script warps with `when` from the compiled events |

### 5.2 Search

A\* over places, requirements checked against the belief. An edge whose
requirement is `Unknown` is treated as in §4.3 (optimistic with penalty, or
probe). An edge whose requirement is `false` is not dropped: it is returned
as a **blocked alternative** with the unmet predicates, so the goal planner
can decide whether satisfying them (get Surf) is cheaper than the open
route. This is the "it may need to talk to an NPC" case in the brief.

### 5.3 Legs

A route is a list of legs `(place → place, edge)`. Each leg maps to one tool
invocation: `Go` for walk/warp/connection legs, `Fly`, `Surf`, `UseItem`,
`Talk` for script warps. The route planner is called again from the current
pose whenever a leg fails or an interrupt moved the player.

## 6. Motion controller and syncer (`crates/agent::motion`)

Walking is the one place where open-loop holds are used for speed, so it
gets its own model.

### 6.1 Timing model

Per input kind `k` (walk tile, turn, menu press, warp fade, battle
transition) the syncer keeps

```
latency[k]   ms from command issue to first visible effect   (EWMA, α = 0.2)
duration[k]  ms per unit (per tile, per press)                (EWMA, α = 0.2)
spread[k]    absolute deviation of the last 16 samples
```

Samples come from frames the runtime already timestamps (`captured_at`) and
from localization: for a run of `n` tiles, the issue time and the frame at
which the player was first located at each tile give `n` duration samples
and one latency sample. Turns (a tap that didn't move) give `turn` samples.
Values are persisted per device profile (`emulator`, `switch`) in
`saves/timing.json` and shown in the web UI.

Hold lengths become `latency + n·duration − duration/2 + margin`, with
`margin = 2·spread`, instead of the fixed `TILE_MS = 268`. A run that ends
short or long feeds back the same way: the brief's "5× wasn't enough, raise
the factor" is the EWMA.

### 6.2 Predictive tracking

While a hold is active, the controller predicts the player's tile at every
frame: `tiles_done = floor((now − issue − latency) / duration)`. It compares
with the localized pose:

- prediction ahead of observation by > 1 tile for > `spread` ms → the run is
  stalling (blocked or slower than modelled): cancel with `Neutral`, tap once
  to learn the blocker (existing behaviour), and resample.
- observation ahead of prediction → the model is slow; adjust and let the
  hold finish early.

### 6.3 Local replanning around NPCs

Two layers, matching where the information lives:

**Planning layer (static).** Every NPC has a movement area from the data
(§2.1). The route planner never plans *through* a one-tile NPC and treats a
wanderer's rectangle as passable but uncertain: the leg is planned to the
closest tile outside the rectangle, and the remainder is left to the motion
controller. Tiles inside the rectangle cost +2 so routes prefer to skirt it
when that is cheap.

**Perception layer (per frame).** An **NPC detector** runs on every
perceived frame, as part of the existing perception pass (the first layer
of event emission), not only when walking:

- the localizer already knows which map crop the frame should match;
  the residual (frame minus expected render, on the tiles not covered by
  the player, text boxes or menus) is where sprites are;
- residual blobs are snapped to the tile grid and matched against the
  `objects` list for this map: an NPC is identified by being the only
  object whose area contains that tile with that graphics id's palette;
- each identified NPC produces `NpcSeen { map, local_id, x, y, facing }`
  when its tile changes, and an absent NPC that should be visible produces
  `NpcAbsent`, which feeds the belief (`FLAG_HIDE_*`, §3.2).

The detector runs on the CPU. Measured on 2026-09-25, the whole
`FireRedPerception::observe` pass takes about 1.3 ms (p50) per 240×160
frame on this machine, so a GPU round trip (upload, dispatch, readback)
would cost about as much as the work it replaced; the earlier idea of a
`wgpu` compute path with a bit-identical CPU fallback is dropped. If
throughput is ever needed, split the work over threads the way
`locate_anywhere` was (PR #8: ~150 ms → ~10 ms on 32 cores).

**Control.** The motion controller keeps, per visible NPC,
`(tile, facing, frames since last move)`. Before every hold and every 4
frames during one, it checks the next `min(3, remaining)` tiles of the run
against each NPC's tile and the tiles it can step to inside its area. A
predicted conflict cancels the hold and re-runs A\* from the current tile
with those tiles marked as obstacles for the next 2 s (a **short-horizon
replan**: only the current leg, not the route). Wanderers already learnt as
blockers by the existing `learned` set keep that behaviour, and the learnt
set is now seeded from the data instead of discovered by bumping.

### 6.4 Interfaces

```rust
pub struct Syncer { … }                       // timing model, per device profile
impl Syncer {
    fn hold_for(&self, kind: InputKind, units: usize) -> Duration;
    fn observe(&mut self, kind: InputKind, issued: Instant, seen: Instant, units: usize);
    fn expect_frames(&self, kind: InputKind) -> u64;   // replaces Executor::latency_frames
}
pub struct Walker { … }                       // one leg within a map
impl Walker {
    fn next(&mut self, obs: &Observation, sync: &Syncer) -> WalkStep;   // Hold / Tap / Cancel / Arrived / Blocked
}
```

`Navigator` (today's `nav.rs`) becomes `Walker` + the route planner; its
tap/turn/blocker learning is kept.

## 7. Tools (`crates/agent::tools`)

### 7.1 Contract

```rust
pub trait Tool {
    fn name(&self) -> &str;
    /// Static: which intents this tool serves.
    fn serves(&self, intent: &Intent) -> bool;
    /// Runs to completion or failure; may call other tools through `ctx`.
    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome;
}
pub struct ToolOutcome {
    pub result: Result<(), ToolError>,
    /// Facts established or refuted while running, with provenance.
    pub learned: Vec<GameEvent>,
    /// Where the player ended up, if known.
    pub pose: Option<PlayerPose>,
}
```

`ToolContext` exposes the executor (issue an `Action` and get its
`Outcome`), the belief, the world data, the syncer and, importantly,
**`ctx.invoke(intent)`** so tools compose: `Fly` = `OpenMenu(Pokémon)` →
`SelectMon(with FLY)` → `SelectMove(FLY)` → `PickTown` → `AwaitWarp`;
`Talk` = `Go(Facing)` → `Dialogue(script, answers)`.

Tools never sleep; they wait on expectations. Every tool is deterministic
given the same observations.

### 7.2 Catalogue

| Tool | Serves | Learns |
|---|---|---|
| `Go` | walk/warp/connection legs | blocked tiles, NPC positions, `visited` |
| `Fly` | fly legs | lit towns on the map (`visited`), whether Fly is in the party |
| `Surf`, `Cut`, `Strength`, `RockSmash` | gated edges | whether the prompt appeared (badge/move present) |
| `UseItem { item }` | Escape Rope, Repel | item counts |
| `Dialogue { script, answers }` | `RunScript` | the branch taken (from recognised lines → flags), items received, money |
| `Talk` | object scripts | as `Dialogue`, plus the NPC's position |
| `Battle { policy }` | `Beat`, wild fights | HUD facts, PP, moves learnt, evolution (existing) |
| `Catch { species }` | `Catch` | balls, caught/seen, PC transfers (existing design) |
| `Buy`, `Heal`, `Train` | as today | as today |
| `OpenMenu`, `Audit { pocket / party / card / flymap }` | `Probe` | the probed facts |
| `Save` | end of each intent that changed the belief | — |
| `Unstick` | any unrecognised screen | the text seen, for the overlay |
| `Puzzle { kind }` | boulder puzzles (BFS over boulder positions on the map model, deterministic, solved offline and cached), spinner mazes (precomputed endpoints), switch-gated doors (Pokémon Mansion: the switches are script vars, so they are ordinary `RunScript` intents) | boulder positions read from the residual, like NPCs (§6.3) |

### 7.3 Interrupts

`ToolContext::act` wraps every executor call. Before returning an outcome
it checks the observation:

| Seen | Handling |
|---|---|
| Battle box | run `Battle` (wild: catch policy → fight/flee; trainer: fight) and return `Interrupted`; the tool re-plans its leg from the new pose |
| Dialogue not expected by the tool | run `Dialogue` with recognition; if the script is known, its effects update the belief; then resume |
| HOME menu / outside the game | existing recovery |
| Level-up / evolution / new move | existing `learn` handling |

A tool may declare `interruptible: false` for actions where an interrupt is
impossible (menus), which keeps the check cheap.

### 7.4 Unknown conversations (`Unstick`)

1. Read every line on screen with the font.
2. Look the first line up in `dialogue.json` (wildcards for names/numbers).
   One match → we know the script and branch: answer from the plan's
   `answers`, or from the branch's `does` if the plan didn't expect it
   (default NO to any YES/NO that would spend money or items, YES to heals
   and free items, B on anything that offers a menu the plan doesn't need).
3. Several matches → keep reading pages until one is left.
4. No match → generic policy: advance with A while ▼ is shown; on a YES/NO
   pick NO; on a menu pick the last row (CANCEL/EXIT) if its text reads so,
   else B; after 3 pages log the text and the frame as an `unknown_dialogue`
   probe artifact for the overlay.
5. Never press A on an unrecognised YES/NO in battle (existing rule).

### 7.5 Party, PC and the screens a campaign needs

Breeding, HM carriers, evolution training and catching with a full party
make party composition a planning matter, not an audit. New predicates:
`in_party(S)`, `in_pc(S)`, `party_has(role)` where a role is `carrier(FLY)`,
`lead_safe_for(species)` (can weaken it under the 2 % risk rule), or
`parent_for(species)`. New tools and the screens they read:

| Tool | Screen / detector | Learns |
|---|---|---|
| `Deposit`, `Withdraw`, `MoveToBox` | PC box UI (box name, 30-slot grid, the name/Lv panel) | `BoxObserved`, party |
| `Pokedex` | Pokédex list (caught marks) | `SpeciesCaught` for every entry: the ground-truth probe for campaigns |
| `Breed { parent, partner }` | Day Care man's dialogue branches; the egg icon in the party | egg present, steps walked (from the syncer's tile count) |
| `Hatch` | the hatch screen ("Huh?" … "hatched") | `SpeciesCaught` (hatching registers the species) |
| `Fish { rod }` | the fishing prompt and "Oh! A bite!" text | encounter started |
| `Trade { give, get }` | the in-game trade prompt | party change, `SpeciesCaught` |
| `GameCorner { prize }` | coin counter, prize list | coins as a currency in `money`-style knowledge |
| `Revive { fossil }` | Cinnabar lab dialogue | party change |
| `RaiseHappiness` | Daisy's grooming rating in Pallet | a happiness bucket per party slot (`Knowledge`, `Observed` from her line) |
| `Catch` with the **Safari policy** | Safari Zone battle panel (BALL / BAIT / ROCK / RUN), step and ball counters | no fighting; bait/rock chosen by the catch and flee formulas; stops when the step budget can't reach the exit |

Legendary catches use the normal policy with the risk limit enforced by the
evaluator against the legendary's moves; the campaign decides beforehand
which of them gets the Master Ball (§4.5).

## 8. Execution loop (`crates/agent::goal`)

```
loop {
    plan = goal_planner.plan(goal, belief, world)            // deterministic
    for intent in plan {
        outcome = tools.run(intent)                          // with interrupts
        belief.apply(outcome.learned)
        if outcome.failed() || belief.contradicts(intent.assumes) { break }   // replan
        if intent.changed_belief() { tools.run(Save) }
    }
    if goal.satisfied(belief) { return Done }
    if replans > limit { return Fail(with the last plan and the facts learned) }
}
```

Every plan and replan is logged with its reason and shown in the web UI
under the existing goal panel: current intent, current leg, the belief
facts it depends on and their provenance.

**Execution monitoring rules**, from the failure modes every published
agent hit:

- **Contradiction:** an observation that contradicts an `Observed` belief
  replaces it (newest wins) and logs `belief_contradiction` with both
  frames; it never argues with the screen. A `Tracked` belief contradicted
  by an observation is simply corrected.
- **Plan-level loops:** the same `(intent, failure reason)` pair twice in a
  row marks that intent `infeasible` for the session (a belief fact with
  provenance `Derived`), which forces the next plan to take another branch;
  three different intents failing at the same pose escalates to a full
  re-localisation (`locate_anywhere`) and a probe of the facts the plan
  assumed.
- **Motion loops:** the executor's existing stuck rule (45 s without an
  act) is joined by a pose-history rule: visiting the same tile 4 times in
  one leg without progress on the path fails the leg.
- **Progress audit:** after any intent whose effects were only `Tracked`,
  the next intent that can observe them cheaply is preferred (the campaign
  gives a small bonus to plans that turn `Tracked` into `Observed`).

The `pokebot story` command keeps running the old milestones; a new
`pokebot goal <goal> [--dry-run] [--state saves/state.json]` runs the loop.
`--dry-run` prints the plan without touching the console, which is the main
development tool for the planner and the way the use case below is checked
before hardware.

## 9. Use cases

### 9.1 "Catch a RATTATA"

Goal predicate: `caught(RATTATA)`.

1. **Start.** `pokebot goal "catch RATTATA" --continue`. The bootstrap locates
   the player (Pewter Pokémon Center, from the current checkpoint) and
   restores `state.json` (IVYSAUR Lv 18, bag, money). No menu is opened: the
   planner needs balls (`Unknown` unless in `state.json`) and the encounter
   areas only.
2. **Plan.** `Catch { RATTATA }` needs an area with RATTATA in its wild table
   (Route 1, 2, 22 and others from `gamedata.json`), balls ≥ expected throws
   + reserve, and risk ≤ 2 %. The route planner prices Pewter → Route 2
   (walk, 2 minutes) against the others; the cheapest wins. Balls `Unknown`
   → the probe branch (Start → BAG, ≈ 10 s) is cheaper than the optimistic
   penalty (walking back to a mart), so the plan is
   `[Probe(balls), Buy(POKE_BALL, k) if needed, Go(Route2 grass), Catch]`.
3. **Later in the game**, same goal from Cinnabar with Fly and Surf in the
   party: the route planner finds Fly → Pallet Town + walk to Route 1 as
   the cheapest. `visited[PalletTown]` is `Observed = true`, so no probe.
   Suppose the goal were on an island whose `visited` is `Unknown`: the Fly
   edge is taken optimistically (penalty = cost of the Surf route × 0.5).
4. **Fly fails.** The `Fly` tool opens the map, reads which towns are lit,
   emits `visited[X] = false (Observed)` for the dark ones and fails the
   intent. The loop replans: the Fly edge is now `false`, its blocked
   alternative carries `visited[X]`, which no intent can establish except
   walking there. The next cheapest route is Fly to the nearest lit town +
   Surf. The plan becomes `[Fly(nearest), Go(shore), Surf, Go(area), Catch]`.
5. **Interrupted.** On the Surf leg a swimmer spots the player. The
   interrupt wrapper runs `Battle` (trainer: fight with the evaluator's best
   move); on return the `Surf` tool re-plans its leg from the current tile
   and continues. The battle's HUD facts and money update the belief.
6. **Catch.** Existing catching design: weaken, throw, verify "Gotcha!",
   `SpeciesCaught { RATTATA }` (`Observed`). The goal predicate holds; `Save`
   runs; done.

What the walkthrough validates: the premise holds with the belief layer,
probes and fact-returning tools added; without them the "learns the area
isn't unlocked, recalculates" step has no mechanism. Nothing in the
walkthrough needs a hand-written milestone.

### 9.2 "Catch every Pokémon obtainable in FireRed" (campaign)

Goal set: `caught(S)` for every `S` in `obtain.json`.

1. **Report.** The campaign classifies all species before moving. On one
   console, roughly 125 of Kanto's 151 are reachable, plus the Johto species
   of the Sevii Islands after the National Dex. Unreachable, each with its
   reason: the two other starters and lines (`needs_choice`, already made),
   Alakazam, Machamp, Golem, Gengar (`needs_link`), the fossil not chosen,
   the LeafGreen exclusives (~13, `other_version`), Mew and the ticket
   legendaries (`event_only`), Espeon and Umbreon (`needs_clock`: FireRed has
   no day/night). The exact list is the planner's output, not this document.
2. **Ordering.** `unlock_value` puts the story first: badges (gates and HMs),
   then the League (`FLAG_SYS_GAME_CLEAR` → Cerulean Cave), then the National
   Dex (60 caught + Sevii → Day Care breeding, Johto species). Locality fills
   in: every route walked for the story has its wild species caught on the
   way, since a catch there costs a few minutes and later costs a trip.
3. **Scarce resources.** The Master Ball is reserved for Mewtwo (worst catch
   odds under the 2 % risk rule); the birds are caught with Ultra Balls after
   sleep at low HP. The one Eevee is reserved for Vaporeon (Water Stone,
   cheapest); Jolteon and Flareon come from Eevee eggs (Eevee × Ditto).
4. **Exclusivity.** Helix vs Dome: whichever is chosen, the other line is
   reported unreachable; the choice is deterministic (name order) because
   both branches lose exactly one line. Hitmonlee vs Hitmonchan: free, via
   Tyrogue.
5. **Breeding, e.g. `caught(ELEKID)`:** National Dex holds → `Withdraw`
   Electabuzz (Route 10) and Ditto (Pokémon Mansion) if boxed → `Go` Four
   Island → `Breed` (deposit both) → paced walking until the egg cycles are
   done (the syncer's tile count is the step counter) → `Talk` for the egg →
   `Hatch` → `SpeciesCaught { ELEKID }` from the hatch text → `Save`. Magby
   follows with Magmar's line swapped in, kept close by locality.
6. **Safari Zone** species (Chansey, Kangaskhan, Scyther, Tauros, …) use the
   Safari catch policy; the step budget is a resource of that plan.
7. **Game Corner** prizes (Porygon, Dratini, Abra, …): coins are bought with
   money, which the planner prices like any item; trainer rematches by
   Vs. Seeker are the money source once the story is done.

What this validates: the same recursion reaches every method; what the
campaign adds is ordering, resource reservation and the reachable-set
report. Without §4.5 the greedy order would waste the Master Ball on the
first legendary met and could pick the fossil without pricing the loss.

## 10. Work streams

The four streams below can run in parallel and meet at the
`pokebot goal --dry-run` use case, then live on the emulator, then on the
Switch. **Whether to run them in parallel or in sequence (Data → Belief →
Planners → Motion) is not decided yet.** The dependencies, for that
decision: C needs A's `events.json` schema and B's `WorldBelief` types
(both can be fixed early with stubs); D needs nothing from the others
until the interrupt wrapper reads the belief; A and B need nothing.

| Stream | Deliverables | Verified by |
|---|---|---|
| A. Data | `compile_events.py` (front-end + IR), `events.json`, `dialogue.json`, `places.json` (incl. forced-movement endpoints and boulder solutions), `obtain.json`, `methods.json`, NPC movement areas, overlay + hot reload, ROM versioning | Unit tests on hand-checked scripts (Brock, Oak, Gym guy, Cut tree); the compiled Brock path matches §2.1; every `msgbox` in the maps we've played resolves through `dialogue.json` from the recorded frames in `captures/`; `obtain.json` names a method for all 151 Kanto species; **oracle:** a test loads Archipelago's `pokemon_frlg` region/entrance data (MIT, fetched at test time, not vendored) and checks that every location it marks reachable under the vanilla progression is reachable in our compiled graph, and every unreachable one is not |
| B. Belief | `WorldBelief`, events, inference rules, priors, trainer-card / fly-map / Pokédex readers, NPC detector (CPU, threaded if needed), bootstrap | Reducer replay tests; the checkpoint from `saves/route3-ready` yields `FLAG_BADGE01_GET = Observed true` after one trainer-card probe; the NPC detector finds the Pallet Town sign lady on recorded frames |
| C. Planners | goal planner, route planner with gated edges, priors and probes, campaign scheduler with reachable-set report, `pokebot goal --dry-run` | The §9.1 plans, printed deterministically; "win the League" expands to the eight gyms with the right HM subgoals; the §9.2 report lists the unreachable species with reasons; the Master Ball is reserved for Mewtwo |
| D. Motion + tools | `Syncer`, `Walker`, predictive tracking, NPC replanning, `Tool` trait, interrupt wrapper, `Unstick`, `Fly`/`Surf`, party/PC tools, Safari policy | The existing milestones through `PrepareForRoute3` run unchanged as a regression script; timing model converges on the emulator and shows a different profile on the Switch; a staged wandering NPC on Route 1 causes a local replan, seen in the recording |

Order inside each stream follows TDD; live checks go through
`tools/live-run.sh` so the web UI shows them.

## 11. Open questions

- Which `special` calls need modelling beyond `HealPlayerParty`,
  `StartLegendaryBattle`, `ChoosePartyMon`, `SetCableClubWarp`? The compiler
  will list every unmodelled one it meets; decide from that list.
- The no-evidence prior (0.5), the expensive threshold (600 s) and the probe
  costs start as guesses; the probe costs are measured from the first run,
  the other two only affect which of two valid plans is chosen.
- The NPC detector (§6.3) needs a fixture set of recorded frames with
  wandering NPCs; the residual approach is the first attempt and a small
  sprite-template detector the fallback.
- Parallel vs sequenced work streams (§10).
- Boulder puzzle solving reads boulder positions from the residual; the
  puzzles' initial layouts are in the map data, so the solver can also run
  blind and only verify each push on screen. Decide after the NPC detector
  exists.

## 12. Sources consulted (§0)

- PokéAgent Challenge: [paper](https://arxiv.org/abs/2603.15563), [winning solution (heatz)](https://github.com/heatz123/pokeagent-solution), [DEEPEST](https://github.com/lastdefiance20/pokeagent-speedrun-DEEPEST), [Continual Harness](https://arxiv.org/abs/2605.09998)
- Gemini Plays Pokémon: [making-of](https://blog.jcz.dev/the-making-of-gemini-plays-pokemon), [Gemini 3 vs 2.5 in Crystal](https://blog.jcz.dev/gemini-3-pro-vs-25-pro-in-pokemon-crystal)
- Claude Plays Pokémon: [LessWrong analysis](https://www.lesswrong.com/posts/HyD3khBjnBhvsp8Gb/so-how-well-is-claude-playing-pokemon), [Anthropic case notes](https://www.zenml.io/llmops-database/building-and-deploying-a-pokemon-playing-llm-agent-at-anthropic)
- RL: [Pokémon Red via RL](https://arxiv.org/abs/2502.19920), [PokeRL](https://arxiv.org/abs/2604.10812), [PokemonRedExperiments](https://github.com/PWhiddy/PokemonRedExperiments)
- Battles: [PokéLLMon](https://arxiv.org/abs/2402.01118), [PokéChamp](https://arxiv.org/pdf/2503.04094)
- Reachability model of FRLG: [Archipelago `pokemon_frlg`](https://github.com/vyneras/Archipelago) (`rules.py`, `logic.py`, `data/regions/*.json`, `data/events.json`)
- Speedrun routing: [RouteThreeFour](https://github.com/UnderscorePoY/pokemon-RouteThreeFour), [Automating Speedrun Routing: Overview and Vision](https://arxiv.org/abs/2106.01182)
- Planning: [GOAP in F.E.A.R.](https://www.gamedeveloper.com/design/building-the-ai-of-f-e-a-r-with-goal-oriented-action-planning), [HTN in Decima](https://www.guerrilla-games.com/read/htn-planning-in-decima), [HTN overview](https://arxiv.org/pdf/1403.7426), [Planning with state uncertainty via contingency planning and execution monitoring](https://www.academia.edu/1932961/Planning_with_State_Uncertainty_via_Contingency_Planning_and_Execution_Monitoring)
- General agents: [Cradle](https://arxiv.org/abs/2403.03186), [Voyager](https://arxiv.org/abs/2305.16291)
- Latency: [survey of latency compensation techniques](https://dl.acm.org/doi/10.1145/3519023)
