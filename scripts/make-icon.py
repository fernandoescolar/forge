#!/usr/bin/env python3
"""Renders Forge's app icon (1024x1024 PNG) without third-party libraries.

A macOS-style rounded square in Forge Dark's greys, a bold "F" glowing from amber to
ember orange, like metal in a forge, and a warm yellow spark. Shapes are signed-distance functions,
so edges are anti-aliased.

    scripts/make-icon.py out.png
"""
import math, struct, sys, zlib

N = 1024


def rgb(h):
    return tuple(int(h[i:i + 2], 16) / 255 for i in (1, 3, 5))


BG_TOP, BG_BOTTOM = rgb("#363c47"), rgb("#1b1e24")
F_TOP, F_BOTTOM = rgb("#ffbe5c"), rgb("#ec6b2d")
SPARK = rgb("#ffe08a")


def sd_round_rect(x, y, cx, cy, hw, hh, r):
    qx, qy = abs(x - cx) - hw + r, abs(y - cy) - hh + r
    return math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - r


def coverage(d):
    return min(max(0.5 - d, 0.0), 1.0)


def mix(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


def over(dst, src, alpha):
    (dr, dg, db, da), (sr, sg, sb) = dst, src
    oa = alpha + da * (1 - alpha)
    if oa == 0:
        return (0, 0, 0, 0)
    return (
        (sr * alpha + dr * da * (1 - alpha)) / oa,
        (sg * alpha + dg * da * (1 - alpha)) / oa,
        (sb * alpha + db * da * (1 - alpha)) / oa,
        oa,
    )


def pixel(x, y):
    px = (0.0, 0.0, 0.0, 0.0)
    # Background: Apple's icon grid uses ~824px squares with ~185px corners.
    bg = coverage(sd_round_rect(x, y, 512, 512, 412, 412, 185))
    if bg > 0:
        px = over(px, mix(BG_TOP, BG_BOTTOM, y / N), bg)
    # The "F": a stem and two arms.
    f = min(
        sd_round_rect(x, y, 400, 512, 70, 270, 28),   # stem
        sd_round_rect(x, y, 530, 312, 200, 70, 28),   # top arm
        sd_round_rect(x, y, 495, 520, 165, 62, 28),   # middle arm
    )
    fc = coverage(f)
    if fc > 0:
        px = over(px, mix(F_TOP, F_BOTTOM, (y - 240) / 540), fc * bg)
    # Spark: a four-pointed star (astroid-ish) by the top arm.
    sx, sy = x - 735, y - 470
    s = (abs(sx) ** 0.6 + abs(sy) ** 0.6) ** (1 / 0.6) - 70
    sc = coverage(s / 1.5)
    if sc > 0:
        px = over(px, SPARK, sc * bg)
    return px


def png(rows):
    raw = b"".join(b"\x00" + bytes(row) for row in rows)
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", N, N, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def main(out):
    rows = []
    for y in range(N):
        row = []
        for x in range(N):
            r, g, b, a = pixel(x + 0.5, y + 0.5)
            row += [round(r * 255), round(g * 255), round(b * 255), round(a * 255)]
        rows.append(row)
    with open(out, "wb") as f:
        f.write(png(rows))


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "forge-icon.png")
