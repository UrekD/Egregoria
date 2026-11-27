import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from pathpark_compare import CORE_WORK, FIXTURES, OPENING_COUNTS, OTHER_WORK, compare


class PathParkEvidenceTest(unittest.TestCase):
    def campaign(self, root, duplicate=False, swap=False, ratio=1):
        manifest = {"pairs": 3, "cpu": 7, "campaign_id": "1" * 64, "fixtures": {}}
        sequence = 0
        for fixture in FIXTURES:
            opening = dict.fromkeys(OPENING_COUNTS, 100)
            digests = dict.fromkeys(
                ("runner_sha256", "assets_sha256", "prototypes_sha256",
                 "slot_sha256", "sidecar_sha256"), "a" * 64
            )
            inputs = digests | {"slot_revision": 2}
            windows = {"warmup_ticks": 2_000, "measured_ticks": 10_000}
            floors = dict.fromkeys(CORE_WORK, 98) | dict.fromkeys(OTHER_WORK, 0)
            pinned = manifest["fixtures"][fixture] = {
                "inputs": dict.fromkeys(("parent", "candidate"), inputs),
                "opening": opening, "opening_sha256": "b" * 64, "topology_sha256": "c" * 64,
                "p99_ms_per_tick": 10, "rss_kib": 1_200, "workload_floors": floors, **windows,
            }
            runs = []
            for pair in range(1, 4):
                builds = ("parent", "candidate") if pair % 2 else ("candidate", "parent")
                if swap and pair == 2:
                    builds = tuple(reversed(builds))
                runs.extend((pair, build) for build in builds)
            for run, build in runs:
                sequence += 1
                self.write_run(root, fixture, run, build, sequence, manifest, pinned, inputs,
                               duplicate, ratio)
        self.write_run(root, "perf_10k", "absolute", "candidate", sequence + 1, manifest,
                       manifest["fixtures"]["perf_10k"], inputs, duplicate, ratio)
        return manifest

    def write_run(self, root, fixture, run, build, sequence, manifest, pinned, inputs,
                  duplicate, ratio):
        windows = {key: pinned[key] for key in ("warmup_ticks", "measured_ticks")}
        sample = {
            "label": "measured", "tick": 12_001,
            "ticks_per_sec": 180 if duplicate else 180 + sequence / 100,
            "extra": {
                "median_ticks_per_sec": 200 if build == "parent" else 200 * ratio,
                "p99_ms_per_tick": 9, **dict.fromkeys(CORE_WORK, 100), **dict.fromkeys(OTHER_WORK, 0),
            },
        }
        if fixture == "perf_10k":
            payload = {fixture: [{"label": "setup", "tick": 1},
                                 {"label": "warmup", "tick": 2_001}, sample]}
        else:
            payload = sample | {"slot": fixture, "workers": 1, **windows}
        stem = root / f"{fixture}-{run}-{build}"
        report = json.dumps(payload).encode()
        stem.with_suffix(".json").write_bytes(report)
        stem.with_suffix(".workload.json").write_text(json.dumps({
            "fixture": fixture, "run": run, "build": build, "workers": 1, "cpu": 7,
            "report_sha256": hashlib.sha256(report).hexdigest(),
            "campaign_id": manifest["campaign_id"], "sequence": sequence,
            "start_ns": sequence * 100, "end_ns": sequence * 100 + 50,
            "opening_sha256": pinned["opening_sha256"], "topology_sha256": pinned["topology_sha256"],
            "opening": pinned["opening"], **inputs, **windows,
        }))
        stem.with_suffix(".status").write_text("0")
        stem.with_suffix(".rss-kib").write_text("1000")

    def test_duplicate_reports_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = self.campaign(root, duplicate=True)
            with self.assertRaisesRegex(ValueError, "duplicate"):
                compare(manifest, root)

    def test_execution_must_alternate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = self.campaign(root, swap=True)
            with self.assertRaisesRegex(ValueError, "order"):
                compare(manifest, root)

    def test_exact_throughput_boundary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertTrue(compare(self.campaign(root, ratio=0.97), root)["qualified"])
            self.assertFalse(compare(self.campaign(root, ratio=0.969), root)["qualified"])


if __name__ == "__main__":
    unittest.main()
