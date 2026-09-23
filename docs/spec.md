# PokéBot FireRed — Deterministic Vision-Only Automation Architecture

**Status:** Design specification + coding-agent execution prompt  
**Primary target:** Pokémon FireRed / LeafGreen-style gameplay driven only by video observations and controller inputs  
**Production target:** Nintendo Switch video capture + ESP32 controller bridge  
**Development target:** Emulator video/window capture + external controller/input adapter  
**Core implementation language:** Rust

---

## 1. Executive Summary

PokéBot is a deterministic game-playing system for Pokémon FireRed that must operate under the same constraints on an emulator and on physical console hardware:

> **video in → observations → reconstructed game state → planning → controller actions out**

The bot must **never depend on emulator RAM, save-state APIs, memory inspection, debug hooks, scripting APIs, internal coordinates, or other privileged game state** that would not be available from an HDMI capture card and controller on real hardware.

The emulator is only a development substitute for:

- the video capture device; and
- the physical ESP32 controller bridge.

Every gameplay decision must derive from:

1. pixels observed from the game;
2. the bot's deterministic history of prior observations and actions;
3. a static world model built from known FireRed game data;
4. explicit planning rules.

The architecture is intentionally layered so that high-level goals such as:

- progress through the story;
- defeat a trainer;
- navigate to a location;
- capture a Pokémon;
- farm experience;
- train a specific EV spread;
- level a Pokémon in Day Care;
- breed Pokémon;
- hatch Eggs;
- replenish items;
- heal;
- recover from loss of synchronization;

can recursively invoke lower-level goals while reusing the same navigation, battle, menu, perception, and controller systems.

The preferred planning architecture is:

- **HTN-style hierarchical task decomposition** for strategic goals;
- **GOAP / state-space planning** for medium-level actions;
- **A\*** or Dijkstra for overworld pathfinding;
- **specialized deterministic planners** for battle, capture, EV training, farming, Day Care, and breeding;
- **finite-state machines** for menu/UI interaction;
- **closed-loop execution** for all controller actions.

No LLM or generative model should be in the live gameplay decision loop.

---

## 2. Non-Negotiable Design Constraints

### 2.1 Vision-only state acquisition

The runtime may consume:

- video frames;
- elapsed wall-clock/monotonic time;
- its own previously issued controller commands;
- static game/world data;
- configuration supplied by the user.

The runtime must not consume:

- emulator memory;
- game RAM;
- save-state internals;
- emulator scripting APIs;
- debugger data;
- hidden RNG state;
- internal entity positions;
- internal menu state;
- game variables or flags read directly from memory.

### 2.2 Input-only actuation

The runtime controls the game exclusively by producing controller commands.

Production:

```text
Planner
  ↓
Action Executor
  ↓
Controller abstraction
  ↓
ESP32 / PABotBase-compatible bridge
  ↓
Nintendo Switch
```

Development:

```text
Planner
  ↓
Action Executor
  ↓
Controller abstraction
  ↓
virtual controller / keyboard / emulator-compatible external input
  ↓
Emulator
```

The core must not know which backend is active.

### 2.3 Determinism

For a fixed:

- observation stream;
- world database version;
- configuration;
- prior event history;

the planner must produce the same logical actions.

Tie-breaking must always be explicit and stable.

Avoid randomness in planning unless a game mechanic itself is stochastic. When modeling stochastic mechanics, enumerate or analytically represent outcomes rather than introducing random choices into the planner.

### 2.4 Closed-loop control

Never assume a button press succeeded.

Every semantic action must have:

- preconditions;
- controller command(s);
- expected observations/state changes;
- timeout;
- alternate valid outcomes;
- recovery behavior.

Example:

```text
Semantic action:
  SelectFight

Precondition:
  BattleCommandMenu is visible
  cursor == Fight

Command:
  press A

Expected:
  MoveSelectionMenu becomes visible

Alternate:
  BattleText appears
  animation/transition is still active

Timeout:
  configured number of observed frames

Failure:
  re-observe → reconstruct state → replan
```

### 2.5 No gameplay logic based on arbitrary sleeps

Timing can be used for:

- input debouncing;
- frame pacing;
- hardware constraints;
- timeout detection.

But a fixed `sleep(500ms)` must never be treated as evidence that the game reached a particular state.

---

## 3. Hardware and Development Topology

### 3.1 Production topology

```text
Nintendo Switch HDMI output
        │
        ▼
HDMI capture device
        │
        ▼
PC video capture
        │
        ▼
PokéBot runtime
        │
        ▼
serial / USB protocol
        │
        ▼
ESP32 / ESP32-S3 controller bridge
        │
        ▼
Nintendo Switch controller input
```

A PABotBase-compatible firmware/protocol is a strong candidate for the controller bridge.

### 3.2 Development topology

```text
Emulator rendered window/video
        │
        ▼
Window/video capture adapter
        │
        ▼
Same PokéBot runtime
        │
        ▼
virtual controller adapter
        │
        ▼
Emulator input
```

No emulator-only game-state capability may appear behind these interfaces.

---

## 4. Recommended Technology Stack

### 4.1 Rust: primary runtime

Use Rust for:

- frame ingestion;
- image normalization;
- deterministic vision algorithms;
- OCR integration;
- state estimation;
- event sourcing;
- world model access;
- graph search;
- pathfinding;
- battle simulation;
- capture planning;
- farming planning;
- Day Care planning;
- breeding planning;
- EV training/search;
- goal planning;
- action execution;
- controller abstraction;
- hardware communication;
- replay;
- diagnostics;
- telemetry.

Recommended design goal:

> The complete gameplay runtime should eventually be able to run without Python.

### 4.2 Python: offline tooling only

Python is appropriate for:

- extracting structured data from `pret/pokefirered`;
- one-time dataset conversion;
- offline computer-vision experimentation;
- asset generation;
- annotation tools;
- debugging notebooks;
- evaluation scripts.

Production logic discovered in Python should be ported to Rust once stabilized.

### 4.3 TypeScript + Tauri: optional UI

A desktop observability/configuration UI can use:

- Tauri;
- TypeScript;
- a Rust backend.

The UI should visualize rather than own game logic.

Suggested panels:

- live frame;
- normalized 240×160 frame;
- detected screen;
- current state;
- current map/position;
- current hierarchical goal;
- active plan;
- confidence/provenance;
- EV ledger;
- Day Care status;
- battle state;
- recent events;
- issued controller inputs;
- planner explanation;
- replay controls.

### 4.4 ESP32 firmware

Prefer reusing an established Switch-controller firmware/protocol such as a PABotBase-compatible implementation instead of embedding game logic on the microcontroller.

The ESP32 is an actuator, not the brain.

---

## 5. Repository Layout

Recommended workspace:

