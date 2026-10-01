#!/usr/bin/env python3
"""Seeds the fleet's continuing workers (saves/emulators/continue-<k>).

Continuing worker k plays the game of autonomy worker k (fleet.rs:
new_game_choice(k) for the starter and fossil, a girl when k is odd). Its
first save is copied from the most advanced finished new-game worker
(saves/emulators/emu-*) that played the same game: same starter and gender,
and the same fossil once one was taken (before Mt. Moon any does). Most
badges first, then the newest save. Only the in-game save and the bot's own
files are copied (game.sav, progress.json, hunt.json, nugget-farm.json, and
the belief: state.json, ledger.json); never a save state.

A continuing worker that already has a game.sav is left alone (--force
replaces it). Usage: tools/seed-continue.py [--dry-run] [--force] [K ...]
"""
import json
import os
import shutil
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EMULATORS = os.path.join(ROOT, 'saves', 'emulators')
FILES = ['game.sav', 'progress.json', 'hunt.json', 'nugget-farm.json',
         'state.json', 'ledger.json']


def choice(k):
    """fleet.rs new_game_choice(k) and the worker's gender."""
    i = k - 1
    starter = ['Bulbasaur', 'Charmander', 'Squirtle'][i % 3]
    fossil = ['DOME', 'HELIX'][(i // 3) % 2]
    return starter, fossil, 'Boy' if k % 2 == 0 else 'Girl'


def describe(d):
    """(badges, fossil or None, starter, gender) of a worker dir, or None."""
    try:
        with open(os.path.join(d, 'state.json')) as f:
            state = json.load(f)
        with open(os.path.join(d, 'progress.json')) as f:
            progress = json.load(f)
    except (OSError, ValueError):
        return None
    flags = state.get('world', {}).get('flags', {})
    badges = sum(1 for name, v in flags.items()
                 if name.startswith('FLAG_BADGE') and isinstance(v, dict)
                 and v.get('value') is True)
    taken = set()
    for path in state.get('world', {}).get('paths_run', []):
        script = path[0] if isinstance(path, list) and path else str(path)
        for fossil in ('DOME', 'HELIX'):
            if script == f'MtMoon_B2F_EventScript_{fossil.title()}Fossil':
                taken.add(fossil)
    for pocket in state.get('bag', {}).get('pockets', {}).values():
        for row in (pocket or {}).get('value') or []:
            for fossil in ('DOME', 'HELIX'):
                if isinstance(row, list) and row and row[0] == f'ITEM_{fossil}_FOSSIL':
                    taken.add(fossil)
    if len(taken) > 1:
        return None
    return badges, next(iter(taken), None), progress.get('starter'), progress.get('gender')


def best(k):
    starter, fossil, gender = choice(k)
    found = []
    for name in os.listdir(EMULATORS):
        d = os.path.join(EMULATORS, name)
        sav = os.path.join(d, 'game.sav')
        if not name.startswith('emu-') or not os.path.isfile(sav):
            continue
        # A worker still playing may be writing its files.
        log = os.path.join(d, 'worker.log')
        if os.path.exists(log) and time.time() - os.path.getmtime(log) < 600:
            continue
        info = describe(d)
        if not info:
            continue
        badges, taken, s, g = info
        if s == starter and g == gender and taken in (None, fossil):
            found.append((badges, os.path.getmtime(sav), d))
    return max(found) if found else None


def main(argv):
    dry = '--dry-run' in argv
    force = '--force' in argv
    ks = [int(a) for a in argv if a.isdigit()] or list(range(1, 7))
    for k in ks:
        dest = os.path.join(EMULATORS, f'continue-{k}')
        label = '{} {} {}'.format(*choice(k))
        if os.path.isfile(os.path.join(dest, 'game.sav')) and not force:
            print(f'continue-{k} ({label}): has its own save, left alone')
            continue
        pick = best(k)
        if not pick:
            print(f'continue-{k} ({label}): no finished worker played this game')
            continue
        badges, _, src = pick
        print(f'continue-{k} ({label}): {badges} badges from {os.path.basename(src)}')
        if dry:
            continue
        os.makedirs(dest, exist_ok=True)
        for f in FILES:
            target = os.path.join(dest, f)
            if os.path.exists(os.path.join(src, f)):
                shutil.copy2(os.path.join(src, f), target)
            elif os.path.exists(target):
                os.remove(target)


if __name__ == '__main__':
    main(sys.argv[1:])
