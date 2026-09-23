# Global State — Phase 1 core (model, events, checkpoints, passive sources) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move all party knowledge into the event-sourced `GameState`, add the bag / money / PC / Pokédex model, persist and restore it with the in-game save, and feed it from what is already visible (battle HUD, move menu, dialogue text).

**Architecture:**
- New state types live in `crates/state` (`party.rs`, `inventory.rs`), and new `GameEvent` variants are applied by a separate pure reducer module (`knowledge.rs` → `reduce_knowledge.rs`).
- The agent stops mutating its private `Party`. It emits events, and each tick it rebuilds `agent::Party` as a read-only view of `ctx.state`.
- `saves/state.json` holds a `SavedKnowledge` snapshot written after every in-game save. It is restored through a `CheckpointRestored` event, so replays stay pure.

**Tech Stack:** Rust 2021 workspace (serde, serde_json), Python 3 offline tooling (Pillow) for font extraction, the existing mGBA libretro emulator for live runs.

**Spec:** `docs/superpowers/specs/2026-09-23-global-state-design.md` (Phase 1, delivery step 1). Delivery steps 2–5 (bag, party/summary, money and PC audits) get their own plans after their screens are probed.

## Global Constraints

- Vision only: never read emulator memory, save states or scripting APIs (`adapters/emulator-libretro/tests/no_privileged_access.rs` enforces it).
- Closed loop: every action carries an `Expectation` checked on screen, plus a timeout.
- Deterministic: same observations → same plans and events; use `BTreeMap`/`Vec`, never `HashMap` iteration order, in state and in anything that emits events.
- `Unknown` is legitimate: a tracked delta applied to an `Unknown` value leaves it `Unknown`.
- Provenance per fact with `Knowledge<T>`; **stale** = source `Tracked` or `Assumed` (never time-based).
- No ROMs or game assets in git (`data/world/`, `captures/`, `saves/` stay ignored).
- Species, moves and items are stored as decompilation constants (`SPECIES_IVYSAUR`, `MOVE_TACKLE`, `ITEM_POKE_BALL`), never display names.
- Every bot run goes through `tools/live-run.sh <log> <pokebot args…>` with a fresh `--record` dir; verify with ffmpeg contact sheets.
- Keep `~/.cargo/bin/cargo fmt --all`, `cargo clippy --workspace --all-targets` (no warnings) and `cargo test --workspace` green; commit per task on branch `rust-bot` with the trailer `Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>`.

## Review Focus