```text
pokebot/
├── Cargo.toml
├── crates/
│   ├── core/
│   ├── video/
│   ├── vision/
│   ├── ocr/
│   ├── state/
│   ├── events/
│   ├── world/
│   ├── navigation/
│   ├── planner/
│   ├── battle/
│   ├── capture/
│   ├── farming/
│   ├── ev_training/
│   ├── daycare/
│   ├── breeding/
│   ├── menus/
│   ├── executor/
│   ├── controller/
│   ├── replay/
│   └── telemetry/
│
├── adapters/
│   ├── emulator-video/
│   ├── emulator-controller/
│   ├── capture-card/
│   └── pabotbase/
│
├── tools/
│   ├── firered-extractor/
│   ├── asset-generator/
│   ├── replay-inspector/
│   └── annotation/
│
├── data/
│   ├── world/
│   ├── templates/
│   ├── fonts/
│   └── fixtures/
│
├── ui/
├── tests/
│   ├── perception/
│   ├── state/
│   ├── planner/
│   ├── replay/
│   └── integration/
└── docs/
```

Crates should communicate through typed domain objects, not raw JSON internally.

---

## 6. Core Interfaces

These interfaces are foundational and should be implemented before game logic.

### 6.1 Video source

```rust
pub trait VideoSource {
    fn next_frame(&mut self) -> Result<CapturedFrame>;
}

pub struct CapturedFrame {
    pub frame_id: u64,
    pub captured_at: std::time::Instant,
    pub image: ImageBuffer,
}
```

Adapters:

- `EmulatorWindowVideoSource`
- `VideoFileSource`
- `UvcCaptureCardVideoSource`
- `ReplayVideoSource`

### 6.2 Controller

```rust
pub trait Controller {
    fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt>;
}

pub enum ControllerCommand {
    Press(Button),
    Hold {
        button: Button,
        duration: Duration,
    },
    Release(Button),
    Chord(Vec<Button>),
    Sequence(Vec<TimedInput>),
    Neutral,
}
```

Adapters:

- emulator virtual controller;
- keyboard adapter;
- PABotBase/serial adapter;
- replay/no-op controller.

### 6.3 Perception

```rust
pub trait PerceptionSystem {
    fn observe(
        &mut self,
        frame: &NormalizedFrame,
        context: &PerceptionContext,
    ) -> Observation;
}
```

Perception produces facts. It does not make strategic decisions.

### 6.4 Reducer / state estimator

```rust
pub trait StateReducer {
    fn reduce(
        &self,
        previous: &GameState,
        events: &[GameEvent],
    ) -> GameState;
}
```

The reducer should be as pure and replayable as possible.

### 6.5 Planner

```rust
pub trait Planner {
    fn plan(
        &self,
        state: &GameState,
        goal: &Goal,
        world: &WorldModel,
    ) -> Result<Plan>;
}
```

### 6.6 Semantic action

```rust
pub trait SemanticAction {
    fn preconditions(&self) -> &[Condition];
    fn expected_effects(&self) -> &[Effect];
    fn execute(&self, ctx: &mut ExecutionContext) -> ActionResult;
}
```

---

## 7. Frame Normalization

FireRed's logical GBA viewport is 240×160.

Regardless of whether the source is:

- 240×160;
- 640×480;
- 720p;
- 1080p;
- another captured resolution;

the pipeline must produce a canonical representation:

```text
Captured frame
    ↓
locate game viewport
    ↓
crop
    ↓
remove borders / integer-scale artifacts
    ↓
normalize color if required
    ↓
240 × 160 canonical frame
```

Every ROI, template, glyph, menu detector, map detector, and cursor detector should operate in canonical game coordinates.

Store both:

- original captured frame;
- normalized frame;

in diagnostic/replay sessions when configured.

---

## 8. Perception Pipeline

Recommended pipeline:

```text
NormalizedFrame
      │
      ├─ Screen classifier
      ├─ Transition detector
      ├─ UI geometry detector
      ├─ Cursor detector
      ├─ Text/glyph recognition
      ├─ Sprite/tile matching
      ├─ Player localization
      ├─ Battle HUD detector
      ├─ HP bar estimation
      ├─ Menu state detector
      └─ Dialog detector
             │
             ▼
         Observation
```

### 8.1 Prefer deterministic CV

FireRed is highly structured:

- fixed logical resolution;
- finite fonts;
- finite menu layouts;
- finite sprites;
- tile-based maps;
- predictable UI;
- known palette/style.

Prefer:

- exact/near-exact template matching;
- perceptual hashes;
- connected components;
- region comparison;
- fixed ROIs;
- tile matching;
- known glyph matching;
- finite dictionaries;
- deterministic image metrics;

before machine-learning models.

### 8.2 OCR

Do not begin with arbitrary general-purpose OCR as the only solution.

Build a FireRed-specific glyph recognizer using extracted or reconstructed font glyphs.

Pipeline:

```text
text ROI
  ↓
pixel normalization
  ↓
glyph segmentation
  ↓
template classification
  ↓
contextual lexicon
  ↓
structured text token
```

Useful lexicons:

- Pokémon species;
- moves;
- items;
- trainers;
- locations;
- numeric levels;
- HP;
- EXP;
- menu labels;
- battle phrases.

Unknown OCR should remain `Unknown`, not be guessed aggressively.

---

## 9. Observation Model

An `Observation` describes what is visible now.

Example:

```rust
pub struct Observation {
    pub frame_id: u64,
    pub screen: Observed<ScreenState>,
    pub player_pose: Option<Observed<PlayerPose>>,
    pub dialogue: Option<Observed<DialogueObservation>>,
    pub menu: Option<Observed<MenuObservation>>,
    pub battle: Option<Observed<BattleObservation>>,
    pub transition: Option<TransitionObservation>,
}
```

Example:

```text
screen = BattleMoveSelection
opponent_species = Rattata
opponent_level = 4
opponent_hp_fraction = 0.63
selected_move_index = 1
```

Observations do not themselves update persistent state.

---

## 10. Event Model

Convert observations and action receipts into semantic events.

Examples:

```text
MapObserved
PlayerPositionConfirmed
PlayerMoved
PlayerTurned
CollisionObserved
WarpEntered
MapChanged
DialogueOpened
DialogueAdvanced
MenuOpened
MenuClosed
BattleStarted
WildPokemonIdentified
TrainerBattleStarted
MoveSelected
DamageObserved
PokemonFainted
BattleEnded
ItemUsed
PokemonCaught
PokemonLeveled
EggReceived
EggHatched
PokemonDepositedInDaycare
PokemonWithdrawnFromDaycare
EvYieldCredited
StateDesynchronized
StateResynchronized
```

Events are the authoritative input to persistent state reconstruction.

---

## 11. Event-Sourced Global State

The main principle:

```text
Frame
  ↓
Observation
  ↓
Events
  ↓
Reducer
  ↓
GameState
```

Never update strategic state directly from arbitrary vision code.

Suggested model:

```rust
pub struct GameState {
    pub screen: Knowledge<ScreenState>,
    pub world: WorldState,
    pub player: PlayerState,
    pub party: PartyState,
    pub storage: StorageState,
    pub inventory: InventoryState,
    pub progression: ProgressionState,
    pub battle: Option<BattleState>,
    pub daycare: DaycareState,
    pub ev_ledger: EvLedger,
    pub navigation: NavigationState,
    pub active_goal: GoalState,
    pub synchronization: SynchronizationState,
}
```

---

## 12. Knowledge and Provenance

Because the bot cannot inspect RAM, not every state field is equally trustworthy.

Use explicit provenance:

