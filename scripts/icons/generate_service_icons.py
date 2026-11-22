"""Regenerate the base-mod service sprites with Python's standard library."""

import struct
import zlib
from pathlib import Path


SIZE = 192
SCALE = 4
GLYPH_WIDTH = 5
GLYPH_ADVANCE = 6
LINE_ADVANCE = 10

# Each group describes one row, and lowercase descenders extend past row seven.
FONT = {
    "C": ".#### ##... ##... ##... ##... ##... .####",
    "F": "##### ##... ##... ####. ##... ##... ##...",
    "G": ".#### ##... ##... ##.## ##..# ##..# .###.",
    "P": "####. ##..# ##..# ####. ##... ##... ##...",
    "S": ".#### ##... ##... .###. ...## ...## ####.",
    "W": "#...# #...# #...# #.#.# ##### ##.## #...#",
    "a": "..... ..... .###. ...## .#### ##.## .####",
    "b": "##... ##... ####. ##.## ##.## ##.## ####.",
    "c": "..... ..... .#### ##... ##... ##... .####",
    "d": "...## ...## .#### ##.## ##.## ##.## .####",
    "e": "..... ..... .###. ##.## ##### ##... .####",
    "g": "..... ..... .#### ##.## ##.## ##.## .#### ...## .###.",
    "h": "##... ##... ####. ##.## ##.## ##.## ##.##",
    "i": "..##. ..... .###. ..##. ..##. ..##. .####",
    "k": "##... ##... ##.## ####. ###.. ####. ##.##",
    "l": ".###. ..##. ..##. ..##. ..##. ..##. ...##",
    "m": "..... ..... ##.#. ##### #.#.# #.#.# #.#.#",
    "n": "..... ..... ####. ##.## ##.## ##.## ##.##",
    "o": "..... ..... .###. ##.## ##.## ##.## .###.",
    "p": "..... ..... ####. ##.## ##.## ##.## ####. ##... ##...",
    "r": "..... ..... ##.## ###.. ##... ##... ##...",
    "s": "..... ..... .#### ##... .###. ...## ####.",
    "t": ".##.. .##.. ####. .##.. .##.. .##.. ..###",
    "u": "..... ..... ##.## ##.## ##.## ##.## .####",
    "w": "..... ..... #.#.# #.#.# ##### ##.## #...#",
}

LABELS = {
    "water_pump": "Water\npump",
    "sewage_outlet": "Sewage\noutlet",
    "clinic": "Clinic",
    "police_station": "Police",
    "garbage_depot": "Garbage\ndepot",
    "park": "Park",
    "school": "School",
    "fire_station": "Fire\nstation",
}


def png_chunk(kind: bytes, data: bytes) -> bytes:
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def render_label(label: str) -> bytes:
    pixels = []
    for line_index, line in enumerate(label.splitlines()):
        line_width = GLYPH_ADVANCE * (len(line) - 1) + GLYPH_WIDTH
        left = (SIZE - line_width * SCALE) // 2
        for letter_index, letter in enumerate(line):
            for y, row in enumerate(FONT[letter].split()):
                for x, pixel in enumerate(row):
                    if pixel == "#":
                        pixels.append((
                            left + (letter_index * GLYPH_ADVANCE + x) * SCALE,
                            (line_index * LINE_ADVANCE + y) * SCALE,
                        ))

    top = min(y for _, y in pixels)
    bottom = max(y for _, y in pixels) + SCALE
    y_offset = (SIZE - (bottom - top)) // 2 - top
    # Start each RGBA scanline with PNG filter byte 0 (unfiltered).
    stride = 1 + SIZE * 4
    rows = bytearray((b"\0" + b"\x00\x00\x00\xff" * SIZE) * SIZE)
    for x, y in pixels:
        for row_index in range(y + y_offset, y + y_offset + SCALE):
            start = row_index * stride + 1 + x * 4
            rows[start:start + SCALE * 4] = b"\xff" * (SCALE * 4)

    header = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + b"".join((
        png_chunk(b"IHDR", header),
        png_chunk(b"IDAT", zlib.compress(rows, level=9)),
        png_chunk(b"IEND", b""),
    ))


def main() -> None:
    sprites = Path(__file__).resolve().parents[2] / "assets" / "sprites"
    for name, label in LABELS.items():
        (sprites / f"{name}.png").write_bytes(render_label(label))


if __name__ == "__main__":
    main()
