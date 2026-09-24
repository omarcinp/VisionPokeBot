#!/usr/bin/env python3
"""Build the web UI's map views from the decompilation and the world model.

Offline tool (not used by the bot). Writes to ``data/world/``:

* ``region_map.png`` – the Kanto Town Map / Fly map as the game draws it
  (240x160: background + map layers);
* ``overworld.png`` – every outdoor map connected to Pallet Town stitched
  together through map connections, ``OVERWORLD_SCALE`` pixels per block;
* ``region_map.json`` – where each map lies on both: its region map section
  (and the section's cells), its origin in the overworld, and for maps that
  are not part of the overworld (interiors, caves, Viridian Forest), the
  entrance they are reached from.

Run after ``extract_world.py`` (``tools/world/build.sh`` does both).
"""

import json
import re
from collections import deque
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
PRET = ROOT / "data/pret-pokefirered"
WORLD = ROOT / "data/world"
BLOCK = 16
OVERWORLD_SCALE = 4
OUTDOOR = {"MAP_TYPE_TOWN", "MAP_TYPE_CITY", "MAP_TYPE_ROUTE", "MAP_TYPE_OCEAN_ROUTE"}
GRID_WIDTH, GRID_HEIGHT = 22, 15
# Top-left pixel of grid cell (0, 0) on the 240x160 screen: the cursor sprite
# is centred at 8 * x + 36 (region_map.c).
GRID_ORIGIN = (32, 32)
# GetPlayerPositionOnRegionMap_HandleOverrides (Kanto entries).
OVERRIDES = {
    "MAPSEC_KANTO_SAFARI_ZONE": (12, 12),
    "MAPSEC_SILPH_CO": (14, 6),
    "MAPSEC_POKEMON_MANSION": (4, 14),
    "MAPSEC_POKEMON_TOWER": (18, 6),
    "MAPSEC_POWER_PLANT": (18, 4),
    "MAPSEC_S_S_ANNE": (14, 9),
    "MAPSEC_POKEMON_LEAGUE": (2, 3),
    "MAPSEC_ROCKET_HIDEOUT": (11, 6),
    "MAPSEC_UNDERGROUND_PATH": (14, 7),
    "MAPSEC_UNDERGROUND_PATH_2": (12, 6),
}


def read_jasc(path):
    lines = path.read_text().split()
    count = int(lines[2])
    values = list(map(int, lines[3 : 3 + 3 * count]))
    return [tuple(values[i : i + 3]) for i in range(0, len(values), 3)]


def draw_bg(canvas, tiles_png, tilemap_bin, palette, transparent):
    """Draws a 4bpp GBA background (30x20 tilemap) onto an RGBA canvas."""
    tiles = np.array(Image.open(tiles_png))  # indexed pixels
    per_row = tiles.shape[1] // 8
    entries = np.frombuffer(tilemap_bin.read_bytes(), dtype="<u2")
    for i, entry in enumerate(entries[: 30 * 20]):
        index, hflip, vflip, bank = entry & 0x3FF, entry >> 10 & 1, entry >> 11 & 1, entry >> 12
        ty, tx = divmod(int(index), per_row)
        tile = tiles[ty * 8 : ty * 8 + 8, tx * 8 : tx * 8 + 8] & 0xF
        if tile.shape != (8, 8):
            continue
        if hflip:
            tile = tile[:, ::-1]
        if vflip:
            tile = tile[::-1, :]
        y0, x0 = divmod(i, 30)
        for y in range(8):
            for x in range(8):
                c = int(tile[y, x])
                if transparent and c == 0:
                    continue
                r, g, b = palette[(bank * 16 + c) % len(palette)]
                canvas[y0 * 8 + y, x0 * 8 + x] = (r, g, b, 255)


def render_region_map():
    gfx = PRET / "graphics/region_map"
    palette = read_jasc(gfx / "region_map.pal")
    canvas = np.zeros((160, 240, 4), dtype=np.uint8)
    draw_bg(canvas, gfx / "background.png", gfx / "background.bin", palette, False)
    draw_bg(canvas, gfx / "region_map.png", gfx / "kanto.bin", palette, True)
    Image.fromarray(canvas, "RGBA").convert("RGB").save(WORLD / "region_map.png")


def section_cells():
    """MAPSEC → cells it covers on the Kanto grid (map and dungeon layers)."""
    text = (PRET / "src/data/region_map/region_map_layout_kanto.h").read_text()
    cells = {}
    rows = re.findall(r"\{(MAPSEC_[A-Z0-9_, ]+)\}", text)
    for layer_row, row in enumerate(rows):
        y = layer_row % GRID_HEIGHT
        for x, sec in enumerate(s.strip() for s in row.split(",")):
            if sec != "MAPSEC_NONE":
                cells.setdefault(sec, set()).add((x, y))
    return {sec: sorted(c) for sec, c in cells.items()}


