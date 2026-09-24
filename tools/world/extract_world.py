#!/usr/bin/env python3
"""Build PokéBot's static world model from the pret/pokefirered decompilation.

Offline tool (not used at runtime). For every map it writes:

* ``data/world/maps/<Map>.json`` – size, collision/elevation/behaviour per tile,
  warps, connections, NPCs, signs and trigger tiles;
* ``data/world/renders/<Map>.png`` – the map's background as the game draws
  it (bottom + top metatile layers), padded by ``PAD`` tiles on every side with
  the border pattern and connected maps, so any camera view can be matched.

The output is derived from game assets, so it stays local (gitignored), like
the ROM. Run ``tools/world/build.sh`` to set up Python and run this.
"""

import argparse
import json
import struct
import sys
from pathlib import Path

import numpy as np
from PIL import Image

TILE = 8
BLOCK = 16
PAD = 8  # blocks of context around each map (camera shows 15x10 blocks)
NUM_TILES_PRIMARY = 640
NUM_METATILES_PRIMARY = 640
NUM_PALS_PRIMARY = 7
NUM_PALS_TOTAL = 13

# FireRed metatile attribute bit fields (u32 per metatile).
ATTR_BEHAVIOR = (0x000001FF, 0)
ATTR_ENCOUNTER = (0x07000000, 24)
ATTR_LAYER = (0x60000000, 29)


class TilesetIndex:
    """Resolves gTileset_* symbols to asset files by parsing the decomp's C
    headers (tilesets may share tiles/palettes with others)."""

    def __init__(self, pret):
        import re
        src = pret / "src/data/tilesets"
        headers = (src / "headers.h").read_text()
        graphics = (src / "graphics.h").read_text()
        metatiles = (src / "metatiles.h").read_text()
        self.tilesets = {}
        for m in re.finditer(r"const struct Tileset (\w+) =\s*\{(.*?)\};", headers, re.S):
            fields = dict(re.findall(r"\.(\w+)\s*=\s*(\w+)", m.group(2)))
            self.tilesets[m.group(1)] = fields
        self.files = {}
        for m in re.finditer(r"(gTilesetTiles_\w+)\[\]\s*=\s*INCBIN_U32\(\"([^\"]+)\"\)", graphics):
            self.files[m.group(1)] = pret / m.group(2).replace(".4bpp.lz", ".png").replace(".4bpp", ".png")
        for m in re.finditer(r"(gTilesetPalettes_\w+)\[\]\[16\]\s*=\s*\{\s*INCBIN_U16\(\"([^\"]+)\"\)", graphics):
            self.files[m.group(1)] = (pret / m.group(2)).parent
        for m in re.finditer(r"(gMetatile\w+)\[\]\s*=\s*INCBIN_U(?:16|32)\(\"([^\"]+)\"\)", metatiles):
            self.files[m.group(1)] = pret / m.group(2)

    def load(self, symbol, pret, secondary, shade=False):
        fields = self.tilesets[symbol]
        # A few tilesets (e.g. General) are declared outside graphics.h; those
        # follow the directory naming convention.
        folder = pret / "data/tilesets" / ("secondary" if secondary else "primary") / snake(symbol.removeprefix("gTileset_"))
        defaults = {
            "tiles": folder / "tiles.png",
            "palettes": folder / "palettes",
            "metatiles": folder / "metatiles.bin",
            "metatileAttributes": folder / "metatile_attributes.bin",
        }
        get = lambda key: self.files.get(fields[key], defaults[key])
        return Tileset(get("tiles"), get("palettes"), get("metatiles"), get("metatileAttributes"), shade)


def snake(name):
    """GenericBuilding1 -> generic_building_1"""
    out = []
    for i, c in enumerate(name):
        if i and (c.isupper() or (c.isdigit() and not name[i - 1].isdigit())):
            out.append("_")
        out.append(c.lower())
    return "".join(out)


def gba_rgb(r8, g8, b8, shade=False):
    """Colour as mGBA outputs it (RGB565 via libretro), from a JASC palette
    entry. `shade` applies WEATHER_SHADE (each 5-bit channel × 13/16, as
    measured from the game)."""
    r5, g5, b5 = r8 >> 3, g8 >> 3, b8 >> 3
    if shade:
        r5, g5, b5 = r5 * 13 // 16, g5 * 13 // 16, b5 * 13 // 16
    expand5 = lambda v: (v << 3) | (v >> 2)
    g6 = g5 << 1
    return (expand5(r5), (g6 << 2) | (g6 >> 4), expand5(b5))


