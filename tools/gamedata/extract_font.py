#!/usr/bin/env python3
"""Extract FireRed's normal Latin text font from the pret/pokefirered
decompilation.

Offline tool. Writes data/world/font_normal.json (local, gitignored): for
every single-byte character code with a charmap entry, its text, advance
width and 16-row bitmap (`#` = ink, `s` = shadow, `.` = background inside the
glyph, trimmed to the advance width). The runtime text reader matches ink
masks against these.
"""

import argparse
import json
import re
from pathlib import Path

from PIL import Image

# Named charmap entries that print as text. Others (control codes, colour
# names) aren't glyphs.
NAMED = {
    "PK": "PK",
    "MN": "MN",
    "LV": "Lv",
    "UP_ARROW": "↑",
    "DOWN_ARROW": "↓",
    "LEFT_ARROW": "←",
    "RIGHT_ARROW": "→",
    "SUPER_E": "ᵉ",
    "SUPER_ER": "ᵉʳ",
    "SUPER_RE": "ʳᵉ",
}


def parse_charmap(path):
    """code → text, first mapping wins (the Latin table precedes Japanese)."""
    chars = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith("@ Hiragana"):
            break  # the Japanese tables reuse the same codes
        m = re.match(r"^'(.+)'\s*=\s*([0-9A-F]{2})\s*$", line)
        if m:
            text = m.group(1).replace("\\'", "'").replace("\\\\", "\\")
            chars.setdefault(int(m.group(2), 16), text)
            continue
        m = re.match(r"^(\w+)\s*=\s*([0-9A-F]{2})(?:\s+([0-9A-F]{2}))?\s*$", line)
        if m and m.group(1) in NAMED:
            chars.setdefault(int(m.group(2), 16), NAMED[m.group(1)])
        if m and m.group(1) == "PKMN" and m.group(3):
            chars.setdefault(int(m.group(2), 16), "PK")
            chars.setdefault(int(m.group(3), 16), "MN")
    return chars


def parse_widths(text_c, table):
    body = re.search(rf"{table}\[\]\s*=\s*\{{([^}}]*)\}}", text_c).group(1)
    return [int(v) for v in re.findall(r"\d+", body)]


FONTS = {
    "normal": {
        "sheet": "graphics/fonts/latin_normal.png",
        "widths_table": "sFontNormalLatinGlyphWidths",
        "cell_width": 16,
        "out": Path("data/world/font_normal.json"),
    },
    "small": {
        "sheet": "graphics/fonts/latin_small.png",
        "widths_table": "sFontSmallLatinGlyphWidths",
        "cell_width": 8,
        "out": Path("data/world/font_small.json"),
    },
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pret", type=Path, default=Path("data/pret-pokefirered"))
    parser.add_argument("--font", choices=sorted(FONTS), default="normal")
    parser.add_argument("--out", type=Path, default=None)
    args = parser.parse_args()
    font = FONTS[args.font]
    out = args.out or font["out"]

    chars = parse_charmap(args.pret / "charmap.txt")
    widths = parse_widths((args.pret / "src/text.c").read_text(encoding="utf-8"), font["widths_table"])
    sheet = Image.open(args.pret / font["sheet"])
    cell_w = font["cell_width"]
    columns = sheet.width // cell_w
    pixels = sheet.load()

    glyphs = []
    for code in range(0x100):
        text = chars.get(code)
        if text is None or code >= len(widths):
            continue
        row, col = divmod(code, columns)
        width = widths[code]
        rows = [
            "".join(
                {1: "#", 2: "s"}.get(pixels[col * cell_w + x, row * 16 + y], ".")
                for x in range(width)
            )
            for y in range(16)
        ]
        glyphs.append({"code": code, "text": text, "width": width, "rows": rows})

    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({"height": 16, "glyphs": glyphs}, ensure_ascii=False, indent=1))
    print(f"font: {len(glyphs)} glyphs → {out}")


if __name__ == "__main__":
    main()
