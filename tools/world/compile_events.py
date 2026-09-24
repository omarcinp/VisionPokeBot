#!/usr/bin/env python3
"""Compile FireRed's event scripts into planner data (spec §2.1).

Offline tool (not used at runtime). Reads the pret/pokefirered decompilation
and writes, under ``data/world/`` (local, gitignored):

* ``events.json`` – every object/sign/trigger/map script as a list of
  **paths**: the conditions tested (``when``) and the effects applied
  (``does``) from the entry label to the end of the script;
* ``dialogue.json`` – every text label as the pages and lines the game
  prints, plus an index from the first line to the labels starting with it;
* ``places.json`` – heal spots, fly spots, marts, field-move gates and water;
* ``obtain.json`` – per species, every way to obtain it on this ROM.

The compiler has two halves. The **front-end** (``parse_file``) understands
pret's script dialect: labels, commands, ``.string`` blocks, ``.2byte`` data,
``.equ`` aliases and ``.ifdef FIRERED`` blocks. The **IR** pass
(``compile_script``) enumerates paths through the commands we model and is
game-agnostic in its output.

Path order is deterministic: at every branch the *taken* branch comes first
(the jump target, or the cases of a switch in source order), then the
fall-through. Loops are cut after one iteration: a ``goto`` back to a label
already on the path ends the path there. Paths per script are capped at
``MAX_PATHS`` (the script is marked ``truncated``).

Run ``tools/world/build.sh`` to set up Python and run this.
"""

import argparse
import json
import re
import sys
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path

ROM = "firered_rev1"
MAX_PATHS = 128  # the plan said 64; the Game Corner prize clerk needs ~120
DEFINED = {"FIRERED"}  # assembler symbols for this ROM (.ifdef)

# Symbolic values that scripts compare against; everything else stays a name.
CONSTANTS = {"TRUE": 1, "FALSE": 0, "YES": 1, "NO": 0, "PARTY_SIZE": 6, "LOCALID_PLAYER": 255}

# Placeholders that print a name or number the compiler can't know: `*`
# wildcard for Dialogue::identify. Other braces are control codes (colours,
# pauses) or fixed words.
WILDCARDS = {"PLAYER", "RIVAL", "STR_VAR_1", "STR_VAR_2", "STR_VAR_3"}
LITERALS = {"PKMN": "POKéMON", "POKEBLOCK": "POKéBLOCK", "LV": "Lv"}

COMPARE_OPS = {"lt": "ge", "eq": "ne", "gt": "le", "le": "gt", "ge": "lt", "ne": "eq"}
COMPARE_CODES = {"0": "lt", "1": "eq", "2": "gt", "3": "le", "4": "ge", "5": "ne", "TRUE": "eq", "FALSE": "ne"}

# Field moves gated by badges (party_menu.c: FLAG_BADGE01_GET + FIELD_MOVE_*).
GATES = {
    "OBJ_EVENT_GFX_CUT_TREE": ("cut_tree", "MOVE_CUT", "FLAG_BADGE02_GET"),
    "OBJ_EVENT_GFX_ROCK_SMASH_ROCK": ("rock_smash", "MOVE_ROCK_SMASH", "FLAG_BADGE06_GET"),
    "OBJ_EVENT_GFX_PUSHABLE_BOULDER": ("boulder", "MOVE_STRENGTH", "FLAG_BADGE04_GET"),
}
# Water metatile behaviours (crates/world/src/behavior.rs `is_water`).
WATER_BEHAVIORS = set(range(0x10, 0x16)) | set(range(0x19, 0x1C))

MAP_SCRIPT_KINDS = {
    "MAP_SCRIPT_ON_LOAD": "on_load",
    "MAP_SCRIPT_ON_FRAME_TABLE": "on_frame",
    "MAP_SCRIPT_ON_TRANSITION": "on_transition",
    "MAP_SCRIPT_ON_WARP_INTO_MAP_TABLE": "on_warp",
    "MAP_SCRIPT_ON_RESUME": "on_resume",
    "MAP_SCRIPT_ON_RETURN_TO_FIELD": "on_return_to_field",
}


# --------------------------------------------------------------------------
# Front-end: pret's script dialect
# --------------------------------------------------------------------------


@dataclass
class Command:
    name: str
    args: list
    line: int


@dataclass
class ParsedFile:
    path: Path
    labels: dict = field(default_factory=dict)  # label -> [Command]
    texts: dict = field(default_factory=dict)  # label -> raw .string text
    data: dict = field(default_factory=dict)  # label -> [.2byte values]
    order: list = field(default_factory=list)  # labels in source order


def strip_comment(line):
    """Drops an `@ comment`, leaving quoted strings alone."""
    out, quoted = [], False
    for c in line:
        if c == '"':
            quoted = not quoted
        elif c == "@" and not quoted:
            break
        out.append(c)
    return "".join(out).strip()


def preprocess(lines):
    """Resolves `.ifdef`/`.ifndef`/`.else`/`.endif` for this ROM."""
    active = [True]
    for line in lines:
        s = line.strip()
        if s.startswith(".ifdef ") or s.startswith(".ifndef "):
            sym = s.split()[1]
            cond = (sym in DEFINED) == s.startswith(".ifdef ")
            active.append(active[-1] and cond)
        elif s.startswith(".if "):
            active.append(active[-1])  # unused in the script sources
        elif s == ".else":
            active[-1] = (not active[-1]) and active[-2]
        elif s == ".endif":
            active.pop()
        elif active[-1]:
            yield line


def split_args(text):
    """Comma-separated args; a missing comma between two tokens (one typo in
    pkmn_center_nurse.inc) is tolerated."""
    out = []
    for arg in text.split(","):
        out.extend(arg.split())
    return out


def parse_file(path):
    """Parses one `.inc`/`.s` file into labels → commands, texts and data."""
    path = Path(path)
    parsed = ParsedFile(path)
    equs = {}
    current = None
    in_macro = False
    for n, raw in enumerate(preprocess(path.read_text().splitlines()), 1):
        line = strip_comment(raw)
        if not line:
            continue
        if in_macro:
            in_macro = line != ".endm"
            continue
        if line.startswith(".macro"):
            in_macro = True
            continue
        m = re.match(r"^(\w+)::?$", line)
        if m:
            current = m.group(1)
            parsed.labels.setdefault(current, [])
            parsed.order.append(current)
            continue
        if line.startswith(".equ ") or line.startswith(".set "):
            name, value = split_args(line[5:])
            equs[name] = value
            continue
        if line.startswith(".string"):
            m = re.match(r'\.string\s+"(.*)"\s*(,.*)?$', line)
            if m and current:
                parsed.texts[current] = parsed.texts.get(current, "") + m.group(1)
            continue
        if line.startswith(".2byte") or line.startswith(".byte") or line.startswith(".4byte"):
            if current is not None:
                for value in split_args(line.split(None, 1)[1] if " " in line else ""):
                    if value not in ("0", "NULL"):
                        parsed.data.setdefault(current, []).append(value)
            continue
        if line.startswith(".") or line.startswith("#"):
            continue  # .align, .include, .text, ...
        if current is None:
            continue
        name, _, rest = line.partition(" ")
        args = [equs.get(a, a) for a in split_args(rest)]
        parsed.labels[current].append(Command(name, args, n))
    return parsed


