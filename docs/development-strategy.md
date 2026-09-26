# Development strategy: fail fast on emulators, polish vision on the Switch

The Switch is the target and the only place a clean run counts. It is also slow: real time, one console, and a sleeping console needs a person to wake it. Emulators are fast, run many at once, and can be snapshotted. So:

- **Emulators write and break the rules.** New behaviour is developed against emulator runs (`pokebot fleet`, the `emu` instance) and against a growing library of scenarios: snapshots of situations the bot met.
- **The Switch polishes vision and proves the rules.** It runs the clean `goal … --restart` campaign. Its logs and recordings show where it gets stuck, and each Switch failure gets a regression test with its frame as a fixture (CLAUDE.md §4).

## Unsticking: generic first, specific last

Every goal falls back on the same generic recourses before anything specific is written. They are slow but game-agnostic, and they are what makes a stuck run *likelier* to get unstuck than standing still. The most basic one is **talking to everyone reachable, nearest first**, while the plan waits for the belief to change (a flag set, an item gained, a script path run). A change is the event that ends the stall: the goal loop replans from what the game showed.

Specific help comes only after the generic ladder has failed on a situation, and it is still data (a `methods.json` hint, a detector, a world-model fix), never a per-NPC script (see the "tools, not scripts" rule).

### What is built (`crates/agent`)

- **`ledger.rs`**: what the bot has done in the world, kept across reloads and restarts in `ledger.json` beside `state.json`:
  - every tile walked, per map;
  - every NPC seen, how many visits it was seen on and where;
  - every talk: what was said (recognised text labels), the script, the answers given, whether it changed the belief, and why it failed if it did.
  
  Each talk is stamped with a fingerprint of the belief (flag and var values, script paths run, bag). A person is worth talking to again only if the fingerprint changed since, or if their question still has its other answer to try. YES is tried only where no path of the script spends money or items, fights, or does something unmodelled such as a trade.
- **`recourse.rs`**: recourses that can be stacked by priority. Each one is offered with a chance (that it changes the belief) and an expected time. The priority is the chance per second, recomputed every time one is needed:
  - `Probe`: open the screens that settle the plan's unobserved assumptions;
  - `Explore { map }`: talk to or read what can still teach something on a map. The stuck map comes first, then outward up to 3 hops, priced with real route costs. A map the failing plan names counts double.

  The chance of one talk is the share of all logged talks that taught something. Each kind's success record scales its offers, so a kind that keeps failing sinks and one that works rises, across restarts. Adding a recourse means an enum variant, its offers and its run.
- **Goal loop** (`goal.rs`): a stall triggers the ranked recourses, tried best first until one changes the belief. That covers an infeasible intent, the same reason failing under different intents (the Bill[5]/[7]/[9]/[30] loop), no plan, or spent replans. The limits are 6 per stall and 24 per run. Every ranking is logged as a `recourse` explain line with the top offers and their numbers.

### Next recourses (in the order they would pay off)

1. **Wider radius over time**: raise `MAX_HOPS` as closer maps are spent, up to every map reachable from the current one.
2. **Trainers** as a lower-priority tier, offered only while the party is healthy (a talk to a trainer starts a battle the plan didn't price).
3. **Unwalked tiles**: walk to reachable tiles never stood on (the ledger has the set), which finds hidden triggers and step events.
4. **Items and field moves** on the stuck map (use each key item, Cut or Strength what can be cut or pushed).
5. **Waiting**: watch the screen for a scripted NPC to arrive (Explore already watches 150 frames when nothing is fresh).

## Scenario library (development only)

Snapshots are a development tool. The bot never restores one to get something done, and a clean run never takes one. The architectural guard (`adapters/emulator-libretro/tests/no_privileged_access.rs`) allows the core's state entry points only in `snapshot.rs`. It forbids any crate under `crates/` from naming development snapshots. A capture-card run refuses the flags.

- **Record.** `pokebot goal … --save-game --dev-snapshots [DIR]` (default `saves/scenarios`) takes a snapshot after every in-game save (which follows each step that changed the save-relevant belief) and when the goal is met. Each one is a directory with:
  - `snapshot.bin` (the emulator state, opaque);
  - `game.sav`, `state.json`, `progress.json` and `ledger.json`;
  - `frame.png`;
  - `meta.json`: the commit (`+dirty` when built with uncommitted changes), the goal, the plan step that led there, the pose, badges, flags set, party and money.

  Every snapshot is also a line in `DIR/index.jsonl`.
- **Query.** `pokebot scenario list [--map M] [--flag FLAG_X | --flag '!FLAG_X'] [--badges N] [--text S] [--json]`.
- **Start from one.** `pokebot goal "<goal>" --scenario <id|dir>` copies the scenario to a scratch directory under the system temp dir, restores the snapshot and the checkpoint, and plays the goal from there. The library itself is left untouched.

Scenarios for edge cases grow from runs: when an emulator run gets stuck, the snapshot before the stall is where a fix is tried again in seconds. A Switch failure can't be snapshotted, but the same situation reached on the emulator can.
