import re
import struct
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


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


if __name__ == "__main__":
    unittest.main()