def expand_placeholders(text):
    def sub(m):
        word = m.group(1).split()[0]
        if word in WILDCARDS or word.startswith("B_"):
            return "*"
        return LITERALS.get(word, "")

    return re.sub(r"\{([^}]*)\}", sub, text)


def text_pages(raw):
    """Pages of lines as the game prints them: `\\p` breaks the page, `\\n`
    and `\\l` the line; `$` ends the text."""
    text = expand_placeholders(raw.removesuffix("$"))
    pages = []
    for page in text.split("\\p"):
        lines = [l for l in re.split(r"\\n|\\l", page)]
        lines = [l.strip() for l in lines if l.strip()]
        if lines:
            pages.append(lines)
    return pages


# --------------------------------------------------------------------------
# IR: paths of conditions and effects
# --------------------------------------------------------------------------

# Commands with no planner-relevant semantics: animation, sound, text
# buffers, locking. Not counted as opaque.
IGNORED = set(
    """
    lock lockall release releaseall faceplayer closemessage waitmessage
    waitmovement applymovement applymovement_at delay playse waitse playbgm
    savebgm fadedefaultbgm fadenewbgm fadeoutbgm fadeinbgm playfanfare
    waitfanfare playmoncry waitmoncry fadescreen fadescreenswapbuffers
    fadescreenspeed textcolor setfieldeffectargument dofieldeffect
    waitfieldeffect bufferpartymonnick buffermovename bufferspeciesname
    bufferitemname bufferitemnameplural bufferstring buffernumberstring
    bufferleadmonspeciesname bufferstdstring bufferboxname bufferdecorationname
    buffertrainerclassname buffertrainername showmonpic hidemonpic opendoor
    closedoor waitdooranim setdoorclosed setdooropened setdoorclosed2
    setdooropened2 waitstate waitbuttonpress signmsg normalmsg
    setobjectmovementtype turnobject copyobjectxytoperm setobjectxy
    resetobjectsubpriority setobjectsubpriority showcoinsbox hidecoinsbox
    showmoneybox hidemoneybox updatemoneybox updatecoinsbox incrementgamestat
    setmetatile setobjectpriority resetobjectpriority famechecker
    set_gym_trainers goto_if_questlog questlog setvaddress vgoto vcall
    vmessage vloadword vbufferstring setmonmetlocation
    playslotmachine setfieldeffect resetweather setweather doweather
    setstepcallback dotimebasedevents releasepc lockfortrainer initclock
    braillemessage braillemessage_wait dowildbattle cmdC3 cmdC8 cmdC9 cmdCA cmdCB cmdCC cmdCD cmdCE cmdCF
    cmdD0 drawbox erasebox drawboxtext showcontestwinner contestlinktransfer
    setflashlevel animateflash lightsoff lightson pokenavcall setanimation
    turnrotatingtile lockgrid vmessage2 loadhelp unloadhelp signmsg2
    setvirtualaddress
    """.split()
)
# Specials with no planner-relevant semantics.
IGNORED_SPECIALS = set(
    """
    DrawWholeMapView QuestLog_CutRecording HelpSystem_Enable HelpSystem_Disable
    Script_SetHelpContext ShakeScreen SetSeenMon StartLegendaryBattle
    PlayTrainerEncounterMusic SetUpTrainerMovement DoPokemonLeagueLightingEffect
    SetHiddenItemFlag DisableMsgBoxWalkaway DrawElevatorCurrentFloorWindow
    CloseElevatorCurrentFloorWindow AnimateElevator SetUsedPkmnCenterQuestLogEvent
    CloseLink SpawnCameraObject RemoveCameraObject BufferMonNickname
    Script_BufferFanClubTrainerName BufferBigGuyOrBigGirlString
    ShowFieldMessageStringVar4 QuestLog_StartRecordingInputsAfterDeferredEvent
    """.split()
)

# Semantic quantities. A command with a known meaning types the var it
# writes (`state.typed[var]`); `copyvar` carries the type along; any other
# write drops it; a comparison on a typed var becomes a typed condition
# instead of `{"var": ...}`. A type is `("count", base)` for a number
# (the operator and value are added: `{"pokedex": "caught", "ge": 10}`) or
# `("bool", cond)` for TRUE/FALSE (`cond` when TRUE, its negation when
# FALSE). Specials not listed here are reported at the end of the run.
NATIONAL_DEX = {"flag": "FLAG_SYS_NATIONAL_DEX", "is": True}
SPECIAL_QUANTITIES = {
    # VAR_0x8004 = 0 (Kanto) / 1 (National); returns IsNationalPokedexEnabled.
    "GetPokedexCount": {
        "VAR_0x8005": ("count", {"pokedex": "seen"}),
        "VAR_0x8006": ("count", {"pokedex": "caught"}),
        "VAR_RESULT": ("bool", NATIONAL_DEX),
    },
    "IsNationalPokedexEnabled": {"VAR_RESULT": ("bool", NATIONAL_DEX)},
    # Every Kanto species but Mew caught; HasAllMons also wants the others.
    "HasAllKantoMons": {"VAR_RESULT": ("bool", {"pokedex_complete": "kanto", "is": True})},
    "HasAllMons": {"VAR_RESULT": ("bool", {"pokedex_complete": "national", "is": True})},
    "CalculatePlayerPartyCount": {"VAR_RESULT": ("count", {"party": "size"})},
    "CountPartyNonEggMons": {"VAR_RESULT": ("count", {"party": "non_egg"})},
    # VAR_0x8004 = the species.
    "DoesPlayerPartyContainSpecies": {"VAR_RESULT": ("bool", {"in_party": "VAR_0x8004", "is": True})},
    # Money >= VAR_0x8005.
    "IsEnoughForCostInVar0x8005": {"VAR_RESULT": ("bool", {"money": "player", "ge": "VAR_0x8005"})},
}


def value_of(token, env=None):
    """Numeric literals and known constants become ints; a var with a value
    known on this path resolves to it; other names stay names."""
    if env and token in env:
        return env[token]
    if token in CONSTANTS:
        return CONSTANTS[token]
    try:
        return int(token, 0)
    except (ValueError, TypeError):
        return token


def negate(cond):
    out = dict(cond)
    for key in ("is", "has", "known", "defeated"):
        if key in out:
            out[key] = not out[key]
            return out
    if "answer" in out:
        out["answer"] = "no" if out["answer"] == "yes" else "yes"
        return out
    for op, inverse in COMPARE_OPS.items():
        if op in out:
            del out[op]
            out[inverse] = cond[op]
            return out
    return out


