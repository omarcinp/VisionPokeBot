# Cascade Badge segment with catching — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** From the Route 3 preparation save (IVYSAUR Lv 18 in Pewter), play to the Cascade Badge, catching new wild species by the global-state policy and keeping a Poké Ball stock by reading and buying, all verified live.

**Architecture:**
- Pure policy first: the catch decision, the weakening move, the best ball and the buy count are functions over `GameData` + `GameState` (`agent::catch`, `agent::stock`), with the per-turn risk computed by the planner's evaluator.
- New screens are probed with deterministic scripted replays, recorded in a committed probe note, and read by rule-based detectors in `crates/vision` that add fields to `Observation` (`bag`, `shop`, battle `opponent_caught` / `opponent_shiny`, `pokedex_page`).
- Menu flows are small closed-loop state machines (`agent::bag::PocketAudit`, `agent::shop::Purchase`, `agent::catch::Thrower`) driven by `StoryTask`, like `MoveLearning` today. New `StoryStep`s: `AuditPocket`, `StockUp`, `Buy`. The milestones for Route 3 → Misty live in `story::to_cerulean()`.

**Tech Stack:** Rust 2021 workspace (serde, serde_json), Python 3 offline tooling (`.venv`, Pillow/numpy) for extraction and pixel measurement, the in-process mGBA libretro emulator for probes, `tools/live-run.sh` for live runs.

**Spec:** `docs/superpowers/specs/2026-09-24-cascade-badge-and-catching-design.md` (implements Phase 2 of `docs/superpowers/specs/2026-09-23-global-state-design.md`).

## Global Constraints