```rust
pub enum KnowledgeSource {
    Observed,
    Derived,
    Tracked,
    Assumed,
    UserProvided,
    Unknown,
}

pub struct Knowledge<T> {
    pub value: Option<T>,
    pub source: KnowledgeSource,
    pub last_verified_frame: Option<u64>,
}
```

Optional confidence can be included for perception, but strategic rules should prefer categorical provenance over opaque floating-point certainty.

Example:

```text
current map:
  value = Route1
  source = Observed

position:
  value = (14, 22)
  source = Tracked

Potion count:
  value = 5
  source = Observed
  last verified 9000 frames ago

story flag "Oak Parcel delivered":
  value = true
  source = Derived

current EV spread:
  value = [0, 124, 0, 0, 0, 0]
  source = Tracked
```

The planner may create an information-gathering subgoal when knowledge is insufficient.

Example:

```text
Need exact Poké Ball count
  ↓
OpenBag
  ↓
Navigate to Balls pocket
  ↓
Observe inventory
  ↓
Update state
  ↓
Resume previous goal
```

---

## 13. Screen State Model

Use a detailed typed state.

```rust
pub enum ScreenState {
    Overworld,
    Dialogue,
    StartMenu,
    Bag,
    PartyMenu,
    PokemonSummary,
    PokemonMoves,
    PcStorage,
    Shop,
    PokemonCenter,
    DaycareDialogue,
    BattleIntro,
    BattleText,
    BattleCommand,
    BattleMoveSelection,
    BattlePokemonSelection,
    BattleBag,
    Evolution,
    LearnMove,
    EggHatching,
    Naming,
    SaveMenu,
    Transition,
    Unknown,
}
```

Substates should retain cursor positions and observed menu data.

---

## 14. Static World Model

Use `pret/pokefirered` as the preferred structured source instead of reverse-engineering static data from a ROM whenever practical.

The generated runtime database should contain normalized facts rather than source-code syntax.

SQLite is a good initial format.

Suggested tables/domain collections:

```text
species
base_stats
types
type_effectiveness
abilities
growth_rates
learnsets
moves
move_effects
items
tms
hms
evolutions

maps
map_connections
map_tiles
collisions
warps
object_events
npcs
trainer_sight_lines

trainers
trainer_parties

wild_encounter_areas
wild_encounter_slots

shops
ground_items
hidden_items
pokemon_centers

egg_groups
egg_cycles
breeding_attributes

ev_yields

story_nodes
story_dependencies
story_effects

semantic_locations
safe_step_loops
```

The decompilation exposes structured wild encounter data containing map references, encounter rates, species, and level ranges, which makes it suitable for building the encounter planner.

Do not distribute copyrighted ROM images or game binaries as project assets. Prefer structured open decompilation data and user-generated local assets.

---

## 15. Overworld Localization

The bot should not solve localization from scratch on every frame.

### 15.1 Initial localization

At startup or after desynchronization:

1. detect the visible tile/sprite arrangement;
2. generate candidate map/position/facing tuples;
3. score candidates;
4. use temporal observations if ambiguous;
5. establish a tracked player pose.

```rust
pub struct PlayerPose {
    pub map: MapId,
    pub x: i16,
    pub y: i16,
    pub facing: Direction,
}
```

### 15.2 Predict then verify

Once localized:

```text
known pose
  +
issued input
  +
known map collision
  =
predicted pose
```

Vision then confirms or corrects that prediction.

Example:

```text
pose = Route2 (17,54), facing north
command = Up
world model says (17,53) is traversable

expected:
  position becomes (17,53)

alternatives:
  random battle starts
  NPC/script interrupts
  movement is blocked
  map transition starts
```

This makes localization far cheaper and more robust than global image matching on every frame.

---

## 16. Navigation

Use two graph layers.

### 16.1 World graph

Nodes:

- maps;
- entrances;
- exits;
- warps;
- semantic destinations.

Edges:

- map connections;
- doors;
- caves;
- stairs;
- Surf;
- Cut;
- Strength;
- Fly;
- other progression-dependent transitions.

### 16.2 Tile graph

Within a map:

- walkable tiles are nodes;
- legal movement is edges;
- dynamic obstacles can temporarily disable edges.

Use A* with deterministic tie-breaking.

Path cost may include:

```text
movement time
encounter probability
required HMs
trainer-trigger risk
one-way ledges
dynamic NPC uncertainty
```

---

## 17. Goal System

Do not treat gameplay as a single flat "mode" switch.

Represent behavior as nested goals.

Example:

```rust
pub enum Goal {
    Story(StoryGoal),
    Navigate(NavigateGoal),
    Battle(BattleGoal),
    Capture(CaptureGoal),
    Farm(FarmGoal),
    TrainEvs(EvTrainingGoal),
    Daycare(DaycareGoal),
    Breed(BreedingGoal),
    Hatch(HatchGoal),
    Heal(HealGoal),
    AcquireItem(AcquireItemGoal),
    VerifyKnowledge(VerificationGoal),
    Recover(RecoveryGoal),
}
```

A high-level goal can spawn temporary subgoals.

Example:

```text
Story: DefeatBrock
  ├─ EnsurePartyHealthy
  │    └─ NavigateToPokemonCenter
  ├─ EnsurePreparation
  │    └─ FarmExperience
  │         └─ BattleWildPokemon
  ├─ NavigateToPewterGym
  ├─ BattleGymTrainer
  └─ BattleBrock
```

---

## 18. Hierarchical Planning

Recommended layers:

```text
Strategic Goal
     ↓
HTN task decomposition
     ↓
Medium-level state-space / GOAP planning
     ↓
Domain planner
     ↓
Semantic actions
     ↓
UI/navigation FSM
     ↓
Controller commands
```

### 18.1 Strategic level

Examples:

- complete next story milestone;
- obtain a target Pokémon;
- prepare a team for a gym;
- breed N Eggs;
- train a Pokémon to a target EV spread.

### 18.2 Tactical/domain level

Examples:

- navigate to Route 3;
- heal;
- buy 10 Poké Balls;
- farm Attack EVs;
- collect an Egg;
- hatch an Egg;
- defeat a wild Pokémon;
- flee from a non-target encounter.

### 18.3 Input level

Examples:

- press `A`;
- move cursor down;
- hold left until one tile movement is confirmed.

---

## 19. Action Model

Semantic actions should be declarative.

```rust
pub struct ActionDefinition {
    pub id: ActionId,
    pub preconditions: Vec<Condition>,
    pub effects: Vec<Effect>,
    pub cost: ActionCost,
}
```

Example:

```text
Action:
  HealAtPokemonCenter(Viridian)

Preconditions:
  player is inside Viridian Pokémon Center
  healing NPC interaction is reachable

Effects:
  known party HP = full
  known party status = cleared
  known move PP = restored

Cost:
  expected frames
```

### 19.1 Multi-dimensional cost

```rust
pub struct ActionCost {
    pub expected_frames: u64,
    pub wipe_risk: RationalProbability,
    pub money: u32,
    pub consumables: ResourceVector,
    pub backtracking: u32,
}
```

Avoid arbitrary weighted floats when possible.

Support lexicographic policy profiles.

Example `SafeStory`:

```text
1. minimize wipe risk
2. satisfy progression
3. minimize irreversible resource use
4. minimize expected time
```