def static_eval(cond, env):
    """True/False when `cond` compares a var whose value this path set
    against a constant; None when it depends on the game."""
    if not cond or "var" not in cond or cond["var"] not in env:
        return None
    have = env[cond["var"]]
    for op in COMPARE_OPS:
        if op in cond:
            want = cond[op]
            if type(have) is not type(want):
                return None
            if isinstance(have, str) and op not in ("eq", "ne"):
                return None
            return {
                "eq": have == want, "ne": have != want, "lt": have < want,
                "gt": have > want, "le": have <= want, "ge": have >= want,
            }[op]
    return None


@dataclass
class PathState:
    when: list = field(default_factory=list)
    does: list = field(default_factory=list)
    opaque: list = field(default_factory=list)
    env: dict = field(default_factory=dict)  # var -> value set on this path
    typed: dict = field(default_factory=dict)  # var -> ("count"|"bool", base) quantity it holds
    result: tuple = None  # what last wrote VAR_RESULT (checkitem, msgbox yesno...)
    compare: tuple = None  # last `compare`/`checkflag`/`checktrainerflag`
    trail: frozenset = frozenset()  # labels entered by goto (loop cut)
    stack: tuple = ()  # (label, next index) return addresses of inlined calls
    dead: bool = False  # a statically decided branch took the whole path

    def fork(self):
        return PathState(
            list(self.when), list(self.does), list(self.opaque), dict(self.env), dict(self.typed),
            self.result, self.compare, self.trail, self.stack,
        )

    def finish(self):
        path = {"when": self.when, "does": self.does}
        if self.opaque:
            path["opaque"] = sorted(set(self.opaque))
        return path


