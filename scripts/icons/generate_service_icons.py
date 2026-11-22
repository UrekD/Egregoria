"""Regenerate the base-mod service and warehouse sprites with Python's standard library."""

import struct
import zlib
from pathlib import Path


SIZE = 192
GLYPH_WIDTH = 5
GLYPH_ADVANCE = 6
LINE_ADVANCE = 10
LETTER_SCALE = 14
LETTER_MAX_WIDTH = 140
LABEL_SCALE = 3
# The big letter sits in the top two thirds and the label below it.
LETTER_BAND = (0, 128)
LABEL_BAND = (128, 190)

# Placeholder palette (sRGB). Kept clear of the zone hues (green, blue, orange,
# purple); native_app/src/rendering/map_rendering/placeholder_color.rs mirrors it.
CIVIC = (0xFA, 0xF6, 0xEE)
UTILITY = (0x42, 0x46, 0x4E)
WAREHOUSE = (0x20, 0x22, 0x26)
BLACK = (0x00, 0x00, 0x00)
WHITE = (0xFF, 0xFF, 0xFF)
RED = (0xC8, 0x1E, 0x1E)

# Each group describes one row, and lowercase descenders extend past row seven.
FONT = {
    "C": ".#### ##... ##... ##... ##... ##... .####",
    "D": "####. ##.## ##.## ##.## ##.## ##.## ####.",
    "F": "##### ##... ##... ####. ##... ##... ##...",
    "G": ".#### ##... ##... ##.## ##..# ##..# .###.",
    "H": "##.## ##.## ##.## ##### ##.## ##.## ##.##",
    "L": "##... ##... ##... ##... ##... ##... #####",
    "O": ".###. ##.## ##.## ##.## ##.## ##.## .###.",
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

# sprite name: (big letter, label, box colour, letter colour, label colour)
SPRITES = {
    "school": ("S", "School", CIVIC, BLACK, BLACK),
    "clinic": ("H", "Clinic", CIVIC, RED, BLACK),
    "fire_station": ("F", "Fire\nstation", CIVIC, RED, BLACK),
    "police_station": ("P", "Police", CIVIC, BLACK, BLACK),
    "park": ("Pk", "Park", CIVIC, BLACK, BLACK),
    "garbage_depot": ("G", "Garbage\ndepot", CIVIC, BLACK, BLACK),
    "water_pump": ("W", "Water\npump", UTILITY, WHITE, WHITE),
    "sewage_outlet": ("O", "Sewage\noutlet", UTILITY, WHITE, WHITE),
    "small_depot": ("D", "Small\ndepot", WAREHOUSE, WHITE, WHITE),
    "large_warehouse": ("WH", "Large\nwarehouse", WAREHOUSE, WHITE, WHITE),
}


def png_chunk(kind: bytes, data: bytes) -> bytes:
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def text_cells(text: str, scale: int, band: tuple[int, int]) -> list[tuple[int, int]]:
    """Top-left corners of the scale-sized cells drawing `text` centred in a band of rows."""
    cells = []
    for line_index, line in enumerate(text.splitlines()):
        line_width = GLYPH_ADVANCE * (len(line) - 1) + GLYPH_WIDTH
        left = (SIZE - line_width * scale) // 2
        for letter_index, letter in enumerate(line):
            for y, row in enumerate(FONT[letter].split()):
                for x, pixel in enumerate(row):
                    if pixel == "#":
                        cells.append((
                            left + (letter_index * GLYPH_ADVANCE + x) * scale,
                            (line_index * LINE_ADVANCE + y) * scale,
                        ))

    top = min(y for _, y in cells)
    bottom = max(y for _, y in cells) + scale
    y_offset = band[0] + (band[1] - band[0] - (bottom - top)) // 2 - top
    return [(x, y + y_offset) for x, y in cells]


def render_sprite(letter: str, label: str, box, letter_color, label_color) -> bytes:
    # Start each RGBA scanline with PNG filter byte 0 (unfiltered).
    stride = 1 + SIZE * 4
    rows = bytearray((b"\0" + bytes((*box, 0xFF)) * SIZE) * SIZE)
    letter_width = GLYPH_ADVANCE * (len(letter) - 1) + GLYPH_WIDTH
    letter_scale = min(LETTER_SCALE, LETTER_MAX_WIDTH // letter_width)
    for text, scale, band, color in (
        (letter, letter_scale, LETTER_BAND, letter_color),
        (label, LABEL_SCALE, LABEL_BAND, label_color),
    ):
        for x, y in text_cells(text, scale, band):
            for row_index in range(y, y + scale):
                start = row_index * stride + 1 + x * 4
                rows[start:start + scale * 4] = bytes((*color, 0xFF)) * scale

    header = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + b"".join((
        png_chunk(b"IHDR", header),
        png_chunk(b"IDAT", zlib.compress(rows, level=9)),
        png_chunk(b"IEND", b""),
    ))


def main() -> None:
    sprites = Path(__file__).resolve().parents[2] / "assets" / "sprites"
    for name, style in SPRITES.items():
        (sprites / f"{name}.png").write_bytes(render_sprite(*style))


if __name__ == "__main__":
    main()
