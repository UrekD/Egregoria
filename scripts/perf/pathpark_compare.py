#!/usr/bin/env python3
"""Compare three or more alternating PathPark stock performance pairs."""

import argparse
import json
import math
import statistics
import sys
from pathlib import Path

FIXTURES = ("perf_10k", "lesce_osm")


def positive(value, label):
    if isinstance(value, bool) or not isinstance(value, (float, int)):
        raise ValueError(f"{label} must be numeric")
    if not math.isfinite(value) or value <= 0:
        raise ValueError(f"{label} must be finite and positive")
    return value


def score(artifacts, fixture, run, build):
    stem = artifacts / f"{fixture}-{run}-{build}"
    payload = json.loads(stem.with_suffix(".json").read_text())
    if fixture == "perf_10k":
        samples = [row for row in payload[fixture] if row["label"] == "measured"]
        if len(samples) != 1:
            raise ValueError(f"{stem} must have exactly one measured sample")
        sample = samples[0]
    else:
        sample = payload
        if (sample["slot"], sample["workers"]) != (fixture, 1):
            raise ValueError(f"{stem} is not Lesce on one simulation worker")
    extra = sample["extra"]
    return {
        "median_ticks_per_sec": positive(extra["median_ticks_per_sec"], stem),
        "p99_ms_per_tick": positive(extra["p99_ms_per_tick"], stem),
        "wall_ticks_per_sec": positive(sample["ticks_per_sec"], stem),
        "rss_kib": positive(int(stem.with_suffix(".rss-kib").read_text()), stem),
        "activity": positive(extra["active_vehicles"], stem),
        "opening": tuple(sample.get(k) for k in (
            "initial_agents", "initial_population", "warmup_ticks", "measured_ticks",
            "persons_per_agent", "lot_layout",
        )),
        "status": int(stem.with_suffix(".status").read_text()),
    }


def compare(manifest, artifacts):
    pairs = manifest["pairs"]
    if type(pairs) is not int or pairs < 3:
        raise ValueError("at least three complete pairs are required")
    verdict = {}
    failed = []
    for fixture in FIXTURES:
        ceilings = manifest["fixtures"][fixture]
        tail_limit = positive(ceilings["p99_ms_per_tick"], fixture)
        rss_limit = positive(ceilings["rss_kib"], fixture)
        parent = []
        candidate = []
        for pair in range(1, pairs + 1):
            master = score(artifacts, fixture, pair, "parent")
            wave = score(artifacts, fixture, pair, "candidate")
            if master["opening"] != wave["opening"] or master["activity"] != wave["activity"]:
                failed.append(f"{fixture} pair {pair} did not match the recorded workload")
            if wave["p99_ms_per_tick"] > tail_limit or wave["rss_kib"] > rss_limit:
                failed.append(f"{fixture} pair {pair} exceeded a frozen tail or RSS ceiling")
            if fixture == "lesce_osm" and (wave["status"] or master["status"]):
                failed.append(f"{fixture} pair {pair} benchmark failed")
            parent.append(master["median_ticks_per_sec"])
            candidate.append(wave["median_ticks_per_sec"])
        ratio = statistics.median(candidate) / statistics.median(parent)
        verdict[fixture] = {
            "parent_median": statistics.median(parent),
            "candidate_median": statistics.median(candidate),
            "ratio": ratio,
            "pairs": [wave / master for master, wave in zip(parent, candidate)],
        }
        if ratio < 0.97:
            failed.append(f"{fixture} throughput is below 97% of master")
    absolute = score(artifacts, "perf_10k", "absolute", "candidate")
    if absolute["status"] != 0 or absolute["median_ticks_per_sec"] < 150.0:
        failed.append("perf_10k independent absolute gate failed")
    return {"qualified": not failed, "fixtures": verdict, "failures": failed}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    args = parser.parse_args()
    try:
        result = compare(json.loads(args.manifest.read_text()), args.artifacts)
    except (OSError, KeyError, TypeError, ValueError) as error:
        print(f"invalid PathPark benchmark evidence: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2))
    return 0 if result["qualified"] else 1


if __name__ == "__main__":
    sys.exit(main())