class Compiler:
    def __init__(self, labels, data, local_ids=None, label_map=None, map_names=None):
        self.labels = labels  # label -> [Command]
        self.data = data  # label -> [values]
        self.local_ids = local_ids or {}  # map name -> {LOCALID_X: n}
        self.label_map = label_map or {}  # label -> map name (for LOCALID resolution)
        self.map_names = map_names or {}  # MAP_X id -> map name
        self.unmodelled = Counter()  # command -> distinct sites reached
        self.typed_sites = Counter()  # quantity ("pokedex caught") -> distinct comparison sites
        self.untyped_specials = Counter()  # "specialvar VAR_X, Name" -> distinct sites
        self._sites = set()
        self._typed_sites = set()
        self.site = None  # (label, index) of the command being compiled

    # -- helpers --

    def local_id(self, token, label, map_id=None):
        map_name = self.map_names.get(map_id) if map_id else self.label_map.get(label)
        return self.local_ids.get(map_name, {}).get(token, value_of(token))

    @staticmethod
    def set_result(state, source):
        state.result = source
        state.env.pop("VAR_RESULT", None)
        state.typed.pop("VAR_RESULT", None)

    @staticmethod
    def write_var(state, var):
        """`var` was written by something without a known meaning."""
        state.env.pop(var, None)
        state.typed.pop(var, None)
        if var == "VAR_RESULT":
            state.result = None

    @staticmethod
    def set_typed(state, var, kind, base):
        """`var` now holds the quantity `base`; vars named in `base` are
        resolved to what this path set them to (`{"in_party": "VAR_0x8004"}`)."""
        Compiler.write_var(state, var)
        state.typed[var] = (kind, {k: value_of(v, state.env) if isinstance(v, str) else v for k, v in base.items()})

    def typed_condition(self, state, var, op, value):
        typed = state.typed.get(var)
        if not typed:
            return None
        kind, base = typed
        if kind == "count":
            cond = {**base, op: value}
        elif op in ("eq", "ne") and value in (0, 1):
            cond = dict(base) if (value == 1) == (op == "eq") else negate(base)
        else:
            return None
        k, v = next(iter(base.items()))  # the key naming the quantity
        key = f"{k} {v}" if isinstance(v, str) and not v.startswith(("SPECIES_", "VAR_")) else k
        if (self.site, key) not in self._typed_sites:
            self._typed_sites.add((self.site, key))
            self.typed_sites[key] += 1
        return cond

    def condition(self, state, var, op, value):
        """A comparison of `var` against `value`, reading a typed var (see
        SPECIAL_QUANTITIES) or VAR_RESULT through whatever wrote it last on
        this path."""
        value = value_of(value, state.env)
        typed = self.typed_condition(state, var, op, value)
        if typed is not None:
            return typed
        if var == "VAR_RESULT" and state.result:
            kind, *rest = state.result
            if kind == "item":
                return {"item": rest[0], "count": rest[1], "has": (value == 1) == (op == "eq")}
            if kind == "bagspace":
                return {"bagspace": rest[0], "count": rest[1], "has": (value == 1) == (op == "eq")}
            if kind == "move":
                # checkpartymove leaves the party slot, PARTY_SIZE when nobody knows it.
                if value == 6:
                    return {"move": rest[0], "known": op != "eq"}
                return {"move": rest[0], "known": True}
            if kind == "yesno" and op in ("eq", "ne"):
                yes = (value == 1) == (op == "eq")
                return {"answer": "yes" if yes else "no"}
            if kind == "choice":
                return {"choice": rest[0], op: value}
            if kind == "special":
                return {"special": rest[0], op: value}
            return {"result": kind, op: value}
        return {"var": var, op: value}

    # -- entry --

    def compile(self, entry):
        self.paths = []
        self.truncated = False
        if entry not in self.labels:
            return {"paths": [], "missing": True}
        self.run(entry, 0, PathState(trail=frozenset([entry])))
        out = {"paths": self.paths}
        if self.truncated:
            out["truncated"] = True
        return out

    def emit(self, state):
        if len(self.paths) < MAX_PATHS:
            self.paths.append(state.finish())
        else:
            self.truncated = True

    def branch(self, taken_label, cond, label, i, state, call=False):
        """Explore the taken branch (first), then continue with the negation.
        A condition on a value the path already set (`setvar VAR_TEMP_1, 0`
        then `goto_if_eq VAR_TEMP_1, 0`) is decided here instead of forking."""
        decided = static_eval(cond, state.env)
        if decided is not None:
            cond = None
        if decided is False:
            return
        taken = state.fork()
        if cond is not None:
            taken.when.append(cond)
        if call:
            taken.stack = state.stack + ((label, i + 1),)
            self.enter(taken_label, taken, is_call=True)
        else:
            self.jump(taken_label, taken)
        if cond is not None:
            state.when.append(negate(cond))
        elif decided is True:
            # Only the taken branch exists; the caller must not continue.
            state.dead = True

    def jump(self, target, state):
        if target in state.trail or target not in self.labels:
            if target not in self.labels:
                state.opaque.append(f"goto {target}")
            self.emit(state)
            return
        state.trail = state.trail | {target}
        self.run(target, 0, state)

    def enter(self, target, state, is_call):
        if target not in self.labels:
            state.opaque.append(f"call {target}")
            self.emit(state)
            return
        self.run(target, 0, state)

    def ret(self, state):
        if not state.stack:
            self.emit(state)
            return
        label, i = state.stack[-1]
        state.stack = state.stack[:-1]
        self.run(label, i, state)

    def unmodelled_site(self, label, i, name):
        if (label, i) not in self._sites:
            self._sites.add((label, i))
            self.unmodelled[name] += 1

    def untyped_special(self, name):
        if (self.site, name) not in self._sites:
            self._sites.add((self.site, name))
            self.untyped_specials[name] += 1

    # -- the walk --

    def run(self, label, i, state):
        if self.truncated or len(self.paths) >= MAX_PATHS:
            self.truncated = True
            return
        cmds = self.labels[label]
        while i < len(cmds):
            if state.dead:
                return
            cmd = cmds[i]
            name, a = cmd.name, cmd.args
            self.site = (label, i)
            i += 1
            if name in IGNORED:
                continue
            # -- flow --
            if name == "end":
                self.emit(state)
                return
            if name == "return":
                self.ret(state)
                return
            if name == "goto":
                self.jump(a[0], state)
                return
            if name == "call":
                state.stack = state.stack + ((label, i),)
                self.enter(a[0], state, is_call=True)
                return
            if name in ("goto_if_set", "goto_if_unset", "call_if_set", "call_if_unset"):
                cond = {"flag": a[0], "is": name.endswith("_set")}
                self.branch(a[1], cond, label, i - 1, state, call=name.startswith("call"))
                continue
            if name in ("goto_if_defeated", "goto_if_not_defeated", "call_if_defeated", "call_if_not_defeated"):
                cond = {"trainer": a[0], "defeated": "not_" not in name}
                self.branch(a[1], cond, label, i - 1, state, call=name.startswith("call"))
                continue
            m = re.match(r"^(goto|call)_if_(lt|eq|gt|le|ge|ne)$", name)
            if m:
                op = m.group(2)
                if len(a) == 3:
                    cond = self.condition(state, a[0], op, a[1])
                    target = a[2]
                elif state.compare and state.compare[0] == "var":
                    cond = self.condition(state, state.compare[1], op, state.compare[2])
                    target = a[0]
                else:
                    state.opaque.append(name)
                    target = a[-1]
                    cond = None
                self.branch(target, cond, label, i - 1, state, call=m.group(1) == "call")
                continue
            if name in ("goto_if", "call_if"):
                op = COMPARE_CODES.get(a[0])
                if state.compare and op:
                    kind = state.compare[0]
                    if kind == "flag":
                        cond = {"flag": state.compare[1], "is": op == "eq"}
                    elif kind == "trainer":
                        cond = {"trainer": state.compare[1], "defeated": op == "eq"}
                    else:
                        cond = self.condition(state, state.compare[1], op, state.compare[2])
                else:
                    cond = None
                    state.opaque.append(name)
                self.branch(a[1], cond, label, i - 1, state, call=name == "call_if")
                continue
            if name == "compare":
                state.compare = ("var", a[0], a[1])
                continue
            if name == "checkflag":
                state.compare = ("flag", a[0])
                continue
            if name == "checktrainerflag":
                state.compare = ("trainer", a[0])
                continue
            if name == "switch":
                state.compare = ("var", a[0], None)
                continue
            if name == "case":
                var = state.compare[1] if state.compare else "VAR_0x8000"
                cond = self.condition(state, var, "eq", a[0])
                self.branch(a[1], cond, label, i - 1, state)
                continue
            # -- trainer battles --
            if name.startswith("trainerbattle"):
                self.trainerbattle(name, a, label, i - 1, state)
                if self.done_after_battle:
                    return
                continue
            # -- VAR_RESULT sources --
            if name == "checkitem":
                self.set_result(state, ("item", a[0], value_of(a[1]) if len(a) > 1 else 1))
                continue
            if name == "checkitemspace":
                self.set_result(state, ("bagspace", a[0], value_of(a[1]) if len(a) > 1 else 1))
                continue
            if name == "checkpartymove":
                self.set_result(state, ("move", a[0]))
                continue
            if name == "yesnobox":
                self.set_result(state, ("yesno",))
                continue
            if name.startswith("multichoice"):
                self.set_result(state, ("choice", a[2]))
                continue
            if name == "checkcoins":
                self.set_typed(state, a[0], "count", {"coins": "player"})
                continue
            if name == "checkmoney":
                self.set_typed(state, "VAR_RESULT", "bool", {"money": "player", "ge": value_of(a[0], state.env)})
                continue
            if name == "getpartysize":
                self.set_typed(state, "VAR_RESULT", "count", {"party": "size"})
                continue
            if name == "specialvar":
                writes = SPECIAL_QUANTITIES.get(a[1])
                if writes:
                    # The counts are National when VAR_0x8004 = 1 (GetPokedexCount).
                    national = a[1] == "GetPokedexCount" and state.env.get("VAR_0x8004") == 1
                    for var, (kind, base) in writes.items():
                        if national and "pokedex" in base:
                            base = {**base, "national": True}
                        self.set_typed(state, var, kind, base)
                    if a[0] not in writes:
                        self.write_var(state, a[0])
                elif a[0] == "VAR_RESULT":
                    self.set_result(state, ("special", a[1]))
                    self.untyped_special(f"specialvar {a[0]}, {a[1]}")
                else:
                    self.write_var(state, a[0])
                    self.untyped_special(f"specialvar {a[0]}, {a[1]}")
                continue
            if name == "random":
                self.set_result(state, ("random", a[0]))
                continue
            if name == "getplayerxy":
                for var in a:
                    self.write_var(state, var)
                continue
            if name in ("checkplayergender", "checkdecor", "checkdecorspace",
                        "checkpcitem", "checkitemtype", "countgiftmons"):
                self.set_result(state, (name.removeprefix("check").removeprefix("get"),))
                continue
            # -- effects --
            if name in ("msgbox", "message"):
                state.does.append({"say": a[0]})
                if len(a) > 1 and a[1] == "MSGBOX_YESNO":
                    self.set_result(state, ("yesno",))
                continue
            if name in ("setflag", "setworldmapflag"):
                state.does.append({"set": a[0]})
                continue
            if name == "clearflag":
                state.does.append({"clear": a[0]})
                continue
            if name == "settrainerflag":
                state.does.append({"defeated": a[0]})
                continue
            if name == "cleartrainerflag":
                state.does.append({"undefeated": a[0]})
                continue
            if name == "setvar":
                v = value_of(a[1], state.env)
                self.write_var(state, a[0])
                state.env[a[0]] = v
                state.does.append({"var": a[0], "eq": v})
                continue
            if name in ("addvar", "subvar"):
                self.write_var(state, a[0])
                state.does.append({"var": a[0], "add" if name == "addvar" else "sub": value_of(a[1], state.env)})
                continue
            if name in ("copyvar", "setorcopyvar"):
                src = value_of(a[1], state.env)
                self.write_var(state, a[0])
                if isinstance(src, int) or not src.startswith("VAR_"):
                    state.env[a[0]] = src
                elif src in state.typed:
                    state.typed[a[0]] = state.typed[src]
                continue
            if name in ("giveitem", "additem", "finditem"):
                effect = {"give": a[0], "count": value_of(a[1]) if len(a) > 1 else 1}
                if name == "finditem":
                    effect["find"] = True
                state.does.append(effect)
                continue
            if name == "giveitem_msg":
                state.does.append({"give": a[1], "count": value_of(a[2]) if len(a) > 2 else 1, "text": a[0]})
                continue
            if name == "removeitem":
                state.does.append({"take": a[0], "count": value_of(a[1]) if len(a) > 1 else 1})
                continue
            if name == "givemon":
                state.does.append({"givemon": value_of(a[0], state.env), "level": value_of(a[1], state.env)})
                self.set_result(state, ("givemon",))
                continue
            if name == "giveegg":
                state.does.append({"giveegg": value_of(a[0], state.env)})
                self.set_result(state, ("giveegg",))
                continue
            if name == "setwildbattle":
                state.does.append({"wild": value_of(a[0], state.env), "level": value_of(a[1], state.env)})
                continue
            if name in ("addmoney", "removemoney", "addcoins", "removecoins"):
                amount = value_of(a[0], state.env)
                if name.startswith("remove") and isinstance(amount, int):
                    amount = -amount
                state.does.append({"money" if "money" in name else "coins": amount})
                continue
            if name == "msgreceiveditem":
                state.does.append({"say": a[0]})
                continue
            if name == "seteventmon":
                state.does.append({"wild": value_of(a[0], state.env), "level": value_of(a[1], state.env)})
                continue
            if name in ("warp", "warpsilent", "warpdoor", "warpteleport", "warphole", "setdynamicwarp", "setescapewarp"):
                key = {"setdynamicwarp": "set_warp", "setescapewarp": "escape_warp"}.get(name, "warp")
                effect = {key: a[0]}
                if len(a) == 2:
                    effect["warp_id"] = value_of(a[1], state.env)
                elif len(a) >= 3:
                    effect["x"], effect["y"] = value_of(a[-2], state.env), value_of(a[-1], state.env)
                state.does.append(effect)
                continue
            if name == "special":
                if a[0] == "HealPlayerParty":
                    state.does.append({"heal": True})
                elif a[0] not in IGNORED_SPECIALS:
                    state.opaque.append(f"special {a[0]}")
                    self.unmodelled_site(label, i - 1, f"special {a[0]}")
                continue
            if name == "setrespawn":
                state.does.append({"respawn": a[0]})
                continue
            if name == "setobjectxyperm":
                state.does.append({
                    "move_object": self.local_id(a[0], label),
                    "x": value_of(a[1], state.env), "y": value_of(a[2], state.env),
                })
                continue
            if name in ("addobject", "removeobject"):
                effect = {"add_object" if name == "addobject" else "remove_object": self.local_id(a[0], label, a[1] if len(a) > 1 else None)}
                if len(a) > 1:
                    effect["map"] = a[1]
                state.does.append(effect)
                continue
            if name == "pokemart":
                state.does.append({"mart": [x for x in self.data.get(a[0], []) if x != "ITEM_NONE"]})
                continue
            # -- everything else --
            state.opaque.append(name)
            self.unmodelled_site(label, i - 1, name)
        # Fell off the end of the label without `end`: the assembler would run
        # into the next label; the scripts always end explicitly.
        if not state.dead:
            self.emit(state)

    def trainerbattle(self, name, a, label, i, state):
        """`trainerbattle_*`: the not-yet-defeated branch fights (and continues
        into the `after` script when given); the defeated branch skips it."""
        self.done_after_battle = False
        kind = name.removeprefix("trainerbattle_") if name != "trainerbattle" else None
        after = None
        if name == "trainerbattle":
            # Raw form: type, trainer, local_id, intro, defeat, [extra, after]
            trainer, texts = a[1], a[3:]
            battle = {"battle": trainer, "intro": texts[0] if texts else None, "defeat": texts[1] if len(texts) > 1 else None}
            after = texts[2] if len(texts) > 2 else None
            gated = True
        elif kind == "single":
            battle = {"battle": a[0], "intro": a[1], "defeat": a[2]}
            after = a[3] if len(a) > 3 and a[3] != "FALSE" else None
            gated = True
        elif kind == "double":
            battle = {"battle": a[0], "intro": a[1], "defeat": a[2], "double": True}
            after = a[4] if len(a) > 4 and a[4] != "FALSE" else None
            gated = True
        elif kind in ("rematch", "rematch_double"):
            battle = {"battle": a[0], "intro": a[1], "defeat": a[2], "rematch": True}
            if kind == "rematch_double":
                battle["double"] = True
            gated = False
        elif kind == "no_intro":
            battle = {"battle": a[0], "intro": None, "defeat": a[1]}
            gated = True
        elif kind == "earlyrival":
            battle = {"battle": a[0], "intro": None, "defeat": a[2], "victory": a[3]}
            gated = False
        else:
            state.opaque.append(name)
            self.unmodelled_site(label, i, name)
            return
        battle = {k: v for k, v in battle.items() if v is not None}
        fight = state.fork()
        if gated:
            fight.when.append({"trainer": battle["battle"], "defeated": False})
        fight.does.append(battle)
        fight.does.append({"defeated": battle["battle"]})
        if after:
            self.jump(after, fight)
        else:
            self.run(label, i + 1, fight)
        if gated:
            state.when.append({"trainer": battle["battle"], "defeated": True})
            # The caller continues with the next command on this state.
        else:
            self.done_after_battle = True



