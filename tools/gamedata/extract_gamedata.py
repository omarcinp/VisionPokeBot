#!/usr/bin/env python3
"""Extract FireRed game mechanics data from the pret/pokefirered decompilation.

Offline tool. Writes data/world/gamedata.json (local, gitignored) with:
species (stats, types, catch rate, exp yield, growth rate, learnset,
evolutions), moves, the type chart, trainers and their parties, which map
objects are trainers, wild encounter tables (FireRed versions), marts and
item prices.
"""

import argparse
import json
import re
from pathlib import Path


def parse_designated(text, prefix):
    """Designated-initializer tables: `[PREFIX_KEY] = { .field = value, ... }`
    → {KEY: {field: raw}}. Entries are split at each `[PREFIX_...] =` so empty
    ones like `[SPECIES_NONE] = {0}` can't swallow their neighbours."""
    out = {}
    parts = re.split(r"\n\s*\[(" + prefix + r"\w+)\]\s*=", text)
    for key, body in zip(parts[1::2], parts[2::2]):
        fields = {}
        for f in re.finditer(r"\.(\w+)\s*=\s*(\{[^}]*\}|[^,\n]+)", body):
            fields.setdefault(f.group(1), f.group(2).strip())
        out[key] = fields
    return out


def num(v, default=0):
    try:
        return int(v, 0)
    except (TypeError, ValueError):
        return default


def listing(v):
    return [x.strip() for x in v.strip("{}").split(",") if x.strip()]


def species_data(src):
    text = (src / "data/pokemon/species_info.h").read_text()
    raw = parse_designated(text, "SPECIES_")
    species = {}
    for name, f in raw.items():
        if name == "SPECIES_NONE":
            continue
        species[name] = {
            "base": [num(f.get(k)) for k in ("baseHP", "baseAttack", "baseDefense", "baseSpeed", "baseSpAttack", "baseSpDefense")],
            "types": listing(f.get("types", "{}")),
            "catch_rate": num(f.get("catchRate")),
            "exp_yield": num(f.get("expYield")),
            "ev_yield": [num(f.get(k)) for k in ("evYield_HP", "evYield_Attack", "evYield_Defense", "evYield_Speed", "evYield_SpAttack", "evYield_SpDefense")],
            "growth_rate": f.get("growthRate", "GROWTH_MEDIUM_FAST"),
            "abilities": listing(f.get("abilities", "{}")),
            "learnset": [],
            "evolutions": [],
        }
    # Level-up learnsets.
    learn = (src / "data/pokemon/level_up_learnsets.h").read_text()
    tables = {m.group(1): [(int(l), mv) for l, mv in re.findall(r"LEVEL_UP_MOVE\(\s*(\d+),\s*(MOVE_\w+)\)", m.group(2))]
              for m in re.finditer(r"static const u16 (\w+)\[\] = \{(.*?)\};", learn, re.S)}
    pointers = (src / "data/pokemon/level_up_learnset_pointers.h").read_text()
    for sp, table in re.findall(r"\[(SPECIES_\w+)\]\s*=\s*(\w+)", pointers):
        if sp in species and table in tables:
            species[sp]["learnset"] = tables[table]
    # Evolutions.
    evo = (src / "data/pokemon/evolution.h").read_text()
    for m in re.finditer(r"\[(SPECIES_\w+)\]\s*=\s*\{(.*?)\},\s*\n", evo, re.S):
        for method, param, target in re.findall(r"\{\s*(EVO_\w+),\s*(\w+),\s*(SPECIES_\w+)\s*\}", m.group(2)):
            if m.group(1) in species:
                species[m.group(1)]["evolutions"].append([method, num(param, param), target])
    return species


def move_data(src):
    text = (src / "data/battle_moves.h").read_text()
    raw = parse_designated(text, "MOVE_")
    names = dict(re.findall(r'\[(MOVE_\w+)\]\s*=\s*_\("(.*?)"\)', (src / "data/text/move_names.h").read_text()))
    return {
        name: {
            "name": names.get(name),
            "effect": f.get("effect"),
            "power": num(f.get("power")),
            "type": f.get("type"),
            "accuracy": num(f.get("accuracy")),
            "pp": num(f.get("pp")),
            "priority": num(f.get("priority")),
            "secondary_chance": num(f.get("secondaryEffectChance")),
        }
        for name, f in raw.items()
        if name != "MOVE_NONE"
    }


def type_chart(src):
    text = (src / "battle_main.c").read_text()
    table = re.search(r"gTypeEffectiveness\[\d+\]\s*=\s*\{(.*?)\};", text, re.S).group(1)
    mult = {"TYPE_MUL_NO_EFFECT": 0, "TYPE_MUL_NOT_EFFECTIVE": 5, "TYPE_MUL_NORMAL": 10, "TYPE_MUL_SUPER_EFFECTIVE": 20}
    chart = []
    for atk, dfn, m in re.findall(r"(TYPE_\w+),\s*(TYPE_\w+),\s*(TYPE_MUL_\w+)", table):
        if atk.startswith("TYPE_FORESIGHT") or dfn.startswith("TYPE_FORESIGHT"):
            continue
        chart.append([atk, dfn, mult[m]])
    return chart