Example `FastFarm`:

```text
1. maximize target yield per expected minute
2. minimize healing interruptions
3. minimize travel
```

---

## 20. Story Planner

Use a small explicit story/task DSL instead of initially attempting to infer the entire story from scripts automatically.

Example:

```yaml
task: defeat_brock

requires:
  - oak_parcel_delivered

subtasks:
  - navigate: pewter_city
  - ensure_party_health: full
  - ensure_battle_readiness:
      minimum_policy: brock_safe
  - navigate: pewter_gym
  - battle: pewter_gym_trainer
  - battle: brock

produces:
  - boulder_badge
```

Eventually, tooling can extract or validate parts of this graph from map scripts.

---

## 21. Battle Planner

Battle should use a specialized deterministic planner.

Legal top-level actions:

```text
Fight
Pokemon
Bag
Run
```

Model:

- species;
- level;
- observed HP;
- known moves;
- PP;
- types;
- STAB;
- accuracy;
- damage ranges;
- critical hit branches;
- status;
- speed;
- items;
- switching;
- opponent known/possible moves;
- trainer party knowledge.

### 21.1 Search

For short horizons, use:

- expectiminimax;
- exact discrete outcome expansion;
- memoization;
- pruning.

Do not use Monte Carlo randomness as the primary decision method for the deterministic MVP.

Example:

```text
Move X
  ├─ miss
  └─ hit
       ├─ normal damage range
       ├─ critical damage range
       └─ secondary-effect branches
```

The utility function depends on the parent goal.

`WinBattle` differs from `CaptureTarget` and `EVTrainTarget`.

---

## 22. Capture Planner

A capture goal changes utility:

```text
primary:
  capture target

secondary:
  avoid fainting target
  avoid party wipe
  conserve rare balls
  minimize expected time
```

Possible subplan:

```text
apply safe status
  ↓
reduce HP without KO
  ↓
switch if needed
  ↓
select ball
  ↓
observe capture outcome
```

The world model can rank capture locations by encounter availability and expected acquisition time.

---

# 23. Farming Mode

Farming is a reusable subgoal.

Examples:

```text
Farm until:
  level >= 15

Farm until:
  money >= 20_000

Farm until:
  target item count >= N

Farm until:
  target species encountered

Farm until:
  target EV condition reached
```

A farming planner evaluates candidate areas with expected costs such as:

```text
travel time
encounter rate
target encounter probability
average battle duration
expected XP
healing overhead
PP consumption
risk
```

---

# 24. EV Training / EV Hunt Mode

This is a first-class subsystem and is separate from Day Care.

## 24.1 Why EV state must be explicitly tracked

In Generation III, EVs are hidden from the normal UI.

Therefore the bot cannot simply inspect an EV screen.

PokéBot must maintain an **EV Ledger** based on:

- a known starting point;
- every observed defeated opponent;
- which party members received experience;
- active EV modifiers;
- explicit item use.

Exact EV planning must distinguish:

```rust
pub enum EvKnowledge {
    Exact(EvSpread),
    Bounded(EvBounds),
    Unknown,
}
```

For exact optimization, prefer Pokémon whose baseline is known, especially:

- newly caught Pokémon;
- newly hatched Pokémon;
- Pokémon whose complete EV history has been tracked by PokéBot.

If a Pokémon enters the system with an unknown battle history, do **not** pretend its EVs are exact.

## 24.2 Generation III targets

The planner should understand the Generation III constraints:

```text
hard cap per stat: 255
hard total cap: 510

effective useful target per stat:
  normally 252 because EV contribution truncates by groups of 4

common efficient spread:
  252 / 252 / 4
```

Targets must nevertheless be configurable, not hard-coded.

## 24.3 EV data

Add to `WorldModel`:

```rust
pub struct EvYield {
    pub hp: u8,
    pub attack: u8,
    pub defense: u8,
    pub special_attack: u8,
    pub special_defense: u8,
    pub speed: u8,
}
```

Every species receives an EV yield.

The static extractor should populate this from FireRed/Generation III species data.

## 24.4 EV ledger

```rust
pub struct EvLedgerEntry {
    pub pokemon_id: TrackedPokemonId,
    pub spread: EvSpread,
    pub source: EvKnowledgeSource,
}

pub struct EvBattleCredit {
    pub battle_id: BattleId,
    pub defeated_species: SpeciesId,
    pub base_yield: EvYield,
    pub recipients: Vec<EvRecipientCredit>,
}
```

An EV update must be emitted as an event:

```text
PokemonFainted(opponent)
  ↓
Determine EV recipients
  ↓
Apply modifiers
  ↓
EvYieldCredited
  ↓
Reducer updates EvLedger
```

## 24.5 Exp. Share

The planner must account for Generation III behavior:

> A Pokémon that gains experience also receives the full EV award from the defeated Pokémon.

Therefore EV-training plans must understand:

- participants;
- Exp. Share holders;
- fainted party members;
- whether the Pokémon actually receives experience.

This is critical to avoid accidental EV contamination.

## 24.6 Macho Brace and modifiers

Represent EV modifiers explicitly.

Example:

```rust
pub struct EvModifiers {
    pub macho_brace: bool,
    pub pokerus: PokerusKnowledge,
}
```

Macho Brace doubles EVs gained in battle.

Near an exact target, the planner may need to remove a multiplier so that the remaining target can be reached exactly without overshoot.

## 24.7 EV search / hunting algorithm

Given:

```rust
EvTrainingGoal {
    pokemon,
    target_spread,
    contamination_policy,
    allowed_maps,
}
```

the planner should enumerate candidate encounter zones.

For each zone:

```text
wild encounter table
  ↓
species probabilities
  ↓
EV yield by species
  ↓
expected battle duration
  ↓
target species kill policy
  ↓
non-target flee policy
  ↓
healing/PP overhead
  ↓
travel overhead
```

Compute metrics such as:

```text
expected target EV / minute
expected encounters / target EV
expected healing interval
expected travel cost
undesired EV risk
```

A strict exact-EV policy can:

- battle only species with compatible EV yields;
- flee from every incompatible encounter;
- prefer maps where desired-yield species dominate;
- dynamically change zones as the remaining EV vector changes;
- equip/remove Macho Brace as the remainder requires;
- remove Exp. Share from Pokémon that must not receive EVs;
- switch party composition to isolate EV recipients.

## 24.8 Exact remainder planning

Suppose the target Pokémon needs:

```text
Attack remaining = 5
```

The planner must solve the integer remainder problem using available yields and modifiers.

It should not blindly farm the globally fastest location.

Model the final portion as a bounded shortest-path problem over:

```text
remaining EV vector
×
equipment state
×
reachable encounter zones
```

Goal:

```text
remaining == zero
```

Forbidden states:

```text
target stat overshoot beyond configured policy
non-target EV contamination beyond policy
total EV cap violation
```

## 24.9 EV training behavior loop

Example:

```text
Goal: Train Attack EV to 252

1. Verify EV baseline is Exact.
2. Verify target Pokémon is capable of receiving XP.
3. Remove unintended Exp. Share recipients if required.
4. Determine best encounter area.
5. Navigate there.
6. For each encounter:
     a. identify species;
     b. query EV yield;
     c. if compatible with remaining target:
          battle using safe/fast policy;
        else:
          flee;
     d. confirm opponent fainted;
     e. update EV ledger;
     f. re-evaluate remaining target.
7. Change location/equipment near target if needed.
8. Stop exactly at configured target.
9. Optionally verify derived stat changes at a level-up/PC refresh boundary.
```

---

# 25. Day Care Mode

Day Care is its own domain planner.

It has two important FireRed use cases:

1. **single-Pokémon passive leveling on Route 5**;
2. **two-Pokémon breeding on Four Island once that facility is accessible**.

A Pokémon in Day Care gains experience from player steps, which is especially useful because this does not add battle EVs.

That makes Day Care valuable for:

- leveling a Pokémon while preserving a clean EV baseline;
- raising breeding parents;
- generating Eggs;
- integrating breeding → hatch → EV training workflows.

## 25.1 Day Care goals

```rust
pub enum DaycareGoal {
    Level {
        pokemon: PokemonSelector,
        target_level: u8,
    },

    LevelPreservingEvs {
        pokemon: PokemonSelector,
        target_level: u8,
    },

    Deposit {
        pokemon: PokemonSelector,
        facility: DaycareFacility,
    },

    Withdraw {
        pokemon: PokemonSelector,
    },

    GenerateEgg {
        parents: ParentPair,
    },

    GenerateEggs {
        parents: ParentPair,
        count: u16,
    },
}
```

## 25.2 Day Care state

```rust
pub struct DaycareState {
    pub facility: Option<DaycareFacility>,
    pub deposited: Vec<TrackedPokemonId>,
    pub deposit_levels: HashMap<TrackedPokemonId, u8>,
    pub tracked_steps: u64,
    pub egg_available: Knowledge<bool>,
    pub compatibility: Knowledge<BreedingCompatibility>,
}
```

## 25.3 Step ledger

Because internal step counters are unavailable, PokéBot maintains its own step ledger.

Increment a step only when movement is confirmed visually:

```text
controller requests movement
  ↓
player tile transition observed
  ↓
PlayerMoved event
  ↓
StepLedger += 1
```

Do not increment for:

- turning in place;
- blocked movement;
- menu interaction;
- ambiguous transitions.

For Day Care leveling, use visible Summary information when possible to obtain:

- level;
- current EXP;
- EXP to next level.

Combine that with the species growth curve to estimate how many confirmed steps are required.

## 25.4 Safe walking loops

Store semantic loops:

```rust
pub struct SafeStepLoop {
    pub id: StepLoopId,
    pub map: MapId,
    pub waypoints: Vec<TilePosition>,
    pub supports_bicycle: bool,
    pub random_encounters: bool,
    pub expected_steps_per_cycle: u32,
}
```

Prefer:

- deterministic collision-free routes;
- minimal NPC interference;
- no random encounters when possible;
- convenient proximity to the Day Care;
- bicycle-compatible loops when useful.

The navigation executor should still validate every movement.

---

# 26. Breeding Mode

FireRed breeding should be supported at the two-Pokémon Day Care on Four Island.

## 26.1 Breeding world data

Store:

```text
egg groups
gender ratios
breedability
Ditto compatibility
egg species
egg cycles
level-1/egg moves if needed later
```

## 26.2 Compatibility

Generation III can attempt Egg generation at an Egg-cycle boundary, with an Egg cycle consisting of 256 steps.

The planner should not rely only on theoretical timing.

Instead:

```text
walk confirmed steps
  ↓
reach/approach Egg-check boundary
  ↓
observe Day-Care Man visual state / dialogue
  ↓
collect Egg if available
  ↓
continue
```

Compatibility can be learned by speaking to the Day-Care Man and parsing his finite set of compatibility messages.

## 26.3 Breeding goal

```rust
pub struct BreedingGoal {
    pub parent_a: PokemonSelector,
    pub parent_b: PokemonSelector,
    pub desired_eggs: u16,
    pub acceptance: OffspringAcceptancePolicy,
}
```

For MVP, acceptance can simply be:

```text
accept every Egg
```

Later extensions may inspect:

- species;
- gender;
- nature;
- ability;
- moves;
- estimated IV ranges.

## 26.4 Egg collection loop

```text
Verify Four Island Day Care is accessible
  ↓
Deposit parents
  ↓
Verify compatibility
  ↓
Select safe step loop
  ↓
Walk/bike while tracking confirmed steps
  ↓
At appropriate intervals, inspect Day-Care Man
  ↓
If Egg available:
    collect Egg
  ↓
If party full:
    hatch or store Eggs
  ↓
Resume loop
```

---

# 27. Egg Hatching

Egg hatching should be a separate reusable goal.

```rust
pub struct HatchGoal {
    pub eggs: EggSelection,
    pub count: u16,
}
```

The world model contains each species' base Egg cycles.

Generation III uses 256-step Egg cycles.

The bot tracks movement but treats the actual hatch animation as authoritative.

Loop:

```text
choose safe step loop
  ↓
walk/bike
  ↓
detect EggHatching screen
  ↓
handle animation/dialogue
  ↓
identify hatched Pokémon
  ↓
create tracked Pokémon record
  ↓
initialize EV ledger to exact zero
  ↓
continue until goal complete
```

A newly hatched Pokémon is ideal for deterministic EV training because its EV ledger can begin from a known zero baseline.

---

# 28. Composite Breeding + Day Care + EV Goals

The hierarchical planner should support strategic goals such as:

```text
PrepareCompetitivePokemon
  ├─ BreedTargetSpecies
  │    ├─ DepositParents
  │    ├─ GenerateEgg
  │    └─ CollectEgg
  │
  ├─ HatchEgg
  │
  ├─ OptionalDaycareLevel
  │    └─ level without EV contamination
  │
  ├─ TrainEvs
  │    ├─ Attack 252
  │    ├─ Speed 252
  │    └─ HP 4
  │
  └─ VerifyResult
```

This composition is one of the strongest reasons to keep Day Care and EV training as distinct planners.

---

## 29. Inventory and Item Strategy

Inventory should be a tracked model.

```rust
pub struct InventoryState {
    pub pockets: HashMap<Pocket, InventoryPocket>,
}
```

Every observed acquisition/use updates it.

The planner can reason about:

- healing items;
- status items;
- balls;
- Repels;
- escape items;
- TMs;
- held items;
- Macho Brace;
- key items.

Example choice:

```text
party damaged

Candidate A:
  use Potion

Candidate B:
  navigate to Pokémon Center

Candidate C:
  continue without healing
```

Cost/risk policy selects the semantic action.

---

## 30. Information-Gathering Actions

Because state is partially observable, "look at something" is a legitimate action.

Examples:

```text
InspectParty
InspectPokemonSummary
InspectBagPocket
InspectMovePP
InspectDaycareCompatibility
InspectPcStorage
ReLocalizeMap
VerifyCurrentPosition
```

This is essential for planning under partial observability.

---

## 31. Recovery Planner

`Unknown` is a legitimate state.

Never press arbitrary buttons until something happens.

Safe recovery sequence can include:

```text
1. stop issuing movement;
2. observe additional frames;
3. detect transition/animation;
4. try safe cancel (`B`) only when applicable;
5. detect current screen;
6. optionally open Start menu as a synchronization anchor;
7. close it;
8. re-localize overworld;
9. reconstruct active goal;
10. replan.
```

If confidence remains insufficient:

```text
pause automation
record diagnostics
leave controller neutral
surface a recoverable error
```

---

## 32. Replay System

Implement replay early.

Suggested session format:

```text
session-YYYYMMDD-HHMMSS/
├── metadata.json
├── video.mkv
├── observations.jsonl
├── events.jsonl
├── states.jsonl
├── plans.jsonl
├── controller.jsonl
└── diagnostics/
```

A replay must be able to run:

```text
recorded normalized frames
  ↓
perception
  ↓
events
  ↓
state reducer
  ↓
planner
```

without launching an emulator.

Essential CLI examples:

```bash
pokebot replay session-dir
pokebot replay session-dir --from-frame 120000
pokebot inspect frame.png
pokebot plan state.json goal.json
```

---

## 33. Deterministic Testing Strategy

### 33.1 Perception fixtures

For each screen type, store representative frames:

```text
tests/fixtures/
  overworld/
  battle/
  menus/
  dialogue/
  daycare/
  egg/
```

Assertions:

```text
frame X → exact expected screen classification
frame Y → exact expected cursor index
frame Z → exact expected OCR token
```

### 33.2 State reducer tests

Given:

```text
initial state
+ ordered events
=
expected final state
```

Reducers should be easy to property-test.

### 33.3 Planner golden tests

Given fixed:

```text
GameState
WorldModel
Goal
Policy
```

assert the exact generated plan.

### 33.4 Replay regression tests

When a gameplay bug occurs:

1. retain the smallest relevant replay segment;
2. add it as a regression fixture;
3. fix the bug;
4. ensure the replay now produces the desired state/action sequence.

---

# 34. MVP Roadmap

Do not begin by trying to beat the entire game.

## Phase 0 — Workspace and deterministic infrastructure

Implement:

- Rust workspace;
- typed domain IDs;
- deterministic serialization;
- event log;
- replay skeleton;
- `VideoSource`;
- `Controller`;
- test fixtures.

Acceptance:

- the same recorded input produces byte-equivalent semantic event output where practical;
- no emulator memory API exists in the repository.

## Phase 1 — Emulator adapter and frame normalization

Implement:

- emulator/window capture;
- canonical 240×160 normalization;
- virtual controller;
- frame recorder.

Acceptance:

- bot can capture FireRed and issue controller inputs without memory integration.

## Phase 2 — Screen recognition

Recognize:

- Overworld;
- Dialogue;
- Start menu;
- Party;
- Summary;
- Bag;
- Battle command;
- Move selection;
- transition;
- Pokémon Center interaction.

Acceptance:

- high reliability against replay fixtures.

## Phase 3 — Local map model

Start with:

- Pallet Town;
- Route 1;
- Viridian City.

Implement:

- extracted map/collision data;
- player pose;
- predict-and-verify movement;
- A* navigation.

Acceptance:

```text
navigate(Pallet → Viridian Pokémon Center)
```

works closed-loop.

## Phase 4 — Wild battle handling

Implement:

- battle start;
- opponent recognition;
- menu control;
- basic attack selection;
- faint/end detection;
- return to overworld.

Acceptance:

- random Route 1 encounters do not destroy navigation.

## Phase 5 — Composite navigation goal

Scenario:

```text
Pallet
→ Route 1
→ handle random battles
→ Viridian
→ enter Pokémon Center
→ heal
→ return to Pallet
```

This validates most architectural boundaries.

## Phase 6 — Early story slice

Implement enough story DSL for:

```text
obtain starter
→ rival battle
→ Viridian
→ obtain Oak's Parcel
→ return to Oak
```

## Phase 7 — Capture and farming

Implement:

- capture planner;
- experience farming;
- location ranking.

## Phase 8 — EV training

Implement:

- species EV yield extraction;
- exact EV ledger;
- Exp. Share handling;
- Macho Brace modifier;
- candidate-zone ranking;
- target-vs-flee encounter policy;
- exact remainder planner.

Acceptance:

Given a newly caught/hatched Pokémon with exact zero EVs:

```text
TrainEvs(target = { attack: X, speed: Y, hp: Z })
```

must terminate at the configured exact spread without contaminating forbidden stats.

## Phase 9 — Day Care leveling

Implement:

- Route 5 Day Care interaction;
- deposit/withdraw;
- confirmed-step ledger;
- safe walking loop;
- level target.

Acceptance:

```text
DaycareLevel(pokemon, target_level)
```

returns the Pokémon at or above the configured level while its tracked EV spread remains unchanged.

## Phase 10 — Four Island breeding

Implement:

- two-parent Day Care;
- compatibility dialogue;
- 256-step cycle tracking;
- Egg availability detection;
- Egg collection.

## Phase 11 — Hatching

Implement:

- safe hatch loop;
- Egg hatch detection;
- hatched Pokémon identity;
- new tracked-Pokémon record;
- EV baseline initialized to exact zero.

## Phase 12 — Composite preparation planner

Implement:

```text
Breed
→ Hatch
→ optionally Day Care level
→ exact EV train
```

as one hierarchical strategic goal.

## Phase 13 — Physical Switch adapters

Replace:

```text
emulator video
emulator controller
```

with:

```text
capture card
ESP32 / PABotBase controller
```

No changes should be required in:

- perception;
- state;
- world model;
- planner;
- battle;
- EV training;
- Day Care;
- breeding.

---

# 35. First End-to-End Architecture Target

The first serious validation scenario should be:

```text
Start:
  player known to be in Pallet Town

Goal:
  heal at Viridian Pokémon Center and return

Bot behavior:
  localize player
  plan Route 1 path
  walk
  interrupt plan when wild encounter occurs
  solve battle
  restore navigation goal
  enter Viridian
  enter Pokémon Center
  interact with Nurse
  confirm heal completion
  navigate back
```

The second should be:

```text
Start:
  exact-zero-EV Pokémon available

Goal:
  gain 10 Attack EVs

Bot behavior:
  inspect target Pokémon
  select best reachable encounter area
  navigate there
  identify every encounter
  flee from incompatible yields
  defeat compatible species
  update EV ledger after every confirmed faint
  stop at exactly 10 Attack EVs
```

The third should be:

```text
Start:
  breeding-capable Four Island save state in normal gameplay

Goal:
  obtain and hatch one Egg

Bot behavior:
  deposit two parents
  verify compatibility
  walk deterministic step loop
  detect Egg availability
  collect Egg
  hatch it
  create exact-zero EV ledger for offspring
```

---

# 36. Coding Standards

- Rust stable.
- Strongly typed IDs rather than arbitrary strings.
- Avoid global mutable state.
- Core reducers/planners should be side-effect free where practical.
- `serde` for diagnostic/replay formats.
- SQLite behind a repository interface.
- Prefer integer/fixed/rational representations for deterministic costs/probabilities where reasonable.
- All planner tie-breaks documented.
- No hidden random seeds.
- All actions observable in logs.
- Every `Unknown` case handled explicitly.
- Do not panic on normal perception uncertainty.
- Keep hardware adapters out of domain crates.
- No direct dependency from planner to video/capture implementation.
- No direct dependency from perception to controller implementation.