# --------------------------------------------------------------------------
# Driver: the whole decompilation
# --------------------------------------------------------------------------


@dataclass
class WorldSources:
    pret: Path
    maps: dict  # map name -> map.json
    map_names: dict  # MAP_X id -> map name
    labels: dict  # label -> [Command]
    texts: dict  # label -> raw text
    data: dict  # label -> [values]
    label_map: dict  # label -> map name (for labels defined in a map's files)
    local_ids: dict  # map name -> {LOCALID_X: n}
    sha1: str


def load_world(pret):
    """Parses every script file and map of the decompilation."""
    pret = Path(pret)
    maps, map_names, local_ids = {}, {}, {}
    for f in sorted((pret / "data/maps").glob("*/map.json")):
        m = json.loads(f.read_text())
        maps[m["name"]] = m
        map_names[m["id"]] = m["name"]
        local_ids[m["name"]] = {
            o["local_id"]: i + 1 for i, o in enumerate(m.get("object_events") or []) if isinstance(o.get("local_id"), str)
        }
    labels, texts, data, label_map = {}, {}, {}, {}
    files = [pret / "data/event_scripts.s"] + sorted((pret / "data/scripts").glob("*.inc"))
    files += sorted((pret / "data/maps").glob("*/scripts.inc")) + sorted((pret / "data/maps").glob("*/text.inc"))
    for f in files:
        if not f.exists():
            continue
        parsed = parse_file(f)
        map_name = f.parent.name if f.parent.parent.name == "maps" else None
        for label, cmds in parsed.labels.items():
            if cmds or label not in labels:
                labels[label] = cmds
            if map_name:
                label_map[label] = map_name
        texts.update(parsed.texts)
        data.update(parsed.data)
    sha1_file = pret / f"{ROM}.sha1"
    sha1 = sha1_file.read_text().split()[0] if sha1_file.exists() else ""
    return WorldSources(pret, maps, map_names, labels, texts, data, label_map, local_ids, sha1)