def load_palettes(path, shade=False):
    pals = []
    for i in range(16):
        f = path / f"{i:02}.pal"
        if not f.exists():
            pals.append([(0, 0, 0)] * 16)
            continue
        lines = f.read_text().split()
        values = list(map(int, lines[3:3 + 48]))
        pals.append([gba_rgb(*values[j * 3:j * 3 + 3], shade) for j in range(16)])
    return pals


class Tileset:
    def __init__(self, tiles_png, palette_dir, metatiles_bin, attributes_bin, shade=False):
        image = Image.open(tiles_png)
        if image.mode != "P":
            raise ValueError(f"{tiles_png} is not palette-indexed")
        idx = np.array(image, dtype=np.uint8) & 0x0F
        h, w = idx.shape
        self.tiles = idx.reshape(h // TILE, TILE, w // TILE, TILE).transpose(0, 2, 1, 3).reshape(-1, TILE, TILE)
        self.metatiles = np.frombuffer(metatiles_bin.read_bytes(), dtype="<u2").reshape(-1, 8)
        self.attributes = np.frombuffer(attributes_bin.read_bytes(), dtype="<u4")
        self.palettes = load_palettes(palette_dir, shade)


class Tilesets:
    def __init__(self, primary, secondary):
        self.primary, self.secondary = primary, secondary
        palettes = primary.palettes[:NUM_PALS_PRIMARY] + secondary.palettes[NUM_PALS_PRIMARY:NUM_PALS_TOTAL]
        palettes += [[(0, 0, 0)] * 16] * (16 - len(palettes))
        self.palettes = np.array(palettes, dtype=np.uint8)  # 16 x 16 x 3

    def tile(self, index):
        if index < NUM_TILES_PRIMARY:
            tiles = self.primary.tiles
        else:
            tiles, index = self.secondary.tiles, index - NUM_TILES_PRIMARY
        return tiles[index] if index < len(tiles) else np.zeros((TILE, TILE), np.uint8)

    def metatile(self, mid):
        if mid < NUM_METATILES_PRIMARY:
            ts, i = self.primary, mid
        else:
            ts, i = self.secondary, mid - NUM_METATILES_PRIMARY
        if i >= len(ts.metatiles):
            return None, 0
        return ts.metatiles[i], int(ts.attributes[i]) if i < len(ts.attributes) else 0

    def render_metatile(self, mid, cache):
        if mid in cache:
            return cache[mid]
        entries, _ = self.metatile(mid)
        out = np.zeros((BLOCK, BLOCK, 3), np.uint8)
        out[:] = self.palettes[0][0]  # backdrop
        if entries is not None:
            for layer in range(2):
                for q in range(4):
                    e = int(entries[layer * 4 + q])
                    t = self.tile(e & 0x3FF)
                    if e & 0x400:
                        t = t[:, ::-1]
                    if e & 0x800:
                        t = t[::-1, :]
                    pal = self.palettes[(e >> 12) & 0xF]
                    y0, x0 = (q // 2) * TILE, (q % 2) * TILE
                    region = out[y0:y0 + TILE, x0:x0 + TILE]
                    opaque = t != 0
                    region[opaque] = pal[t[opaque]]
        cache[mid] = out
        return out


def field(value, mask_shift):
    mask, shift = mask_shift
    return (value & mask) >> shift


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--pret", default="data/pret-pokefirered")
    ap.add_argument("--out", default="data/world")
    ap.add_argument("--maps", nargs="*", help="only these maps (default: all)")
    args = ap.parse_args()
    pret, out = Path(args.pret), Path(args.out)
    (out / "maps").mkdir(parents=True, exist_ok=True)
    (out / "renders").mkdir(parents=True, exist_ok=True)

    layouts = {l["id"]: l for l in json.loads((pret / "data/layouts/layouts.json").read_text())["layouts"] if "id" in l}
    maps = {}
    for f in sorted((pret / "data/maps").glob("*/map.json")):
        m = json.loads(f.read_text())
        if m.get("layout") in layouts:
            maps[m["id"]] = m
    tileset_cache, render_cache = {}, {}
    index_ts = TilesetIndex(pret)

    def tilesets_for(layout, shade):
        key = (layout["primary_tileset"], layout["secondary_tileset"], shade)
        if key not in tileset_cache:
            tileset_cache[key] = Tilesets(index_ts.load(key[0], pret, False, shade), index_ts.load(key[1], pret, True, shade))
            render_cache[key] = {}
        return tileset_cache[key], render_cache[key]

    def blocks(layout):
        data = np.frombuffer((pret / layout["blockdata_filepath"]).read_bytes(), dtype="<u2")
        return data.reshape(layout["height"], layout["width"])

    def border(layout):
        data = np.frombuffer((pret / layout["border_filepath"]).read_bytes(), dtype="<u2")
        return data.reshape(layout["border_height"], layout["border_width"])

    selected = [m for m in maps.values() if not args.maps or m["name"] in args.maps]
    index = []
    for m in selected:
        layout = layouts[m["layout"]]
        # Weather that recolours the map (Viridian Forest is shaded).
        ts, cache = tilesets_for(layout, m.get("weather") == "WEATHER_SHADE")
        grid = blocks(layout)
        h, w = grid.shape
        # Padded block grid: border pattern, then connected maps, then this map.
        H, W = h + 2 * PAD, w + 2 * PAD
        canvas = np.zeros((H, W), np.int32)
        bd = border(layout)
        for y in range(H):
            for x in range(W):
                canvas[y, x] = bd[(y - PAD) % bd.shape[0], (x - PAD) % bd.shape[1]] & 0x3FF
        for c in m.get("connections") or []:
            other = maps.get(c["map"])
            if not other or c["direction"] not in ("up", "down", "left", "right"):
                continue
            og = blocks(layouts[other["layout"]])
            oh, ow = og.shape
            off = int(c["offset"])
            if c["direction"] == "up":
                oy, ox = -oh, off
            elif c["direction"] == "down":
                oy, ox = h, off
            elif c["direction"] == "left":
                oy, ox = off, -ow
            else:
                oy, ox = off, w
            for y in range(oh):
                for x in range(ow):
                    cy, cx = y + oy + PAD, x + ox + PAD
                    if 0 <= cy < H and 0 <= cx < W:
                        canvas[cy, cx] = og[y, x] & 0x3FF
        canvas[PAD:PAD + h, PAD:PAD + w] = grid & 0x3FF
        # Connected maps may use other tilesets; rendered with ours they would
        # be wrong, so only same-tileset neighbours are drawn (others keep the
        # border pattern). Most outdoor Kanto maps share tilesets.
        image = np.zeros((H * BLOCK, W * BLOCK, 3), np.uint8)
        for y in range(H):
            for x in range(W):
                image[y * BLOCK:(y + 1) * BLOCK, x * BLOCK:(x + 1) * BLOCK] = ts.render_metatile(int(canvas[y, x]), cache)
        Image.fromarray(image).save(out / "renders" / f"{m['name']}.png", optimize=True)

        tiles = []
        for y in range(h):
            row = []
            for x in range(w):
                b = int(grid[y, x])
                _, attrs = ts.metatile(b & 0x3FF)
                row.append([
                    (b >> 10) & 0x3,  # collision (non-zero = blocked)
                    (b >> 12) & 0xF,  # elevation
                    field(attrs, ATTR_BEHAVIOR),
                    field(attrs, ATTR_ENCOUNTER),
                ])
            tiles.append(row)
        model = {
            "id": m["id"],
            "name": m["name"],
            "width": w,
            "height": h,
            "pad": PAD,
            "map_type": m.get("map_type"),
            "tiles": tiles,
            "warps": [
                # Dynamic warps (e.g. building exits back to "where you came
                # from") have no fixed destination: dest_warp = -1.
                {"x": e["x"], "y": e["y"], "dest_map": e["dest_map"],
                 "dest_warp": int(e["dest_warp_id"]) if str(e["dest_warp_id"]).isdigit() else -1}
                for e in m.get("warp_events") or []
            ],
            "connections": [
                {"direction": c["direction"], "offset": int(c["offset"]), "map": c["map"]}
                for c in m.get("connections") or []
            ],
            "objects": [
                {
                    "local_id": i + 1,
                    "graphics": e.get("graphics_id"),
                    "x": e.get("x"),
                    "y": e.get("y"),
                    "movement": e.get("movement_type"),
                    "script": e.get("script"),
                    "flag": e.get("flag"),
                    "trainer_type": e.get("trainer_type"),
                }
                for i, e in enumerate(m.get("object_events") or [])
            ],
            "signs": [{"x": e["x"], "y": e["y"], "script": e.get("script")} for e in m.get("bg_events") or []],
            "triggers": [
                {"x": e["x"], "y": e["y"], "var": e.get("var"), "value": e.get("var_value"), "script": e.get("script")}
                for e in m.get("coord_events") or []
            ],
        }
        (out / "maps" / f"{m['name']}.json").write_text(json.dumps(model, separators=(",", ":")))
        index.append({"id": m["id"], "name": m["name"], "width": w, "height": h})
    (out / "index.json").write_text(json.dumps({"pad": PAD, "maps": index}, indent=1))
    print(f"wrote {len(index)} maps to {out}", file=sys.stderr)


if __name__ == "__main__":
    main()
