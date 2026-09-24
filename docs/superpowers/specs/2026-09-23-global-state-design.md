# Global state, catching policy and planner costs — design

Status: approved in conversation on 2026-09-23 (sections 1–4). This spec is
the reference for the implementation plans; it is delivered in three
phases, each verified live before the next.

## Goal

The bot should know, from the screen and from its own tracked history, everything
it needs to plan: party (including current and maximum PP), bag, money, PC
boxes and the Pokédex's caught flags. With that knowledge it follows policies for
catching, shinies and keeping Poké Balls, and its planner prices every way of
reaching a goal: grinding, fighting trainers on the way, catching, buying,
fetching from the PC.

The project's ground rules still hold: vision and controller only, closed-loop
actions, deterministic decisions, and `Unknown` as a legitimate value.

## Decisions taken

| Question | Decision |
|---|---|
| Order | 1. state, 2. policies, 3. planner costs; each phase verified live |
| Keeping state accurate | Track facts from text and HUD; open menus (audit) only when a decision needs a fact that is `Unknown` or stale |
| Where state lives | Event-sourced in `crates/state` (`GameState` + pure reducer); `agent::Party` becomes a view |
| Battle safety target | Keep P(win) ≥ 90 % per battle (planner) |
| Catch risk limit | P(our active Pokémon faints during the attempt) ≤ 2 % |
| Ball stock | Shiny reserve 5, target 15, keep money for 2 Potions |

## Phase 1 — Global state

### Model (`crates/state`)

```rust
GameState {
    // …existing fields…
    party:   Knowledge<Vec<PartyMon>>,   // index = party slot
    bag:     Bag,
    money:   Knowledge<u32>,
    pc:      PcStorage,
    pokedex: Pokedex,
}

PartyMon {
    species:   Knowledge<Species>,
    nickname:  Knowledge<String>,
    level:     Knowledge<u8>,
    hp:        Knowledge<(u16, u16)>,        // current, max
    status:    Knowledge<Status>,
    moves:     [Option<MoveSlot>; 4],        // menu order
    held_item: Knowledge<Option<Item>>,
    shiny:     Knowledge<bool>,
}
MoveSlot { mv: Knowledge<Move>, pp: Knowledge<(u8, u8)> }   // current, max (PP Ups)

Bag       { pockets: BTreeMap<Pocket, Knowledge<Vec<(Item, u16)>>> }
Pocket    = Items | KeyItems | PokeBalls | TmCase | BerryPouch
PcStorage { boxes: Vec<Knowledge<Vec<BoxMon>>>,       // 14 boxes × 30
            items: Knowledge<Vec<(Item, u16)>> }
BoxMon    { slot: u8, species, level, nickname }     // each Knowledge<…>
Pokedex   { caught: BTreeMap<Species, Knowledge<bool>>,
            seen:   BTreeMap<Species, Knowledge<bool>> }
```

- Provenance is kept per fact, using the existing `Knowledge<T>` (source plus
  `last_verified_frame`).
- **Stale:** a value is stale once a tracked event changed it after its last
  observation (source `Tracked`), or once a menu action that could change it
  finished unconfirmed. Staleness never comes from a timer.
- Collections use `BTreeMap` / `Vec` so that iteration order, and therefore
  plans, are deterministic.

### Events

New `GameEvent` variants, applied by the pure reducer:

| Event | Provenance | Emitted by |
|---|---|---|
| `PartyObserved { slot, species, nickname, level, hp, status, held_item }` (each `Option`: only what that screen shows) | Observed | party menu / summary audit, battle HUD |
| `MovePpObserved { slot, move_slot, cur, max }` | Observed | move menu `PP a/b`, KNOWN MOVES list, summary moves page |
| `MoveUsed { slot, move }` | Tracked | battle (move confirmed by "X used M!") |
| `MoveReplaced { slot, move_slot, old, new }` | Observed (text) | "forgot / learned" pages |
| `Evolved { slot, species }` | Observed (text / HUD) | "evolved into", HUD name |
| `ItemsChanged { pocket, item, delta, reason }` | Tracked | "Obtained X!", "You bought N", "used X", ball thrown |
| `PocketObserved { pocket, items }` | Observed | bag audit |
| `MoneyObserved { amount }` | Observed | mart window, trainer card |
| `MoneyChanged { delta, reason }` | Tracked | "You got ₽N for winning!", purchase total |
| `BoxObserved { box, mons }` / `PcItemsObserved` | Observed | PC audit |
| `MonDeposited` / `MonWithdrawn` / `SentToPc { box }` | Observed (text) | PC flows, "transferred to BILL's PC" |
| `SpeciesCaught { species }` / `SpeciesSeen` | Observed | caught icon on the HUD, "Gotcha!" |
| `ShinySeen { species }` | Observed | battle intro detector |
| `Healed` | Observed (text) | Pokémon Center nurse "restored" line |

`GameState` stays replayable from `events.jsonl`.

### Checkpoints

- Every in-game save writes `saves/state.json` (a `GameState` snapshot, minus
  screen and input) beside `progress.json`.
- Reloading after a faint restores the snapshot, so state matches the cartridge
  again.
- `progress.json` keeps only milestones and the save position; its `party`
  field is migrated into the snapshot on first load.

### Readers (vision) and audits (agent)

All readers use `vision::text` (the font extracted from the decompilation).