1. **Knowledge after a reload.** A faint → reload must restore the snapshot. Tracked changes made since the save (PP used, HP, moves learned) must be discarded. Tested in Task 3 (`checkpoint_restore_replaces_knowledge`).
2. **Every-frame HUD must not flood `events.jsonl`.** An event may only be emitted when the observation differs from the state. Tested in Task 6 (`unchanged_hud_emits_nothing`).
3. **Legacy `progress.json` without `state.json`.** The Brock and route3-ready backups have only `party`. They must migrate into the state (Tracked) instead of starting blank. Tested in Task 8 (`legacy_party_migrates`).
4. **Unknown PP must not make the bot choose an empty move.** Unknown PP counts as full, but a `There's no PP left` page must mark that move 0. Tested in Task 7 (`no_pp_page_zeroes_move`).
5. **Name readings with `?` wildcards.** For example `I?YSAUR` from the HUD, or a move name with an unknown glyph. These must resolve only when the match is unique; otherwise no event. Tested in Task 6 (`ambiguous_move_name_emits_nothing`).

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/state/src/knowledge.rs` (modify) | `Knowledge` helpers: `derived`, `tracked`, `is_stale`, `needs_audit` |
| `crates/state/src/party.rs` (create) | `PartyMon`, `MoveSlot`, `Status` |
| `crates/state/src/inventory.rs` (create) | `Pocket`, `Bag`, `BoxMon`, `PcStorage`, `Pokedex`, `SavedKnowledge` |
| `crates/state/src/state.rs` (modify) | `GameState` gains `party`, `bag`, `money`, `pc`, `pokedex`; `saved_knowledge()` |
| `crates/state/src/events.rs` (modify) | new `GameEvent` variants |
| `crates/state/src/reduce_knowledge.rs` (create) | pure application of the new events |
| `crates/state/src/reducer.rs` (modify) | delegates new events to `reduce_knowledge::apply` |
| `crates/runtime/src/lib.rs` (modify) | `summarize` for new events |
| `tools/gamedata/extract_gamedata.py`, `crates/gamedata/src/lib.rs` (modify) | item `pocket`; `item_named`, `species_named` |
| `tools/gamedata/extract_font.py`, `tools/world/build.sh` (modify) | also emit `font_small.json` |
| `crates/vision/src/lib.rs`, `crates/vision/src/detect/battle.rs`, `crates/state/src/observation.rs` (modify) | move names in the battle move menu (small font) |
| `crates/agent/src/party.rs` (modify) | `Party::from_state`, `battle_events`, `starter_mon`, `legacy_knowledge` |
| `crates/agent/src/learn.rs` (modify) | returns events instead of mutating |
| `crates/agent/src/track.rs` (create) | text facts → money / item / heal / no-PP events |
| `crates/agent/src/story.rs` (modify) | view refresh, event emission, no private party mutation |
| `crates/agent/src/checkpoint.rs` (create) | `saves/state.json` load/store |
| `crates/agent/src/progress.rs` (modify) | `party` becomes legacy (read-only) |
| `apps/pokebot-cli/src/main.rs` (modify) | seed starter, restore/store checkpoints |
| `crates/telemetry/web/index.html` (modify) | party / money / bag panel |
| `docs/architecture.md`, `docs/HANDOFF.md` (modify) | document the model |

---

### Task 1: State model types

**Files:**
- Modify: `crates/state/src/knowledge.rs`
- Create: `crates/state/src/party.rs`, `crates/state/src/inventory.rs`
- Modify: `crates/state/src/state.rs`, `crates/state/src/lib.rs`

**Interfaces:**
- Produces:
  - `Knowledge::{derived(T, u64), tracked(T, Option<u64>), is_stale(&self) -> bool, needs_audit(&self) -> bool}`;
  - `PartyMon`, `MoveSlot`, `Status`;
  - `Pocket`, `Bag`, `BoxMon`, `PcStorage`, `Pokedex`, `SavedKnowledge`;
  - `GameState.{party, bag, money, pc, pokedex}`;
  - `GameState::saved_knowledge(&self) -> SavedKnowledge`;
  - `pub const BOXES: usize = 14`.

- [ ] **Step 1: Write the failing tests** (append to `crates/state/src/inventory.rs` after creating it with just `#[cfg(test)] mod tests` — the test module references types defined in Step 3)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GameState, Knowledge, KnowledgeSource};

    #[test]
    fn new_state_knows_nothing_about_inventory() {
        let s = GameState::default();
        assert_eq!(s.party, Knowledge::unknown());
        assert_eq!(s.money, Knowledge::unknown());
        assert_eq!(s.pc.boxes.len(), BOXES);
        assert!(s.pc.boxes.iter().all(|b| b.value.is_none()));
        assert!(s.bag.pockets.values().all(|p| p.value.is_none()));
        assert_eq!(s.bag.pockets.len(), 5);
    }

    #[test]
    fn staleness_comes_from_provenance() {
        assert!(Knowledge::<u8>::unknown().needs_audit());
        assert!(!Knowledge::observed(3u8, 1).needs_audit());
        assert!(Knowledge::tracked(3u8, Some(1)).is_stale());
        assert!(!Knowledge::derived(3u8, 1).is_stale());
        assert_eq!(Knowledge::tracked(3u8, Some(1)).source, KnowledgeSource::Tracked);
    }

    #[test]
    fn saved_knowledge_round_trips() {
        let mut s = GameState::default();
        s.money = Knowledge::observed(4600, 10);
        let saved = s.saved_knowledge();
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(serde_json::from_str::<SavedKnowledge>(&json).unwrap(), saved);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `~/.cargo/bin/cargo test -p pokebot-state inventory`
Expected: compile errors (`BOXES`, `GameState.party`, `Knowledge::tracked` … not found).

- [ ] **Step 3: Implement**

`crates/state/src/knowledge.rs`, add to `impl<T> Knowledge<T>`:

```rust
    /// Worked out from static game data or story knowledge (e.g. a move's
    /// maximum PP when it is learned).
    pub fn derived(value: T, frame_id: u64) -> Self {
        Self {
            value: Some(value),
            source: KnowledgeSource::Derived,
            last_verified_frame: Some(frame_id),
        }
    }

    /// Changed by a tracked event since it was last observed at
    /// `last_verified_frame`.
    pub fn tracked(value: T, last_verified_frame: Option<u64>) -> Self {
        Self {
            value: Some(value),
            source: KnowledgeSource::Tracked,
            last_verified_frame,
        }
    }

    /// Changed by tracking (or assumed) since the last observation.
    pub fn is_stale(&self) -> bool {
        matches!(self.source, KnowledgeSource::Tracked | KnowledgeSource::Assumed)
    }

    /// A decision relying on this should audit it first.
    pub fn needs_audit(&self) -> bool {
        self.value.is_none() || self.is_stale()
    }
```

`crates/state/src/party.rs`:

```rust
//! What the bot knows about each party Pokémon. Every field has its own
//! provenance; names are decompilation constants.

use serde::{Deserialize, Serialize};

use crate::Knowledge;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Healthy,
    Poisoned,
    BadlyPoisoned,
    Burned,
    Paralyzed,
    Asleep,
    Frozen,
    Fainted,
}

/// One move slot: the move and its (current, maximum) PP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveSlot {
    pub mv: Knowledge<String>,
    pub pp: Knowledge<(u8, u8)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PartyMon {
    pub species: Knowledge<String>,
    pub nickname: Knowledge<String>,
    pub level: Knowledge<u8>,
    /// (current, maximum)
    pub hp: Knowledge<(u16, u16)>,
    pub status: Knowledge<Status>,
    /// Menu order; `None` = no move in that slot (or not known to exist).
    pub moves: [Option<MoveSlot>; 4],
    pub held_item: Knowledge<Option<String>>,
    pub shiny: Knowledge<bool>,
}
```

`crates/state/src/inventory.rs` (above the test module):

```rust
//! Bag, money, PC storage and the Pokédex's caught/seen flags.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Knowledge, PartyMon};

/// PC boxes in FireRed.
pub const BOXES: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Pocket {
    Items,
    KeyItems,
    PokeBalls,
    TmCase,
    BerryPouch,
}

impl Pocket {
    pub const ALL: [Pocket; 5] = [
        Pocket::Items,
        Pocket::KeyItems,
        Pocket::PokeBalls,
        Pocket::TmCase,
        Pocket::BerryPouch,
    ];

    /// From the decompilation's `POCKET_*` name.
    pub fn from_decomp(name: &str) -> Option<Pocket> {
        Some(match name {
            "POCKET_ITEMS" => Pocket::Items,
            "POCKET_KEY_ITEMS" => Pocket::KeyItems,
            "POCKET_POKE_BALLS" => Pocket::PokeBalls,
            "POCKET_TM_CASE" => Pocket::TmCase,
            "POCKET_BERRY_POUCH" => Pocket::BerryPouch,
            _ => return None,
        })
    }
}

/// (item constant, count), in the pocket's display order.
pub type ItemList = Vec<(String, u16)>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bag {
    pub pockets: BTreeMap<Pocket, Knowledge<ItemList>>,
}

impl Default for Bag {
    fn default() -> Self {
        Self {
            pockets: Pocket::ALL.iter().map(|p| (*p, Knowledge::unknown())).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxMon {
    /// 0..30 within the box.
    pub slot: u8,
    pub species: Knowledge<String>,
    pub level: Knowledge<u8>,
    pub nickname: Knowledge<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcStorage {
    pub boxes: Vec<Knowledge<Vec<BoxMon>>>,
    pub items: Knowledge<ItemList>,
}

impl Default for PcStorage {
    fn default() -> Self {
        Self {
            boxes: vec![Knowledge::unknown(); BOXES],
            items: Knowledge::unknown(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Pokedex {
    pub caught: BTreeMap<String, Knowledge<bool>>,
    pub seen: BTreeMap<String, Knowledge<bool>>,
}

/// The knowledge that belongs to a save file: stored beside it after every
/// in-game save and restored when that save is loaded.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SavedKnowledge {
    pub party: Knowledge<Vec<PartyMon>>,
    pub bag: Bag,
    pub money: Knowledge<u32>,
    pub pc: PcStorage,
    pub pokedex: Pokedex,
}
```

`Knowledge<T>` needs `Clone` for `vec![Knowledge::unknown(); BOXES]`; it already derives `Clone`.

`crates/state/src/state.rs`, add fields to `GameState` (after `active_goal`) and a method:

```rust
    /// Party in slot order.
    pub party: Knowledge<Vec<crate::PartyMon>>,
    pub bag: crate::Bag,
    pub money: Knowledge<u32>,
    pub pc: crate::PcStorage,
    pub pokedex: crate::Pokedex,
}

impl GameState {
    /// The part of the state that belongs to the save file.
    pub fn saved_knowledge(&self) -> crate::SavedKnowledge {
        crate::SavedKnowledge {
            party: self.party.clone(),
            bag: self.bag.clone(),
            money: self.money.clone(),
            pc: self.pc.clone(),
            pokedex: self.pokedex.clone(),
        }
    }
}
```

`crates/state/src/lib.rs`: add `mod party; mod inventory;` and
`pub use party::{MoveSlot, PartyMon, Status};`
`pub use inventory::{Bag, BoxMon, ItemList, PcStorage, Pocket, Pokedex, SavedKnowledge, BOXES};`

- [ ] **Step 4: Run tests**

Run: `~/.cargo/bin/cargo test -p pokebot-state`
Expected: PASS (existing `replaying_events_rebuilds_state` still passes: the new fields have defaults and round-trip through serde).

- [ ] **Step 5: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add crates/state
git commit -m "state: party, bag, money, PC and Pokédex knowledge model

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Party events and their reducer

**Files:**
- Modify: `crates/state/src/events.rs`, `crates/state/src/reducer.rs`, `crates/runtime/src/lib.rs`
- Create: `crates/state/src/reduce_knowledge.rs`

**Interfaces:**
- Consumes (Task 1): `PartyMon`, `MoveSlot`, `Status`, `Knowledge::{derived, tracked}`.
- Produces the `GameEvent` variants (exact fields):
  - `PartyMonDerived { slot: u8, mon: Box<PartyMon> }` — a member known from the story or legacy data;
  - `PartyObserved { slot: u8, species: Option<String>, nickname: Option<String>, level: Option<u8>, hp: Option<(u16, u16)>, status: Option<Status>, held_item: Option<Option<String>> }`;
  - `MovesObserved { slot: u8, moves: Vec<String> }` — the complete move list in menu order;
  - `MovePpObserved { slot: u8, move_slot: u8, cur: u8, max: u8 }`;
  - `MoveUsed { slot: u8, move_slot: u8 }`;
  - `MoveLearned { slot: u8, move_slot: u8, mv: String, max_pp: u8 }`;
  - `MoveReplaced { slot: u8, move_slot: u8, old: String, new: String, max_pp: u8 }`;
  - `MoveOutOfPp { slot: u8, move_slot: u8 }`;
  - `Evolved { slot: u8, species: String }`;
  - `Healed`.
- Also produces `reduce_knowledge::apply(state: &mut GameState, frame: u64, event: &GameEvent)`.

- [ ] **Step 1: Write the failing tests** in `crates/state/src/reduce_knowledge.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefaultReducer, EventRecord, KnowledgeSource, StateReducer};

    fn run(events: Vec<GameEvent>) -> GameState {
        let records: Vec<EventRecord> = events
            .into_iter()
            .enumerate()
            .map(|(i, event)| EventRecord { frame_id: i as u64 + 1, event })
            .collect();
        DefaultReducer.reduce(&GameState::default(), &records)
    }

    fn starter() -> GameEvent {
        let mut mon = PartyMon::default();
        mon.species = Knowledge::derived("SPECIES_BULBASAUR".into(), 0);
        mon.level = Knowledge::derived(5, 0);
        mon.moves[0] = Some(MoveSlot {
            mv: Knowledge::derived("MOVE_TACKLE".into(), 0),
            pp: Knowledge::derived((35, 35), 0),
        });
        GameEvent::PartyMonDerived { slot: 0, mon: Box::new(mon) }
    }

    #[test]
    fn pp_is_observed_then_tracked_then_healed() {
        let s = run(vec![
            starter(),
            GameEvent::MovePpObserved { slot: 0, move_slot: 0, cur: 30, max: 35 },
            GameEvent::MoveUsed { slot: 0, move_slot: 0 },
        ]);
        let pp = &s.party.value.as_ref().unwrap()[0].moves[0].as_ref().unwrap().pp;
        assert_eq!(pp.value, Some((29, 35)));
        assert_eq!(pp.source, KnowledgeSource::Tracked);
        assert_eq!(pp.last_verified_frame, Some(2));
        let healed = run(vec![starter(), GameEvent::MoveUsed { slot: 0, move_slot: 0 }, GameEvent::Healed]);
        let pp = &healed.party.value.as_ref().unwrap()[0].moves[0].as_ref().unwrap().pp;
        assert_eq!(pp.value, Some((35, 35)));
    }

    #[test]
    fn replacing_a_move_keeps_the_slot() {
        let s = run(vec![
            starter(),
            GameEvent::MoveReplaced {
                slot: 0,
                move_slot: 0,
                old: "MOVE_TACKLE".into(),
                new: "MOVE_VINE_WHIP".into(),
                max_pp: 10,
            },
        ]);
        let m = s.party.value.as_ref().unwrap()[0].moves[0].as_ref().unwrap();
        assert_eq!(m.mv.value.as_deref(), Some("MOVE_VINE_WHIP"));
        assert_eq!(m.pp.value, Some((10, 10)));
    }

    #[test]
    fn observed_moves_keep_known_pp_for_the_same_move() {
        let s = run(vec![
            starter(),
            GameEvent::MovePpObserved { slot: 0, move_slot: 0, cur: 12, max: 35 },
            GameEvent::MovesObserved { slot: 0, moves: vec!["MOVE_TACKLE".into(), "MOVE_GROWL".into()] },
        ]);
        let mon = &s.party.value.as_ref().unwrap()[0];
        assert_eq!(mon.moves[0].as_ref().unwrap().pp.value, Some((12, 35)));
        assert_eq!(mon.moves[1].as_ref().unwrap().pp.value, None);
        assert!(mon.moves[2].is_none());
    }

    #[test]
    fn out_of_pp_and_evolution_are_observed() {
        let s = run(vec![
            starter(),
            GameEvent::MoveOutOfPp { slot: 0, move_slot: 0 },
            GameEvent::Evolved { slot: 0, species: "SPECIES_IVYSAUR".into() },
        ]);
        let mon = &s.party.value.as_ref().unwrap()[0];
        assert_eq!(mon.moves[0].as_ref().unwrap().pp.value, Some((0, 35)));
        assert_eq!(mon.species.value.as_deref(), Some("SPECIES_IVYSAUR"));
        assert_eq!(mon.species.source, KnowledgeSource::Observed);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `~/.cargo/bin/cargo test -p pokebot-state reduce_knowledge`
Expected: compile error (variants not defined).

- [ ] **Step 3: Implement**

`crates/state/src/events.rs`, add to `GameEvent` (import `crate::{PartyMon, Status}`):

```rust
    /// A party member known without seeing it (story, legacy data).
    PartyMonDerived { slot: u8, mon: Box<PartyMon> },
    /// Fields of a party member read from the screen (`None` = not shown).
    PartyObserved {
        slot: u8,
        species: Option<String>,
        nickname: Option<String>,
        level: Option<u8>,
        hp: Option<(u16, u16)>,
        status: Option<Status>,
        held_item: Option<Option<String>>,
    },
    /// A member's complete move list, in menu order.
    MovesObserved { slot: u8, moves: Vec<String> },
    MovePpObserved { slot: u8, move_slot: u8, cur: u8, max: u8 },
    /// A move was used (one PP spent).
    MoveUsed { slot: u8, move_slot: u8 },
    /// "X learned M!" into an empty slot.
    MoveLearned { slot: u8, move_slot: u8, mv: String, max_pp: u8 },
    /// "X forgot O … learned N!"
    MoveReplaced { slot: u8, move_slot: u8, old: String, new: String, max_pp: u8 },
    /// "There's no PP left for this move!"
    MoveOutOfPp { slot: u8, move_slot: u8 },
    Evolved { slot: u8, species: String },
    /// The nurse restored the party.
    Healed,
```

`crates/state/src/reduce_knowledge.rs`:

```rust
//! Applying knowledge events (party, bag, money, PC, Pokédex) to the state.
//! Pure: the same events always give the same state.

use crate::{GameEvent, GameState, Knowledge, KnowledgeSource, MoveSlot, PartyMon, Status};

/// Applies `event` if it is a knowledge event; returns whether it was one.
pub(crate) fn apply(state: &mut GameState, frame: u64, event: &GameEvent) -> bool {
    match event {
        GameEvent::PartyMonDerived { slot, mon } => {
            *member(state, *slot, KnowledgeSource::Derived, frame) = (**mon).clone();
        }
        GameEvent::PartyObserved { slot, species, nickname, level, hp, status, held_item } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            if let Some(v) = species {
                m.species = Knowledge::observed(v.clone(), frame);
            }
            if let Some(v) = nickname {
                m.nickname = Knowledge::observed(v.clone(), frame);
            }
            if let Some(v) = level {
                m.level = Knowledge::observed(*v, frame);
            }
            if let Some(v) = hp {
                m.hp = Knowledge::observed(*v, frame);
            }
            if let Some(v) = status {
                m.status = Knowledge::observed(*v, frame);
            }
            if let Some(v) = held_item {
                m.held_item = Knowledge::observed(v.clone(), frame);
            }
        }
        GameEvent::MovesObserved { slot, moves } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            for i in 0..4 {
                m.moves[i] = match (moves.get(i), m.moves[i].take()) {
                    (Some(mv), Some(old)) if old.mv.value.as_ref() == Some(mv) => Some(MoveSlot {
                        mv: Knowledge::observed(mv.clone(), frame),
                        pp: old.pp,
                    }),
                    (Some(mv), _) => Some(MoveSlot {
                        mv: Knowledge::observed(mv.clone(), frame),
                        pp: Knowledge::unknown(),
                    }),
                    (None, _) => None,
                };
            }
        }
        GameEvent::MovePpObserved { slot, move_slot, cur, max } => {
            if let Some(s) = move_slot_mut(state, *slot, *move_slot) {
                s.pp = Knowledge::observed((*cur, *max), frame);
            }
        }
        GameEvent::MoveUsed { slot, move_slot } => {
            if let Some(s) = move_slot_mut(state, *slot, *move_slot) {
                if let Some((cur, max)) = s.pp.value {
                    s.pp = Knowledge::tracked((cur.saturating_sub(1), max), s.pp.last_verified_frame);
                }
            }
        }
        GameEvent::MoveOutOfPp { slot, move_slot } => {
            if let Some(s) = move_slot_mut(state, *slot, *move_slot) {
                let max = s.pp.value.map_or(0, |(_, max)| max);
                s.pp = Knowledge::observed((0, max), frame);
            }
        }
        GameEvent::MoveLearned { slot, move_slot, mv, max_pp }
        | GameEvent::MoveReplaced { slot, move_slot, new: mv, max_pp, .. } => {
            let m = member(state, *slot, KnowledgeSource::Observed, frame);
            if let Some(entry) = m.moves.get_mut(usize::from(*move_slot)) {
                *entry = Some(MoveSlot {
                    mv: Knowledge::observed(mv.clone(), frame),
                    pp: Knowledge::derived((*max_pp, *max_pp), frame),
                });
            }
        }
        GameEvent::Evolved { slot, species } => {
            member(state, *slot, KnowledgeSource::Observed, frame).species =
                Knowledge::observed(species.clone(), frame);
        }
        GameEvent::Healed => {
            for m in state.party.value.iter_mut().flatten() {
                if let Some((_, max)) = m.hp.value {
                    m.hp = Knowledge::derived((max, max), frame);
                }
                m.status = Knowledge::derived(Status::Healthy, frame);
                for s in m.moves.iter_mut().flatten() {
                    if let Some((_, max)) = s.pp.value {
                        s.pp = Knowledge::derived((max, max), frame);
                    }
                }
            }
        }
        _ => return false,
    }
    true
}

