# Probe: first `goal --new-game --restart` run on the Switch (2026-09-25)

Command (from the worktree, through `tools/live-run.sh --instance switch`):

```
goal "flag FLAG_SYS_GAME_CLEAR" --video capture-card:/dev/video0 --viewport 180,5,1560,1040
  --card-controls switch --controller esp32:10.10.100.185 --new-game --save-game
  --progress saves/switch/progress.json --restart --record /tmp/switch-goal-1
  --plan-budget-secs 120 --max-replans 8
```

Log `/tmp/switch-goal-1.log`, recording `/tmp/switch-goal-1`, checkpoint
`saves/switch/{progress,state}.json` of the worktree
`.claude/worktrees/agent-a6e7be9ee00f1ed93`.

## What worked (my files: `goal_session.rs`, CLI `goal.rs`)

- Soft reset → title → NEW GAME → intro → names → bedroom: `NewGame: Done` at
  frame 6158 (about 2 min).
- The nine opening milestones through `StoryTask`, one save each
  (`checkpoint after …` lines): LeaveBedroom (house 1F 10,2), TalkToMom,
  LeaveHouse (Pallet 6,8), MeetOak (lab 6,4), ChooseStarter, RivalBattle
  (lost, `loss_ok`), HealAtHome, GetParcel (Viridian Mart 4,3; Bulbasaur
  Lv6), DeliverParcel (lab 6,4). Frame 59538, about 15 min.
- Handover: `goal: FLAG_SYS_GAME_CLEAR`; plan 1 in under the 120 s budget:
  82 steps, 119459 s. Step 1 `Probe(trainer card)` read badges [] and 1
  caught on the real hardware. Step 2 `Go(Route22)` walked Pallet → Route 1.
- The device error below ended the cycle; the session logged the report,
  wrote `captures/stuck/1790297751-goal-f73796`, and scheduled cycle 2
  (`NOTIFY restart: cycle 2 in 240 s: soft reset and CONTINUE …`).

## Failures in files I don't own

### 1. Whiteout on Route 1: no heal before the walk, no RUN at low HP

`crates/agent/src/tools/go.rs`, `tools/battle.rs`, `crates/planner`.

- The plan's step 2 sent the Lv6 Bulbasaur to Route 22 at **10/22 HP** (the
  state after DeliverParcel; the lab is two doors from Mom's free heal). No
  `Heal` step was planned before the first `Go`.
- Two wild battles on Route 1: `Battle Auto` kept using move 0 at 7/22 and
  then **3/22 HP** (frames 67296–67778) instead of running. A Pidgey's
  Tackle took it to 0/22: `RED whited out!` (frame ≈71000).
- The whiteout screens (`RED scurried back home, protecting the exhausted
  and fainted POKéMON from further harm…`, black text on a white screen with
  a ▼) are **not recognised**: perception says `Unknown` for ~1200 frames
  (72094–73290) until `recover: stuck waiting (battle screen), press B`.
  Fixture: `captures/fixtures/switch-whiteout-scurried-home.png` (frame
  72600 of the recording; `pokebot inspect` → `Unknown`). A whiteout also
  moves the player to the last heal spot (Mom's house), which nothing
  tracks: the belief still had the player on Route 1.
- The user's rule "no Pokémon may faint" is only honoured indirectly here:
  the next cycle CONTINUEs the DeliverParcel save (the goal loop has no
  faint → reload rule of its own the way `story` does per milestone).

### 2. ESP32 timed out once: `no reply to request 622`

Right after the whiteout dialogue, `controller rejected Press(A): device
disconnected: 10.10.100.185:7878: no reply to request 622`; the tool chain
(`Battle Auto` → `Go Route22` → `Goal`) failed with the device error. The
board answered `/api/status` (`idle: true, usb_mounted: true`) a minute
later, so it was a transient. The session's restart covers it (the client
reconnects on the next command); worth a retry inside the pabotbase client
(`adapters/pabotbase`) so one lost reply does not cost a whole cycle.

### 3. Plan quality (`crates/planner`)

16 of the 82 steps are `Unsupported(training reaches only N% against …)`
placeholders (6000 s each), e.g. step 8 against Misty right after step 7
`Train(BULBASAUR to Lv20)`. The loop will hit the first at step 8 and
replan; expect `out of replans` cycles until the planner can express a
catch/party plan there.

## Cycle 2 (restart proven on the hardware)

After the 240 s wait: `console: the game is on screen` → `ContinueGame
completed` (frame 89552) → `continuing after: … DeliverParcel` → `Checkpoint
knowledge restored from saves/switch/state.json` → RED back in Oak's lab at
the DeliverParcel save. Plan 1 of cycle 2: 81 steps (the trainer-card probe
is no longer needed; its result is in the checkpoint), step 1 `Go(Route22)`
again at 10/22 HP — failure 1 above will repeat until a Heal is planned or
the battle tool runs at low HP.
