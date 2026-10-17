#!/usr/bin/env python3
"""Generates Omni's logo and every icon derived from it.

The mark is an orbit traced by a sweeping signal: a ring in Signal lime that
brightens clockwise toward a leading dot at the upper right, with a faint track
closing the gap. Run from the repository root (needs `rsvg-convert`):

    python3 scripts/brand/icons.py

Writes the web icons to crates/omni-web/icons/ and the iOS app icon set.
"""

import math
import struct
import pathlib
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
WEB = ROOT / "crates/omni-web/icons"
IOS = ROOT / "ios/OmniLive/Resources/Assets.xcassets/AppIcon.appiconset"

CENTER = 256
RADIUS = 136
STROKE = 48
DOT = 37
LEAD = -45.0  # dot angle, degrees clockwise from 3 o'clock
GAP = 31.0  # angle from the dot to each end of the ring
SEGMENTS = 180

DIM = "#34461a"
MID = "#8fbb41"
HI = "#d8fa94"


def rgb(color):
    return tuple(int(color[i : i + 2], 16) for i in (1, 3, 5))


def trail_color(t):
    """Ring color at t, from 0 (tail) to 1 (head, beside the dot)."""
    a, b, u = (DIM, MID, t / 0.55) if t < 0.55 else (MID, HI, (t - 0.55) / 0.45)
    u = u * u * (3 - 2 * u)
    return "#%02x%02x%02x" % tuple(
        round(x + (y - x) * u) for x, y in zip(rgb(a), rgb(b))
    )


def point(r, degrees):
    a = math.radians(degrees)
    return CENTER + r * math.cos(a), CENTER + r * math.sin(a)


def ring(min_t):
    """The trail as short annular segments, since SVG has no conic gradient."""
    tail, head = LEAD + GAP - 360, LEAD - GAP
    inner, outer = RADIUS - STROKE / 2, RADIUS + STROKE / 2
    shapes = []
    for i in range(SEGMENTS):
        a0 = tail + (head - tail) * i / SEGMENTS
        # Overlap neighbours slightly so antialiasing leaves no seams.
        a1 = tail + (head - tail) * (i + 1) / SEGMENTS + (0.6 if i < SEGMENTS - 1 else 0)
        t = min_t + (1 - min_t) * (i + 0.5) / SEGMENTS
        (x0, y0), (x1, y1) = point(outer, a0), point(outer, a1)
        (x2, y2), (x3, y3) = point(inner, a1), point(inner, a0)
        shapes.append(
            f'<path d="M{x0:.2f} {y0:.2f}A{outer:g} {outer:g} 0 0 1 {x1:.2f} {y1:.2f}'
            f'L{x2:.2f} {y2:.2f}A{inner:g} {inner:g} 0 0 0 {x3:.2f} {y3:.2f}Z" '
            f'fill="{trail_color(t)}"/>'
        )
    for angle, t in ((tail, min_t), (head, 1.0)):
        x, y = point(RADIUS, angle)
        shapes.append(f'<circle cx="{x:.2f}" cy="{y:.2f}" r="{STROKE / 2:g}" fill="{trail_color(t)}"/>')
    return "\n    ".join(shapes)


DOT_X, DOT_Y = point(RADIUS, LEAD)

DEFS = f"""  <defs>
    <radialGradient id="bg" cx="62%" cy="30%" r="85%">
      <stop offset="0" stop-color="#1a2012"/>
      <stop offset="0.5" stop-color="#0f1210"/>
      <stop offset="1" stop-color="#08090b"/>
    </radialGradient>
    <radialGradient id="glow" cx="{DOT_X / 512:.4f}" cy="{DOT_Y / 512:.4f}" r="0.36">
      <stop offset="0" stop-color="#c6f36a" stop-opacity="0.5"/>
      <stop offset="0.35" stop-color="#c6f36a" stop-opacity="0.12"/>
      <stop offset="1" stop-color="#c6f36a" stop-opacity="0"/>
    </radialGradient>
    <radialGradient id="dot" cx="0.38" cy="0.34" r="0.7">
      <stop offset="0" stop-color="#f4ffd9"/>
      <stop offset="0.55" stop-color="#d8fa94"/>
      <stop offset="1" stop-color="#b5e255"/>
    </radialGradient>
    <filter id="mono" color-interpolation-filters="sRGB">
      <feColorMatrix type="saturate" values="0"/>
      <feComponentTransfer><feFuncR type="gamma" exponent="0.6"/><feFuncG type="gamma" exponent="0.6"/><feFuncB type="gamma" exponent="0.6"/></feComponentTransfer>
    </filter>
  </defs>"""