def trainers(src):
    parties_text = (src / "data/trainer_parties.h").read_text()
    parties = {}
    for m in re.finditer(r"static const struct (TrainerMon\w+) (\w+)\[\] = \{(.*?)\n\};", parties_text, re.S):
        mons = []
        for body in re.findall(r"\{(.*?)\n    \}", m.group(3), re.S):
            f = dict((k, v.strip()) for k, v in re.findall(r"\.(\w+)\s*=\s*(\{[^}]*\}|[^,\n]+)", body))
            mons.append({
                "species": f.get("species"),
                "level": num(f.get("lvl")),
                "iv": num(f.get("iv")),
                "item": f.get("heldItem"),
                "moves": [x for x in listing(f["moves"]) if x != "MOVE_NONE"] if "moves" in f else None,
            })
        parties[m.group(2)] = mons
    text = (src / "data/trainers.h").read_text()
    out = {}
    for name, f in parse_designated(text, "TRAINER_").items():
        party = re.search(r"\((\w+)\)", f.get("party", ""))
        out[name] = {
            "class": f.get("trainerClass"),
            "name": (re.search(r'_\("(.*)"\)', f.get("trainerName", "")) or [None, ""])[1],
            "double": f.get("doubleBattle") == "TRUE",
            "items": [x for x in listing(f.get("items", "{}")) if x != "ITEM_NONE"],
            "party": parties.get(party.group(1), []) if party else [],
        }
    return out


def map_scripts(pret, maps):
    """Trainer objects and marts per map (from data/maps/*/scripts.inc; route
    trainers' scripts are shared in data/scripts/trainers.inc)."""
    map_trainers, marts = {}, {}

    def script_blocks(text):
        return {
            b.group(1): b.group(2)
            for b in re.finditer(r"^(\w+)::\n(.*?)(?=^\w+::|\Z)", text, re.S | re.M)
        }

    shared = {}
    for path in sorted((pret / "data/scripts").glob("*.inc")):
        shared.update(script_blocks(path.read_text()))
    for name, m in maps.items():
        path = pret / "data/maps" / name / "scripts.inc"
        if not path.exists():
            continue
        text = path.read_text()
        blocks = {**shared, **script_blocks(text)}
        for i, obj in enumerate(m.get("object_events") or []):
            body = blocks.get(obj.get("script") or "", "")
            t = re.search(r"trainerbattle\w*\s+(TRAINER_\w+)", body)
            if t:
                map_trainers.setdefault(name, []).append({
                    "local_id": i + 1, "trainer": t.group(1), "x": obj.get("x"), "y": obj.get("y"),
                    "sight": num(obj.get("trainer_sight_or_berry_tree_id"), 0),
                })
        for label in re.findall(r"pokemart\s+(\w+)", text):
            items = re.findall(r"\.2byte\s+(ITEM_\w+)", blocks.get(label, ""))
            marts[name] = [x for x in items if x != "ITEM_NONE"]
    return map_trainers, marts


def wild(src, id_to_name):
    data = json.loads((src / "data/wild_encounters.json").read_text())
    out = {}
    for group in data["wild_encounter_groups"]:
        rates = {f["type"]: f["encounter_rates"] for f in group["fields"]}
        for e in group["encounters"]:
            label = e.get("base_label", "")
            if "LeafGreen" in label or e.get("map") not in id_to_name:
                continue
            tables = {}
            for kind in ("land_mons", "water_mons", "rock_smash_mons", "fishing_mons"):
                if kind not in e:
                    continue
                slots = [
                    {"species": s["species"], "min_level": s["min_level"], "max_level": s["max_level"], "chance": rates[kind][i]}
                    for i, s in enumerate(e[kind]["mons"])
                ]
                tables[kind.removesuffix("_mons")] = {"rate": e[kind]["encounter_rate"], "slots": slots}
            out[id_to_name[e["map"]]] = tables
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--pret", default="data/pret-pokefirered")
    ap.add_argument("--out", default="data/world/gamedata.json")
    args = ap.parse_args()
    pret = Path(args.pret)
    src = pret / "src"
    maps = {}
    for f in (pret / "data/maps").glob("*/map.json"):
        m = json.loads(f.read_text())
        maps[m["name"]] = m
    id_to_name = {m["id"]: n for n, m in maps.items()}
    map_trainers, marts = map_scripts(pret, maps)
    items = {i["itemId"]: {"price": i.get("price", 0), "name": i.get("english", i["itemId"]), "pocket": i.get("pocket")} for i in json.loads((src / "data/items.json").read_text())["items"]}
    data = {
        "species": species_data(src),
        "moves": move_data(src),
        "type_chart": type_chart(src),
        "trainers": trainers(src),
        "map_trainers": map_trainers,
        "wild": wild(src, id_to_name),
        "marts": marts,
        "items": items,
    }
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(data, separators=(",", ":")))
    counts = {k: len(v) for k, v in data.items()}
    print(f"wrote {args.out}: {counts}")


if __name__ == "__main__":
    main()