def none_if_zero(value):
    return None if value in (None, "0", "0x0", 0) else value


def compile_events(world):
    """Compiles every entry script. Returns (events, unmodelled counter,
    typing report: {"typed": quantity -> sites, "untyped_specials": specialvar -> sites})."""
    compiler = Compiler(world.labels, world.data, world.local_ids, world.label_map, world.map_names)
    entries = {}  # label -> (kind, map, local_id) of its first reference

    def refer(label, kind, map_name, local_id=None):
        label = none_if_zero(label)
        if label is None:
            return None
        if label not in entries:
            entries[label] = [kind, map_name, local_id, 1]
        else:
            entries[label][3] += 1
        return label

    objects, triggers, map_scripts = [], [], {}
    for name in sorted(world.maps):
        m = world.maps[name]
        for i, o in enumerate(m.get("object_events") or []):
            script = refer(o.get("script"), "object", name, i + 1)
            trainer_type = o.get("trainer_type")
            objects.append({
                "map": name, "local_id": i + 1,
                "graphics": o.get("graphics_id"),
                "x": o.get("x"), "y": o.get("y"),
                "range_x": int(o.get("movement_range_x") or 0), "range_y": int(o.get("movement_range_y") or 0),
                "moves": (o.get("movement_type") or "").removeprefix("MOVEMENT_TYPE_") or None,
                "hidden_by": none_if_zero(o.get("flag")),
                "trainer_type": None if trainer_type in (None, "TRAINER_TYPE_NONE") else trainer_type,
                "sight": int(o.get("trainer_sight_or_berry_tree_id") or 0),
                "script": script,
            })
        for e in m.get("bg_events") or []:
            refer(e.get("script"), "sign", name)
        for e in m.get("coord_events") or []:
            script = refer(e.get("script"), "trigger", name)
            when = [{"var": e["var"], "eq": value_of(str(e.get("var_value")))}] if e.get("var") else []
            triggers.append({"map": name, "x": e["x"], "y": e["y"], "when": when, "script": script})
        table = world.labels.get(f"{name}_MapScripts", [])
        per_kind = {}
        for cmd in table:
            if cmd.name != "map_script":
                continue
            kind = MAP_SCRIPT_KINDS.get(cmd.args[0], cmd.args[0])
            if cmd.args[0].endswith("_TABLE"):
                for entry in world.labels.get(cmd.args[1], []):
                    if entry.name == "map_script_2":
                        per_kind.setdefault(kind, []).append({
                            "var": entry.args[0], "value": value_of(entry.args[1]),
                            "script": refer(entry.args[2], "map", name),
                        })
            else:
                per_kind.setdefault(kind, []).append(refer(cmd.args[1], "map", name))
        if per_kind:
            map_scripts[name] = per_kind

    scripts = {}
    for label in sorted(entries):
        kind, map_name, local_id, refs = entries[label]
        compiled = compiler.compile(label)
        script = {"kind": kind, "map": map_name if refs == 1 or label in world.label_map else None}
        if kind == "object" and refs == 1:
            script["local_id"] = local_id
        if refs > 1:
            script["refs"] = refs
        script.update(compiled)
        scripts[label] = script
    events = {
        "rom": ROM, "sha1": world.sha1,
        "scripts": scripts, "map_scripts": map_scripts, "triggers": triggers, "objects": objects,
    }
    typing = {"typed": compiler.typed_sites, "untyped_specials": compiler.untyped_specials}
    return events, compiler.unmodelled, typing


def compile_dialogue(world):
    labels, index = {}, {}
    for label in sorted(world.texts):
        pages = text_pages(world.texts[label])
        if not pages:
            continue
        labels[label] = pages
        index.setdefault(pages[0][0], []).append(label)
    return {"rom": ROM, "sha1": world.sha1, "labels": labels, "index": index}


# --------------------------------------------------------------------------
# Places
# --------------------------------------------------------------------------


def compile_places(world, events, world_dir=None):
    heal_json = json.loads((world.pret / "src/data/heal_locations.json").read_text())["heal_locations"]
    heal_spots = [
        {"id": h["id"], "map": world.map_names.get(h["map"], h["map"]), "x": h["x"], "y": h["y"],
         "respawn_map": world.map_names.get(h["respawn_map"], h["respawn_map"])}
        for h in heal_json
    ]
    # Fly lands on the heal spot of the town (region_map.c sMapFlyDestinations);
    # the Route 4/10 Pokémon Centers are the respawn maps of their heal spots.
    fly_spots = []
    flags = (world.pret / "include/constants/flags.h").read_text()
    for flag in re.findall(r"#define (FLAG_WORLD_MAP_\w+)", flags):
        map_id = "MAP_" + flag.removeprefix("FLAG_WORLD_MAP_")
        for h in heal_json:
            if map_id in (h["map"], h["respawn_map"]):
                fly_spots.append({"flag": flag, "map": world.map_names.get(h["map"], h["map"]), "x": h["x"], "y": h["y"]})
                break
    marts = {}
    for label, script in events["scripts"].items():
        for path in script["paths"]:
            for effect in path["does"]:
                if "mart" in effect and script["map"] and script["map"] not in marts:
                    marts[script["map"]] = effect["mart"]
    gates = []
    for o in events["objects"]:
        if o["graphics"] in GATES:
            kind, move, badge = GATES[o["graphics"]]
            gates.append({"map": o["map"], "x": o["x"], "y": o["y"], "local_id": o["local_id"], "kind": kind,
                          "requires": {"move": move, "badge": badge}})
    water = {}
    if world_dir and (Path(world_dir) / "maps").is_dir():
        for f in sorted((Path(world_dir) / "maps").glob("*.json")):
            m = json.loads(f.read_text())
            count = sum(1 for row in m["tiles"] for t in row if t[2] in WATER_BEHAVIORS)
            if count:
                water[m["name"]] = count
    return {"rom": ROM, "sha1": world.sha1, "heal_spots": heal_spots, "fly_spots": fly_spots,
            "marts": dict(sorted(marts.items())), "gates": gates, "water": water}


