# Probe scripts (bag, mart, catching, Mt. Moon)

These are deterministic `pokebot run --script` replays for the in-process
stepped emulator. They reach new screens and save fixture PNGs to
`captures/fixtures/` (gitignored). The measurements taken from those fixtures
are in `docs/superpowers/probes/2026-09-24-bag-mart-catch.md`.

**They replay only against the route3-ready checkpoint** (`saves/route3-ready.sav`:
RED, IVYSAUR Lv18, saved at `PewterCity_PokemonCenter_1F` (7,4), ¥4880, 5 POKé
BALLs, empty ITEMS pocket). Any other save, or even a single changed frame, makes
the replay diverge. Always work on a copy, never on `roms/*.sav` or `saves/`:

```bash
mkdir -p /tmp/probe && cp saves/route3-ready.sav /tmp/probe/game.sav
./target/release/pokebot run --save /tmp/probe/game.sav --script tools/scripts/probe/battle-catch.txt
```

The emulator never writes the cartridge save unless the game saves in-game, and
these scripts do not save in-game.

| Script | Fixtures | Final frame fingerprint |
|---|---|---|
| `mart.txt` | `mart-menu`, `mart-list`, `mart-quantity-1`, `mart-quantity-3`, `mart-confirm` | #4434 `f1ce10a8ec48228f` |
| `field-bag.txt` | `bag-items`, `bag-pokeballs`, `bag-pokeballs-cursor1` | #4954 `952063f3b5f860b1` |
| `battle-catch.txt` | `battle-wild-uncaught`, `bag-use-prompt`, `battle-throw`, `battle-broke-free`, `battle-gotcha`, `pokedex-page`, `nickname-prompt`, `battle-wild-caught` | #12118 `af154a569104fbd3` |
| `mtmoon.txt` | `mtmoon-entry-intro`, `mtmoon-1f`, `mtmoon-1f-b` | #46036 `4240b5899dcb6905` |

Each script starts the same way: soft reset, title, CONTINUE, then skip the
recap. That prefix was recorded from a stepped `story --continue --milestones 0`
run and converted with `tools/probe/to_script.py`. The rest was written by hand
in "walk" notation and expanded with `tools/probe/expand_walk.py`
(`walk Up 4` becomes a 4-tile hold plus a settle wait), then concatenated. The
scripts are committed in their expanded form, so they run without any tooling.

Catch outcomes depend on the frame you throw on (the game's RNG advances every
frame). In `battle-catch.txt`, the `wait 7` before the first USE gives "broke
free", and the `wait 9` before the second USE gives "Gotcha!". To get the other
broke-free texts, replace the `wait 9` with `wait 1` ("Shoot! It was so close,
too!"), `wait 13` ("Aargh! Almost had it!") or `wait 14` ("Aww! It appeared to
be caught!"), and screenshot about 620 to 720 frames after the `press A`.

Tools:
- `tools/probe/to_script.py SESSION [-o OUT] [--until-frame N] [--append FILE] [--no-tail]`
  converts a recorded `controller.jsonl` into a script. By default it ends on the
  recording's last frame.
- `tools/probe/expand_walk.py FILE...` expands `walk <Dir> <tiles>` lines.
- `tools/probe/fontread.py IMAGE X Y W H [--font …] [--exclude X,Y,W,H]` is a
  Python port of `Font::read`, used to find text regions that read cleanly.
