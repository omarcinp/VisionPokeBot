#!/usr/bin/env python3
"""Expand `walk <Dir> <tiles>` lines into `pokebot run --script` steps.

A tile step takes 16 frames (~267.5 ms at 59.73 fps). `walk Up 4` becomes a
4-step hold plus a wait long enough for the last step to finish. Other lines
pass through unchanged, so a probe source can mix walks with ordinary steps.
Probe helper only: gameplay code never trusts frame counts.
"""

import sys

STEP_MS = 267.5
STEP_FRAMES = 16
SETTLE_FRAMES = 24


def expand(lines):
    for line in lines:
        words = line.split()
        if words[:1] == ["walk"]:
            direction, tiles = words[1], int(words[2])
            yield f"hold {direction} {round(tiles * STEP_MS)}ms"
            yield f"wait {tiles * STEP_FRAMES + SETTLE_FRAMES}"
        else:
            yield line.rstrip("\n")


if __name__ == "__main__":
    for path in sys.argv[1:]:
        with open(path) as f:
            for out in expand(f):
                print(out)