/// The party member in `slot`, creating unknown members up to it. The party
/// list's provenance becomes `source` when the list itself was unknown.
fn member(state: &mut GameState, slot: u8, source: KnowledgeSource, frame: u64) -> &mut PartyMon {
    if state.party.value.is_none() {
        state.party = Knowledge {
            value: Some(Vec::new()),
            source,
            last_verified_frame: Some(frame),
        };
    }
    let list = state.party.value.as_mut().expect("set above");
    while list.len() <= usize::from(slot) {
        list.push(PartyMon::default());
    }
    &mut list[usize::from(slot)]
}

fn move_slot_mut(state: &mut GameState, slot: u8, move_slot: u8) -> Option<&mut MoveSlot> {
    state
        .party
        .value
        .as_mut()?
        .get_mut(usize::from(slot))?
        .moves
        .get_mut(usize::from(move_slot))?
        .as_mut()
}
```

`crates/state/src/reducer.rs`: at the top of the `for record in events` loop body add

```rust
            if crate::reduce_knowledge::apply(&mut state, record.frame_id, &record.event) {
                continue;
            }
```

and add a match arm `_ => {}` is **not** allowed (keep the match exhaustive): add the new variants to one arm
`GameEvent::PartyMonDerived { .. } | GameEvent::PartyObserved { .. } | … | GameEvent::Healed => unreachable!("handled by reduce_knowledge")`.
`crates/state/src/lib.rs`: `mod reduce_knowledge;`

`crates/runtime/src/lib.rs` `summarize`: add arms, e.g.

```rust
        GameEvent::PartyMonDerived { slot, mon } => format!(
            "Party {slot}: {} Lv{} (derived)",
            mon.species.value.as_deref().unwrap_or("?"),
            mon.level.value.map_or("?".into(), |l| l.to_string())
        ),
        GameEvent::PartyObserved { slot, species, level, hp, .. } => {
            format!("Party {slot} seen: {species:?} Lv{level:?} HP {hp:?}")
        }
        GameEvent::MovesObserved { slot, moves } => format!("Party {slot} moves {moves:?}"),
        GameEvent::MovePpObserved { slot, move_slot, cur, max } => {
            format!("Party {slot} move {move_slot} PP {cur}/{max}")
        }
        GameEvent::MoveUsed { slot, move_slot } => format!("Party {slot} used move {move_slot}"),
        GameEvent::MoveLearned { slot, mv, .. } => format!("Party {slot} learned {mv}"),
        GameEvent::MoveReplaced { slot, old, new, .. } => format!("Party {slot}: {old} → {new}"),
        GameEvent::MoveOutOfPp { slot, move_slot } => format!("Party {slot} move {move_slot} has no PP"),
        GameEvent::Evolved { slot, species } => format!("Party {slot} evolved into {species}"),
        GameEvent::Healed => "Healed (party restored)".into(),
```

- [ ] **Step 4: Run tests**

Run: `~/.cargo/bin/cargo test -p pokebot-state && ~/.cargo/bin/cargo build --workspace`
Expected: PASS; workspace builds (runtime `summarize` exhaustive).

- [ ] **Step 5: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add crates/state crates/runtime
git commit -m "state: party events (moves, PP, evolution, heal) and their reducer

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Bag, money, PC, Pokédex events and checkpoint restore

**Files:**
- Modify: `crates/state/src/events.rs`, `crates/state/src/reduce_knowledge.rs`, `crates/state/src/reducer.rs`, `crates/runtime/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 types; Task 2 `reduce_knowledge::apply`.
- Produces the variants:
  - `ItemsChanged { pocket: Pocket, item: String, delta: i32, reason: String }`;
  - `PocketObserved { pocket: Pocket, items: ItemList }`;
  - `MoneyObserved { amount: u32 }`;
  - `MoneyChanged { delta: i64, reason: String }`;
  - `BoxObserved { box_index: u8, mons: Vec<BoxMon> }`;
  - `PcItemsObserved { items: ItemList }`;
  - `SentToPc { box_index: Option<u8>, mon: BoxMon }`;
  - `MonDeposited { party_slot: u8, box_index: u8 }`;
  - `MonWithdrawn { box_index: u8, box_slot: u8 }`;
  - `SpeciesSeen { species: String }`;
  - `SpeciesCaught { species: String }`;
  - `ShinySeen { species: String }` (log only);
  - `CheckpointRestored { knowledge: Box<SavedKnowledge> }`.

- [ ] **Step 1: Write the failing tests** (append to the test module in `reduce_knowledge.rs`)

