#!/usr/bin/env python3
"""Summarises how long actions took to confirm in recorded sessions.

    tools/timings.py /tmp/switch-run/events.jsonl [/tmp/emu-run/events.jsonl ...]

Groups the executor's ActionOutcome records by action kind (the label with
numbers and coordinates removed) and prints, per kind: count, timeouts, and
frames from issue to confirmation (median / p90 / max) next to the timeout.
Use it to compare a device (Switch) against the emulator and to spot actions
whose timeout is too short for the device's latency.
"""

import json
import re
import sys
from collections import defaultdict


def kind(label: str) -> str:
    label = re.sub(r"\(.*?\)", "()", label)
    label = re.sub(r"\d+", "#", label)
    return label[:70]


def pct(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, int(len(values) * p))] if values else None


def main(paths):
    for path in paths:
        stats = defaultdict(lambda: {"n": 0, "timeouts": 0, "frames": [], "timeout": 0})
        with open(path) as f:
            for line in f:
                event = json.loads(line).get("event")
                if not isinstance(event, dict) or "ActionOutcome" not in event:
                    continue
                o = event["ActionOutcome"]
                s = stats[(o["task"], kind(o["label"]))]
                s["n"] += 1
                s["timeout"] = o["timeout"]
                if o["confirmed"]:
                    s["frames"].append(o["frames"])
                else:
                    s["timeouts"] += 1
        print(f"== {path}")
        print(f"{'task':<10} {'action':<55} {'n':>4} {'t/o':>4} {'med':>4} {'p90':>4} {'max':>4} {'limit':>5}")
        for (task, k), s in sorted(stats.items(), key=lambda kv: -kv[1]["n"]):
            fr = s["frames"]
            print(
                f"{task:<10} {k:<55} {s['n']:>4} {s['timeouts']:>4} "
                f"{pct(fr, .5) or '-':>4} {pct(fr, .9) or '-':>4} {max(fr) if fr else '-':>4} {s['timeout']:>5}"
            )


if __name__ == "__main__":
    main(sys.argv[1:])