---

# 37. Planner Explainability

Every chosen action should be explainable in structured form.

Example:

```json
{
  "goal": "TrainAttackEV",
  "remaining_attack_ev": 5,
  "chosen_action": "TravelToRouteX",
  "reason": {
    "expected_target_ev_per_minute": 7.2,
    "forbidden_ev_probability": 0.0,
    "travel_frames": 3900,
    "macho_brace": false
  }
}
```

The format need not literally use floating-point values if deterministic rational metrics are preferred.

This is invaluable for debugging.

---

# 38. Future Extensions

Not MVP requirements:

- full story completion;
- full National Dex;
- automatic script-to-story-graph extraction;
- advanced IV estimation;
- breeding for IVs/natures;
- shiny hunting;
- speedrun policy;
- multi-game support;
- Emerald/Ruby/Sapphire;
- LeafGreen-specific encounter tables;
- game-independent automation framework;
- distributed replay analysis;
- learned perception fallbacks.

Do not let these delay the MVP.

---

# 39. Key Architectural Insight

PokéBot should not be thought of as:

> a script that presses buttons to play FireRed.

It should be treated as:

> a deterministic partially observable agent whose only sensors are game video pixels and whose only actuator is a controller.

FireRed is the domain model.

The reusable engine is:

```text
Perception
→ Event reconstruction
→ Knowledge/state
→ Hierarchical goals
→ Deterministic planning
→ Closed-loop semantic actions
→ Controller
```

That separation is what allows the exact same logic to graduate from an emulator to a physical capture-card + ESP32 setup.

---

# 40. Final Coding-Agent Prompt

The following prompt is intentionally self-contained and is the recommended prompt to give to a coding agent.

---

## BEGIN AGENT PROMPT

You are implementing **PokéBot FireRed**, a deterministic vision-only automation system for Pokémon FireRed.

Your task is to build the project incrementally as a production-quality Rust workspace.

### Fundamental constraint

The bot must behave as if it were connected to a physical console through:

- an HDMI capture card for video; and
- an ESP32 acting as the controller.

During development, an emulator may replace those two devices, but you **must never read emulator memory or use emulator-specific game-state APIs**.

Do not use:

- RAM inspection;
- save-state internals;
- Lua/debug APIs;
- emulator scripting APIs that expose game state;
- hidden RNG state;
- internal coordinates;
- internal menu variables;
- direct access to game flags.

The only gameplay inputs to the agent are:

1. video frames;
2. its own previous controller commands;
3. static FireRed world/game knowledge;
4. its own previously reconstructed state.

The only way it may affect the game is through controller inputs.

The architectural contract is:

```text
video
→ normalized frame
→ perception
→ observation
→ semantic events
→ deterministic GameState reducer
→ hierarchical planner
→ semantic actions
→ closed-loop executor
→ Controller abstraction
```

### Primary language

Use Rust for the runtime.

Python may only be used for offline data-extraction/development tooling.

An optional observability UI may use Tauri + TypeScript, but do not build the UI before the core works.

### Static game data

Prefer structured data from the `pret/pokefirered` decompilation when building the world database.

Build an offline extractor that converts relevant data into a normalized runtime database.

Include at least:

- maps;
- connections;
- collision/passability information;
- warps;
- NPC/object events;
- trainers;
- trainer parties;
- species;
- types;
- type effectiveness;
- moves;
- learnsets;
- items;
- evolutions;
- wild encounter tables;
- encounter rates;
- Pokémon Centers;
- shops;
- egg groups;
- egg cycles;
- EV yields.

Do not distribute ROM binaries or copyrighted ROM assets with the repository.

### Core abstractions

Create typed interfaces for:

```rust
trait VideoSource;
trait Controller;
trait PerceptionSystem;
trait StateReducer;
trait Planner;
trait SemanticAction;
```

Provide separate adapters for:

```text
emulator/window video
video-file/replay input
capture-card video

emulator/virtual controller
PABotBase/ESP32 controller
```

No domain crate may depend directly on a specific video or controller adapter.

### Canonical frame

Normalize every video source to the logical FireRed viewport:

```text
240 × 160
```

All perception ROIs and templates must operate in canonical coordinates.

### Perception

Prefer deterministic computer vision over ML.

Implement a pipeline capable of eventually recognizing:

- overworld;
- dialogue;
- Start menu;
- Bag;
- Party;
- Pokémon Summary;
- PC;
- shops;
- Pokémon Center;
- battle command menu;
- battle move menu;
- battle Pokémon menu;
- battle Bag;
- transitions;
- evolution;
- move-learning;
- Day Care dialogue;
- Egg hatching.

Build FireRed-specific OCR using finite glyph/template recognition and contextual dictionaries where practical.

Do not make the perception layer choose gameplay actions.

### State architecture

Use:

```text
Frame
→ Observation
→ Events
→ Reducer
→ GameState
```

Use event sourcing so gameplay sessions can be replayed deterministically.

State fields must retain knowledge provenance:

```rust
enum KnowledgeSource {
    Observed,
    Derived,
    Tracked,
    Assumed,
    UserProvided,
    Unknown,
}
```

Never silently convert an unknown value into a guessed fact.

The state should include at least:

```text
screen
world/player pose
party
storage
inventory
progression
battle
navigation
Day Care
EV ledger
active goal
synchronization status
```

### Navigation

Implement:

1. a global map/warp graph;
2. a local tile graph;
3. deterministic A* pathfinding;
4. predict-then-verify movement.

Once localized, issue one semantic movement action and verify the observed result.

A movement can be interrupted by:

- collision;
- NPC;
- script;
- random battle;
- warp;
- map transition.

Handle these as events and replan.

### Goals and planning

Do not implement the project as one flat finite-state machine.

Implement hierarchical goals.

At minimum:

```text
Story
Navigate
Battle
Capture
Farm
TrainEvs
Daycare
Breed
Hatch
Heal
AcquireItem
VerifyKnowledge
Recover
```

Use:

- HTN-like decomposition at the strategic level;
- GOAP/state-space planning where useful;
- A* for navigation;
- specialized deterministic planners for battle, capture, farming, EV training, Day Care, and breeding;
- FSMs only for local menu interaction.

A story goal may invoke farming, battle, healing, EV training, or other subgoals and then resume.

### Closed-loop action executor

Never implement important gameplay sequences as:

```text
press button
sleep arbitrary duration
press next button
```

Every semantic action must have:

- preconditions;
- issued controller commands;
- expected observations/effects;
- alternate outcomes;
- timeout;
- recovery.

Example:

```text
SelectFight
precondition: BattleCommandMenu
command: A
expected: MoveSelectionMenu
failure: observe current state and replan
```

### Battle planner

Build a deterministic specialized battle planner.

Model:

- legal actions;
- types;
- STAB;
- move power;
- accuracy;
- damage ranges;
- critical-hit branches;
- status effects;
- speed;
- PP;
- switching;
- items;
- known trainer parties.

Prefer exact/discrete expected-outcome search or expectiminimax over random Monte Carlo selection.

