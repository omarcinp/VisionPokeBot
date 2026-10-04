#!/usr/bin/env python3
"""Build the web UI's PC box view from the decompilation.

Offline tool (not used by the bot). Writes to ``data/world/``:

* ``pc_wallpapers.png`` – every box wallpaper as the game draws it (160x144
  each, stacked top to bottom in ``WALLPAPER_*`` order); transparent where
  the scrolling background shows through;
* ``pc_backdrop.png`` – the scrolling background behind the box (256x256,
  tiles seamlessly);
* ``pc_arrow.png`` – the box scroll arrows (8x16 each: left on top, right
  below).

Mon icons are served straight from ``graphics/pokemon/*/icon.png``.
"""

import struct
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
PRET = ROOT / "data/pret-pokefirered"
STORAGE = PRET / "graphics/pokemon_storage"
WORLD = ROOT / "data/world"
# sWallpapers (pokemon_storage_system_graphics.c).
WALLPAPERS = [
    "forest", "city", "desert", "savanna", "crag", "volcano", "snow", "cave",
    "beach", "seafloor", "river", "sky", "stars", "pokecenter", "tiles", "simple",
]
# DrawWallpaper copies a 20x18-tile rect.
COLS, ROWS = 20, 18


def palette(img):
    pal = img.getpalette()
    return [tuple(pal[i:i + 3]) for i in range(0, len(pal), 3)]


def draw_tilemap(tiles, tilemap, cols, rows, colour):
    """Renders a GBA text-mode tilemap. ``colour(pal, index)`` gives the
    RGBA of a 4-bit pixel in palette slot ``pal`` (None = transparent)."""
    entries = struct.unpack(f"<{cols * rows}H", tilemap[: cols * rows * 2])
    px = tiles.load()
    per_row = tiles.width // 8
    out = Image.new("RGBA", (cols * 8, rows * 8))
    o = out.load()
    for i, e in enumerate(entries):
        tile, hflip, vflip, pal = e & 0x3FF, e >> 10 & 1, e >> 11 & 1, e >> 12
        tx, ty = tile % per_row * 8, tile // per_row * 8
        for y in range(8):
            for x in range(8):
                sx, sy = tx + (7 - x if hflip else x), ty + (7 - y if vflip else y)
                index = px[sx, sy] & 15 if sy < tiles.height else 0
                rgba = colour(pal, index)
                if rgba:
                    o[i % cols * 8 + x, i // cols * 8 + y] = rgba
    return out


def main():
    backdrop = Image.open(STORAGE / "scrolling_bg.png")
    backdrop_pal = palette(backdrop)
    # The scrolling background is BG palette 3, the wallpaper's own two
    # palettes go to 4 and 5; DrawWallpaper adds 3 to the tilemap's palette.
    sheet = Image.new("RGBA", (COLS * 8, ROWS * 8 * len(WALLPAPERS)))
    for n, name in enumerate(WALLPAPERS):
        tiles = Image.open(STORAGE / "wallpapers" / name / "tiles.png")
        pal = palette(tiles)
        banks = [backdrop_pal[:16], pal[:16], pal[16:32]]

        def colour(slot, index, banks=banks):
            if index == 0 or slot >= len(banks):
                return None
            return banks[slot][index] + (255,)

        tilemap = (STORAGE / "wallpapers" / name / "tilemap.bin").read_bytes()
        sheet.paste(draw_tilemap(tiles, tilemap, COLS, ROWS, colour), (0, n * ROWS * 8))
    WORLD.mkdir(parents=True, exist_ok=True)
    sheet.save(WORLD / "pc_wallpapers.png", optimize=True)

    tilemap = (STORAGE / "scrolling_bg.bin").read_bytes()
    side = int((len(tilemap) // 2) ** 0.5)
    draw_tilemap(
        backdrop, tilemap, side, side, lambda _, index: backdrop_pal[index] + (255,)
    ).save(WORLD / "pc_backdrop.png", optimize=True)

    Image.open(STORAGE / "box_scroll_arrow.png").convert("RGBA").save(WORLD / "pc_arrow.png")
    print(f"pc box: {len(WALLPAPERS)} wallpapers, backdrop {side * 8}px")


if __name__ == "__main__":
    main()
