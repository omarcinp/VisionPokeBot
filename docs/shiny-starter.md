# Shiny starter hunt

`pokebot shiny-starter` hunts a shiny starter from a save made in Oak's lab,
standing in front of the chosen Poké Ball. Every attempt restarts the game,
presses A on the title screen at a planned frame (this picks the RNG seed),
continues, takes the Pokémon, and presses the last A at a planned frame
(this picks the RNG advance). It then reads the starter's summary. From the
nature and stats it works out which (seed, advance) the attempt hit, and aims
the next attempt at a frame the RNG makes shiny for this trainer's IDs.

Status (2026-09-29):
- **Works on the emulator.** Starting from nothing, it found a shiny
  CHARMANDER on attempt 17: 3 min 42 s on the stepped emulator. It was
  exactly the predicted frame (seed AD63, advance 2053, PID 69E7958B,
  NAUGHTY).
- **The Switch is analysed below but not tried yet.** It needs a hard-reset
  step and a repeatability check first.

## How FireRed makes the starter (from the decompilation)

| What | Where | Detail |
|---|---|---|
| RNG | `src/random.c` | 32-bit LCG `s' = 0x41C64E6D·s + 0x6073`; `Random()` returns the top 16 bits |
| Seed | `SeedRngAndSetTrainerId` (`src/main.c`), called from the title's cry scene (`src/title_screen.c`) | Timer1 (CPU clock, 16.78 MHz, 16-bit) counted from `CB2_InitTitleScreen` until the fade after A/Start. One frame is 280 896 cycles, so a press one frame later moves the seed by 18 752 (on paper; see below) |
| Advances | `VBlankIntr` | one `Random()` per frame, plus whatever scripts, NPCs and menus call |
| Starter | `givemon` right after "This POKéMON is really quite energetic!" (`PalletTown_ProfessorOaksLab/scripts.inc`) | the A that closes that text is the timed press; YES before it can come any time |
| PID/IVs | `CreateBoxMon` (`src/pokemon.c`) | method 1: `PID = r1 \| r2<<16`, `r3 = HP\|Atk<<5\|Def<<10`, `r4 = Spe\|SpA<<5\|SpD<<10` |
| Shiny | `IsMonShiny` | `TID ^ SID ^ PIDhi ^ PIDlo < 8` (1/8192 at random) |
| SID | `SaveBlock2.playerTrainerId` (sector 0, offset 0x0A in the `.sav`) | never shown in game: read from the save (`pokebot_gamedata::sav`) or given with `--tid/--sid` |
| Switch release | `REVISION 0xA` (built 2025-12-19) | same RNG and seeding code; it runs in Nintendo's GBA emulator |

Title-screen inputs (`Task_TitleScreenMain`, `Task_CallIntroCallback`):
- The intro skips on A/Start/Select.
- The title's opening scenes skip to RUN on A/B/Start.
- In RUN only A/Start lock the seed.

So the attempt mashes Select, then B, to reach RUN without touching the
seed. They are never held together: B+Select held is the Berry Fix combo on
v1.1. The title stays in RUN for 2700 frames.

## One attempt

1. **Reset.** The emulator is power-cycled. The attempt waits for the Game
   Freak intro; its first Select, 2 frames later, starts the title at a frame
   of its own script.
2. **One timed `Sequence`:** skip taps, A on the title at `title`, CONTINUE,
   B through the "Previously on your quest" recap, A to talk to the ball, A
   per dialogue page and YES, then the last A at `title + last`.
   - Every press sits a margin (30 frames) past the time the **scout**
     measured closed-loop for its screen (`Scouted`, kept in the hunt file).
   - The ESP32 plays a whole `Sequence` from its own 1 ms clock, so on
     hardware the two intervals that matter never depend on Wi-Fi or on when
     the bot sees a frame. It queues 256 steps; a script is about 90.
3. **Closed loop:**
   - Decline the nickname.
   - Wait for the rival's "received".
   - Open START → POKéMON → SUMMARY.
   - Read the nature (INFO), the six stats (SKILLS) and the shiny reading.
   - Go back to the field.

   A shiny is saved in game.

The **shiny reading** is the game's own star, an 8×8 sprite at (106, 40) on
INFO and SKILLS. On INFO it is cross-checked against the picture's
normal/shiny palette. When they disagree the reading is `Unclear` and the
hunt stops to let a person look. Fixtures: `emu-summary-*-shiny.png` (real)
and every Switch summary fixture (normal).

## What was measured on mGBA

- **The model is right.** Four power-on attempts, same `title`, `last` 0, 1,
  2 and 7 frames apart, read RASH / BASHFUL / CALM / CAREFUL. In all 65 536
  seeds × 20 000 advances, only seed C53A at advances 1316 + offset explains
  them. This is a regression test in `rng.rs`.
