#!/usr/bin/env python3
"""Loop watcher for an agent's monitor: follows the Switch log and the live
fleet's worker logs, and prints one line per loop/stuck signal (per game,
deduplicated for 30 min), when the same goal line repeats 20 times in 10 min
(a loop no bound names), when any other line (numbers aside) repeats 600
times in 10 min (a loop inside one step: fleet continue-4 chose a PC cell and
CANCEL for half an hour, its goal line unchanged), and when a game's log goes
quiet for 15 min.
Run it as a Monitor command: `python3 -u tools/loop-watch.py`."""
import glob, os, re, sys, time
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PAT = re.compile(r'stuck:|no progress|going in circles|looping at|out of replans|no recourse|'
                 r'restart: cycle|device disconnected|panicked|intent_infeasible|failed twice')
QUIET_S = 15 * 60
REPEAT_N, REPEAT_S = 20, 10 * 60
CHURN_N = 600
# Lines that repeat while all is well (NPCs pacing, the player walking).
CHURN_SKIP = re.compile(r'NPC|Npc|Sprite|Player|Party \d+ seen|Opponent HP')
recent = {}    # (game, key) -> times a goal line was seen in the last REPEAT_S
seen = {}      # (game, key) -> time
pos = {}       # path -> offset
grew = {}      # path -> last growth time
def logs():
    out = {'switch': '/tmp/pokebot-switch.log'}
    # The live fleet: worker logs written in the last two hours. New-game
    # workers are emu-<generation>-<n> (emuN), continuing ones continue-<k>
    # (contK).
    paths = glob.glob(ROOT + '/saves/emulators/emu-*/worker.log') \
        + glob.glob(ROOT + '/saves/emulators/continue-*/worker.log')
    for p in paths:
        try:
            kind = 'cont' if '/continue-' in p else 'emu'
            key = kind + p.split('-')[-1].split('/')[0]
            t = os.path.getmtime(p)
            if time.time() - t < 7200 and (key not in out or t > os.path.getmtime(out[key])):
                out[key] = p
        except OSError:
            pass
    return out
for game, p in logs().items():   # start at the end: only new lines count
    pos[p] = os.path.getsize(p) if os.path.exists(p) else 0
    grew[p] = time.time()
while True:
    now = time.time()
    for game, p in logs().items():
        try:
            size = os.path.getsize(p)
        except OSError:
            continue
        if p not in pos or size < pos[p]:
            pos[p] = 0 if p in pos else size
            grew[p] = now
        if size > pos[p]:
            with open(p, errors='replace') as f:
                f.seek(pos[p]); chunk = f.read()
            pos[p] = size; grew[p] = now
            for line in chunk.splitlines():
                key = re.sub(r'\d+', '#', line)[:160]
                if 'Goal: ' in line:
                    # Only the frame number differs: counters in the text
                    # ("battle over (20/150)") are progress.
                    same = re.sub(r'^\[\d+\] ', '', line)[:160]
                    times = [t for t in recent.get((game, same), []) if now - t < REPEAT_S] + [now]
                    recent[(game, same)] = times
                    if len(times) >= REPEAT_N and now - seen.get((game, 'repeat', same), 0) > 1800:
                        seen[(game, 'repeat', same)] = now
                        print(f'[{game}] repeating ({len(times)}x in 10 min): {line[:280]}', flush=True)
                elif not CHURN_SKIP.search(line):
                    churn = re.sub(r'\d+', '#', re.sub(r'^\[\d+\] ', '', line))[:160]
                    times = [t for t in recent.get((game, churn), []) if now - t < REPEAT_S] + [now]
                    recent[(game, churn)] = times
                    if len(times) >= CHURN_N and now - seen.get((game, 'churn', churn), 0) > 1800:
                        seen[(game, 'churn', churn)] = now
                        print(f'[{game}] churning ({len(times)}x in 10 min): {line[:280]}', flush=True)
                if PAT.search(line):
                    if now - seen.get((game, key), 0) > 1800:
                        seen[(game, key)] = now
                        print(f'[{game}] {line[:300]}', flush=True)
        elif now - grew.get(p, now) > QUIET_S and now - seen.get((game, 'quiet'), 0) > 1800:
            seen[(game, 'quiet')] = now
            print(f'[{game}] log quiet for {int((now - grew[p]) / 60)} min (hung?): {p}', flush=True)
    time.sleep(20)