The same state and goal must produce the same choice.

### Farming

Create reusable farming goals.

Candidate areas should be ranked using:

```text
travel time
encounter rate
target probability
battle duration
XP/resource yield
healing overhead
PP use
risk
```

### EV training / EV hunt

EV training is a first-class planner.

Important Generation III behavior:

```text
max EV per stat = 255
max total EV = 510
normally useful max per stat = 252
```

The UI does not directly expose exact EVs, so maintain an internal deterministic EV ledger.

Represent EV knowledge as:

```rust
enum EvKnowledge {
    Exact(EvSpread),
    Bounded(EvBounds),
    Unknown,
}
```

Do not perform "exact" EV planning from an unknown baseline.

Newly caught or newly hatched Pokémon may be initialized with a known clean EV baseline.

For every confirmed opponent faint:

1. identify defeated species;
2. obtain its Generation III EV yield;
3. determine which party Pokémon received experience;
4. account for Exp. Share;
5. account for Macho Brace and known modifiers;
6. emit `EvYieldCredited`;
7. update the ledger through the reducer.

In Generation III, a Pokémon receiving experience receives the full EV award from that defeated Pokémon. Model this carefully to avoid contaminating Pokémon that should not receive EVs.

Implement:

```rust
struct EvTrainingGoal {
    pokemon: TrackedPokemonId,
    target: EvSpread,
    contamination_policy: EvContaminationPolicy,
}
```

The EV planner must search wild encounter locations and evaluate:

```text
desired EV yield per expected minute
encounter probabilities
battle time
travel time
healing overhead
PP overhead
undesired-EV contamination
equipment modifiers
```

For non-compatible encounters, the bot should flee rather than defeat them.

Near the target, solve the remaining EV vector exactly instead of blindly remaining at the globally fastest route.

Allow changing:

- encounter area;
- held item/Macho Brace;
- party composition;
- Exp. Share assignment;

to satisfy the exact remainder.

Acceptance target:

```text
Given an exact-zero EV Pokémon and a reachable world state,
TrainEvs({attack: 10})
must terminate at exactly 10 tracked Attack EV
and zero forbidden EVs.
```

### Day Care

Implement Day Care as a specialized planner separate from EV training.

Support:

1. Route 5 single-Pokémon leveling;
2. Four Island two-Pokémon Day Care/breeding when accessible.

A Pokémon in Day Care gains experience through player movement and does not receive battle EVs from those steps.

This makes Day Care useful for leveling while preserving a clean EV baseline.

Maintain a confirmed `StepLedger`.

Increment it only when a real tile movement is visually confirmed.

Do not count:

- turning;
- blocked movement;
- menu actions;
- ambiguous frames.

Implement safe deterministic walking/biking loops.

Goals should include:

```text
DaycareDeposit
DaycareWithdraw
DaycareLevelTo
DaycareLevelPreservingEvs
```

### Breeding

Implement FireRed breeding at the Four Island two-Pokémon Day Care.

Store:

- egg groups;
- breedability;
- gender information;
- egg species;
- egg cycles.

Generation III Egg cycles consist of 256 steps.

Do not rely only on an estimated step counter; inspect the Day-Care Man/interaction state as the authoritative indication that an Egg exists.

Implement:

```text
deposit parents
verify compatibility
walk confirmed step loop
periodically inspect Egg availability
collect Egg
handle full party
resume
```

### Hatching

Create a separate `Hatch` goal.

Track estimated egg cycles, but treat the visible Egg-hatching sequence as authoritative.

After a hatch:

1. identify the Pokémon;
2. create a tracked-Pokémon record;
3. initialize its EV ledger to exact zero;
4. emit an `EggHatched` event.

### Composite breeding + EV goal

The architecture must allow:

```text
PreparePokemon
  ├─ Breed
  ├─ Hatch
  ├─ Optional Day Care leveling
  └─ exact EV training
```

without special-case coupling between planners.

### Information gathering

Because the game is partially observable, implement explicit information-gathering actions:

```text
InspectParty
InspectSummary
InspectBag
InspectMovePP
InspectDaycareCompatibility
InspectPc
ReLocalize
```

A planner may insert these actions when required information is stale or unknown.

### Recovery

Implement `Unknown` as a valid state.

When desynchronized:

```text
stop movement
observe
detect transition
perform only safe cancellation if valid
identify current screen
optionally use Start menu as a synchronization anchor
re-localize
reconstruct state
replan
```

If recovery is not safe, leave the controller neutral and emit a diagnostic error.

Never button-mash as recovery.

### Replay and diagnostics

Implement replay before attempting full-game automation.

Record:

```text
video
normalized frames or references
observations
events
states
plans
controller inputs
diagnostics
```

The planner and state reducer must be runnable from recorded sessions without the emulator.

Provide CLI tools such as:

```bash
pokebot replay <session>
pokebot replay <session> --from-frame <n>
pokebot inspect <frame>
pokebot plan <state> <goal>
```

### Required development phases

Implement in this order.

#### Phase 0
Workspace, domain types, interfaces, deterministic event/replay foundation.

#### Phase 1
Emulator/window video source + virtual controller + 240×160 normalizer.

#### Phase 2
Screen classifier and core menu perception.

#### Phase 3
Pallet Town / Route 1 / Viridian map extraction, localization, and navigation.

#### Phase 4
Basic wild battle detection and deterministic battle execution.

#### Phase 5
End-to-end:

```text
Pallet
→ Route 1
→ handle wild battle
→ Viridian Pokémon Center
→ heal
→ return to Pallet
```

#### Phase 6
Early story DSL and Oak's Parcel story slice.

#### Phase 7
Capture + general farming.

#### Phase 8
EV ledger and EV-training planner.

#### Phase 9
Route 5 Day Care leveling.

#### Phase 10
Four Island breeding.

#### Phase 11
Egg hatching.

#### Phase 12
Composite Breed → Hatch → Day Care → EV Training goals.

#### Phase 13
Capture-card and ESP32/PABotBase hardware adapters.

### Testing requirements

Use:

- unit tests;
- golden tests;
- recorded-frame fixtures;
- replay regression tests;
- planner determinism tests.

For a fixed:

```text
observations
world database
configuration
goal
```

the semantic plan must be stable.

### Implementation discipline

Do not attempt the entire game at once.

At the beginning of each phase:

1. inspect the existing repository;
2. write or update the phase design/acceptance criteria;
3. implement the smallest complete vertical slice;
4. add tests;
5. run formatter, linter, and test suite;
6. fix failures;
7. document what is complete and what remains.

Prefer working code over speculative abstraction, but preserve the architectural boundaries in this prompt.

When an assumption is uncertain, represent it explicitly in state rather than hiding the uncertainty.

Do not introduce an LLM into the live gameplay decision loop.

Do not read emulator memory.

The final goal is that replacing:

```text
EmulatorWindowVideoSource
VirtualController
```

with:

```text
CaptureCardVideoSource
PABotBaseController
```

requires **zero changes to gameplay logic**.

Begin with Phase 0 and Phase 1, create the repository structure, implement the interfaces, frame/replay primitives, and tests, then proceed incrementally according to the acceptance criteria above.

## END AGENT PROMPT