def svg(*, tile=True, corner=0, glow=True, track=True, min_t=0.0, scale=1.0, mono=False):
    """One icon variant. `tile` paints the background; `corner` rounds it."""
    rx = f' rx="{corner}"' if corner else ""
    body = []
    if tile:
        body.append(f'<rect width="512" height="512"{rx} fill="url(#bg)"/>')
    if glow:
        body.append(f'<rect width="512" height="512"{rx} fill="url(#glow)"/>')
    mark = []
    if track:
        mark.append(
            f'<circle cx="{CENTER}" cy="{CENTER}" r="{RADIUS}" fill="none" '
            f'stroke="#c6f36a" stroke-opacity="0.07" stroke-width="{STROKE}"/>'
        )
    mark.append(ring(min_t))
    mark.append(f'<circle cx="{DOT_X:.2f}" cy="{DOT_Y:.2f}" r="{DOT}" fill="url(#dot)"/>')
    transform = (
        f' transform="translate({CENTER} {CENTER}) scale({scale}) translate(-{CENTER} -{CENTER})"'
        if scale != 1
        else ""
    )
    group_filter = ' filter="url(#mono)"' if mono else ""
    body.append(f"<g{transform}{group_filter}>\n    " + "\n    ".join(mark) + "\n  </g>")
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">\n'
        + DEFS
        + "\n  "
        + "\n  ".join(body)
        + "\n</svg>\n"
    )


def render(svg_text, size, out):
    with tempfile.NamedTemporaryFile("w", suffix=".svg", delete=False) as f:
        f.write(svg_text)
    subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), "-o", str(out), f.name], check=True)
    pathlib.Path(f.name).unlink()


def write_ico(png, out):
    """An ICO holding one 32 px PNG, for clients that only ask for /favicon.ico."""
    data = png.read_bytes()
    header = struct.pack("<HHH", 0, 1, 1)
    entry = struct.pack("<BBBBHHII", 32, 32, 0, 0, 1, 32, len(data), 6 + 16)
    out.write_bytes(header + entry + data)


def main():
    WEB.mkdir(parents=True, exist_ok=True)
    full = svg()  # full bleed: iOS and maskable icons round their own corners
    rounded = svg(corner=112)
    # Tab-sized: a bigger, brighter mark with no track, legible at 16 px.
    small = svg(corner=112, track=False, min_t=0.4, scale=1.22)
    # The bare mark for the app rail, on the canvas.
    mark = svg(tile=False, glow=False, track=False, min_t=0.3, scale=1.3)

    (WEB / "mark.svg").write_text(mark)
    (WEB / "favicon.svg").write_text(small)
    render(small, 32, WEB / "favicon-32.png")
    write_ico(WEB / "favicon-32.png", WEB / "favicon.ico")
    render(full, 180, WEB / "apple-touch-icon.png")
    render(rounded, 192, WEB / "icon-192.png")
    render(rounded, 512, WEB / "icon-512.png")
    render(full, 512, WEB / "icon-maskable-512.png")

    # iOS 18+ appearances: dark and tinted variants sit on a system backdrop.
    render(full, 1024, IOS / "OmniIcon.png")
    render(svg(tile=False), 1024, IOS / "OmniIconDark.png")
    render(svg(tile=False, glow=False, mono=True), 1024, IOS / "OmniIconTinted.png")


if __name__ == "__main__":
    main()
