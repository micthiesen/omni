#!/usr/bin/env python3
"""Generates Fond's icons.

The mark is the chart in miniature: three stacked cells merging into one tall
cell. Run from anywhere (needs `rsvg-convert`):

    python3 crates/omni-recipes-web/brand/icons.py

Writes crates/omni-recipes-web/icons/. Change this script and regenerate;
never edit the outputs.
"""

import pathlib
import subprocess
import tempfile

OUT = pathlib.Path(__file__).resolve().parents[1] / "icons"

# Bump when the artwork changes: iOS caches Home Screen icons by URL, so an
# installed app only picks up a new icon under a new file name. Update
# index.html and manifest.webmanifest to match.
VERSION = 1

PAPER = "#f6f1e7"
INK = "#b4532a"
CELL = "#f9e3c8"
TALL = "#fff8ec"


def mark(size, inset, background=True, radius=0.0):
    """The mark on a `size` square with `inset` of padding around the cells."""
    span = size - 2 * inset
    gap = span * 0.07
    left = (span - gap) * 0.56
    right = span - gap - left
    cell = (span - 2 * gap) / 3
    r = span * 0.065
    parts = []
    if background:
        parts.append(f'<rect width="{size}" height="{size}" rx="{radius}" fill="{INK}"/>')
    for n in range(3):
        y = inset + n * (cell + gap)
        parts.append(
            f'<rect x="{inset:.2f}" y="{y:.2f}" width="{left:.2f}" height="{cell:.2f}" rx="{r:.2f}" fill="{CELL}"/>'
        )
    parts.append(
        f'<rect x="{inset + left + gap:.2f}" y="{inset:.2f}" width="{right:.2f}" height="{span:.2f}" rx="{r:.2f}" fill="{TALL}"/>'
    )
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}">'
        + "".join(parts)
        + "</svg>\n"
    )


def png(svg, size, path):
    with tempfile.NamedTemporaryFile("w", suffix=".svg") as source:
        source.write(svg)
        source.flush()
        subprocess.run(
            ["rsvg-convert", "-w", str(size), "-h", str(size), "-o", str(path), source.name],
            check=True,
        )


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    # Full-bleed squares: iOS and Android round the corners themselves. The
    # maskable icon keeps the cells inside the 80% safe zone.
    full = mark(512, 112)
    (OUT / "favicon.svg").write_text(mark(64, 12, radius=14))
    png(mark(64, 12, radius=14), 32, OUT / "favicon-32.png")
    png(full, 180, OUT / f"apple-touch-icon-v{VERSION}.png")
    png(full, 192, OUT / f"icon-192-v{VERSION}.png")
    png(full, 512, OUT / f"icon-512-v{VERSION}.png")
    png(mark(512, 140), 512, OUT / f"icon-maskable-512-v{VERSION}.png")


if __name__ == "__main__":
    main()
