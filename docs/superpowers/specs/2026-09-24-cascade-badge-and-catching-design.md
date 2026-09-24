# Cascade Badge segment with catching — design

Status: approved in conversation on 2026-09-24 (§1–§3). This is the next
sub-project after the global-state Phase 1 core (see
`2026-09-23-global-state-design.md`, whose Phase 2 catching policy it
implements).

## Goal

The bot plays from the Route 3 preparation checkpoint (IVYSAUR Lv 18, saved
in Pewter) to the **Cascade Badge**. Along the way it catches wild Pokémon
by the policy in the global-state spec, and keeps a stock of Poké Balls by
reading and buying them. Every step is verified live on the emulator.

The ground rules still hold:
- vision and controller only;
- closed-loop actions;
- deterministic decisions;
- `Unknown` is a legitimate state;
- no Pokémon may faint (faint → reload the last save and retry).

## Decisions taken

| Question | Decision |
|---|---|
| Priority | Story and catching together, in one sub-project |
| Ball count and money | Read where the game shows them (in-battle bag, mart window). Audit only the Poké Balls pocket, via Start → BAG, and only when unknown |
| Shiny detection | Match the wild sprite's colours against the decompilation's `normal.pal` / `shiny.pal` for the species |
| Story steps | Hand-written milestones; trainer lists from the extracted map data |
| Fossil | Helix Fossil |
| Catch risk limit | P(our lead faints during the attempt) ≤ 2 % |
| Ball stock | Shiny reserve 5, target 15; keep money for 2 Potions |
| Planner safety | P(win) ≥ 90 % per battle (unchanged) |

Out of scope:
- Nugget Bridge and the rival on Route 24, and Bill on Route 25: the next milestone.
- The PC audit and the full party/summary audit.
- Switching Pokémon in battle: the lead fights.
- Phase 3 planner costs.

## §1 Catching in battle

### Vision

Fixtures are captured by probing each screen with deterministic replays.

- **Caught icon.** The small Poké Ball next to a wild Pokémon's HUD name.
  - Present → `SpeciesCaught { species }` (already caught).
  - Absent → `SpeciesSeen { species }`.
  - Only read when the HUD name resolves to a species.
- **Shiny.**
  - `tools/gamedata/extract_gamedata.py` also extracts, for every species, its normal and shiny palettes (`graphics/pokemon/<name>/{normal,shiny}.pal`; `graphics/pokemon` is added to the sparse checkout) into `data/world/gamedata.json`.
  - At the start of a wild battle, the front sprite's opaque pixels are compared against both palettes, colours converted the mGBA way as for map renders.
  - A match to the shiny palette only → `ShinySeen { species }`.
  - Unclear readings (neither palette clearly wins) are treated as not shiny, and logged.
- **Battle bag screen.**
  - The pocket title, read with the font.
  - The list rows, read as `NAME ×N`.
  - The ▶ row.
  - The USE/CANCEL prompt.
- **Pokédex registration page** after a first catch: detected, so it can be dismissed.

### Decision (`agent::catch`)

Made once the wild opponent is identified (HUD name and level):

1. Shiny → catch; any ball may be used, including the reserve.
2. Not caught yet → catch, if both hold:
   - the risk is ≤ 2 %;
   - the ball stock is > reserve (5) + expected throws.
3. Otherwise fight or flee exactly as today.

**Risk:** the evaluator's P(our lead faints) over the whole attempt:
- the weakening turns, with the foe using its most damaging move every turn;
- the expected number of throws, from the catch formula at the weakened HP with the best ball we hold.

### Weakening

1. Open with the best status move by value: sleep before paralysis. Skip this if the foe is already asleep or paralysed (read from the HUD status when available), or if there is no such move.
2. Then attack only with moves whose **maximum** damage roll cannot KO the foe at its current HP. Stop once its HP bar is below 25 %. If no move is safe, go straight to throwing.
3. After every turn, re-evaluate from the HUD:
   - the risk rose above the limit → RUN;
   - the foe fainted → the attempt is over (normal battle end).

### Throwing

Every step is verified on screen.

