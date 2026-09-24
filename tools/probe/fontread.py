#!/usr/bin/env python3
"""Python port of `pokebot_vision::text::Font::read` for probing regions.

Usage:
    fontread.py IMAGE X Y W H [--font data/world/font_normal.json]
                [--exclude X,Y,W,H ...]

Prints the lines the reader finds in the region, and the ink colour it chose.
Mirrors crates/vision/src/text.rs (ink candidates clustered within tolerance
24, exact glyph-column matching, space gap 4, '?' for unexplained ink), so a
region that reads cleanly here reads the same with the Rust font reader.
"""

import argparse
import json
from collections import Counter

from PIL import Image

CELL = 16
SPACE_GAP = 4
MAX_INKS = 4
MIN_INK_PIXELS = 4
TOL = 24


def near(a, b, tol=TOL):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


class Glyph:
    def __init__(self, text, width, columns):
        self.text, self.width, self.columns = text, width, columns
        self.ink = sum(bin(c).count("1") for c in columns)


def is_digit(t):
    return len(t) == 1 and t.isdigit()


class Font:
    def __init__(self, path):
        data = json.load(open(path))
        self.glyphs = []
        for g in data["glyphs"]:
            rows, w = g["rows"], g["width"]
            cols = []
            for x in range(w):
                bits = 0
                for r, row in enumerate(rows):
                    if x < len(row) and row[x] == "#":
                        bits |= 1 << r
                cols.append(bits)
            gl = Glyph(g["text"], w, cols)
            if gl.ink == 0:
                continue
            dup = next((o for o in self.glyphs if o.columns == cols and o.width == w), None)
            if dup:
                if is_digit(dup.text) and not is_digit(gl.text):
                    dup.text = gl.text
            else:
                self.glyphs.append(gl)
        self.by_first = {}
        for g in self.glyphs:
            self.by_first.setdefault(g.columns[0], []).append(g)

    def match_at(self, columns):
        if columns[0] == 0 and all(c == 0 for c in columns[:3]):
            return None
        best = None
        for g in self.by_first.get(columns[0], []):
            w = g.width
            if w <= len(columns):
                fits = columns[:w] == g.columns
            else:
                fits = columns == g.columns[: len(columns)] and all(c == 0 for c in g.columns[len(columns):])
            if fits and (best is None or (g.ink, g.width) > (best.ink, best.width)):
                best = g
        return best

    def read_line(self, mask, y):
        h, w = len(mask), len(mask[0])
        cols = []
        for x in range(w):
            bits = 0
            for r in range(CELL):
                yy = y + r
                if 0 <= yy < h and mask[yy][x]:
                    bits |= 1 << r
            cols.append(bits)
        text, score, gap, unknown, x = "", 0, 0, False, 0
        while x < len(cols):
            g = self.match_at(cols[x:])
            if g:
                if text and gap >= SPACE_GAP:
                    text += " "
                text += g.text
                score += g.ink
                x += g.width
                gap, unknown = 0, False
            elif cols[x] == 0:
                gap += 1
                x += 1
                unknown = False
            else:
                if not unknown:
                    if text and gap >= SPACE_GAP:
                        text += " "
                    text += "?"
                score -= 2 * bin(cols[x]).count("1")
                unknown, gap = True, 0
                x += 1
        return score, text

    def read_mask(self, mask):
        inked = [any(row) for row in mask]
        bands = []
        for y, i in enumerate(inked):
            if i:
                if bands and y <= bands[-1][1] + 3:
                    bands[-1][1] = y
                else:
                    bands.append([y, y])
        score, lines = 0, []
        for top, bottom in bands:
            lowest = max(bottom - (CELL - 1), 0)
            best = None
            for y in range(max(lowest, top - 8), top + 1):
                s, t = self.read_line(mask, y)
                if best is None or s > best[0]:
                    best = (s, t)
            if best and best[0] > 0:
                score += best[0]
                lines.append(best[1])
        return score, lines

    def read(self, img, region, exclude=()):
        x0, y0, w, h = region
        px = img.load()

        def excluded(x, y):
            return any(ex <= x < ex + ew and ey <= y < ey + eh for ex, ey, ew, eh in exclude)

        counts = Counter(px[x, y][:3] for y in range(y0, y0 + h) for x in range(x0, x0 + w) if not excluded(x, y))
        colours = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))
        clusters = []
        for c, n in colours:
            for cl in clusters:
                if near(cl[0], c):
                    cl[1] += n
                    break
            else:
                clusters.append([c, n])
        inks = [c for c, n in clusters[1:] if n >= MIN_INK_PIXELS][:MAX_INKS]
        best = None
        for ink in inks:
            mask = [[(not excluded(x, y)) and near(px[x, y][:3], ink) for x in range(x0, x0 + w)] for y in range(y0, y0 + h)]
            s, lines = self.read_mask(mask)
            if best is None or s > best[0]:
                best = (s, lines, ink)
        return best or (0, [], None)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("region", nargs=4, type=int)
    ap.add_argument("--font", default="data/world/font_normal.json")
    ap.add_argument("--exclude", action="append", default=[])
    a = ap.parse_args()
    font = Font(a.font)
    img = Image.open(a.image).convert("RGB")
    ex = [tuple(int(v) for v in e.split(",")) for e in a.exclude]
    score, lines, ink = font.read(img, tuple(a.region), ex)
    print(f"ink={ink} score={score}")
    for line in lines:
        print(f"  {line!r}")


if __name__ == "__main__":
    main()
