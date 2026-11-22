import re
import struct
import unittest
import zlib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]

CREAM = (0xFA, 0xF6, 0xEE)
SLATE = (0x42, 0x46, 0x4E)
CHARCOAL = (0x20, 0x22, 0x26)
BLACK = (0x00, 0x00, 0x00)
WHITE = (0xFF, 0xFF, 0xFF)
RED = (0xC8, 0x1E, 0x1E)

# sprite: (box colour, big-letter colour)
EXPECTED = {
    "clinic": (CREAM, RED),
    "fire_station": (CREAM, RED),
    "police_station": (CREAM, BLACK),
    "school": (CREAM, BLACK),
    "park": (CREAM, BLACK),
    "garbage_depot": (CREAM, BLACK),
    "water_pump": (SLATE, WHITE),
    "sewage_outlet": (SLATE, WHITE),
    "small_depot": (CHARCOAL, WHITE),
    "large_warehouse": (CHARCOAL, WHITE),
}


def decode_rgba(path: Path):
    data = path.read_bytes()
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    pos, idat, header = 8, b"", None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        if kind == b"IHDR":
            header = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat += body
        pos += 12 + length
    width, height = header[:2]
    raw = zlib.decompress(idat)
    stride = 1 + width * 4
    rows = []
    for y in range(height):
        row = raw[y * stride:(y + 1) * stride]
        assert row[0] == 0, "generator writes unfiltered scanlines"
        rows.append([tuple(row[1 + x * 4:1 + x * 4 + 3]) for x in range(width)])
    return header, rows


class ServiceIconsTest(unittest.TestCase):
    def test_services_use_dedicated_rgba_icons(self):
        services = (ROOT / "base_mod/services.lua").read_text(encoding="utf-8")
        assets = dict(re.findall(r'service\("([^"]+)", "[^"]+", "[^"]+", "([^"]+)"', services))

        for name in (
            "water-pump", "sewage-outlet", "clinic", "police-station",
            "garbage-depot", "park", "school", "fire-station",
        ):
            with self.subTest(service=name):
                own_asset = f"assets/sprites/{name.replace('-', '_')}.png"
                self.assertEqual(assets[name], own_asset)
                header = (ROOT / own_asset).read_bytes()[:33]
                self.assertEqual(header[:16], b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR")
                self.assertEqual(struct.unpack(">IIBBBBB", header[16:29]), (192, 192, 8, 6, 0, 0, 0))

    def test_warehouses_use_dedicated_sprites(self):
        warehouses = (ROOT / "base_mod/warehouses.lua").read_text(encoding="utf-8")
        assets = dict(re.findall(r'warehouse\("([^"]+)", "[^"]+", "([^"]+)"', warehouses))
        for name in ("small-depot", "large-warehouse"):
            with self.subTest(warehouse=name):
                self.assertEqual(assets[name], f"assets/sprites/{name.replace('-', '_')}.png")

    def test_sprites_use_the_placeholder_palette_with_a_big_letter(self):
        for name, (box, letter) in EXPECTED.items():
            with self.subTest(sprite=name):
                header, rows = decode_rgba(ROOT / f"assets/sprites/{name}.png")
                self.assertEqual(header, (192, 192, 8, 6, 0, 0, 0))
                self.assertEqual(rows[0][0], box)
                self.assertEqual(rows[191][191], box)
                # The big letter covers a sizeable share of the upper part of the tile.
                upper = [pixel for row in rows[:128] for pixel in row]
                self.assertGreater(upper.count(letter), 192 * 128 // 12)
                self.assertEqual(set(upper), {box, letter})

    def test_renderer_palette_matches_the_sprites(self):
        source = (ROOT / "native_app/src/rendering/map_rendering/placeholder_color.rs").read_text(
            encoding="utf-8"
        )
        palette = {
            name: tuple(bytes.fromhex(hex_value))
            for name, hex_value in re.findall(r"const (\w+): u64 = 0x([0-9A-Fa-f]{6});", source)
        }
        self.assertEqual(palette, {"CIVIC": CREAM, "UTILITY": SLATE, "WAREHOUSE": CHARCOAL})


if __name__ == "__main__":
    unittest.main()