1. BAG → switch to the POKé BALLS pocket with Left/Right, reading the title.
2. Read the list and pick the best ball by the catch formula; ties go to the cheapest ball.
3. Move ▶ to it and press USE.
4. The ball text follows:
   - "Gotcha! X was caught!" → caught.
   - "Oh, no! The POKéMON broke free!" (or the game's variants) → next turn.
5. After a catch:
   1. Dismiss the Pokédex registration page.
   2. Answer NO to "Give a nickname to the caught X?".
   3. If the party was full, read "X was transferred to … PC" and "BOX N".

**Events:**
- `ItemsChanged { pocket: PokeBalls, item, delta: -1, reason: "thrown" }` per throw;
- `PocketObserved` whenever the in-battle list is read;
- `SpeciesCaught`;
- either `PartyMonDerived` (the next free slot; species and level from the HUD, default moves and full PP from game data, provenance Derived) or `SentToPc`.

The party grows; battles still use only the lead.

## §2 Keeping Poké Balls and money

### Poké Balls pocket audit (Start → BAG)

Runs only when a decision needs the ball count and the pocket is `Unknown` or stale: the first catch decision, or a buy decision.

1. Open the Start menu and pick BAG.
2. Switch to the POKé BALLS pocket with Left/Right, reading the pocket title.
3. Read the list rows (`NAME ×N`), scrolling Down until the rows repeat or CANCEL appears → `PocketObserved`.
4. Close with B until no menu is left.

Every screen and cursor move is verified. The in-battle bag uses the same list reader.

### Money

- **Observed:** from the mart window's `MONEY ₽N` whenever we shop (`MoneyObserved`).
- **Tracked:** from "got ¥N for winning!" and purchase totals (`MoneyChanged`).
- A mart visit reads money before buying, so no separate money audit is needed.

### Buying (`StoryStep::Buy { item, count }`)

1. Talk to the clerk and choose BUY.
2. Read the item list and move ▶ to the item.
3. Press A, then raise the quantity while reading `×N` and the total price, until it equals the target.
4. Answer YES, and read "Here you are!".
5. Confirm that `MONEY` dropped by the total → `ItemsChanged` + `MoneyChanged`.
6. Close the mart.

The count is capped by money minus a reserve for 2 Potions (priced from game data).

### When to buy

- Before leaving a town whose mart sells Poké Balls, if the stock is below 15 and money allows.
- Mandatory, even as a detour to the nearest reachable mart, when the stock is below 5.

## §3 Story: Route 3 → Cascade Badge

Hand-written milestones in the existing style. Trainer lists come from `map_trainers`; Super Nerd Miguel is added explicitly (a floor trigger starts his battle, so it isn't in `map_trainers`). Each milestone ends with an in-game save and `state.json`.

1. **StockUpPewter:** audit the Poké Balls pocket, then `Buy` Poké Balls up to 15 at Pewter Mart if money allows.
2. **CrossRoute3:**
   - Walk Route 3 to the Route 4 Pokémon Center.
   - Trainers who spot us are fought.
   - Wild catches follow §1.
   - Heal at the Route 4 Pokémon Center.
3. **PrepareForMtMoon:**
   - `Prepare` against the Mt. Moon 1F trainers, Miguel and the four B2F Team Rocket grunts, with training areas Route 3, Route 4 and Mt. Moon.
   - Then heal.
4. **CrossMtMoon:**
   1. 1F → B1F → B2F by the ladders (warps).
   2. Beat Miguel.
   3. Take the **Helix Fossil**: object 2 at (14, 7) on `MtMoon_B2F`; talk, answer YES.
   4. Continue through B1F to Route 4, fighting grunts who spot us.
5. **ReachCerulean:**
   - Route 4, whose one-way ledges are already modelled, to Cerulean.
   - Heal, then StockUp at Cerulean Mart.
6. **PrepareForMisty:** `Prepare` against the gym's two trainers and Leader Misty (Staryu Lv 18, Starmie Lv 21). Train if needed.
7. **BeatMisty:** challenge Misty and receive the Cascade Badge.

**Probed first (before a milestone depends on them):**
- **Localization inside Mt. Moon:** cave palettes haven't been matched against our renders yet. Park in Mt. Moon and check `pokebot inspect --world`; fix the renderer if needed.
- **The fossil and Miguel scripts:** checked against `data/maps/MtMoon_B2F/scripts.inc`.
- **Trainer sight chains on Route 3:** battles interrupt navigation, as in Viridian Forest.

## Validation

- Each new screen (bag, mart, catch prompts, Pokédex page, caught icon, shiny sprite) gets fixtures in `captures/fixtures/` and `inspect`/unit tests. The shiny matcher is also tested on synthetic renders using the extracted palettes.
- Policy unit tests on real game data:
  - catch or not;
  - shiny uses the reserve;
  - no catch below the reserve;
  - a risky foe is not caught;
  - weakening picks only non-KO moves;
  - the buy count is capped by money.
- Each milestone runs live through `tools/live-run.sh` with a fresh `--record`, is verified with contact sheets, and is saved (`.sav` + `progress.json` + `state.json`).
- **Done** when BeatMisty completes with no faint, at least one wild Pokémon caught, and Poké Balls bought at least once.
