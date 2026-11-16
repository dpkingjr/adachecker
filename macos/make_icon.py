#!/usr/bin/env python3
"""Render the 1024x1024 app icon PNG using only the standard library.

Usage: make_icon.py <output.png>

Draws a macOS-style rounded square with a blue gradient and a white checkmark,
anti-aliased via signed-distance fields.
"""
import math
import struct
import sys
import zlib

SIZE = 1024
# Apple's icon grid: 824px body centred in the 1024px canvas.
BODY = 824
OFF = (SIZE - BODY) / 2
RADIUS = 185
TOP = (31, 95, 191)      # #1F5FBF
BOTTOM = (12, 52, 120)   # #0C3478
CHECK = [(330, 530), (455, 655), (700, 385)]
STROKE = 44  # half-width of the checkmark stroke


def sd_round_rect(x, y):
    cx = cy = SIZE / 2
    hx = hy = BODY / 2 - RADIUS
    dx = abs(x - cx) - hx
    dy = abs(y - cy) - hy
    outside = math.hypot(max(dx, 0), max(dy, 0))
    inside = min(max(dx, dy), 0)
    return outside + inside - RADIUS


def sd_segment(px, py, ax, ay, bx, by):
    pax, pay, bax, bay = px - ax, py - ay, bx - ax, by - ay
    h = max(0.0, min(1.0, (pax * bax + pay * bay) / (bax * bax + bay * bay)))
    return math.hypot(pax - bax * h, pay - bay * h)


def coverage(d):
    return max(0.0, min(1.0, 0.5 - d))


def main(out):
    rows = []
    for y in range(SIZE):
        row = bytearray([0])  # PNG filter type: none
        t = max(0.0, min(1.0, (y - OFF) / BODY))
        base = [TOP[i] + (BOTTOM[i] - TOP[i]) * t for i in range(3)]
        for x in range(SIZE):
            px, py = x + 0.5, y + 0.5
            a = coverage(sd_round_rect(px, py))
            if a == 0.0:
                row += b"\x00\x00\x00\x00"
                continue
            d = min(
                sd_segment(px, py, *CHECK[0], *CHECK[1]),
                sd_segment(px, py, *CHECK[1], *CHECK[2]),
            ) - STROKE
            c = coverage(d)
            rgb = [round(base[i] * (1 - c) + 255 * c) for i in range(3)]
            row += bytes((*rgb, round(255 * a)))
        rows.append(bytes(row))

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(b"".join(rows), 9))
    png += chunk(b"IEND", b"")
    with open(out, "wb") as f:
        f.write(png)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "AppIcon.png")
