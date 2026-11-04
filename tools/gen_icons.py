#!/usr/bin/env python3
"""Regenerate the home-screen icons in crates/nearpushd/assets/.

Draws the page's asterisk on black, using only the standard library.
Run from the repo root: python3 tools/gen_icons.py
"""
import math
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "nearpushd", "assets")
BG = (0, 0, 0)
FG = (255, 69, 58)  # the page's red
SS = 4  # supersampling per axis


def inside(x, y):
    """Six-armed asterisk centred at (0.5, 0.5) in unit coordinates."""
    dx, dy = x - 0.5, y - 0.5
    r = math.hypot(dx, dy)
    if r > 0.30:
        return False
    if r < 0.075:
        return True
    for k in range(6):
        a = math.pi / 2 + k * math.pi / 3
        ux, uy = math.cos(a), math.sin(a)
        along = dx * ux + dy * uy
        across = abs(-dx * uy + dy * ux)
        if along > 0 and across < 0.045:
            return True
    return False


def png(size):
    rows = []
    for py in range(size):
        row = bytearray([0])
        for px in range(size):
            hits = 0
            for sy in range(SS):
                for sx in range(SS):
                    if inside((px + (sx + 0.5) / SS) / size, (py + (sy + 0.5) / SS) / size):
                        hits += 1
            t = hits / (SS * SS)
            row += bytes(round(BG[i] + (FG[i] - BG[i]) * t) for i in range(3))
        rows.append(bytes(row))
    raw = b"".join(rows)

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


for size in (180, 192, 512):
    with open(os.path.join(OUT, f"icon-{size}.png"), "wb") as f:
        f.write(png(size))
    print(f"icon-{size}.png")