def load_maps():
    maps = {}
    for path in sorted((WORLD / "maps").glob("*.json")):
        m = json.loads(path.read_text())
        pret = PRET / "data/maps" / m["name"] / "map.json"
        m["section"] = json.loads(pret.read_text()).get("region_map_section") if pret.exists() else None
        maps[m["id"]] = m
    return maps


def stitch(maps, start="MAP_PALLET_TOWN"):
    """Block origin of every outdoor map reachable through connections."""
    origin = {start: (0, 0)}
    queue = deque([start])
    while queue:
        mid = queue.popleft()
        m = maps[mid]
        x, y = origin[mid]
        for c in m["connections"]:
            other = maps.get(c["map"])
            if not other or c["map"] in origin or c["direction"] not in ("up", "down", "left", "right"):
                continue
            o = c["offset"]
            origin[c["map"]] = {
                "down": (x + o, y + m["height"]),
                "up": (x + o, y - other["height"]),
                "right": (x + m["width"], y + o),
                "left": (x - other["width"], y + o),
            }[c["direction"]]
            queue.append(c["map"])
    return origin


def render_overworld(maps, origin):
    min_x = min(x for x, _ in origin.values())
    min_y = min(y for _, y in origin.values())
    width = max(x + maps[m]["width"] for m, (x, _) in origin.items()) - min_x
    height = max(y + maps[m]["height"] for m, (_, y) in origin.items()) - min_y
    s = OVERWORLD_SCALE
    canvas = Image.new("RGB", (width * s, height * s), (16, 20, 26))
    for mid, (x, y) in origin.items():
        m = maps[mid]
        render = Image.open(WORLD / "renders" / f"{m['name']}.png")
        pad = m["pad"] * BLOCK
        inner = render.crop((pad, pad, pad + m["width"] * BLOCK, pad + m["height"] * BLOCK))
        inner = inner.resize((m["width"] * s, m["height"] * s), Image.Resampling.BOX)
        canvas.paste(inner, ((x - min_x) * s, (y - min_y) * s))
    canvas.save(WORLD / "overworld.png", optimize=True)
    return {mid: (x - min_x, y - min_y) for mid, (x, y) in origin.items()}, (width, height)


def entrance(maps, mid, placed, depth=6):
    """(outdoor map, x, y) of the warp leading into `mid`, following warps
    out of nested interiors (a gym's back room → the gym → the city)."""
    seen = {mid}
    frontier = [mid]
    for _ in range(depth):
        nxt = []
        for cur in frontier:
            for w in maps[cur]["warps"]:
                dest = maps.get(w["dest_map"])
                if not dest or w["dest_map"] in seen:
                    continue
                if w["dest_map"] in placed and dest["map_type"] in OUTDOOR:
                    if w["dest_warp"] < len(dest["warps"]):
                        dw = dest["warps"][w["dest_warp"]]
                        return w["dest_map"], dw["x"], dw["y"]
                seen.add(w["dest_map"])
                nxt.append(w["dest_map"])
        frontier = nxt
    return None


def main():
    render_region_map()
    maps = load_maps()
    placed, (width, height) = render_overworld(maps, stitch(maps))
    cells = section_cells()
    out_maps = {}
    for mid, m in maps.items():
        info = {"section": m["section"], "width": m["width"], "height": m["height"], "type": m["map_type"]}
        if mid in placed:
            info["overworld"] = placed[mid]
        else:
            door = entrance(maps, mid, placed)
            if door:
                info["entrance"] = {"map": maps[door[0]]["name"], "x": door[1], "y": door[2]}
        out_maps[m["name"]] = info
    (WORLD / "region_map.json").write_text(
        json.dumps(
            {
                "grid": {"x": GRID_ORIGIN[0], "y": GRID_ORIGIN[1], "cell": 8, "width": GRID_WIDTH, "height": GRID_HEIGHT},
                "sections": {sec: c for sec, c in cells.items()},
                "overrides": OVERRIDES,
                "overworld": {"scale": OVERWORLD_SCALE, "width": width, "height": height},
                "maps": out_maps,
            },
            separators=(",", ":"),
        )
    )
    print(f"region map, overworld {width}x{height} blocks ({len(placed)} maps), {len(out_maps)} maps indexed")


if __name__ == "__main__":
    main()