# --------------------------------------------------------------------------
# Obtain table
# --------------------------------------------------------------------------

# Never obtainable on one console: distributed at events (tickets, Mew).
EVENT_ONLY = {"SPECIES_MEW", "SPECIES_CELEBI", "SPECIES_JIRACHI", "SPECIES_DEOXYS", "SPECIES_LUGIA", "SPECIES_HO_OH"}
FOSSIL_ITEMS = {"SPECIES_OMANYTE": "ITEM_HELIX_FOSSIL", "SPECIES_KABUTO": "ITEM_DOME_FOSSIL", "SPECIES_AERODACTYL": "ITEM_OLD_AMBER"}
FISHING_SLOTS = {"old_rod": range(0, 2), "good_rod": range(2, 5), "super_rod": range(5, 10)}


def c_preprocess(text):
    """Keeps the FIRERED side of `#if defined(FIRERED)`/`#elif`/`#else`."""
    out, active = [], [True]
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("#if"):
            active.append(active[-1] and "FIRERED" in s)
        elif s.startswith("#elif"):
            active[-1] = active[-2] and "FIRERED" in s
        elif s.startswith("#else"):
            active[-1] = active[-2] and not active[-1]
        elif s.startswith("#endif"):
            active.pop()
        elif active[-1]:
            out.append(line)
    return "\n".join(out)


def species_table(pret):
    """Internal id, egg groups, gender ratio and egg cycles per species."""
    ids = {}
    for name, value in re.findall(r"#define (SPECIES_\w+)\s+(\d+)", (pret / "include/constants/species.h").read_text()):
        if 0 < int(value) < 412 and not name.startswith("SPECIES_OLD_UNOWN") and name != "SPECIES_EGG":
            ids[name] = int(value)
    info = {}
    text = (pret / "src/data/pokemon/species_info.h").read_text()
    for m in re.finditer(r"\[(SPECIES_\w+)\]\s*=\s*\{(.*?)\n\s*\}", text, re.S):
        block = m.group(2)
        gender = re.search(r"genderRatio\s*=\s*([\w.()]+)", block).group(1)
        if gender.startswith("PERCENT_FEMALE"):
            female = float(gender[len("PERCENT_FEMALE("):-1])
        else:
            female = {"MON_MALE": 0.0, "MON_FEMALE": 100.0}.get(gender)
        groups = re.search(r"eggGroups\s*=\s*\{([^}]*)\}", block).group(1)
        info[m.group(1)] = {
            "egg_groups": sorted(set(g.strip().removeprefix("EGG_GROUP_") for g in groups.split(","))),
            "female_percent": female,
            "egg_cycles": int(re.search(r"eggCycles\s*=\s*(\d+)", block).group(1)),
        }
    return ids, info


def evolutions(pret):
    out = []
    text = (pret / "src/data/pokemon/evolution.h").read_text()
    for m in re.finditer(r"\[(SPECIES_\w+)\]\s*=\s*\{(.*?)\}\},", text, re.S):
        for kind, param, to in re.findall(r"\{(EVO_\w+),\s*(\w+),\s*(SPECIES_\w+)", m.group(2)):
            method = {"method": "evolve", "from": m.group(1)}
            if kind.startswith("EVO_LEVEL"):
                method["how"], method["param"] = "level", int(param)
                if kind != "EVO_LEVEL":
                    method["variant"] = kind.removeprefix("EVO_LEVEL_").lower()
            elif kind == "EVO_ITEM":
                method["how"], method["param"] = "item", param
            elif kind.startswith("EVO_FRIENDSHIP"):
                method["how"] = "friendship"
                if kind != "EVO_FRIENDSHIP":
                    # Needs a clock: only in RSE, i.e. after a link.
                    method["param"], method["needs_link"] = kind.removeprefix("EVO_FRIENDSHIP_").lower(), True
            elif kind.startswith("EVO_TRADE"):
                method["how"], method["needs_link"] = "trade", True
                if kind == "EVO_TRADE_ITEM":
                    method["param"] = param
            else:
                method["how"], method["needs_link"] = kind.removeprefix("EVO_").lower(), True
            out.append((to, method))
    return out


def wild_methods(world, version="FireRed"):
    """Wild methods per species from this version's encounter tables."""
    other = "LeafGreen" if version == "FireRed" else "FireRed"
    out = {}
    data = json.loads((world.pret / "src/data/wild_encounters.json").read_text())
    for group in data["wild_encounter_groups"]:
        for enc in group["encounters"]:
            if enc["base_label"].endswith("_" + other):
                continue
            map_name = world.map_names.get(enc["map"], enc["map"])
            for field_type, slot in (("land_mons", "land"), ("water_mons", "water"), ("rock_smash_mons", "rock_smash"), ("fishing_mons", None)):
                table = enc.get(field_type)
                if not table:
                    continue
                rates = next(f["encounter_rates"] for f in group["fields"] if f["type"] == field_type)
                per = {}
                for i, mon in enumerate(table["mons"]):
                    name = slot or next(k for k, r in FISHING_SLOTS.items() if i in r)
                    entry = per.setdefault((mon["species"], name), {"rate": 0, "min_level": mon["min_level"], "max_level": mon["max_level"]})
                    entry["rate"] += rates[i]
                    entry["min_level"] = min(entry["min_level"], mon["min_level"])
                    entry["max_level"] = max(entry["max_level"], mon["max_level"])
                for (species, name), entry in per.items():
                    out.setdefault(species, []).append({"method": "wild", "map": map_name, "slot": name, **entry})
    return out


def trade_methods(world, events):
    text = c_preprocess((world.pret / "src/data/ingame_trades.h").read_text())
    scripts = {}
    for label, script in events["scripts"].items():
        for path in script["paths"]:
            for effect in path["does"]:
                if effect.get("var") == "VAR_0x8008" and str(effect.get("eq", "")).startswith("INGAME_TRADE_"):
                    scripts.setdefault(effect["eq"], (label, script["map"]))
    out = {}
    for m in re.finditer(r"\[(INGAME_TRADE_\w+)\]\s*=\s*\{(.*?)\n\s*\}", text, re.S):
        species = re.search(r"\.species\s*=\s*(SPECIES_\w+)", m.group(2)).group(1)
        wanted = re.search(r"\.requestedSpecies\s*=\s*(SPECIES_\w+)", m.group(2)).group(1)
        label, map_name = scripts.get(m.group(1), (None, None))
        out.setdefault(species, []).append({"method": "trade", "give": wanted, "script": label, "map": map_name})
    return out