```rust
    use crate::{BoxMon, ItemList, Pocket, SavedKnowledge};

    #[test]
    fn tracked_deltas_need_a_known_base() {
        let s = run(vec![GameEvent::ItemsChanged {
            pocket: Pocket::PokeBalls,
            item: "ITEM_POKE_BALL".into(),
            delta: 5,
            reason: "bought".into(),
        }]);
        assert_eq!(s.bag.pockets[&Pocket::PokeBalls], Knowledge::unknown());
        let s = run(vec![
            GameEvent::PocketObserved { pocket: Pocket::PokeBalls, items: vec![("ITEM_POKE_BALL".into(), 3)] },
            GameEvent::ItemsChanged { pocket: Pocket::PokeBalls, item: "ITEM_POKE_BALL".into(), delta: -3, reason: "thrown".into() },
            GameEvent::ItemsChanged { pocket: Pocket::PokeBalls, item: "ITEM_GREAT_BALL".into(), delta: 2, reason: "bought".into() },
        ]);
        let balls = &s.bag.pockets[&Pocket::PokeBalls];
        assert_eq!(balls.value, Some(vec![("ITEM_GREAT_BALL".to_owned(), 2)] as ItemList));
        assert!(balls.is_stale());
        let s = run(vec![GameEvent::MoneyObserved { amount: 4600 }, GameEvent::MoneyChanged { delta: -1000, reason: "bought".into() }]);
        assert_eq!(s.money.value, Some(3600));
        assert!(s.money.is_stale());
    }

    fn boxed(slot: u8, species: &str) -> BoxMon {
        BoxMon {
            slot,
            species: Knowledge::observed(species.into(), 1),
            level: Knowledge::observed(5, 1),
            nickname: Knowledge::unknown(),
        }
    }

    #[test]
    fn pc_moves_mons_between_party_and_boxes() {
        let s = run(vec![
            starter(),
            GameEvent::BoxObserved { box_index: 0, mons: vec![boxed(0, "SPECIES_PIDGEY")] },
            GameEvent::MonWithdrawn { box_index: 0, box_slot: 0 },
        ]);
        assert_eq!(s.pc.boxes[0].value.as_ref().unwrap().len(), 0);
        let party = s.party.value.as_ref().unwrap();
        assert_eq!(party[1].species.value.as_deref(), Some("SPECIES_PIDGEY"));
        let s = run(vec![
            starter(),
            GameEvent::BoxObserved { box_index: 0, mons: vec![] },
            GameEvent::SentToPc { box_index: Some(0), mon: boxed(0, "SPECIES_RATTATA") },
            GameEvent::SpeciesCaught { species: "SPECIES_RATTATA".into() },
        ]);
        assert_eq!(s.pc.boxes[0].value.as_ref().unwrap().len(), 1);
        assert_eq!(s.pokedex.caught["SPECIES_RATTATA"].value, Some(true));
        assert_eq!(s.pokedex.seen["SPECIES_RATTATA"].value, Some(true));
    }

    #[test]
    fn checkpoint_restore_replaces_knowledge() {
        let saved = run(vec![starter()]).saved_knowledge();
        let s = run(vec![
            starter(),
            GameEvent::MoveUsed { slot: 0, move_slot: 0 },
            GameEvent::MoneyObserved { amount: 1 },
            GameEvent::CheckpointRestored { knowledge: Box::new(saved.clone()) },
        ]);
        assert_eq!(s.saved_knowledge(), saved);
        assert_eq!(SavedKnowledge::default().money, Knowledge::unknown());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `~/.cargo/bin/cargo test -p pokebot-state reduce_knowledge`
Expected: compile error (variants missing).

- [ ] **Step 3: Implement**

`events.rs` (import `crate::{BoxMon, ItemList, Pocket, SavedKnowledge}`):

```rust
    /// Items gained (+) or spent (−) as told by text or a confirmed action.
    ItemsChanged { pocket: Pocket, item: String, delta: i32, reason: String },
    PocketObserved { pocket: Pocket, items: ItemList },
    MoneyObserved { amount: u32 },
    MoneyChanged { delta: i64, reason: String },
    BoxObserved { box_index: u8, mons: Vec<BoxMon> },
    PcItemsObserved { items: ItemList },
    /// A caught Pokémon went to the PC ("transferred to BILL's PC").
    SentToPc { box_index: Option<u8>, mon: BoxMon },
    MonDeposited { party_slot: u8, box_index: u8 },
    MonWithdrawn { box_index: u8, box_slot: u8 },
    SpeciesSeen { species: String },
    SpeciesCaught { species: String },
    /// A shiny appeared (for the log and the catch policy).
    ShinySeen { species: String },
    /// The knowledge stored with the save that was just loaded.
    CheckpointRestored { knowledge: Box<SavedKnowledge> },
```

`reduce_knowledge.rs`, add arms before `_ => return false`:

```rust
        GameEvent::ItemsChanged { pocket, item, delta, .. } => {
            if let Some(k) = state.bag.pockets.get_mut(pocket) {
                if let Some(list) = &k.value {
                    *k = Knowledge::tracked(add_items(list, item, *delta), k.last_verified_frame);
                }
            }
        }
        GameEvent::PocketObserved { pocket, items } => {
            state.bag.pockets.insert(*pocket, Knowledge::observed(items.clone(), frame));
        }
        GameEvent::MoneyObserved { amount } => state.money = Knowledge::observed(*amount, frame),
        GameEvent::MoneyChanged { delta, .. } => {
            if let Some(m) = state.money.value {
                let new = (i64::from(m) + delta).clamp(0, i64::from(u32::MAX)) as u32;
                state.money = Knowledge::tracked(new, state.money.last_verified_frame);
            }
        }
        GameEvent::BoxObserved { box_index, mons } => {
            if let Some(b) = state.pc.boxes.get_mut(usize::from(*box_index)) {
                *b = Knowledge::observed(mons.clone(), frame);
            }
        }
        GameEvent::PcItemsObserved { items } => state.pc.items = Knowledge::observed(items.clone(), frame),
        GameEvent::SentToPc { box_index: Some(i), mon } => {
            if let Some(b) = state.pc.boxes.get_mut(usize::from(*i)) {
                if let Some(list) = &b.value {
                    let mut list = list.clone();
                    list.push(mon.clone());
                    *b = Knowledge::tracked(list, b.last_verified_frame);
                }
            }
        }
        GameEvent::SentToPc { box_index: None, .. } | GameEvent::ShinySeen { .. } => {}
        GameEvent::MonDeposited { party_slot, box_index } => {
            let taken = state.party.value.as_mut().and_then(|p| {
                (usize::from(*party_slot) < p.len()).then(|| p.remove(usize::from(*party_slot)))
            });
            if state.party.value.is_some() {
                state.party.source = KnowledgeSource::Tracked;
            }
            if let (Some(mon), Some(b)) = (taken, state.pc.boxes.get_mut(usize::from(*box_index))) {
                if let Some(list) = &b.value {
                    let mut list = list.clone();
                    let slot = (0..30u8).find(|s| list.iter().all(|m| m.slot != *s)).unwrap_or(0);
                    list.push(crate::BoxMon {
                        slot,
                        species: mon.species,
                        level: mon.level,
                        nickname: mon.nickname,
                    });
                    *b = Knowledge::tracked(list, b.last_verified_frame);
                }
            }
        }
        GameEvent::MonWithdrawn { box_index, box_slot } => {
            let mon = state.pc.boxes.get_mut(usize::from(*box_index)).and_then(|b| {
                let list = b.value.as_mut()?;
                let at = list.iter().position(|m| m.slot == *box_slot)?;
                let mon = list.remove(at);
                b.source = KnowledgeSource::Tracked;
                Some(mon)
            });
            if let Some(mon) = mon {
                let slot = state.party.value.as_ref().map_or(0, |p| p.len()) as u8;
                let m = member(state, slot, KnowledgeSource::Tracked, frame);
                m.species = mon.species;
                m.level = mon.level;
                m.nickname = mon.nickname;
            }
        }
        GameEvent::SpeciesSeen { species } => {
            state.pokedex.seen.insert(species.clone(), Knowledge::observed(true, frame));
        }
        GameEvent::SpeciesCaught { species } => {
            state.pokedex.seen.insert(species.clone(), Knowledge::observed(true, frame));
            state.pokedex.caught.insert(species.clone(), Knowledge::observed(true, frame));
        }
        GameEvent::CheckpointRestored { knowledge } => {
            let k = (**knowledge).clone();
            state.party = k.party;
            state.bag = k.bag;
            state.money = k.money;
            state.pc = k.pc;
            state.pokedex = k.pokedex;
        }
```

and the helper:

```rust
/// `list` with `delta` of `item` added (entries reaching 0 are removed; new
/// items go to the end, as the game lists them).
fn add_items(list: &crate::ItemList, item: &str, delta: i32) -> crate::ItemList {
    let mut out = list.clone();
    match out.iter().position(|(i, _)| i == item) {
        Some(at) => {
            let n = (i32::from(out[at].1) + delta).max(0) as u16;
            if n == 0 {
                out.remove(at);
            } else {
                out[at].1 = n;
            }
        }
        None if delta > 0 => out.push((item.to_owned(), delta.min(i32::from(u16::MAX)) as u16)),
        None => {}
    }
    out
}
```

Add the new variants to the `unreachable!` arm in `reducer.rs`, and `summarize` arms in `crates/runtime/src/lib.rs`:

```rust
        GameEvent::ItemsChanged { item, delta, reason, .. } => format!("Bag {item} {delta:+} ({reason})"),
        GameEvent::PocketObserved { pocket, items } => format!("Bag {pocket:?}: {} items seen", items.len()),
        GameEvent::MoneyObserved { amount } => format!("Money ₽{amount}"),
        GameEvent::MoneyChanged { delta, reason } => format!("Money {delta:+} ({reason})"),
        GameEvent::BoxObserved { box_index, mons } => format!("PC box {}: {} Pokémon", box_index + 1, mons.len()),
        GameEvent::PcItemsObserved { items } => format!("PC items: {}", items.len()),
        GameEvent::SentToPc { box_index, mon } => format!("{:?} sent to PC box {:?}", mon.species.value, box_index.map(|b| b + 1)),
        GameEvent::MonDeposited { party_slot, box_index } => format!("Party {party_slot} deposited in box {}", box_index + 1),
        GameEvent::MonWithdrawn { box_index, box_slot } => format!("Withdrew box {} slot {box_slot}", box_index + 1),
        GameEvent::SpeciesSeen { species } => format!("Seen {species}"),
        GameEvent::SpeciesCaught { species } => format!("Caught {species}"),
        GameEvent::ShinySeen { species } => format!("SHINY {species}!"),
        GameEvent::CheckpointRestored { .. } => "Checkpoint knowledge restored".into(),
```

- [ ] **Step 4: Run tests**

Run: `~/.cargo/bin/cargo test -p pokebot-state && ~/.cargo/bin/cargo build --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add crates/state crates/runtime
git commit -m "state: bag, money, PC, Pokédex events and checkpoint restore

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Game data lookups (item pockets, names) and the small font

**Files:**
- Modify: `tools/gamedata/extract_gamedata.py:207`, `crates/gamedata/src/lib.rs` (`Item`, lookups)
- Modify: `tools/gamedata/extract_font.py`, `tools/world/build.sh`

**Interfaces:**
- Produces:
  - `Item.pocket: Option<String>` (decomp `POCKET_*`);
  - `GameData::item_named(&self, read: &str) -> Option<&str>`;
  - `GameData::species_named(&self, read: &str) -> Option<&str>` (both `?`-wildcard, unique match only, as `move_named`);
  - `data/world/font_small.json` (same format as `font_normal.json`).

- [ ] **Step 1: Write the failing test** (append to `crates/gamedata/src/lib.rs` tests; create `#[cfg(test)] mod tests` if absent)