- Vision only: never read emulator memory, save states or scripting APIs (`adapters/emulator-libretro/tests/no_privileged_access.rs` enforces it).
- Closed loop: every action carries an `Expectation` checked on screen, plus a timeout. Never treat elapsed frames as evidence.
- Deterministic: same observations → same decisions and events. Use `BTreeMap`/`Vec`, never `HashMap` iteration order, in anything that decides or emits events. Tie-breaks are explicit.
- `Unknown` is a legitimate state: a decision that needs an unknown fact either audits it or declines; it never guesses.
- No Pokémon may faint: the story task fails on HP 0 and the CLI reloads the last save and retries (existing behaviour; keep it).
- Species, moves and items are decompilation constants (`SPECIES_ZUBAT`, `MOVE_VINE_WHIP`, `ITEM_POKE_BALL`), never display names, in state and events.
- Catch risk limit: P(our lead faints during the attempt) ≤ 2 % (`0.02`).
- Ball stock: shiny reserve **5**, target **15**; purchases keep money for **2 Potions** (priced from game data, `ITEM_POTION` = ¥300 → ¥600).
- Planner safety: P(win) ≥ 90 % per battle (`confidence: 0.9`), unchanged.
- Fossil: **Helix Fossil** — `MtMoon_B2F` object 2 at (14, 7); talk, answer YES.
- Out of scope: Nugget Bridge / Route 24 rival / Bill; PC and party/summary audits; switching Pokémon in battle (the lead fights); Phase 3 planner costs.
- No ROMs or game assets in git (`data/world/`, `captures/`, `saves/` stay ignored). Probe *notes* (coordinates, colours, fixture names) are committed under `docs/superpowers/probes/`.
- Every bot run goes through `tools/live-run.sh <log> <pokebot args…>` with a fresh `--record` dir, so the LAN web UI (http://10.10.100.21:8080) shows it; verify with ffmpeg contact sheets (`tile=`).
- Keep `~/.cargo/bin/cargo fmt --all`, `~/.cargo/bin/cargo clippy --workspace --all-targets` (no warnings) and `~/.cargo/bin/cargo test --workspace` green. Build the release binary (`~/.cargo/bin/cargo build --release`) before any live run.
- Commit per task on branch `story-mt-moon`, message ending with the trailer `Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>`.
- Tests that need `data/world/*.json` or `captures/fixtures/*.png` skip themselves (`let Some(x) = … else { return };`) when the file is missing, as the existing tests do.

## Review Focus

1. **Buying 10+ Poké Balls adds a Premier Ball** ("I'll throw in a PREMIER BALL, too."). The bag must gain `ITEM_PREMIER_BALL` and the flow must not treat the extra page as an error. Tested in Task 8 (`premier_ball_bonus_page_is_tracked`).
2. **The ball count is unknown in a wild battle** (no audit ran, or the pocket went stale). A non-shiny catch must be declined, never attempted on a guessed count. Tested in Task 2 (`unknown_ball_count_declines_non_shiny`).
3. **A crit during weakening KOs the foe.** "Safe" moves must be judged by the critical-hit maximum, against the lowest HP the bar allows. Tested in Task 2 (`weakening_move_is_safe_even_on_a_crit`).
4. **A catch with a full party (6)** goes to the PC. It must emit `SentToPc` with the box read from text, not `PartyMonDerived` into a 7th slot. Tested in Task 9 (`full_party_sends_catch_to_pc`).
5. **The "broke free" variants** ("Oh, no! The POKéMON broke free!", "Aww! It appeared to be caught!", "Aargh! Almost had it!", "Shoot! It was so close, too!") must each read as "throw again". Unmatched text must never read as caught. Tested in Task 9 (`every_broke_free_variant_means_throw_again`).

---

## File Structure

| File | Responsibility |
|---|---|
| `tools/gamedata/extract_gamedata.py`, `tools/world/build.sh` (modify) | species `normal`/`shiny` palettes (mGBA colours); sparse checkout adds `graphics/pokemon` |
| `crates/gamedata/src/lib.rs`, `mechanics.rs` (modify) | `Species.palettes`; `ball_multiplier`; `catch_probability_status` |
| `crates/planner/src/evaluate.rs` (modify) | `faint_probability(data, attacker, defender, turns)` |
| `crates/agent/src/catch.rs` (create) | catch decision, risk, weakening move, best ball, throw flow (`Thrower`), catch text facts |
| `crates/agent/src/stock.rs` (create) | ball stock and buy-count policy |
| `tools/probe/to_script.py` (create), `tools/scripts/probe/*.txt` (create) | turn a recorded `controller.jsonl` into a replay script; probe scripts |
| `docs/superpowers/probes/2026-09-24-bag-mart-catch.md` (create) | measured coordinates/colours and the fixture list |
| `crates/state/src/observation.rs` (modify) | `BagObservation`, `ShopObservation`, `ShinyReading`; new `Observation`/`BattleObservation` fields |
| `crates/vision/src/detect/bag.rs` (create) | bag screen: pocket title, rows `NAME ×N`, ▶ row, USE/CANCEL prompt |
| `crates/vision/src/detect/shop.rs` (create) | mart: MONEY window, item list, quantity `×N` and total |
| `crates/vision/src/detect/pokedex.rs` (create) | Pokédex registration page |
| `crates/vision/src/shiny.rs` (create) | front-sprite palette matcher |
| `crates/vision/src/detect/battle.rs`, `hud.rs`, `lib.rs` (modify) | caught icon; wire new detectors into `FireRedPerception` |
| `crates/agent/src/bag.rs` (create) | `PocketAudit` (Start → BAG → pocket → read → close) |
| `crates/agent/src/shop.rs` (create) | `Purchase` (clerk → BUY → item → quantity → YES → confirm → close) |
| `crates/agent/src/track.rs` (modify) | "used POKé BALL!", "Gotcha!", "transferred to … PC / BOX N", purchase facts |
| `crates/agent/src/battle.rs` (modify) | catch mode: status opener, safe moves, BAG, RUN on risk |
| `crates/agent/src/story.rs` (modify) | `StoryStep::{AuditPocket, StockUp, Buy}`; catching and mandatory-buy hooks; `to_cerulean()` |
| `apps/pokebot-cli/src/main.rs` (modify) | load palettes; `inspect` prints the new observations |
| `docs/architecture.md`, `docs/HANDOFF.md`, `README.md` (modify) | document the segment |

---

### Task 1: Species palettes and ball data

**Files:**
- Modify: `tools/world/build.sh:13`
- Modify: `tools/gamedata/extract_gamedata.py`
- Modify: `crates/gamedata/src/lib.rs`, `crates/gamedata/src/mechanics.rs`

**Interfaces:**
- Produces:
  - `Species.palettes: Option<Palettes>` where `pub struct Palettes { pub normal: Vec<[u8; 3]>, pub shiny: Vec<[u8; 3]> }` (16 colours each, already converted the mGBA way; index 0 is transparent);
  - `pub fn ball_multiplier(item: &str) -> Option<u32>` in `mechanics` (×10; `None` = not a ball we auto-use);
  - `pub fn catch_probability_status(catch_rate: u16, max_hp: u32, current_hp: u32, ball: u32, status_x10: u32) -> f64` in `mechanics` (`status_x10`: 20 asleep/frozen, 15 paralysed/poisoned/burned, 10 none). `catch_probability` keeps its signature and calls it with 10.

- [ ] **Step 1: Sparse checkout adds the Pokémon graphics.** In `tools/world/build.sh` change line 13 to:

```bash
git -C "${PRET}" sparse-checkout add graphics/fonts data/scripts graphics/pokemon
```

- [ ] **Step 2: Extract palettes.** In `extract_gamedata.py` add, after `move_data`:

```python
def gba_rgb(r8, g8, b8):
    """Colour as mGBA outputs it (RGB565 via libretro); same as
    tools/world/extract_world.py."""
    r5, g5, b5 = r8 >> 3, g8 >> 3, b8 >> 3
    expand5 = lambda v: (v << 3) | (v >> 2)
    g6 = g5 << 1
    return [expand5(r5), (g6 << 2) | (g6 >> 4), expand5(b5)]


def read_pal(path):
    values = list(map(int, path.read_text().split()[3:3 + 48]))
    return [gba_rgb(*values[i * 3:i * 3 + 3]) for i in range(16)]


def species_palettes(pret, species):
    """graphics/pokemon/<name>/{normal,shiny}.pal per species (directory
    names are the lower-case species constant; NIDORAN_F → nidoran_f)."""
    root = pret / "graphics/pokemon"
    for name, sp in species.items():
        d = root / name.removeprefix("SPECIES_").lower()
        normal, shiny = d / "normal.pal", d / "shiny.pal"
        if normal.exists() and shiny.exists():
            sp["palettes"] = {"normal": read_pal(normal), "shiny": read_pal(shiny)}
```

and in `main()` call `species_palettes(pret, data["species"])` right after `data` is built. Check the directory naming against the checkout (`ls data/pret-pokefirered/graphics/pokemon | head`) and adjust the name mapping if some species differ (e.g. `mr_mime`, `farfetchd`); the extractor must print how many species got palettes (`print(f"palettes: {n}")`), and it must be ≥ 151.

- [ ] **Step 3: Write the failing Rust tests** at the end of `lookup_tests` in `crates/gamedata/src/lib.rs`:

```rust
    #[test]
    fn species_have_normal_and_shiny_palettes() {
        let Some(d) = data() else { return };
        let p = d.species["SPECIES_ZUBAT"].palettes.as_ref().expect("palettes");
        assert_eq!(p.normal.len(), 16);
        assert_eq!(p.shiny.len(), 16);
        assert_ne!(p.normal, p.shiny);
        // mGBA colours: every channel has its low bits copied from the high ones.
        for c in p.normal.iter().chain(&p.shiny) {
            assert_eq!(c[0] & 7, c[0] >> 5);
        }
    }
```

and in `mechanics::tests`:

```rust
    #[test]
    fn balls_and_status_raise_the_catch_chance() {
        assert_eq!(ball_multiplier("ITEM_POKE_BALL"), Some(10));
        assert_eq!(ball_multiplier("ITEM_GREAT_BALL"), Some(15));
        assert_eq!(ball_multiplier("ITEM_ULTRA_BALL"), Some(20));
        assert_eq!(ball_multiplier("ITEM_PREMIER_BALL"), Some(10));
        assert_eq!(ball_multiplier("ITEM_MASTER_BALL"), None);
        assert_eq!(ball_multiplier("ITEM_POTION"), None);
        let plain = catch_probability_status(45, 30, 7, 10, 10);
        assert_eq!(plain, catch_probability(45, 30, 7, 10));
        assert!(catch_probability_status(45, 30, 7, 10, 20) > plain);
        assert!(catch_probability_status(45, 30, 7, 15, 10) > plain);
    }
```

- [ ] **Step 4: Run to verify failure.** `~/.cargo/bin/cargo test -p pokebot-gamedata` → compile errors (`palettes`, `ball_multiplier`, `catch_probability_status` missing).

- [ ] **Step 5: Implement.** In `lib.rs`:

```rust
/// A species' sprite palettes (16 colours, mGBA output; index 0 is transparent).
#[derive(Debug, Clone, Deserialize)]
pub struct Palettes {
    pub normal: Vec<[u8; 3]>,
    pub shiny: Vec<[u8; 3]>,
}
```

and add `#[serde(default)] pub palettes: Option<Palettes>,` to `Species`. In `mechanics.rs`:

```rust
/// Catch multiplier ×10 for the balls the bot throws on its own. The Master
/// Ball is never auto-used; balls with conditional bonuses (Net, Nest, …)
/// count as a Poké Ball.
pub fn ball_multiplier(item: &str) -> Option<u32> {
    match item {
        "ITEM_ULTRA_BALL" => Some(20),
        "ITEM_GREAT_BALL" | "ITEM_SAFARI_BALL" => Some(15),
        "ITEM_MASTER_BALL" => None,
        i if i.ends_with("_BALL") => Some(10),
        _ => None,
    }
}

/// `catch_probability` with the status bonus ×10 (20 asleep or frozen, 15
/// paralysed, poisoned or burned, 10 none).
pub fn catch_probability_status(
    catch_rate: u16,
    max_hp: u32,
    current_hp: u32,
    ball: u32,
    status_x10: u32,
) -> f64 {
    let hp_term = (3 * max_hp).saturating_sub(2 * current_hp).max(1);
    let a = (f64::from(hp_term) * f64::from(catch_rate) * f64::from(ball) / 10.0)
        / f64::from(3 * max_hp)
        * f64::from(status_x10)
        / 10.0;
    if a >= 255.0 {
        return 1.0;
    }
    let b = 1_048_560.0 / (16_711_680.0 / a.max(1.0)).sqrt().sqrt();
    (b / 65536.0).powi(4).min(1.0)
}
```

and make `catch_probability` return `catch_probability_status(catch_rate, max_hp, current_hp, ball, 10)`.

- [ ] **Step 6: Rebuild the data and run the tests.** `tools/world/build.sh` (prints `palettes: N`), then `~/.cargo/bin/cargo test -p pokebot-gamedata` → PASS; `~/.cargo/bin/cargo test --workspace` stays green.

- [ ] **Step 7: Commit.**

```bash
git add tools/world/build.sh tools/gamedata/extract_gamedata.py crates/gamedata/src
git commit -m "gamedata: species normal/shiny palettes, ball multipliers, status catch bonus"
```

---

### Task 2: Catch and stock policy (pure)

**Files:**
- Modify: `crates/planner/src/evaluate.rs`
- Create: `crates/agent/src/catch.rs` (policy part), `crates/agent/src/stock.rs`
- Modify: `crates/agent/src/lib.rs` (`pub mod catch; pub mod stock;`)

**Interfaces:**
- Consumes: Task 1 `ball_multiplier`, `catch_probability_status`.
- Produces (later tasks rely on these exact names):

```rust
// planner::evaluate
/// P(`defender` faints within `turns` attacks of `attacker`'s most damaging move), from `defender.hp`.
pub fn faint_probability(data: &GameData, attacker: &Combatant, defender: &Combatant, turns: usize) -> f64;

// agent::catch
pub const RISK_LIMIT: f64 = 0.02;
pub const WEAKENED_PER_MILLE: u16 = 250;
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum FoeStatus { None, Asleep, Paralyzed }
pub struct Foe { pub species: String, pub level: u8, pub hp_per_mille: u16, pub status: FoeStatus, pub shiny: bool, pub caught: Option<bool> }
pub struct Lead<'a> { pub member: &'a crate::party::Member, pub hp: (u16, u16) }
#[derive(Debug, Clone, PartialEq)] pub struct CatchPlan { pub ball: String, pub status_move: Option<(u8, String)>, pub expected_throws: u32, pub risk: f64, pub shiny: bool }
pub fn balls_held(state: &GameState) -> Option<Vec<(String, u16)>>;  // PokeBalls pocket value; None when unknown
pub fn best_ball(data: &GameData, balls: &[(String, u16)]) -> Option<String>;
pub fn status_move(data: &GameData, lead: &crate::party::Member, foe: FoeStatus) -> Option<(u8, String)>;
pub fn weakening_move(data: &GameData, lead: &Lead, foe: &Foe) -> Option<(u8, String)>;
pub fn risk(data: &GameData, lead: &Lead, foe: &Foe, turns: u32) -> f64;
pub fn plan_catch(data: &GameData, state: &GameState, lead: &Lead, foe: &Foe, trainer: bool) -> Result<CatchPlan, String>;

// agent::stock
pub const SHINY_RESERVE: u16 = 5;
pub const TARGET_STOCK: u16 = 15;
pub const POTIONS_KEPT: u32 = 2;
pub fn ball_count(state: &GameState) -> Option<u16>;       // all balls in the pocket (Master Ball excluded)
pub fn buy_count(data: &GameData, stock: u16, money: u32) -> u16; // Poké Balls to buy to reach TARGET_STOCK, capped by money − 2 Potions
pub fn must_buy(stock: Option<u16>) -> bool;              // known and < SHINY_RESERVE
pub fn should_buy(stock: Option<u16>) -> bool;            // unknown or < TARGET_STOCK
```

Semantics (binding):
- `plan_catch` returns `Err(reason)` (logged, no catch) when: `trainer`; `foe.caught != Some(false)` and not shiny; the ball pocket is unknown and not shiny; no ball with a multiplier is held; for non-shiny, `ball_count <= SHINY_RESERVE + expected_throws`; for non-shiny, `risk > RISK_LIMIT`.
- For a shiny, the reserve and the risk limit don't block: `plan_catch` returns `Ok` whenever a ball is held; its `status_move` is `None` when the risk with weakening exceeds the limit (it throws at once). Unknown pocket for a shiny → `Ok` with `ball: "ITEM_POKE_BALL"`, `expected_throws` computed for it (the throw flow reads the real pocket in battle).
- Expected throws: `ceil(1 / p)` capped at 20, where `p = catch_probability_status(catch_rate, max_hp, max_hp * 250 / 1000, best ball, status)` — the HP the weakening aims for (or the current HP when no safe move exists) and the status the opener will cause (sleep 20, paralysis 15, none 10).
- Weakening turns: `(status_move.is_some() as u32) + ceil(max(0, hp_now − max_hp/4) / mean damage of the weakening move)` capped at 10; 0 when no safe move.
- `risk(…, turns)` = `faint_probability(foe (iv 31), us (iv 0, hp = lead.hp.0), turns)`.
- Foe HP floor for "safe": `max_hp(iv 0) * hp_per_mille / 1000`, minus one bar pixel (`max_hp / 48`), at least 1. A move is safe iff the **critical** maximum roll (our iv 31 vs foe iv 0) is `<` that floor. Ties between safe moves: highest normal max roll (fastest weakening), then move name.
- `status_move`: prefer a sleep move (`effect == "EFFECT_SLEEP"`) over paralysis (`EFFECT_PARALYZE`); only with PP left; `None` if `foe != FoeStatus::None`. Ties: slot order.
- `best_ball`: highest `ball_multiplier`, ties to the cheapest `items[..].price`, then name.
- `buy_count`: `need = TARGET_STOCK.saturating_sub(stock)`; `budget = money.saturating_sub(POTIONS_KEPT * price(ITEM_POTION))`; `min(need, budget / price(ITEM_POKE_BALL))`.

- [ ] **Step 1: Failing test for `faint_probability`** in `crates/planner/tests/brock.rs` (append):

```rust
#[test]
fn faint_probability_grows_with_turns() {
    let Some(data) = data() else { return };
    let foe = Combatant::new(&data, "SPECIES_GEODUDE", 9, data.default_moves("SPECIES_GEODUDE", 9), 31).unwrap();
    let mut us = Combatant::new(&data, "SPECIES_IVYSAUR", 18, vec!["MOVE_VINE_WHIP".into()], 0).unwrap();
    let one = faint_probability(&data, &foe, &us, 1);
    let ten = faint_probability(&data, &foe, &us, 10);
    assert!(one <= ten && ten <= 1.0);
    us.hp = 1;
    assert!(faint_probability(&data, &foe, &us, 1) > 0.5);
}
```

(Use the file's existing `data()` helper and imports; add `faint_probability` to the `use`.)

- [ ] **Step 2: Implement `faint_probability`** in `evaluate.rs`:

```rust
/// Probability that `defender` faints within `turns` attacks of `attacker`'s
/// most damaging move (every attack is that move), from `defender.hp`.
pub fn faint_probability(
    data: &GameData,
    attacker: &Combatant,
    defender: &Combatant,
    turns: usize,
) -> f64 {
    let Some((_, rolls)) = best_move(data, attacker, defender) else {
        return 0.0;
    };
    ko_distribution(&rolls, defender.hp)
        .iter()
        .take(turns.min(MAX_TURNS))
        .sum::<f64>()
        .min(1.0)
}
```

Run `~/.cargo/bin/cargo test -p pokebot-planner` → PASS.

- [ ] **Step 3: Failing policy tests** in `crates/agent/src/catch.rs` (`#[cfg(test)] mod tests`), on real game data. Build the state with `DefaultReducer` and events (`PocketObserved { pocket: Pocket::PokeBalls, items: vec![("ITEM_POKE_BALL".into(), n)] }`), and the lead with `Member::new(&data, "SPECIES_IVYSAUR", 18)` whose `moves` are `TACKLE, SLEEP_POWDER, LEECH_SEED, VINE_WHIP` (as in `battle.rs` tests):

```rust
#[test] fn uncaught_weak_foe_is_caught() // Pidgey Lv6, 1000‰, caught Some(false), 10 balls, lead 54/54 → Ok; status_move = Sleep Powder (slot 1); risk <= RISK_LIMIT
#[test] fn caught_species_is_not_caught_again() // caught Some(true) → Err
#[test] fn unknown_caught_flag_declines() // caught None, not shiny → Err
#[test] fn unknown_ball_count_declines_non_shiny() // no PocketObserved → Err
#[test] fn no_catch_at_or_below_the_reserve() // 6 balls, expected_throws ≥ 1 → Err (6 <= 5 + throws)
#[test] fn shiny_uses_the_reserve() // 3 balls, shiny → Ok with ball ITEM_POKE_BALL
#[test] fn risky_foe_is_not_caught() // lead hp (3, 54) vs Geodude Lv9 → Err mentioning "risk"
#[test] fn trainer_mons_are_never_caught() // trainer=true → Err
#[test] fn weakening_move_is_safe_even_on_a_crit() // Mankey Lv7 at 400‰: returned move's crit max < floor; at 60‰ → None
#[test] fn asleep_foe_gets_no_status_move() // FoeStatus::Asleep → status_move None
#[test] fn best_ball_prefers_multiplier_then_price() // [(POKE,5),(GREAT,1)] → GREAT; [(PREMIER,1),(POKE,1)] → the cheaper of the two by price
```

and in `crates/agent/src/stock.rs`:

```rust
#[test] fn buy_count_is_capped_by_money_minus_two_potions() // stock 3, money 1000 → (1000-600)/200 = 2
#[test] fn buy_count_reaches_the_target() // stock 3, money 99999 → 12
#[test] fn no_money_buys_nothing() // money 500 → 0
#[test] fn buying_rules() // must_buy(Some(4)) true, must_buy(None) false, should_buy(None) true, should_buy(Some(15)) false
```

Write every test body fully (the comments above give the exact inputs and assertions). Run `~/.cargo/bin/cargo test -p pokebot-agent catch stock` → FAIL (not defined).

- [ ] **Step 4: Implement `stock.rs`** exactly per the semantics above. `ball_count` sums the counts of items in the PokeBalls pocket whose `ball_multiplier` is `Some`.

- [ ] **Step 5: Implement the policy part of `catch.rs`** per the semantics above. Build combatants with `pokebot_planner::Combatant::new`; set `us.hp = u32::from(lead.hp.0)`. The foe's moves are `data.default_moves(species, level)`. Keep each function under ~40 lines; `plan_catch` composes them. Module doc comment: one paragraph describing the policy (spec §1 Decision).

- [ ] **Step 6: Run** `~/.cargo/bin/cargo test -p pokebot-agent` → PASS; `fmt`, `clippy` clean.

- [ ] **Step 7: Commit.**

```bash
git add crates/planner crates/agent/src/catch.rs crates/agent/src/stock.rs crates/agent/src/lib.rs
git commit -m "agent: catch policy (risk, weakening, best ball) and ball stock policy"
```

---

### Task 3: Probe tooling and fixtures

Live, exploratory. The deliverable is a committed probe note that later tasks read for coordinates and colours, plus fixtures in `captures/fixtures/` (gitignored).

**Files:**
- Create: `tools/probe/to_script.py`
- Create: `tools/scripts/probe/README.md`, and one probe script per screen group (`mart.txt`, `field-bag.txt`, `battle-catch.txt`)
- Create: `docs/superpowers/probes/2026-09-24-bag-mart-catch.md`

**Interfaces:**
- Produces: fixtures (exact names — later tasks' tests load them):
  - `captures/fixtures/bag-items.png`, `bag-pokeballs.png`, `bag-pokeballs-cursor1.png` (field bag, Start → BAG; POKé BALLS pocket; ▶ on row 1), `bag-use-prompt.png` (the USE/…/CANCEL window over the battle bag);
  - `captures/fixtures/mart-menu.png` (BUY/SELL/SEE YA!), `mart-list.png` (item list + MONEY window), `mart-quantity-1.png`, `mart-quantity-3.png` (quantity box ×01/×03 with total), `mart-confirm.png` ("That will be ¥…. OK?" YES/NO);
  - `captures/fixtures/battle-wild-uncaught.png`, `battle-wild-caught.png` (HUD without / with the caught ball icon), `battle-throw.png` ("RED used POKé BALL!"), `battle-broke-free.png`, `battle-gotcha.png`, `pokedex-page.png`, `nickname-prompt.png`;
  - `captures/fixtures/mtmoon-1f.png` (overworld inside Mt. Moon, for Task 10).
- The probe note records, per screen: fixture → what it shows; the geometry measured (window rectangles, row pitch, text regions, cursor shape and colour, title region, icon position and colours, sprite box of the wild Pokémon's front sprite in battle); the text as it should read; the exact button path used.

- [ ] **Step 1: `to_script.py`.** Reads a session's `controller.jsonl` (`command_id, elapsed_us, after_frame_id, command`) and writes a script (format in `apps/pokebot-cli/src/script.rs`) that reproduces it on the stepped emulator: for each command, `wait <after_frame_id − previous frame>` then the command (`press X`, `chord A+B`, `hold X Nms`, `sequence …`). Options: `--until-frame N` (stop before commands after N), `--append FILE` (extra lines). Serialized `ControllerCommand` JSON forms: read one line of an existing `captures/live-story/controller.jsonl` to confirm them before writing the converter. Self-check: converting and replaying `captures/live-story` with `pokebot run --video emulator --controller emulator --script …` ends on the same frame fingerprint as the recording (compare the last `frames.jsonl` fingerprint).

- [ ] **Step 2: Reach the screens.** Restore the Route 3 checkpoint first (copy `saves/route3-ready.sav` over `roms/Pokemon - Fire Red Version (U) (V1.1).sav`, `saves/progress.route3-ready.json` over `saves/progress.json`, `saves/state.route3-ready.json` over `saves/state.json`; back up the current three files to `/tmp/` first). Then:
  1. Record a live `story --continue --milestones 0`-style start (title → CONTINUE) with `--record`, or use `pokebot play --record` to walk by hand; convert with `to_script.py`, and append probe inputs with `screenshot` lines.
  2. **Mart:** from the Pewter PC walk to `PewterCity_Mart`, talk to the clerk (object 3 at (2, 3)), BUY, move to POKé BALL, A, Up ×2, A (YES) — screenshot each screen. Buy 3 Poké Balls so the bag has some.
  3. **Field bag:** Start → BAG → Right/Left to POKé BALLS → Down → screenshots; B until closed.
  4. **Battle:** walk into Route 3 grass until a wild battle; BAG from the command menu (FIGHT → Right = BAG), pocket to POKé BALLS, A on a ball → the USE prompt, USE → throw text → shakes → result. Repeat encounters until one catch succeeds (Pokédex page, nickname prompt) and one battle against a species caught before (caught icon). Weaken with Tackle first if needed.
  5. **Mt. Moon:** from Route 4's west end enter `MtMoon_1F` and screenshot a few overworld frames.
  Keep the scripts that reach each screen in `tools/scripts/probe/` (they replay only against the route3-ready save — say so at the top of each).

- [ ] **Step 3: Measure.** With `.venv/bin/python` + PIL/numpy, measure what the probe note must record (see Interfaces). Check readings with the existing font: `./target/release/pokebot inspect captures/fixtures/<f>.png` (dialogue lines appear for message boxes). For the list windows, find where the normal font reads each row by trying `Font::read` regions in a scratch test or a Python re-implementation; record the regions that read cleanly.

- [ ] **Step 4: Write the probe note** `docs/superpowers/probes/2026-09-24-bag-mart-catch.md` with one section per screen (fixture list, geometry, colours, expected reading, button path) and a "Surprises" section for anything that differs from the spec's flow (e.g. extra pages, a remembered pocket, the Premier Ball bonus page).

- [ ] **Step 5: Restore the saves** you backed up in Step 2 (the probe bought balls in a replay only if you used the stepped emulator with `--no-save`; if a live run saved, restore anyway so later tasks start from route3-ready).

- [ ] **Step 6: Commit.**

```bash
git add tools/probe tools/scripts/probe docs/superpowers/probes
git commit -m "probe: controller.jsonl → replay script; bag, mart and catch screen measurements"
```

---

### Task 4: Bag screen reader

**Files:**
- Modify: `crates/state/src/observation.rs`
- Create: `crates/vision/src/detect/bag.rs`; modify `crates/vision/src/detect/mod.rs`, `crates/vision/src/lib.rs`
- Modify: `apps/pokebot-cli/src/main.rs` (`describe_observation`)

**Interfaces:**
- Consumes: probe note §Bag; fixtures `bag-*.png`.
- Produces:

```rust
// pokebot_state
pub struct BagObservation {
    /// Pocket title as read ("POKé BALLS"), `?` for unknown glyphs.
    pub pocket: String,
    /// Visible rows, top to bottom: (name as read, count). CANCEL reads as ("CANCEL", None).
    pub rows: Vec<(String, Option<u16>)>,
    /// Index into `rows` of the ▶.
    pub cursor: Option<u8>,
    /// The USE/…/CANCEL action window is open; its options as read and the ▶ row.
    pub prompt: Option<(Vec<String>, u8)>,
}
// Observation gains: #[serde(default)] pub bag: Option<BagObservation>
```

`FireRedPerception::observe` sets `screen` to `ScreenState::Bag` (overworld) or `ScreenState::BattleBag` (opened from battle, if the probe shows a difference; otherwise `Bag`) and fills `bag`, returning early like `move_list` does. A helper `pub fn pocket_from_title(title: &str) -> Option<Pocket>` in `agent::bag` (Task 7) maps titles; keep the vision side free of item/pocket knowledge.

- [ ] **Step 1: Failing fixture tests** in `bag.rs`:

```rust
#[test]
fn reads_the_poke_balls_pocket() {
    let Some((image, font)) = fixture("bag-pokeballs.png") else { return };
    let bag = detect(&image, &font).expect("bag");
    assert_eq!(bag.pocket, "POKé BALLS");
    assert_eq!(bag.rows.first(), Some(&("POKé BALL".to_owned(), Some(N))));  // N = the count recorded in the probe note
    assert_eq!(bag.rows.last().map(|r| r.0.as_str()), Some("CANCEL"));
    assert_eq!(bag.cursor, Some(0));
}
#[test] fn cursor_row_follows_the_arrow() // bag-pokeballs-cursor1.png → cursor Some(1)
#[test] fn use_prompt_is_read() // bag-use-prompt.png → prompt Some((opts, 0)) with opts[0] == "USE" and opts.last() == "CANCEL"
#[test] fn other_screens_are_not_bags() // move-select.png, learn-list-row0.png, mart-list.png → None
```

`fixture()` loads the PNG with `pokebot_video::png::load` and `font_normal.json` (skip on missing). Also add one synthetic test that draws a bag-like window with `Font::render` (the rows `POKé BALL ×12`, `CANCEL`) on a background filled with the colours from the probe note, and checks `rows`.

- [ ] **Step 2: Run** `~/.cargo/bin/cargo test -p pokebot-vision bag` → FAIL.

- [ ] **Step 3: Implement `detect(image, font) -> Option<BagObservation>`** using the probe note's geometry: a cheap colour gate first (bag background/pocket banner), then the title read, the ▶ search (reuse `menu::find_cursor` if the bag uses the gray ▶; otherwise a local cursor finder like `battle::find_cursor`), the row reads (name left, `×N` right: parse digits after `×`; the font must have `×` — check `font.glyph("×")`, and if it is missing, parse the count from the right-hand digits), and the prompt window. Wire it into `observe` before `dialogue::detect`, like `move_list`.

- [ ] **Step 4: `describe_observation`** prints `bag: <pocket> [rows] ▶<cursor> prompt …` when present. Run `./target/release/pokebot inspect captures/fixtures/bag-*.png` and check the readings match the probe note.

- [ ] **Step 5: Run** the workspace tests, `fmt`, `clippy` → green.

- [ ] **Step 6: Commit** `git commit -m "vision: bag screen (pocket title, NAME ×N rows, cursor, USE prompt)"`.

---

### Task 5: Mart reader

**Files:**
- Modify: `crates/state/src/observation.rs`
- Create: `crates/vision/src/detect/shop.rs`; modify `detect/mod.rs`, `lib.rs`, `apps/pokebot-cli/src/main.rs`

**Interfaces:**
- Consumes: probe note §Mart; fixtures `mart-*.png`.
- Produces:

```rust
pub struct ShopObservation {
    /// MONEY window amount, when shown.
    pub money: Option<u32>,
    /// Item list rows as read (name, price); CANCEL reads as ("CANCEL", None).
    pub items: Vec<(String, Option<u32>)>,
    /// ▶ row in the item list.
    pub cursor: Option<u8>,
    /// Quantity box: (count, total price).
    pub quantity: Option<(u16, u32)>,
}
// Observation gains: #[serde(default)] pub shop: Option<ShopObservation>
```

Screen: `ScreenState::Shop` while the item list or quantity box is visible. The BUY/SELL/SEE YA! menu and the "OK?" YES/NO stay what they are today (menu over dialogue) — the flow reads their text from `dialogue.lines`.

- [ ] **Step 1: Failing fixture tests** in `shop.rs`: `mart-list.png` → `money == Some(<probe>)`, `items[0] == ("POKé BALL", Some(200))`, `cursor == Some(0)`; `mart-quantity-1.png` → `quantity == Some((1, 200))`; `mart-quantity-3.png` → `Some((3, 600))`; `bag-pokeballs.png`, `move-select.png` → `None`. Money and prices print with `¥` (the text tracker already parses `¥`); strip `¥` and `,` before parsing.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** from the probe note; wire into `observe` (before `dialogue::detect`, after `bag`), and into `describe_observation`.
- [ ] **Step 4: Run** tests + `inspect` on the mart fixtures; `fmt`, `clippy`.
- [ ] **Step 5: Commit** `git commit -m "vision: mart window (money, item list, quantity and total)"`.

---

### Task 6: Caught icon, shiny matcher and Pokédex page

**Files:**
- Create: `crates/vision/src/shiny.rs`, `crates/vision/src/detect/pokedex.rs`
- Modify: `crates/vision/src/detect/battle.rs`, `hud.rs`, `detect/mod.rs`, `lib.rs`; `crates/state/src/observation.rs`; `apps/pokebot-cli/src/main.rs` (story + inspect load palettes)

**Interfaces:**
- Consumes: Task 1 `Palettes`; probe note §Battle (icon position/colours, front-sprite box), §Pokédex page.
- Produces:

```rust
// pokebot_state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShinyReading { Normal, Shiny, Unclear }
// BattleObservation gains:
#[serde(default)] pub opponent_caught: Option<bool>,       // icon read only when the HUD name was read
#[serde(default)] pub opponent_shiny: Option<ShinyReading>, // only when palettes are loaded and the name resolves
// Observation gains:
#[serde(default)] pub pokedex_page: bool,

// pokebot_vision
pub mod shiny {
    /// Species display name → (normal, shiny) palettes.
    pub type SpritePalettes = std::collections::BTreeMap<String, ([Rgb; 16], [Rgb; 16])>;
    /// Classifies the opaque pixels of `region` (colours not equal to the
    /// background sampled at the region's corners).
    pub fn classify(image: &RgbImage, region: Region, normal: &[Rgb; 16], shiny: &[Rgb; 16]) -> ShinyReading;
}
impl FireRedPerception { pub fn with_palettes(self, p: Arc<shiny::SpritePalettes>) -> Self; }
```

Shiny rule (binding): count opaque pixels that are within tolerance 8 of some colour in only the normal palette (`n`) and only the shiny palette (`s`); colours in both count for neither. `Shiny` iff `s >= 40 && s >= 4 * n`; `Normal` iff `n >= 40 && n >= 4 * s`; else `Unclear`. Unclear is treated as not shiny by the agent and logged. Only classify once the sprite is fully on screen (the HUD bar is visible).

- [ ] **Step 1: Failing shiny tests** (synthetic, no fixture needed; skip without `gamedata.json`): render a 64×64 sprite region by painting a pattern of palette indices 1..15 in Zubat's normal palette → `Normal`; same pattern in the shiny palette → `Shiny`; a half/half mix → `Unclear`; background-only → `Unclear`.
- [ ] **Step 2: Failing fixture tests**: `battle-wild-uncaught.png` → `opponent_caught == Some(false)`; `battle-wild-caught.png` → `Some(true)`; with palettes loaded, both → `opponent_shiny == Some(Normal)`; `pokedex-page.png` → `pokedex_page`; `battle-gotcha.png` and `mart-list.png` → `!pokedex_page`.
- [ ] **Step 3: Run** → FAIL.
- [ ] **Step 4: Implement** `shiny::classify`, the icon check in `battle.rs` (fill `opponent_caught` only when `opponent_name` is `Some`), `pokedex::is_page`, and `with_palettes` (built in the CLI from `GameData.species`: key `party::display_name(species)` → palettes; put the builder in `apps/pokebot-cli/src/main.rs` next to the font loading for `story` and `inspect`). Resolve the HUD name against the palette keys with `hud::resolve`.
- [ ] **Step 5: Run** tests + `inspect`; `fmt`, `clippy`.
- [ ] **Step 6: Commit** `git commit -m "vision: caught icon, shiny palette matcher, Pokédex page"`.

---

### Task 7: Poké Balls pocket audit

**Files:**
- Create: `crates/agent/src/bag.rs`; modify `crates/agent/src/lib.rs`
- Modify: `crates/agent/src/story.rs` (`StoryStep::AuditPocket`)
- Modify: `crates/agent/src/action.rs` (new expectations)

**Interfaces:**
- Consumes: Task 4 `BagObservation`.
- Produces:

```rust
// agent::bag
pub fn pocket_from_title(title: &str) -> Option<Pocket>;       // "ITEMS", "KEY ITEMS", "POKé BALLS" (wildcards allowed, unique match)
pub fn read_rows(data: &GameData, rows: &[(String, Option<u16>)]) -> Option<ItemList>; // None if any non-CANCEL row doesn't resolve uniquely via item_named
pub struct PocketAudit { /* phase, collected rows, attempts */ }
impl PocketAudit {
    pub fn new(pocket: Pocket) -> Self;
    /// Next input, or Done once the pocket was read and every menu is closed.
    pub fn next(&mut self, o: &Observation, data: &GameData, events: &mut Vec<GameEvent>) -> Decision;
}
// action::Expectation gains:
BagPocket(String),       // bag open on a pocket whose title reads this
BagCursorAt(u8),
BagClosed,               // no bag and no menu
// StoryStep gains:
AuditPocket(Pocket),
```

Flow (binding, spec §2): open Start (expect `MenuOpen`) → move the ▶ to BAG by reading the Start menu's rows with the font (`menu.window` region; find the row whose text is `BAG`; step with `MenuCursorMoved`) → A (expect `BagPocket(any)`) → Left/Right toward the wanted pocket (expect the title to change; pocket order ITEMS, KEY ITEMS, POKé BALLS) → collect visible rows; press Down (expect `BagCursorAt(cursor+1)`) until `CANCEL` is visible or the rows repeat; merge rows in order without duplicates → emit `PocketObserved` → B until `BagClosed` and the overworld is located again. More than 12 retries in one phase → `Decision::Fail`.

- [ ] **Step 1: Failing unit tests** in `bag.rs`: `pocket_from_title("POKé BALLS") == Some(Pocket::PokeBalls)`, `pocket_from_title("?TEMS") == Some(Pocket::Items)`; `read_rows` on `[("POKé BALL", Some(3)), ("CANCEL", None)]` → `Some(vec![("ITEM_POKE_BALL", 3)])`, and with an unresolvable row → `None`. A flow test drives `PocketAudit` with synthetic `Observation`s (Start menu with rows read …, bag on ITEMS, bag on POKé BALLS with CANCEL visible, overworld) and asserts the decisions' labels/buttons and the single `PocketObserved` event.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `bag.rs`, the expectations, and in `StoryTask::next` a branch for `StoryStep::AuditPocket(p)` that owns a `PocketAudit` (stored in the task, reset in `advance_step`/`splice`) and advances when it returns `Done`. The Start-menu row text needs a reading: add `MenuObservation`-region reading in the agent via the dialogue font — if `Observation` has no menu text today, add `#[serde(default)] pub lines: Vec<String>` to `MenuObservation` filled by `FireRedPerception` with `font.read(image, menu.window, &[cursor region])` (only when `menu` is `Some`).
- [ ] **Step 4: Run** the tests. Then check the observations on the stepped emulator: `./target/release/pokebot run --video emulator --controller emulator --no-save --script tools/scripts/probe/field-bag.txt --record /tmp/bag-check`, then `./target/release/pokebot replay /tmp/bag-check`, and confirm the bag observations show the expected pocket and rows. The flow itself is proven live in Task 11: StockUpPewter starts with the audit.
- [ ] **Step 5: Commit** `git commit -m "agent: Poké Balls pocket audit (Start → BAG → pocket → read → close)"`.

---

### Task 8: Buying at a mart and StockUp

**Files:**
- Create: `crates/agent/src/shop.rs`
- Modify: `crates/agent/src/track.rs`, `crates/agent/src/story.rs`, `crates/agent/src/lib.rs`, `crates/agent/src/action.rs`

**Interfaces:**
- Consumes: Task 2 `stock::{ball_count, buy_count, must_buy, should_buy}`; Task 5 `ShopObservation`; Task 7 `AuditPocket`.
- Produces:

```rust
// agent::shop
pub struct Purchase { /* item, count, phase, money_before */ }
impl Purchase {
    pub fn new(item: &str, count: u16) -> Self;
    pub fn next(&mut self, o: &Observation, data: &GameData, events: &mut Vec<GameEvent>) -> Decision;
}
/// Nearest map (fewest maps crossed) whose mart sells `item`, and its clerk's local id.
pub fn nearest_mart(world: &World, data: &GameData, from: &str, item: &str) -> Option<(String, u32)>;
// StoryStep gains:
Buy { item: String, count: u16 },
/// Audit if needed, then Buy up to the target at `mart` (or the nearest one selling balls).
StockUp { mart: Option<String> },
// action::Expectation gains:
ShopCursorAt(u8), ShopQuantity(u16), ShopClosed,
```

Flow (binding, spec §2 Buying): approach the clerk (`OBJ_EVENT_GFX_CLERK` object of the mart map) with the existing `talk_to` approach/press → the BUY/SELL/SEE YA! question menu → select BUY (row whose read text is `BUY`) → item list: emit `MoneyObserved { amount }` from `shop.money` once; move ▶ to the row whose name resolves to `item` → A → quantity box: Up until `quantity.0 == count` (each press expects `ShopQuantity(n+1)`; if Up wraps or overshoots, Down) → A → "That will be ¥T. OK?" → YES → "Here you are!" page → back on the list: confirm `shop.money == money_before − T` → emit `ItemsChanged { pocket: PokeBalls, item, delta: count, reason: "bought" }` and `MoneyChanged { delta: -T, reason: "bought" }`; a money mismatch emits `MoneyObserved` with the new amount and a `GoalProgress` warning instead of the delta → B out of the list, answer the menu's SEE YA!/B, advance the closing text until `ShopClosed` and the overworld is located.
- The "PREMIER BALL, too." page (≥10 Poké Balls) → `ItemsChanged { pocket: PokeBalls, item: "ITEM_PREMIER_BALL", delta: 1, reason: "bonus" }` via `TextTracker` (a page containing `PREMIER BALL, too`).
- `StockUp { mart }`: if the PokeBalls pocket `needs_audit()` → splice `AuditPocket(PokeBalls)` before it; else compute `buy_count(data, stock, money)`, where money is unknown until the list shows it — so splice `Buy { item: "ITEM_POKE_BALL", count: 0 }` meaning "decide on the list": `Purchase` with `count == 0` computes the count from `shop.money` once the list is open, and closes without buying when it is 0. Skip entirely when `!should_buy(stock)`.
- Mandatory buy: in `StoryTask::next`, before running a `Go`/`GoUntil`/`Train`/`Battle`/`Challenge` step, if `must_buy(ball_count(state))` and no mandatory buy was attempted in this milestone, splice `StockUp { mart: None }` (nearest mart selling `ITEM_POKE_BALL`). If that Purchase buys 0 (no money), log it and don't retry this milestone.

- [ ] **Step 1: Failing tests**: `nearest_mart(world, data, "PewterCity", "ITEM_POKE_BALL") == Some(("PewterCity_Mart", 3))`; `premier_ball_bonus_page_is_tracked` in `track.rs`; a `Purchase` flow test with synthetic observations (list with money 3000 → quantity 1 → 3 → confirm → list with money 2400) asserting the Up presses, YES, and the events `MoneyObserved(3000)`, `ItemsChanged(+3)`, `MoneyChanged(-600)`; and `count == 0` with money 700 → buys 0 and closes (budget 100 < 200).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** Reuse `story::talk_to`'s approach for the clerk by making `Buy` approach the clerk with `Destination::Facing` then hand control to `Purchase` once the question menu is open.
- [ ] **Step 4: Run** the unit tests, then replay the mart probe with `--record` (as in Task 7 Step 4, using `tools/scripts/probe/mart.txt`) and check every `shop` observation. The flow itself is proven live in Task 11 (StockUpPewter).
- [ ] **Step 5: Commit** `git commit -m "agent: buy at a mart (read money, set quantity, confirm) and StockUp"`.

---

### Task 9: Catching in battle

**Files:**
- Modify: `crates/agent/src/catch.rs` (flow part), `crates/agent/src/battle.rs`, `crates/agent/src/track.rs`, `crates/agent/src/story.rs`, `crates/agent/src/action.rs`

**Interfaces:**
- Consumes: Task 2 policy; Task 4 bag observation; Task 6 `opponent_caught`, `opponent_shiny`, `pokedex_page`.
- Produces:

```rust
// agent::catch
pub struct Attempt { pub plan: CatchPlan, pub opened: bool, pub throws: u32, pub foe_status: FoeStatus }
/// Per-battle catching memory, owned by BattleMemory.
#[derive(Debug, Default, Clone)] pub struct CatchMemory { pub decided: bool, pub attempt: Option<Attempt>, pub caught: Option<String> }
pub enum ThrowOutcome { Caught, BrokeFree }
/// Facts in battle text about the throw ("used POKé BALL!", "Gotcha!", broke free, sleep/paralysis on the foe).
pub fn throw_text(page: &str) -> Option<ThrowOutcome>;
pub fn foe_status_text(page: &str, foe_name: &str) -> Option<FoeStatus>; // "Wild X fell asleep!", "Wild X is paralyzed!…", "Wild X woke up!" → None-variant
pub struct Thrower { /* phase */ }
impl Thrower { pub fn next(&mut self, o: &Observation, data: &GameData, ball: &str, events: &mut Vec<GameEvent>) -> Decision; }
/// Events after a catch: SpeciesCaught plus PartyMonDerived into the next free slot, or SentToPc when the party is full.
pub fn caught_events(data: &GameData, state: &GameState, species: &str, level: u8, box_index: Option<u8>) -> Vec<GameEvent>;
// BattleMemory gains: pub catch: catch::CatchMemory
```

Behaviour (binding, spec §1):
- **Identification (once per wild battle):** when `identify_opponent` succeeds and `opponent_caught` is read: emit `SpeciesSeen` (icon absent) or `SpeciesCaught` (icon present); `ShinySeen` when `opponent_shiny == Some(Shiny)`; log `Unclear`. Then `plan_catch(...)`; log the plan or the reason (`GoalProgress` phase `Catch`).
- **Weakening:** with an attempt active, at the move menu: the status move once (unless `foe_status` is already set by text), then `weakening_move` while `opponent_hp >= WEAKENED_PER_MILLE`, else go to BAG. After every turn (each time the command menu reappears), recompute `risk` for the remaining turns (remaining weakening + remaining expected throws) from the HUD HP numbers: above the limit → abandon the attempt and RUN (non-shiny); for a shiny, keep throwing. Foe fainted → the battle ends normally.
- **Throwing:** command menu → BAG (cell (1, 0); expect `ScreenIs(Bag|BattleBag)`) → `Thrower`: switch to POKé BALLS reading the title; read rows → `PocketObserved`; re-pick `best_ball` from the rows; move ▶ to it; A → USE prompt → USE (expect bag closed) → battle text. The ball is then counted by text: `TextTracker` turns "used <ball>!" into `ItemsChanged { pocket: PokeBalls, item, delta: -1, reason: "thrown" }`.
- **Result:** `throw_text` → `Caught` ("Gotcha! X was caught!") or `BrokeFree` (the four variants in Review Focus 5) → next turn repeats the throw. Unmatched pages are ignored.
- **After a catch:** dismiss the Pokédex page (A, expect `!pokedex_page`); the "Give a nickname to the caught X?" question → NO (this question is the only YES/NO allowed in battle besides the learning prompts; keep the existing "unexpected question" failure for others); "X was transferred to … PC." / "BOX N" pages → box index `N − 1`. Emit `caught_events` once the battle ends (party full → `SentToPc`).
- `battle::decide` routes to the catch logic when `memory.catch.attempt` is `Some`; the existing flee logic doesn't run during an attempt (the risk check replaces it) except the no-PP case.

- [ ] **Step 1: Failing tests:**
  - `every_broke_free_variant_means_throw_again` (the four texts → `BrokeFree`; "Gotcha! PIDGEY was caught!" → `Caught`; "PIDGEY used TACKLE!" → `None`);
  - `foe_status_from_text` ("Wild PIDGEY fell asleep!" → `Asleep`, "Wild PIDGEY is paralyzed! It may be unable to move!" → `Paralyzed`, "Wild PIDGEY woke up!" → `None`-variant);
  - `thrown_ball_is_counted` in `track.rs` ("RED used POKé BALL!" → `ItemsChanged(-1, "thrown")`; "RED used POTION!" → nothing here);
  - `full_party_sends_catch_to_pc` (state with 6 members, box index `Some(0)` → `[SpeciesCaught, SentToPc { box_index: Some(0), mon }]`, no `PartyMonDerived`);
  - `catch_joins_the_party` (1 member → `PartyMonDerived { slot: 1, mon }` with `Derived` species/level, default moves, full PP);
  - a decision test: lead + synthetic battle observation with `opponent_caught: Some(false)`, Pidgey Lv 6, 10 balls in state → first move-menu decision chooses Sleep Powder; with foe asleep and HP 1000‰ → a safe move; at 200‰ → "BAG" on the command menu.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** in the order: text facts → `caught_events` → `Thrower` → battle routing → story hooks (call identification from the battle branch in `StoryTask::next`; route the nickname question; emit the post-catch events on `BattleEnded`).
- [ ] **Step 4: Run** the tests, `fmt` and `clippy`. Replay the battle probe with `--record` (`tools/scripts/probe/battle-catch.txt`) and check the battle observations (`opponent_caught`, `opponent_shiny`, bag, `pokedex_page`). The flow itself is proven live in Task 11 (CrossRoute3).
- [ ] **Step 5: Commit** `git commit -m "agent: catch wild Pokémon (identify, weaken, throw, after-catch events)"`.

---

### Task 10: Milestones Route 3 → Cascade Badge

**Files:**
- Modify: `crates/agent/src/story.rs` (`to_cerulean()`, `all_milestones`), `crates/agent/src/lib.rs` (export)
- Create: `crates/agent/tests/milestones.rs`
- Modify (if needed): `tools/world/extract_world.py` (cave palettes)

**Interfaces:**
- Consumes: Tasks 7–9 steps.
- Produces: `pub fn to_cerulean() -> Vec<Milestone>` with milestones named exactly `StockUpPewter`, `CrossRoute3`, `PrepareForMtMoon`, `CrossMtMoon`, `ReachCerulean`, `PrepareForMisty`, `BeatMisty`; `all_milestones` appends it after `to_mt_moon()`.

Milestone steps (binding, spec §3; object ids and coordinates from the decompilation):

```rust
const MT_MOON_TRAINERS: [&str; 12] = [
    "TRAINER_LASS_IRIS", "TRAINER_BUG_CATCHER_ROBBY", "TRAINER_SUPER_NERD_JOVAN", "TRAINER_LASS_MIRIAM",
    "TRAINER_BUG_CATCHER_KENT", "TRAINER_YOUNGSTER_JOSH", "TRAINER_HIKER_MARCOS",
    "TRAINER_SUPER_NERD_MIGUEL", // floor trigger at MtMoon_B2F (14, 11); not in map_trainers
    "TRAINER_TEAM_ROCKET_GRUNT", "TRAINER_TEAM_ROCKET_GRUNT_2", "TRAINER_TEAM_ROCKET_GRUNT_3", "TRAINER_TEAM_ROCKET_GRUNT_4",
];
// StockUpPewter: [StockUp { mart: Some("PewterCity_Mart") }]
// CrossRoute3:   [Heal { center: Some("Route4_PokemonCenter_1F") }]
// PrepareForMtMoon: [Prepare { targets: MT_MOON_TRAINERS, areas: ["Route3", "Route4", "MtMoon_1F"], confidence: 0.9 },
//                    Heal { center: Some("Route4_PokemonCenter_1F") }]
// CrossMtMoon:   [Battle { trigger: Tile { map: "MtMoon_B2F", x: 14, y: 11 } },   // Miguel
//                 Talk { map: "MtMoon_B2F", object: 2, answers: vec![Answer::Yes] }, // Helix Fossil
//                 Settle { frames: 180 },                                          // Miguel takes the Dome Fossil
//                 Go(Warp { map: "MtMoon_B1F", warp: 7 })]                          // exit to Route 4
// ReachCerulean: [Heal { center: Some("CeruleanCity_PokemonCenter_1F") }, StockUp { mart: Some("CeruleanCity_Mart") }]
// PrepareForMisty: [Prepare { targets: ["TRAINER_SWIMMER_MALE_LUIS", "TRAINER_PICNICKER_DIANA", "TRAINER_LEADER_MISTY"],
//                             areas: ["Route4", "Route24", "Route25", "Route3"], confidence: 0.9 },
//                   Heal { center: Some("CeruleanCity_PokemonCenter_1F") }]
// BeatMisty:     [Challenge { map: "CeruleanCity_Gym", object: 3 }, Settle { frames: 180 }]
```

Check before writing: `Route24`/`Route25` are reachable before Nugget Bridge only partly — if the planner picks them, the Train step must reach grass there; if that grass is behind the Nugget Bridge trainers, drop them from `areas` (keep `Route4`, `Route3`, `MtMoon_1F`) and note the ruling in the commit message. The prepared party for `Prepare` includes the catches made so far: `StoryTask::plan` only accepts `Train` steps for the lead today; a plan step for another member (`Train { species != lead }`) must still fail (switching is out of scope). If the planner proposes a `Catch` step, turn it into a `Train` at the same area only when the lead-only plan also reaches the confidence; otherwise fail with the planner's reason.

- [ ] **Step 1: Failing test** `crates/agent/tests/milestones.rs` (skips without `data/world`): every `Talk`/`Challenge` object exists on its map; every `Warp` index exists; every `Tile` is walkable (collision 0); every `Heal` center has a nurse; every `StockUp` mart sells `ITEM_POKE_BALL` and has a clerk; `route_from` finds a route from `PewterCity_PokemonCenter_1F (7, 4)` to `Route4_PokemonCenter_1F`, from `Route4_PokemonCenter_1F` to `MtMoon_B2F`, from `MtMoon_B2F (14, 7)` to `CeruleanCity`, and every trainer in the `Prepare` lists exists in `GameData.trainers`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement** `to_cerulean()`; run → PASS.
- [ ] **Step 4: Mt. Moon localization probe.** `./target/release/pokebot inspect captures/fixtures/mtmoon-1f.png --world data/world --map MtMoon_1F` must locate the player with a score comparable to outdoor maps (record the score). If not, compare the render crop and the frame colours (cave tileset palettes, any shade) and fix `extract_world.py`'s palette handling; rebuild and re-check; add the fixture as a localization test in `crates/world` (skip when missing).
- [ ] **Step 5: Check the fossil/Miguel scripts** in `data/pret-pokefirered/data/maps/MtMoon_B2F/scripts.inc` against the steps (trigger at (14, 11) starts Miguel's battle; the Helix Fossil is object 2; the YES/NO answer; Miguel's movement to the Dome Fossil afterwards). Adjust the steps if the scripts disagree, and say how in the commit message.
- [ ] **Step 6: Commit** `git commit -m "story: milestones from Route 3 to the Cascade Badge"`.

---

### Task 11: Live — StockUpPewter and CrossRoute3

**Files:** fixes wherever the live run shows a defect (each fix with a unit test reproducing it).

- [ ] **Step 1:** Restore the route3-ready checkpoint (three files, as in Task 3 Step 2). Build release.
- [ ] **Step 2:** `tools/live-run.sh /tmp/stockup.log story --continue --save-game --milestones 1 --record /tmp/run-stockup` (the UI shows it). Watch the log; when done, build a contact sheet (`ffmpeg -pattern_type glob -i '/tmp/run-stockup/frames/*.png' -vf "select='not(mod(n\,40))',scale=240:160,tile=8x8" -frames:v 1 /tmp/stockup.png`) and read it. Verify in `events.jsonl`: `PocketObserved(PokeBalls)`, `MoneyObserved`, `ItemsChanged(+N, "bought")`, `MoneyChanged(−200·N)`; the audited count plus N equals the pocket after. On a defect: fix with a test, rebuild, restore the checkpoint, rerun.
- [ ] **Step 3:** `tools/live-run.sh /tmp/route3.log story --continue --save-game --milestones 1 --record /tmp/run-route3`. Verify: trainer battles won; at least one wild catch attempt with `SpeciesSeen`/`SpeciesCaught`, `ItemsChanged(−1, "thrown")` per throw, `PartyMonDerived` for the catch; no faint (a faint-retry is allowed but must end in success); the save and `state.json` written.
- [ ] **Step 4:** Back up the checkpoint: `cp roms/*.sav saves/route4.sav`, `cp saves/progress.json saves/progress.route4.json`, `cp saves/state.json saves/state.route4.json`.
- [ ] **Step 5: Commit** any fixes (one commit per fix, message naming the live symptom).

---

### Task 12: Live — PrepareForMtMoon and CrossMtMoon

- [ ] **Step 1:** From the route4 checkpoint: `tools/live-run.sh /tmp/prep-moon.log story --continue --save-game --milestones 1 --record /tmp/run-prep-moon`. Verify the plan in the log (`GoalProgress Plan`) and the training; heal at Route 4.
- [ ] **Step 2:** `tools/live-run.sh /tmp/moon.log story --continue --save-game --milestones 1 --record /tmp/run-moon`. Verify with contact sheets: localization inside Mt. Moon, the ladder warps, Miguel's battle, the Helix Fossil ("obtained the HELIX FOSSIL" → `ItemsChanged(KeyItems, ITEM_HELIX_FOSSIL, +1)`), the exit to Route 4; grunts fought if they spot us; catches as they come.
- [ ] **Step 3:** Back up as `saves/mtmoon-done.sav`, `progress.mtmoon-done.json`, `state.mtmoon-done.json`.
- [ ] **Step 4: Commit** fixes (each with a test).

---

### Task 13: Live — ReachCerulean, PrepareForMisty, BeatMisty

- [ ] **Step 1:** `tools/live-run.sh /tmp/cerulean.log story --continue --save-game --milestones 1 --record /tmp/run-cerulean`: Route 4 ledges to Cerulean, heal, StockUp at Cerulean Mart (a second purchase if below 15).
- [ ] **Step 2:** `… --milestones 1 --record /tmp/run-prep-misty`: the plan against Luis, Diana, Misty; training.
- [ ] **Step 3:** `… --milestones 1 --record /tmp/run-misty`: Misty beaten, "received the CASCADE BADGE" on the contact sheet.
- [ ] **Step 4:** Check the spec's Done: BeatMisty with no faint; ≥ 1 wild Pokémon caught (`SpeciesCaught` from "Gotcha!" in some run's `events.jsonl`); Poké Balls bought ≥ 1 time. Back up as `saves/cascade.sav`, `progress.cascade.json`, `state.cascade.json`.
- [ ] **Step 5: Commit** fixes.

---

### Task 14: Documentation

**Files:** `docs/architecture.md`, `docs/HANDOFF.md`, `README.md`

- [ ] **Step 1:** `architecture.md`: new detectors in the perception table (bag, mart, caught icon, shiny matcher, Pokédex page); "Catching" and "Buying" sections (policy numbers, flows, events); the new story steps and milestones; results of the live runs (battles, catches, purchases, lowest HP vs Misty). Remove the "Catch steps aren't executed yet" line and update "Next".
- [ ] **Step 2:** `HANDOFF.md`: current checkpoint (cascade), the backups list, what's next (Nugget Bridge, Bill).
- [ ] **Step 3:** `README.md`: milestones list through BeatMisty.
- [ ] **Step 4: Commit** `git commit -m "docs: catching, buying and the Cascade Badge segment"`.