def script_methods(events):
    """Gifts, prizes, fossils and static encounters from the compiled paths."""
    out, seen = {}, set()
    for label in sorted(events["scripts"]):
        script = events["scripts"][label]
        for path in script["paths"]:
            for effect in path["does"]:
                if "givemon" in effect or "giveegg" in effect:
                    species = effect.get("givemon", effect.get("giveegg"))
                    if not isinstance(species, str) or not species.startswith("SPECIES_"):
                        continue
                    method = {"method": "gift", "script": label, "map": script["map"]}
                    if "givemon" in effect:
                        method["level"] = effect["level"]
                    else:
                        method["egg"] = True
                    for cond in path["when"]:
                        if "coins" in cond:
                            method["method"] = "prize"
                            method["coins"] = cond.get("ge", cond.get("gt"))
                        if "FOSSIL" in str(cond.get("var", "")) and species in FOSSIL_ITEMS:
                            method["method"] = "fossil"
                            method["item"] = FOSSIL_ITEMS[species]
                elif "wild" in effect and isinstance(effect["wild"], str) and effect["wild"].startswith("SPECIES_"):
                    species = effect["wild"]
                    method = {"method": "static", "script": label, "map": script["map"], "level": effect["level"]}
                else:
                    continue
                key = (species, method["method"], label)
                if key in seen:
                    continue
                seen.add(key)
                method["when"] = path["when"]
                out.setdefault(species, []).append(method)
    return out


def usable(method, obtainable):
    if method.get("needs_link") or method.get("event_only"):
        return False
    kind = method["method"]
    if kind == "trade":
        return method["give"] in obtainable
    if kind == "evolve":
        return method["from"] in obtainable
    if kind == "breed":
        return any(b in obtainable for b in method["baby_of"])
    return True


def compile_obtain(world, events):
    ids, info = species_table(world.pret)
    methods = {s: [] for s in ids}
    for species, ms in wild_methods(world).items():
        methods.setdefault(species, []).extend(ms)
    for species, ms in script_methods(events).items():
        methods.setdefault(species, []).extend(ms)
    for species, ms in trade_methods(world, events).items():
        methods.setdefault(species, []).extend(ms)
    evos = evolutions(world.pret)
    for to, method in evos:
        methods.setdefault(to, []).append(method)
    # Breeding: the base of a family hatches from any evolved member.
    evolved = {to for to, _ in evos}
    family = {}
    for to, method in evos:
        family.setdefault(method["from"], set()).add(to)

    def descendants(s):
        out = set()
        stack = [s]
        while stack:
            for child in family.get(stack.pop(), ()):
                if child not in out:
                    out.add(child)
                    stack.append(child)
        return out

    for species in ids:
        if species in evolved:
            continue
        line = sorted(descendants(species))
        # Babies (Pichu) are UNDISCOVERED themselves: the parents' groups
        # are those of the evolved forms.
        groups = sorted(set(g for s in [species, *line] for g in info.get(s, {}).get("egg_groups", [])) - {"UNDISCOVERED"})
        if line and groups:
            methods[species].append({"method": "breed", "parents": groups, "baby_of": line})
    for species in EVENT_ONLY:
        for m in methods.get(species, []):
            m["event_only"] = True
    # Fixed point: what can be obtained on this console alone.
    obtainable = set()
    changed = True
    while changed:
        changed = False
        for species, ms in methods.items():
            if species not in obtainable and any(usable(m, obtainable) for m in ms):
                obtainable.add(species)
                changed = True
    # Why not: a link is needed somewhere in the chain, the species only
    # exists in LeafGreen's tables, or it was an event distribution.
    reasons = {}
    other_version = set(wild_methods(world, "LeafGreen")) - set(wild_methods(world))
    for species, ms in methods.items():
        if species in obtainable:
            continue
        reasons[species] = set()
        if species in other_version:
            reasons[species].add("other_version")
        if species in EVENT_ONLY:
            reasons[species].add("event_only")
    changed = True
    while changed:
        changed = False
        for species, ms in methods.items():
            if species in obtainable:
                continue
            before = set(reasons[species])
            for m in ms:
                if m.get("event_only"):
                    reasons[species].add("event_only")
                elif m.get("needs_link"):
                    reasons[species].add("needs_link")
                else:
                    sources = {"trade": [m.get("give")], "evolve": [m.get("from")], "breed": m.get("baby_of", [])}.get(m["method"], [])
                    for src in sources:
                        reasons[species] |= reasons.get(src, set())
            changed |= reasons[species] != before
    for species, ms in methods.items():
        if species not in obtainable and not reasons[species]:
            reasons[species].add("other_version")  # only cyclic breed/evolve methods
    table = {}
    for species in sorted(ids, key=ids.get):
        entry = {"id": ids[species], **info.get(species, {}), "methods": methods.get(species, [])}
        entry["reasons"] = sorted(reasons.get(species, ()))
        table[species] = entry
    return {"rom": ROM, "sha1": world.sha1, "species": table}


# --------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--pret", default="data/pret-pokefirered")
    ap.add_argument("--out", default="data/world")
    ap.add_argument("--report", type=int, default=30, help="unmodelled commands to list")
    args = ap.parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    world = load_world(args.pret)
    events, unmodelled, typing = compile_events(world)
    dialogue = compile_dialogue(world)
    places = compile_places(world, events, out)
    obtain = compile_obtain(world, events)
    for name, payload in (("events", events), ("dialogue", dialogue), ("places", places), ("obtain", obtain)):
        (out / f"{name}.json").write_text(json.dumps(payload, sort_keys=True, separators=(",", ":"), ensure_ascii=False))
    scripts = events["scripts"]
    paths = sum(len(s["paths"]) for s in scripts.values())
    truncated = sum(1 for s in scripts.values() if s.get("truncated"))
    opaque = sum(1 for s in scripts.values() for p in s["paths"] if p.get("opaque"))
    species = obtain["species"]
    kanto = [s for s in species.values() if s["id"] <= 151]
    print(
        f"events: {len(scripts)} scripts, {paths} paths ({truncated} truncated, {opaque} with opaque commands); "
        f"{len(dialogue['labels'])} dialogue labels; {len(places['heal_spots'])} heal spots, "
        f"{len(places['fly_spots'])} fly spots, {len(places['gates'])} gates; obtain: "
        f"{sum(1 for s in kanto if s['methods'] and not s['reasons'])}/151 Kanto species obtainable, "
        f"{sum(1 for s in kanto if s['reasons'])} with a reason",
        file=sys.stderr,
    )
    print(f"unmodelled commands (distinct sites on paths), top {args.report}:", file=sys.stderr)
    for name, count in unmodelled.most_common(args.report):
        print(f"{count:5d}  {name}", file=sys.stderr)
    print("typed quantities (distinct comparison sites):", file=sys.stderr)
    for name, count in sorted(typing["typed"].items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"{count:5d}  {name}", file=sys.stderr)
    print(f"specialvar targets left untyped (distinct sites), top {args.report}:", file=sys.stderr)
    for name, count in typing["untyped_specials"].most_common(args.report):
        print(f"{count:5d}  {name}", file=sys.stderr)


if __name__ == "__main__":
    main()