```rust
#[cfg(test)]
mod lookup_tests {
    use super::*;

    fn data() -> Option<GameData> {
        GameData::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json")).ok()
    }

    #[test]
    fn names_resolve_uniquely_with_wildcards() {
        let Some(d) = data() else { return };
        assert_eq!(d.item_named("POKé BALL"), Some("ITEM_POKE_BALL"));
        assert_eq!(d.items["ITEM_POKE_BALL"].pocket.as_deref(), Some("POCKET_POKE_BALLS"));
        assert_eq!(d.species_named("I?YSAUR"), Some("SPECIES_IVYSAUR"));
        assert_eq!(d.move_named("POISONPOWDER"), Some("MOVE_POISON_POWDER"));
        // Too many wildcards: several moves fit, so nothing is resolved.
        assert_eq!(d.move_named("?????"), None);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `~/.cargo/bin/cargo test -p pokebot-gamedata lookup`
Expected: compile error (`item_named`, `pocket`).

- [ ] **Step 3: Implement**

`extract_gamedata.py` line 207 becomes:

```python
    items = {i["itemId"]: {"price": i.get("price", 0), "name": i.get("english", i["itemId"]), "pocket": i.get("pocket")} for i in json.loads((src / "data/items.json").read_text())["items"]}
```

`crates/gamedata/src/lib.rs`: `Item` gains `#[serde(default)] pub pocket: Option<String>,`. Refactor `move_named` into a shared helper and add the two lookups:

```rust
/// The unique key whose printed name matches `read` (`?` = any character).
fn unique_named<'a>(names: impl Iterator<Item = (&'a String, &'a str)>, read: &str) -> Option<&'a str> {
    let fits = |name: &str| {
        name.chars().count() == read.chars().count()
            && name.chars().zip(read.chars()).all(|(a, b)| b == '?' || a == b)
    };
    let mut found = names.filter(|(_, n)| fits(n)).map(|(k, _)| k.as_str());
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

    pub fn move_named(&self, read: &str) -> Option<&str> {
        unique_named(self.moves.iter().filter_map(|(k, m)| Some((k, m.name.as_deref()?))), read)
    }

    pub fn item_named(&self, read: &str) -> Option<&str> {
        unique_named(self.items.iter().map(|(k, i)| (k, i.name.as_str())), read)
    }

    /// Species by printed name (`SPECIES_NIDORAN_F` prints as `NIDORAN♀`).
    pub fn species_named(&self, read: &str) -> Option<&str> {
        let printed: Vec<(&String, String)> = self
            .species
            .keys()
            .map(|k| {
                let base = k.trim_start_matches("SPECIES_");
                let name = match base {
                    "NIDORAN_F" => "NIDORAN♀".to_owned(),
                    "NIDORAN_M" => "NIDORAN♂".to_owned(),
                    "MR_MIME" => "MR. MIME".to_owned(),
                    "FARFETCHD" => "FARFETCH'D".to_owned(),
                    _ => base.replace('_', " "),
                };
                (k, name)
            })
            .collect();
        unique_named(printed.iter().map(|(k, n)| (*k, n.as_str())), read)
    }
```

(If `species`/`items`/`moves` are `HashMap`s, the result is still deterministic because only a *unique* match is returned.)

`extract_font.py`: add `--font {normal,small}` (default `normal`). For `small`: sheet `graphics/fonts/latin_small.png`, width table `sFontSmallLatinGlyphWidths`, cell width 8 (columns = `sheet.width // 8`), output `data/world/font_small.json`. Change `parse_widths(text_c)` to `parse_widths(text_c, table)` and the cell lookup to `row, col = divmod(code, sheet.width // cell_w)` with pixels read at `(col * cell_w + x, row * 16 + y)`. `build.sh` gains the line `"${VENV}/bin/python" tools/gamedata/extract_font.py --font small`.

- [ ] **Step 4: Run**

Run: `.venv/bin/python tools/gamedata/extract_gamedata.py && .venv/bin/python tools/gamedata/extract_font.py && .venv/bin/python tools/gamedata/extract_font.py --font small && ~/.cargo/bin/cargo test -p pokebot-gamedata lookup`
Expected: `font: 152 glyphs → data/world/font_normal.json`, a similar line for `font_small.json`, test PASS.

- [ ] **Step 5: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add tools crates/gamedata
git commit -m "gamedata: item pockets, name lookups; extract the small font too

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Move names in the battle move menu

**Files:**
- Modify: `crates/state/src/observation.rs` (`BattleObservation`), `crates/vision/src/detect/battle.rs`, `crates/vision/src/lib.rs`, `apps/pokebot-cli/src/main.rs` (inspect + `story` font loading)