- **The in-game soft reset (A+B+Start+Select) doesn't repeat.**
  - The same timing after different previous attempts gave different seeds,
    even when resetting from the same idle field screen.
  - Two processes that played the same history got the same seeds, so the
    reset is deterministic but carries hidden state from before.
  - Retail RNG guides say the same for cartridges: hard boot only.
  - So `--reset soft` is for blind (full-odds) hunting only.
- **A power cycle repeats exactly.** The same timing gave SASSY three times.
- **`advance = last + c`, with `c` = −103** (−104 when the timer read slips a
  frame), the same for every seed. The lab's wandering aides stand off screen
  (despawned), so they call nothing.
- **Seeds don't follow the 18 752-per-frame line across titles.**
  - Measured offsets from the line: +46, +3306, +4906, +19005 (a frame plus
    253 cycles), and −1 next door.
  - The read lands a variable number of cycles later. It is probably the
    title's flame spawner, which calls `Random()` and varies the work before
    the read.
  - So each `title` is learned. With `c` known, one reading leaves about ten
    seeds and a second reading at that `title` leaves one.

The planner (`shiny_plan.rs`), in order:
1. A known `title` with a shiny frame within `--window` (60 s past the
   earliest last press) → exact target.
2. A `title` read once → read it again. If one of its candidate seeds has a
   shiny frame in reach, aim at that frame (it may be the hit).
3. Otherwise a new `title`, 7 frames further.

Each title has about a 36 % chance of a shiny frame within 60 s. Expect about
3 calibration attempts, then about 2 per title: roughly 8–20 attempts, against
8192 at full odds.

## Running it (emulator)

```bash
# A save in Oak's lab after he says to choose (any in-lab save works)
pokebot story --new-game --save-game --until MeetOak --save /tmp/sh/game.sav --progress /tmp/sh/progress.json
# Stand in front of the ball, save there, scout the timings, hunt
pokebot shiny-starter --prepare --starter charmander --save /tmp/sh/game.sav --hunt /tmp/sh/hunt.json
```

- `--hunt` keeps the scout, where the save stands, and every attempt. A rerun
  continues from there: it doesn't redo the scout, and knowledge of the titles
  is kept.
- `--fixed TITLE:LAST,…` plays given timings, for repeatability checks.
- `--reset soft` uses the in-game reset (blind only).
- `--blind` or no IDs: full odds.
- `--tid/--sid` replace reading the save.

## The Switch

Why it should work:
- The same code (REVISION 0xA).
- A controller that plays a whole timeline on its own clock.
- A summary reader already validated on Switch captures.

What must be established, in this order:

1. **A hard reset.** Two candidates:
   - The GBA app's suspend menu (ZL+ZR on Nintendo Switch Online GBA) may
     offer "Reset". That would take about 3 s.
   - Otherwise HOME → close the software → start it again. `console.rs`
     already starts it from HOME; this takes about 20 s.

   Either way the attempt anchors on the Game Freak intro. The restart's
   variable length doesn't matter, because the first Select starts the title
   on the script's clock. Also try `--reset soft`: Nintendo's emulator may
   reset Timer1 cleanly, where mGBA and cartridges don't.
2. **Repeatability.** Run `--fixed T:L,T:L,T:L`, then `T:L+1`, `T:L+2` (after
   a hard reset each). Identical starters, then consecutive advances, mean
   the model holds as on mGBA. Scattered results show the jitter:
   - the USB poll (about 8 ms);
   - the emulator sampling input once a frame;
   - the Switch's 60 Hz pacing against the GBA's 59.73 Hz. Frames are
     converted at the GBA rate today; a slope in the identified advances
     against `last` would show the real rate.

   `--jitter` widens the searches. With scattered seeds, "a title's seed"
   becomes a small distribution, and the planner should then aim at the
   likeliest. It doesn't yet.
3. **Save extraction.** The SID only exists in the save. Until the Switch
   save can be exported, pass `--tid/--sid`, or hunt `--blind`. The retail
   guides deduce the SID from the first starters instead.

Time per Switch attempt, estimated: restart (3–20 s) + title (1–40 s) +
continue, recap, dialogue (~20 s) + last press (≥ ~25 s after the title) +
rival and summary (~15 s) ≈ 1–1.5 min. About 20 attempts is roughly 30 min,
against about 100 h expected at full odds.

Rules that still apply on the Switch:
- Run through `tools/live-run.sh --instance switch`.
- Never interrupt a user's manual control.
- The ESP32 keepalive stays on.