| Knowledge | Passive source | Audit |
|---|---|---|
| Party HP, level | battle HUD | party menu (names, Lv, HP numbers) |
| PP cur/max | move menu `PP a/b` (cursor move); KNOWN MOVES list | summary → moves page (all four) |
| Species, moves | HUD name, "evolved into", "forgot / learned" | summary pages |
| Bag | "Obtained X!", "put the X in the … POCKET", "You bought N", "used X", thrown balls | Start → BAG: read each pocket's `NAME ×N` rows, scrolling until the list repeats |
| Money | "You got ₽N for winning!", purchases | mart window `MONEY ₽N`; trainer card |
| PC boxes | "was transferred to BILL's PC" + box name | box view: cursor over each occupied slot, read the name/Lv panel (on demand only) |
| Pokédex caught | caught-ball icon next to the wild Pokémon's HUD name; "Gotcha! X was caught!" | not needed |
| Shiny | battle intro: sparkle animation, and the front sprite's palette matched against the decompilation's `normal.pal` / `shiny.pal` for the species | not needed |

- **Audit rule:** a decision that needs a fact asks for it. If the fact is
  `Unknown` or stale, an audit task runs first and reads only the pocket or
  page it needs. Audits are ordinary closed-loop tasks: every screen is
  verified, then the menu is closed and the close is verified.
- **Fixtures:** each new screen is reached by a deterministic scripted replay
  and its frames are saved to `captures/fixtures/` (gitignored). Readers are
  tested with `pokebot inspect` and with unit tests on synthetic renders drawn
  with the extracted font.
- **Phase 1 delivery order:**
  1. model, events, reducer and checkpoints, with the passive sources
     migrated (HUD, move menu, text facts);
  2. bag audit;
  3. party / summary audit;
  4. money;
  5. PC audit.

  Each step is verified live on the LAN UI.

## Phase 2 — Policies

### Catching (wild battles; decided when the opponent is identified)

1. **Shiny → always catch.** Any ball may be used, including the reserve.
2. **Not yet caught** (no caught-ball icon) **→ catch**, unless:
   - the risk is too high: the evaluator's P(our active Pokémon faints) over
     the whole attempt (weakening turns + expected throws) is above 2 %; or
   - there aren't enough balls: non-shiny attempts may only spend balls above
     the shiny reserve (5), and don't start unless stock ≥ reserve + the
     expected number of throws.
3. **Weakening:**
   1. an opening status move, chosen by value (sleep > paralysis);
   2. then only moves whose **maximum** damage roll cannot KO the target, until
      its HP bar is under about 25 % (red zone);
   3. then the best ball in the bag (by the catch formula).

   Every action is verified on screen. The attempt is re-evaluated after each
   turn and abandoned (RUN) if the risk limit is exceeded.
4. **After a catch:** answer NO to the nickname prompt, and record
   `SpeciesCaught` plus `SentToPc` when the party is full.

### Keeping Poké Balls

- **Minimum stock: 5** (the shiny reserve). **Target stock: 15.**
- Below the target, a `Buy { item, n }` step is inserted at the next reachable
  mart that sells balls. It is priced like any other step and must leave money
  for 2 Potions.
- Below the minimum, buying is mandatory before any further exploration.

### Buying (`Buy { item, n }`)

A closed-loop flow:
1. Talk to the clerk and choose BUY.
2. Pick the item from the read list.
3. Set the quantity by reading the number.
4. Answer YES.
5. Confirm the new money and the bag change.

## Phase 3 — Planner costs

### Candidate steps

`Train(area, to_level)`, `FightTrainers(route segment)`,
`Catch(species, area)`, `Buy(item, n, mart)`, `PcFetch(mon | item)`,
`PcDeposit(mon)`, `Heal(center)`.

### Cost and constraints

- **Cost** = expected game time:
  - travel along the world graph;
  - battles from the evaluator's expected turns;
  - encounter rates;
  - menu time for shops and the PC.
- **Money** is a limited resource that flows through the plan: payouts add,
  purchases subtract.
- **Safety:** every battle in the plan must have P(win) ≥ 90 %.

### Trainers on the way as training

- The path to the goal is simulated in walking order. Each trainer whose sight
  line the path crosses is fought with the party's projected state: level,
  moves learned (using `moves::choose`), HP and PP carried forward, and a heal
  where the path passes a Pokémon Center.
- Experience from each fight carries into the next. The planner searches for
  the least earlier grinding that keeps every fight in the chain at or above
  the safety target.
- Example: "train to 16, then Route 3's trainers carry us to 18" beats
  "grind to 18 on Route 22" when it's cheaper.

### PC detours

- Boxed Pokémon and stored items are candidates like party members.
- Using one costs travel to the nearest reachable PC plus withdraw/deposit
  menu time, including a deposit when the party is full.

### Catches and purchases in plans

- `Catch` is priced from encounter share, the catch formula and the weakening
  turns.
- `Buy` is priced from the mart detour.

### Determinism

The search is exhaustive over bounded options (as `prepare` is today). Ties
break by cost, then by risk (lowest per-battle P(win) higher), then by step
names.

## Out of scope

- Using items in battle (Potions, Revives).
- Switching Pokémon mid-battle.
- HMs and field moves.
- Double battles.
- Trading.

The state model leaves room for these.

## Testing

- Reducer unit tests for every new event, and a replay test that rebuilds the
  state from `events.jsonl`.
- Reader tests on fixtures and synthetic renders.
- Policy tests: catch / no-catch / shiny / low balls / high risk; buy
  insertion.
- Planner tests on real game data:
  - the trainer chain shortens grinding (Route 3 from IVYSAUR Lv 16);
  - a PC detour is chosen only when cheaper.
- Each phase ends with a live run through `tools/live-run.sh`, verified with
  contact sheets.