**Interfaces:**
- Consumes: Task 4 `font_small.json`.
- Produces:
  - `BattleObservation.move_names: Vec<String>` (`#[serde(default)]`; the four cells in menu order, `""` for an empty cell; empty vec when the move menu isn't open);
  - `FireRedPerception::with_small_font(Arc<Font>)`;
  - `detect::battle::MOVE_NAME_CELLS: [Region; 4]`.

Measured on `captures/fixtures/move-select.png` (the frame from `/tmp/run-ml3/frames/00004618.png`): names are in `latin_small`, cells start at x 16 (left) and x 88 (right), and the ink rows are 125–131 (top row) and 141–147 (bottom row). The ▶ sits 7 px left of a cell, so the cells are 64 px wide to keep it out.

- [ ] **Step 1: Write the failing test** (in `crates/vision/src/lib.rs` tests)

```rust
    #[test]
    fn move_menu_names_are_read_with_the_small_font() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(small), Ok(normal)) = (
            text::Font::load(root.join("data/world/font_small.json")),
            text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let Ok(image) = pokebot_video::png::load(root.join("captures/fixtures/move-select.png")) else {
            return;
        };
        let mut p = FireRedPerception::default()
            .with_font(std::sync::Arc::new(normal))
            .with_small_font(std::sync::Arc::new(small));
        let b = p.observe(&frame(0, image)).battle.unwrap();
        assert_eq!(b.move_names, vec!["TACKLE", "GROWL", "LEECH SEED", "VINE WHIP"]);
        assert_eq!(b.move_pp, Some((35, 35)));
    }
```

(`pokebot-video` must be a dev-dependency of `pokebot-vision`: add `[dev-dependencies] pokebot-video.workspace = true`.)

- [ ] **Step 2: Run to verify it fails**

Run: `~/.cargo/bin/cargo test -p pokebot-vision move_menu_names`
Expected: compile error (`with_small_font`, `move_names`).

- [ ] **Step 3: Implement**

`observation.rs` `BattleObservation`: add

```rust
    /// The four move cells of the move menu (menu order, "" when empty).
    #[serde(default)]
    pub move_names: Vec<String>,
```

and `move_names: Vec::new()` in `battle.rs`'s constructor. `battle.rs`:

```rust
/// Move-menu name cells (latin_small), menu order: top-left, top-right,
/// bottom-left, bottom-right.
pub const MOVE_NAME_CELLS: [Region; 4] = [
    Region { x: 16, y: 120, width: 64, height: 14 },
    Region { x: 88, y: 120, width: 64, height: 14 },
    Region { x: 16, y: 136, width: 64, height: 14 },
    Region { x: 88, y: 136, width: 64, height: 14 },
];
```

`lib.rs`: field `small_font: Option<Arc<text::Font>>`, builder

```rust
    /// Reads small-font text (battle move names).
    pub fn with_small_font(mut self, font: Arc<text::Font>) -> Self {
        self.small_font = Some(font);
        self
    }
```

and in the `Moves` branch (next to `move_pp`):

```rust
                if let Some(small) = &self.small_font {
                    b.move_names = detect::battle::MOVE_NAME_CELLS
                        .iter()
                        .map(|r| small.read(image, *r, &[]).join(" "))
                        .collect();
                }
```

`main.rs`: in `story` load `font_small.json` like `font_normal.json` and call `.with_small_font(..)`; in `inspect`, load it when present (default path `data/world/font_small.json`, sibling of `--font`) and print `b.move_names` in `describe_observation`.

- [ ] **Step 4: Run**

Run: `~/.cargo/bin/cargo test -p pokebot-vision && ~/.cargo/bin/cargo build --release -p pokebot-cli && target/release/pokebot inspect captures/fixtures/move-select.png`
Expected: tests PASS; the inspect line contains `["TACKLE", "GROWL", "LEECH SEED", "VINE WHIP"]`. If a name reads with `?` or is cut off, re-measure the cell with the Python dump used in the design (`dark = a.sum(axis=2) < 300` over rows 118–151) and adjust `MOVE_NAME_CELLS`; the test is the acceptance criterion.

- [ ] **Step 5: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add crates apps
git commit -m "vision: read move names in the battle move menu (small font)

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: `agent::Party` as a view of the state, and battle observations as events

**Files:**
- Modify: `crates/agent/src/party.rs`, `crates/agent/src/battle.rs` (tests)

**Interfaces:**
- Consumes: Tasks 1–5 (`GameState.party`, party events, `species_named`, `move_named`, `BattleObservation.{move_names, move_pp}`).
- Produces:
  - `Member.slot: u8` (`#[serde(default)]`);
  - `Party::from_state(state: &GameState) -> Party` — members whose species is known, moves in menu order with `"?"` for a slot whose move is unknown, `pp_used` = max − cur for known PP (unknown PP counts as full);
  - `pub fn battle_events(data: &GameData, party: &Party, battle: &BattleObservation) -> Vec<GameEvent>` — emits only what differs from `party`;
  - `pub fn starter_mon(data: &GameData, species: &str, level: u8) -> PartyMon`.
- Removes: `Party::observe_battle`, `Member::observe_level`, `Party::heal_all`.

- [ ] **Step 1: Write the failing tests** (replace `the_hud_and_move_menu_update_species_and_pp` in `battle.rs` tests with these in `party.rs`)

```rust
#[cfg(test)]
mod tests {
    use std::path::Path;

    use pokebot_state::{BattleMenu, BattleObservation, DefaultReducer, EventRecord, GameState, StateReducer};

    use super::*;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json")).ok()
    }

    fn state_with(data: &GameData, species: &str, level: u8) -> GameState {
        let event = GameEvent::PartyMonDerived { slot: 0, mon: Box::new(starter_mon(data, species, level)) };
        DefaultReducer.reduce(&GameState::default(), &[EventRecord { frame_id: 1, event }])
    }

    fn hud(name: &str, level: u8) -> BattleObservation {
        BattleObservation {
            menu: Some(BattleMenu::Moves { column: 0, row: 0 }),
            player_name: Some(name.into()),
            player_level: Some(level),
            player_hp_numbers: Some((28, 49)),
            opponent_name: None,
            opponent_level: None,
            player_hp: Some(562),
            opponent_hp: None,
            move_pp: Some((12, 35)),
            move_names: vec!["TACKLE".into(), "GROWL".into(), "LEECH SEED".into(), "VINE WHIP".into()],
        }
    }

    #[test]
    fn the_view_follows_the_state() {
        let Some(d) = data() else { return };
        let party = Party::from_state(&state_with(&d, "SPECIES_BULBASAUR", 10));
        let lead = party.lead().unwrap();
        assert_eq!(lead.species, "SPECIES_BULBASAUR");
        assert_eq!(lead.moves, vec!["MOVE_TACKLE", "MOVE_GROWL", "MOVE_LEECH_SEED", "MOVE_VINE_WHIP"]);
        assert_eq!(lead.pp_left(&d, "MOVE_TACKLE"), 35);
    }

    #[test]
    fn hud_and_move_menu_become_events() {
        let Some(d) = data() else { return };
        let state = state_with(&d, "SPECIES_BULBASAUR", 15);
        let party = Party::from_state(&state);
        let events = battle_events(&d, &party, &hud("I?YSAUR", 16));
        assert!(events.contains(&GameEvent::Evolved { slot: 0, species: "SPECIES_IVYSAUR".into() }));
        assert!(events.contains(&GameEvent::MovePpObserved { slot: 0, move_slot: 0, cur: 12, max: 35 }));
        assert!(events.iter().any(|e| matches!(e, GameEvent::PartyObserved { level: Some(16), hp: Some((28, 49)), .. })));
    }

    #[test]
    fn unchanged_hud_emits_nothing() {
        let Some(d) = data() else { return };
        let state = state_with(&d, "SPECIES_BULBASAUR", 10);
        let events = battle_events(&d, &Party::from_state(&state), &hud("BULBASAUR", 10));
        let state = DefaultReducer.reduce(
            &state,
            &events.into_iter().map(|event| EventRecord { frame_id: 2, event }).collect::<Vec<_>>(),
        );
        assert!(battle_events(&d, &Party::from_state(&state), &hud("BULBASAUR", 10)).is_empty());
    }

    #[test]
    fn ambiguous_move_name_emits_nothing() {
        let Some(d) = data() else { return };
        let state = state_with(&d, "SPECIES_BULBASAUR", 10);
        let mut obs = hud("BULBASAUR", 10);
        obs.move_names = vec!["?????".into(), "GROWL".into(), "LEECH SEED".into(), "VINE WHIP".into()];
        let events = battle_events(&d, &Party::from_state(&state), &obs);
        assert!(!events.iter().any(|e| matches!(e, GameEvent::MovesObserved { .. })));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `~/.cargo/bin/cargo test -p pokebot-agent --lib party`
Expected: compile error (`from_state`, `battle_events`, `starter_mon`).

- [ ] **Step 3: Implement** in `crates/agent/src/party.rs` (keep `Member`, `pp_left`, `wild_attack_pp`, `display_name`, `names_match`, `seen_as`; delete `observe_level`, `observe_battle`, `heal_all`).

First add the slot to `Member` (first field), and set `slot: 0` in every `Member { … }` literal: `Member::new`, `moves.rs` tests (`bulbasaur()`) and `battle.rs` tests (`ivysaur()`):

```rust
    /// Party slot (0 = lead).
    #[serde(default)]
    pub slot: u8,
```

Then:

```rust
use pokebot_state::{BattleMenu, BattleObservation, GameEvent, GameState, Knowledge, MoveSlot, PartyMon};

impl Party {
    /// The party as the state knows it (members with a known species).
    pub fn from_state(state: &GameState) -> Party {
        let members = state
            .party
            .value
            .iter()
            .flatten()
            .enumerate()
            .filter_map(|(slot, m)| {
                let species = m.species.value.clone()?;
                let moves: Vec<&MoveSlot> = m.moves.iter().flatten().collect();
                Some(Member {
                    slot: slot as u8,
                    species,
                    level: m.level.value.unwrap_or(0),
                    moves: moves.iter().map(|s| s.mv.value.clone().unwrap_or_else(|| "?".into())).collect(),
                    hp: m.hp.value,
                    pp_used: moves
                        .iter()
                        .filter_map(|s| Some((s.mv.value.clone()?, s.pp.value?)))
                        .map(|(mv, (cur, max))| (mv, max.saturating_sub(cur)))
                        .collect(),
                })
            })
            .collect();
        Party { members }
    }
}

/// A newly obtained Pokémon, known from game data (default moves, full PP).
pub fn starter_mon(data: &GameData, species: &str, level: u8) -> PartyMon {
    let mut mon = PartyMon {
        species: Knowledge::derived(species.to_owned(), 0),
        level: Knowledge::derived(level, 0),
        ..PartyMon::default()
    };
    for (i, mv) in data.default_moves(species, level).into_iter().enumerate().take(4) {
        let max = data.move_(&mv).map_or(0, |m| m.pp);
        mon.moves[i] = Some(MoveSlot { mv: Knowledge::derived(mv, 0), pp: Knowledge::derived((max, max), 0) });
    }
    mon
}

/// Events for what the battle HUD and move menu show that the party view
/// doesn't know yet. Nothing is emitted for unchanged or ambiguous readings.
pub fn battle_events(data: &GameData, party: &Party, battle: &BattleObservation) -> Vec<GameEvent> {
    let mut events = Vec::new();
    let Some(name) = &battle.player_name else { return events };
    let Some((member, species)) = party
        .members
        .iter()
        .find_map(|m| seen_as(data, &m.species, name).map(|s| (m, s)))
    else {
        return events;
    };
    let slot = member.slot;
    if species != member.species {
        events.push(GameEvent::Evolved { slot, species });
    }
    let level = battle.player_level.filter(|l| *l != member.level);
    let hp = battle.player_hp_numbers.filter(|hp| Some(*hp) != member.hp);
    if level.is_some() || hp.is_some() {
        events.push(GameEvent::PartyObserved {
            slot,
            species: None,
            nickname: None,
            level,
            hp,
            status: None,
            held_item: None,
        });
    }
    if let Some(BattleMenu::Moves { column, row }) = battle.menu {
        let names: Option<Vec<String>> = battle
            .move_names
            .iter()
            .filter(|n| !n.is_empty() && n.as_str() != "-")
            .map(|n| data.move_named(n).map(str::to_owned))
            .collect();
        let mut moves = member.moves.clone();
        if let Some(names) = names.filter(|n| !n.is_empty() && *n != member.moves) {
            events.push(GameEvent::MovesObserved { slot, moves: names.clone() });
            moves = names;
        }
        let at = usize::from(row * 2 + column);
        if let (Some((cur, max)), Some(mv)) = (battle.move_pp, moves.get(at)) {
            let known = member.moves.get(at) == Some(mv)
                && data.move_(mv).is_some_and(|m| m.pp == max)
                && member.pp_left(data, mv) == cur;
            if !known {
                events.push(GameEvent::MovePpObserved { slot, move_slot: at as u8, cur, max });
            }
        }
    }
    events
}
```

Note the `known` check compares against the *maximum from game data*. If a PP Up has raised the maximum, `max` differs, so the reading is emitted.

Remove the old `the_hud_and_move_menu_update_species_and_pp` test from `battle.rs`. Fix `ivysaur()` in the `battle.rs` tests to set `slot: 0`.

- [ ] **Step 4: Run tests**

Run: `~/.cargo/bin/cargo test -p pokebot-agent --lib`
Expected: `party::tests` PASS; the crate may not compile yet where `story.rs`/`learn.rs` call removed methods — if so, make only the minimal compile fix of replacing `self.party.observe_battle(data, battle)` with `Vec::new()` temporarily **and do not commit** until Task 7 lands; otherwise commit now.

- [ ] **Step 5: Commit** (together with Task 7 if the crate didn't compile alone)

```bash
~/.cargo/bin/cargo fmt --all
git add crates/agent
git commit -m "agent: party view over GameState; battle HUD and move menu emit events

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Story emits events (learning, PP, text facts) instead of mutating

**Files:**
- Modify: `crates/agent/src/learn.rs`, `crates/agent/src/story.rs`, `crates/agent/src/lib.rs`
- Create: `crates/agent/src/track.rs`

**Interfaces:**
- Consumes: Task 6 `Party::from_state`, `battle_events`, `Member.slot`.
- Produces:
  - `MoveLearning::observe_page(&mut self, lines: &[String], data: &GameData, party: &Party, targets: &[String]) -> (Vec<GameEvent>, Vec<String>)` — events and log lines;
  - `MoveLearning::on_move_list(&mut self, list: &MoveListObservation, data: &GameData, party: &Party, targets: &[String]) -> (Decision, Vec<GameEvent>)`;
  - `track::TextTracker::observe_page(&mut self, lines: &[String], data: &GameData, party: &Party, last_move: Option<(u8, u8)>) -> Vec<GameEvent>`;
  - `StoryTask::with_data(self, data: Arc<GameData>) -> Self` (replaces `with_party`);
  - `StoryTask::party()` keeps returning `&Party` (the last view).

- [ ] **Step 1: Write the failing tests** (`crates/agent/src/track.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_owned).collect()
    }

    fn data() -> Option<GameData> {
        GameData::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json")).ok()
    }

    #[test]
    fn text_becomes_money_item_and_heal_events() {
        let Some(d) = data() else { return };
        let party = Party::default();
        let mut t = TextTracker::default();
        assert_eq!(
            t.observe_page(&lines("RED got ¥1,200\nfor winning!"), &d, &party, None),
            vec![GameEvent::MoneyChanged { delta: 1200, reason: "won a battle".into() }]
        );
        assert_eq!(
            t.observe_page(&lines("RED found a POTION!"), &d, &party, None),
            vec![GameEvent::ItemsChanged {
                pocket: Pocket::Items,
                item: "ITEM_POTION".into(),
                delta: 1,
                reason: "found".into()
            }]
        );
        assert_eq!(
            t.observe_page(&lines("We've restored your POKéMON\nto full health."), &d, &party, None),
            vec![GameEvent::Healed]
        );
        // The same page is read once.
        assert!(t.observe_page(&lines("We've restored your POKéMON\nto full health."), &d, &party, None).is_empty());
    }

    #[test]
    fn no_pp_page_zeroes_move() {
        let Some(d) = data() else { return };
        let mut t = TextTracker::default();
        assert_eq!(
            t.observe_page(&lines("There's no PP left for\nthis move!"), &d, &Party::default(), Some((0, 2))),
            vec![GameEvent::MoveOutOfPp { slot: 0, move_slot: 2 }]
        );
    }
}
```

and in `learn.rs` update `parses_the_pages_of_a_move_swap` unchanged, plus:

```rust
    #[test]
    fn a_swap_becomes_a_replace_event() {
        let Some(d) = pokebot_gamedata::GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok() else {
            return;
        };
        let party = Party {
            members: vec![Member {
                slot: 0,
                species: "SPECIES_BULBASAUR".into(),
                level: 15,
                moves: ["MOVE_TACKLE", "MOVE_GROWL", "MOVE_LEECH_SEED", "MOVE_VINE_WHIP"].map(String::from).to_vec(),
                hp: None,
                pp_used: Default::default(),
            }],
        };
        let mut l = MoveLearning::default();
        let page = |s: &str| vec![s.to_owned()];
        l.observe_page(&page("BULBASAUR is trying to learn POISONPOWDER."), &d, &party, &[]);
        l.observe_page(&page("BULBASAUR forgot GROWL."), &d, &party, &[]);
        let (events, _) = l.observe_page(&page("BULBASAUR learned POISONPOWDER!"), &d, &party, &[]);
        assert_eq!(
            events,
            vec![GameEvent::MoveReplaced {
                slot: 0,
                move_slot: 1,
                old: "MOVE_GROWL".into(),
                new: "MOVE_POISON_POWDER".into(),
                max_pp: 35
            }]
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `~/.cargo/bin/cargo test -p pokebot-agent --lib track learn`
Expected: compile errors.

- [ ] **Step 3: Implement**

`track.rs`:

```rust
//! Facts in dialogue text that change the bag, money or party: money won,
//! items found or received, the nurse's heal, and an empty move.

use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, Pocket};

use crate::party::Party;

#[derive(Debug, Default)]
pub struct TextTracker {
    last_page: String,
}

impl TextTracker {
    /// Events for a fully printed page (each page once). `last_move` is the
    /// (party slot, move slot) chosen last in battle.
    pub fn observe_page(
        &mut self,
        lines: &[String],
        data: &GameData,
        _party: &Party,
        last_move: Option<(u8, u8)>,
    ) -> Vec<GameEvent> {
        let page = lines.join(" ");
        if page.is_empty() || page == self.last_page {
            return Vec::new();
        }
        self.last_page = page.clone();
        let mut events = Vec::new();
        if let Some(amount) = money_won(&page) {
            events.push(GameEvent::MoneyChanged { delta: i64::from(amount), reason: "won a battle".into() });
        }
        if let Some((name, reason)) = item_gained(&page) {
            if let Some(key) = data.item_named(&name) {
                if let Some(pocket) = data.items[key].pocket.as_deref().and_then(Pocket::from_decomp) {
                    events.push(GameEvent::ItemsChanged { pocket, item: key.to_owned(), delta: 1, reason });
                }
            }
        }
        if page.contains("restored your POKéMON") {
            events.push(GameEvent::Healed);
        }
        if page.contains("no PP left for") {
            if let Some((slot, move_slot)) = last_move {
                events.push(GameEvent::MoveOutOfPp { slot, move_slot });
            }
        }
        events
    }
}

/// "RED got ¥1,200 for winning!" → 1200.
fn money_won(page: &str) -> Option<u32> {
    let rest = &page[page.find(" got ¥")? + " got ¥".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == ',').filter(|c| *c != ',').collect();
    page.contains("for winning").then(|| digits.parse().ok()).flatten()
}

/// "RED found a POTION!" / "received the TOWN MAP." / "obtained a X!"
fn item_gained(page: &str) -> Option<(String, String)> {
    for (verb, reason) in [(" found ", "found"), (" received ", "received"), (" obtained ", "obtained")] {
        if let Some(at) = page.find(verb) {
            let rest = &page[at + verb.len()..];
            let rest = ["a ", "an ", "the ", "one "]
                .iter()
                .find_map(|p| rest.strip_prefix(p))
                .unwrap_or(rest);
            let end = rest.find(['!', '.'])?;
            return Some((rest[..end].trim().to_owned(), reason.to_owned()));
        }
    }
    None
}
```

`learn.rs` changes:
- `observe_page` takes `party: &Party` and returns `(Vec<GameEvent>, Vec<String>)`. Replace each mutation with an event, using `member.slot`:
  - `Forgot` → remember `(old, move_slot)` in `forgot_slot`;
  - `Learned` with a remembered slot → `MoveReplaced { slot, move_slot, old, new, max_pp }` (`max_pp` = `data.move_(&key).map_or(0, |m| m.pp)`);
  - `Learned` without one, while `member.moves.len() < 4` → `MoveLearned { slot, move_slot: member.moves.len() as u8, mv, max_pp }`;
  - `Evolved` → `GameEvent::Evolved { slot, species }`.
- `member_named` returns `Option<&Member>`.
- `on_move_list` takes `party: &Party` and returns `(Decision, Vec<GameEvent>)`. When the list's first four names resolve and differ from `member.moves`, emit `MovesObserved { slot, moves: shown.clone() }`, and recompute the choice on a clone of the member with `moves = shown`.

`story.rs`:
- Remove `with_party`; add `with_data(data)` (sets `self.data`) and a field `tracker: TextTracker`.
- The field `party: Party` stays and is refreshed first thing in `next()`: `self.party = Party::from_state(ctx.state);`.
- Replace `self.party.observe_battle(...)` with `ctx.events.extend(party::battle_events(data, &self.party, battle));`.
- Where `learning.observe_page` is called: `let (events, log) = …; ctx.events.extend(events);` and push `log` as `GoalProgress` Party entries (as today). Then call `self.tracker.observe_page(&d.lines, &data, &self.party, self.battle_memory.last_slot)` and extend.
- `on_move_list`: `let (decision, events) = …; ctx.events.extend(events); return decision;`.
- `BattleMemory` gains `last_slot: Option<(u8, u8)>`, set in `battle::decide` when choosing a move: `(lead.slot, slot)`.
- `on_outcome`: instead of `pp_used += 1`, push `GameEvent::MoveUsed { slot, move_slot }` from `battle_memory.last_slot` into `ctx.events`.
- Remove `self.party.heal_all()` (the nurse's text now emits `Healed`).
- `plan()` reads `self.party` as before.

`lib.rs`: `pub mod track;`.

- [ ] **Step 4: Run tests**

Run: `~/.cargo/bin/cargo test -p pokebot-agent && ~/.cargo/bin/cargo clippy -q --workspace --all-targets`
Expected: PASS, no warnings. (`crates/agent/tests/new_game_emulator.rs` must still pass; if it used `with_party`, switch it to `with_data`.)

- [ ] **Step 5: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add crates/agent
git commit -m "agent: story emits party, PP, money, item and heal events

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Checkpoints — `saves/state.json`, seeding, restore on continue and retry

**Files:**
- Create: `crates/agent/src/checkpoint.rs`
- Modify: `crates/agent/src/progress.rs`, `crates/agent/src/lib.rs`, `apps/pokebot-cli/src/main.rs:426-560`

**Interfaces:**
- Consumes: `SavedKnowledge`, `GameEvent::CheckpointRestored`, `GameEvent::PartyMonDerived`, `party::starter_mon`.
- Produces:
  - `checkpoint::path_for(progress: &Path) -> PathBuf` (`progress.json` → `state.json` in the same directory);
  - `checkpoint::store(path: &Path, k: &SavedKnowledge) -> Result<()>`;
  - `checkpoint::load(path: &Path) -> Result<Option<SavedKnowledge>>` (`None` if the file is missing);
  - `checkpoint::legacy_knowledge(data: &GameData, party: &Party) -> SavedKnowledge`;
  - `checkpoint::restore(state_path: &Path, progress: &Progress, data: &GameData) -> Result<SavedKnowledge>`.

- [ ] **Step 1: Write the failing tests** (`checkpoint.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::party::Member;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json")).ok()
    }

    #[test]
    fn legacy_party_migrates() {
        let Some(d) = data() else { return };
        let party = Party {
            members: vec![Member {
                slot: 0,
                species: "SPECIES_IVYSAUR".into(),
                level: 18,
                moves: ["MOVE_TACKLE", "MOVE_SLEEP_POWDER", "MOVE_LEECH_SEED", "MOVE_VINE_WHIP"].map(String::from).to_vec(),
                hp: Some((54, 54)),
                pp_used: [("MOVE_VINE_WHIP".to_owned(), 5)].into_iter().collect(),
            }],
        };
        let k = legacy_knowledge(&d, &party);
        let mon = &k.party.value.as_ref().unwrap()[0];
        assert_eq!(mon.species.value.as_deref(), Some("SPECIES_IVYSAUR"));
        assert!(mon.species.is_stale(), "legacy knowledge is tracked, not observed");
        assert_eq!(mon.moves[3].as_ref().unwrap().pp.value, Some((5, 10)));
        assert_eq!(Party::from_state(&{
            let mut s = pokebot_state::GameState::default();
            s.party = k.party.clone();
            s
        }).lead().unwrap().moves, party.members[0].moves);
    }

    #[test]
    fn store_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("pokebot-ckpt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = path_for(&dir.join("progress.json"));
        assert_eq!(path, dir.join("state.json"));
        assert_eq!(load(&path).unwrap(), None);
        let mut k = SavedKnowledge::default();
        k.money = pokebot_state::Knowledge::observed(42, 7);
        store(&path, &k).unwrap();
        assert_eq!(load(&path).unwrap(), Some(k));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `~/.cargo/bin/cargo test -p pokebot-agent --lib checkpoint`
Expected: compile error.

- [ ] **Step 3: Implement**

`checkpoint.rs`:

```rust
//! The knowledge that goes with a save file (`saves/state.json`), written
//! after every in-game save and restored whenever that save is loaded.

use std::path::{Path, PathBuf};

use pokebot_core::{Error, Result};
use pokebot_gamedata::GameData;
use pokebot_state::{Knowledge, KnowledgeSource, MoveSlot, PartyMon, SavedKnowledge};

use crate::party::Party;
use crate::Progress;

pub fn path_for(progress: &Path) -> PathBuf {
    progress.with_file_name("state.json")
}

pub fn store(path: &Path, knowledge: &SavedKnowledge) -> Result<()> {
    let json = serde_json::to_vec_pretty(knowledge).map_err(|e| Error::InvalidData(e.to_string()))?;
    std::fs::write(path, json).map_err(|e| Error::io(path, e))
}

pub fn load(path: &Path) -> Result<Option<SavedKnowledge>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))
}

/// Knowledge from an older `progress.json` that only had a party: kept, but
/// as tracked (stale) knowledge.
pub fn legacy_knowledge(data: &GameData, party: &Party) -> SavedKnowledge {
    let t = |v| Knowledge::tracked(v, None);
    let mons = party
        .members
        .iter()
        .map(|m| {
            let mut mon = PartyMon {
                species: t(m.species.clone()),
                level: Knowledge::tracked(m.level, None),
                hp: m.hp.map_or_else(Knowledge::unknown, |hp| Knowledge::tracked(hp, None)),
                ..PartyMon::default()
            };
            for (i, mv) in m.moves.iter().enumerate().take(4) {
                let max = data.move_(mv).map_or(0, |x| x.pp);
                let used = *m.pp_used.get(mv).unwrap_or(&0);
                mon.moves[i] = Some(MoveSlot {
                    mv: t(mv.clone()),
                    pp: Knowledge::tracked((max.saturating_sub(used), max), None),
                });
            }
            mon
        })
        .collect();
    SavedKnowledge {
        party: Knowledge { value: Some(mons), source: KnowledgeSource::Tracked, last_verified_frame: None },
        ..SavedKnowledge::default()
    }
}

/// The knowledge for a loaded save: `state.json`, or migrated from the
/// legacy party in `progress.json`.
pub fn restore(state_path: &Path, progress: &Progress, data: &GameData) -> Result<SavedKnowledge> {
    Ok(match load(state_path)? {
        Some(k) => k,
        None => legacy_knowledge(data, &progress.party),
    })
}
```

(`Knowledge::tracked` is generic; the closure `t` needs a type annotation per use. If inference fails, inline `Knowledge::tracked(x, None)`.)

`progress.rs`: mark `party` as legacy, so it's read but no longer written:

```rust
    /// Legacy: party knowledge from before `state.json` (read for migration).
    #[serde(default, skip_serializing)]
    pub party: crate::party::Party,
```

`lib.rs`: `pub mod checkpoint;`.

`main.rs` (`story`):
- Compute `let state_path = pokebot_agent::checkpoint::path_for(&options.progress);`.
- New game: after `NewGameTask`, `runtime.emit(GameEvent::PartyMonDerived { slot: 0, mon: Box::new(party::starter_mon(&data, config_starter_species, 5)) })?`. This replaces the `progress.party.members.push(Member::new(...))` block; the `AsIs` start does the same when the state has no party.
- Continue: after `ContinueTask`, `runtime.emit(GameEvent::CheckpointRestored { knowledge: Box::new(checkpoint::restore(&state_path, &previous, &data)?) })?`.
- Faint retry: after the reload's `ContinueTask`, emit the same `CheckpointRestored` built from the reloaded `saved` progress.
- Build tasks with `StoryTask::new(..).with_data(Arc::clone(&data))`. Remove `progress.party = task.party().clone()`.
- After `SaveGameTask` + `persist_save`: `checkpoint::store(&state_path, &runtime.state().saved_knowledge())?`. The checkpoint log line lists the party from `Party::from_state(runtime.state())`.

- [ ] **Step 4: Run tests and a quick live smoke test**

Run: `~/.cargo/bin/cargo test --workspace && ~/.cargo/bin/cargo build --release -p pokebot-cli`
Expected: PASS.

Smoke test on a scratch copy (it must not touch the real save):

```bash
mkdir -p /tmp/ckpt && cp saves/route3-ready.sav /tmp/ckpt/game.sav && cp saves/progress.route3-ready.json /tmp/ckpt/progress.json
tools/live-run.sh /tmp/ckpt.log story --continue --save /tmp/ckpt/game.sav --progress /tmp/ckpt/progress.json --record /tmp/run-ckpt
```

Expected in `/tmp/ckpt.log`: `Checkpoint knowledge restored`, then `no milestones left` (all are done), with the web UI's state JSON showing the IVYSAUR party (Tracked).

- [ ] **Step 5: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add crates/agent apps/pokebot-cli
git commit -m "Checkpoints: state.json with each save, restored on continue and retry

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: Web UI panel for party, money and bag

**Files:**
- Modify: `crates/telemetry/web/index.html`

**Interfaces:**
- Consumes: the `GameState` JSON (`party`, `money`, `bag.pockets`) already published in `status.state`.

- [ ] **Step 1: Add the markup** after the progression `<dl class="kv">` block (around line 85):

```html
    <h3>party &amp; items</h3>
    <dl class="kv">
      <dt>money</dt><dd><span id="money">–</span> <span class="src" id="money-src">–</span></dd>
      <dt>party</dt><dd id="party">–</dd>
      <dt>bag</dt><dd id="bag">–</dd>
    </dl>
```

- [ ] **Step 2: Render it** in `setStatus` after the `knowledge('control', …)` line:

```js
  knowledge('money', st.money);
  const kv = (k) => (k && k.value != null ? k.value : '?');
  const src = (k) => `<span class="src ${k ? k.source : 'Unknown'}">${k ? k.source : 'Unknown'}</span>`;
  const party = (st.party && st.party.value) || [];
  $('party').innerHTML = party.length ? party.map((m) => {
    const sp = String(kv(m.species)).replace('SPECIES_', '');
    const hp = m.hp && m.hp.value ? `${m.hp.value[0]}/${m.hp.value[1]}` : '?';
    const moves = m.moves.filter(Boolean).map((s) => {
      const pp = s.pp && s.pp.value ? `${s.pp.value[0]}/${s.pp.value[1]}` : '?';
      return `${String(kv(s.mv)).replace('MOVE_', '')} ${pp} ${src(s.pp)}`;
    }).join(' · ');
    return `<div><b>${sp}</b> Lv${kv(m.level)} HP ${hp} ${src(m.hp)}<br><small>${moves}</small></div>`;
  }).join('') : '–';
  const pockets = (st.bag && st.bag.pockets) || {};
  $('bag').innerHTML = Object.entries(pockets).map(([p, k]) =>
    `${p}: ${k.value ? k.value.map(([i, n]) => `${i.replace('ITEM_', '')}×${n}`).join(', ') || 'empty' : '?'} ${src(k)}`
  ).join('<br>') || '–';
```

- [ ] **Step 3: Verify in the browser**

Run: `~/.cargo/bin/cargo build --release -p pokebot-cli`, then rerun the Task 8 smoke-test command. Open http://10.10.100.21:8080 and check that the party shows `IVYSAUR Lv18 HP 54/54 Tracked`, with the four moves and their PP.

- [ ] **Step 4: Commit**

```bash
git add crates/telemetry/web/index.html
git commit -m "web UI: party, money and bag knowledge with provenance

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
```

---

### Task 10: Live verification and docs

**Files:**
- Modify: `docs/architecture.md`, `docs/HANDOFF.md`

- [ ] **Step 1: Full live run on a scratch copy of the Brock save** (re-runs PrepareForRoute3: training, two move swaps, evolution, heals, save):

```bash
mkdir -p /tmp/p1 && cp saves/brock.sav /tmp/p1/game.sav && cp saves/progress.brock.json /tmp/p1/progress.json
tools/live-run.sh /tmp/p1.log story --continue --save-game --save /tmp/p1/game.sav --progress /tmp/p1/progress.json --record /tmp/run-p1
```

Expected, in order, in `/tmp/p1.log`:
1. `Checkpoint knowledge restored` (legacy migration);
2. `Party 0 move … PP a/b` lines as the move menu is used;
3. `Party 0: MOVE_GROWL → MOVE_POISON_POWDER`, then `Party 0: MOVE_POISON_POWDER → MOVE_SLEEP_POWDER`;
4. `Party 0 evolved into SPECIES_IVYSAUR`;
5. `Healed (party restored)` after each nurse visit;
6. `Money +N (won a battle)` lines only if money was observed. Money starts `Unknown`, so no change is applied, but the events still appear;
7. `PrepareForRoute3 — completed`, then `/tmp/p1/state.json` exists.

- [ ] **Step 2: Verify visually and in the state**

Build a contact sheet of the evolution and heal frames (`ffmpeg -pattern_type glob -i '…' -vf tile=4x2`). Then check the saved party:

```bash
python3 -c "import json;k=json.load(open('/tmp/p1/state.json'));m=k['party']['value'][0];print(m['species'],[ (s['mv']['value'],s['pp']['value'],s['pp']['source']) for s in m['moves'] if s])"
```

Expected: `SPECIES_IVYSAUR`, moves Tackle / Sleep Powder / Leech Seed / Vine Whip with full PP (`Derived`, after the final heal).

- [ ] **Step 3: Replay determinism check**

```bash
target/release/pokebot replay /tmp/run-p1 | tail -3
```

Expected: `frames verified` with no mismatch.

- [ ] **Step 4: Docs**
- `docs/architecture.md`: a new "Global state" section describing the model, the events, staleness, `state.json` checkpoints and the passive sources (HUD, move menu names/PP, dialogue facts). Also state that `agent::Party` is now a view.
- `docs/HANDOFF.md`: add `saves/state.json` to the checkpoint notes, and add the next plans (bag, party/summary, money and PC audits).

- [ ] **Step 5: Commit and push**

```bash
git add docs
git commit -m "docs: global state model, checkpoints and passive sources

Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>"
git push
```

---

## Self-review notes

- **Spec coverage (Phase 1, step 1):**
  - model: Task 1;
  - events and reducer: Tasks 2 and 3;
  - checkpoints: Tasks 3 and 8;
  - passive sources: HUD and move menu (Tasks 5 and 6), text facts (Task 7);
  - provenance and staleness: Task 1;
  - web visibility: Task 9;
  - live verification: Task 10.
- **Not covered here** (their own plans): the bag, party/summary, money and PC *audits*, the caught-icon and shiny detectors (they emit `SpeciesCaught` / `ShinySeen`, whose reducer support lands in Task 3), and Phases 2 and 3.
- **Events added beyond the spec's table** because the passive sources needed them:
  - `PartyMonDerived`;
  - `MovesObserved`;
  - `MoveLearned` (an empty slot, as opposed to a replacement);
  - `MoveOutOfPp`;
  - `CheckpointRestored`.
