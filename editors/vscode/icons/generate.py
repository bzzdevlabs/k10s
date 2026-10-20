#!/usr/bin/env python3
"""Generates the Ten/k10s icons from one geometry: `python3 icons/generate.py` (from editors/vscode).

Writes icons/ten.svg (the Ten logo, also docs/logo/ten.svg), icons/k10s.svg and the Material Icon Theme folder
icons icons/folder-{ten,k10s}[-open].svg. The folder silhouettes are Material Icon Theme's (MIT, see NOTICE).
icons/extension.png is the logo rendered at 112 px on a 128 px transparent canvas.
"""

import math
from pathlib import Path

HERE = Path(__file__).resolve().parent

TEAL, TEAL_LIGHT = "#26a69a", "#b2dfdb"  # Material teal 400 / 100
INDIGO, INDIGO_LIGHT = "#7986cb", "#c5cae9"  # Material indigo 300 / 100
INDIGO_FOLDER = "#5c6bc0"  # Material indigo 400

# Material Icon Theme folder silhouettes (16 x 16).
FOLDER = "m6.922 3.768-.644-.536A1 1 0 0 0 5.638 3H2a1 1 0 0 0-1 1v8a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V5a1 1 0 0 0-1-1H7.562a1 1 0 0 1-.64-.232"
FOLDER_OPEN = (
    "M14.483 6H4.721a1 1 0 0 0-.949.684L2 12V5h12a1 1 0 0 0-1-1H7.562a1 1 0 0 1-.64-.232l-.644-.536A1 1 0 0 0 "
    "5.638 3H2a1 1 0 0 0-1 1v8a1 1 0 0 0 1 1h11l2.403-5.606A1 1 0 0 0 14.483 6"
)

# Glyphs on a 32 x 32 grid, polygons listed clockwise (y down).
ONE = [(4.4, 10.4), (10, 6), (13.6, 6), (13.6, 26), (9.6, 26), (9.6, 11.8), (6.6, 13.9)]
HEX_ZERO = [(22, 5.4), (28.6, 9.2), (28.6, 22.8), (22, 26.6), (15.4, 22.8), (15.4, 9.2)]
HEX = [(16, 2.4), (27.8, 9.2), (27.8, 22.8), (16, 29.6), (4.2, 22.8), (4.2, 9.2)]
K = [(0, 0), (3.4, 0), (3.4, 6.6), (7.3, 3.2), (11.9, 3.2), (7.1, 7.3), (12.2, 13), (7.6, 13), (4.3, 9.2), (3.4, 10), (3.4, 13), (0, 13)]


def inset(poly, t):
    """Inward offset of a convex clockwise polygon by `t`, mitered corners."""
    lines = []
    for (x1, y1), (x2, y2) in zip(poly, poly[1:] + poly[:1]):
        dx, dy = x2 - x1, y2 - y1
        n = math.hypot(dx, dy)
        lines.append(((x1 - dy / n * t, y1 + dx / n * t), (dx, dy)))
    out = []
    for (p, d), (q, e) in zip(lines[-1:] + lines[:-1], lines):
        s = ((q[0] - p[0]) * e[1] - (q[1] - p[1]) * e[0]) / (d[0] * e[1] - d[1] * e[0])
        out.append((p[0] + d[0] * s, p[1] + d[1] * s))
    return out


def place(poly, scale, dx, dy):
    return [(x * scale + dx, y * scale + dy) for x, y in poly]


def num(v):
    return f"{v:.3f}".rstrip("0").rstrip(".")


def path(*polys):
    return "".join("M" + " ".join(f"{num(x)} {num(y)}" for x, y in p) + "z" for p in polys)


def svg(view, *paths):
    rule = ' fill-rule="evenodd"'
    body = "".join(f'<path fill="{fill}"{rule if evenodd else ""} d="{d}"/>' for fill, d, evenodd in paths)
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {view} {view}">{body}</svg>\n'


# k centered in the hexagon, 11.6 units tall.
K_SCALE = 11.6 / 13
K_IN_HEX = place(K, K_SCALE, 16 - 12.2 * K_SCALE / 2 - 0.3, 16 - 13 * K_SCALE / 2)


def ten_motive():
    """The Ten logo fitted to the folder's motive area."""
    s, dx, dy = 8.2 / 24.2, 6.9, 6.2
    return path(*(place(p, s, dx - 4.4 * s, dy - 5.4 * s) for p in (ONE, HEX_ZERO, inset(HEX_ZERO, 3.6))))


def k10s_motive():
    """A solid hexagon with the k cut out, fitted to the folder's motive area."""
    s, dx, dy = 8.6 / 27.2, 7.2, 5.3
    return path(*(place(p, s, dx - 4.2 * s, dy - 2.4 * s) for p in (HEX, K_IN_HEX)))


def main():
    ten = svg(32, (TEAL, path(ONE, HEX_ZERO, inset(HEX_ZERO, 3.6)), True))
    files = {
        "ten.svg": ten,
        "k10s.svg": svg(32, (INDIGO, path(HEX, inset(HEX, 3)), True), (INDIGO, path(K_IN_HEX), False)),
        "folder-ten.svg": svg(16, (TEAL, FOLDER, False), (TEAL_LIGHT, ten_motive(), True)),
        "folder-ten-open.svg": svg(16, (TEAL, FOLDER_OPEN, False), (TEAL_LIGHT, ten_motive(), True)),
        "folder-k10s.svg": svg(16, (INDIGO_FOLDER, FOLDER, False), (INDIGO_LIGHT, k10s_motive(), True)),
        "folder-k10s-open.svg": svg(16, (INDIGO_FOLDER, FOLDER_OPEN, False), (INDIGO_LIGHT, k10s_motive(), True)),
    }
    for name, text in files.items():
        (HERE / name).write_text(text)
    logo = HERE.parents[2] / "docs" / "logo" / "ten.svg"
    logo.write_text(ten.replace('viewBox="0 0 32 32"', 'viewBox="0 0 32 32" width="256" height="256"'))


if __name__ == "__main__":
    main()
