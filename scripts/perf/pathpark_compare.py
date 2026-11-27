#!/usr/bin/env python3
"""Compare three or more alternating PathPark stock performance pairs."""

import argparse
import hashlib
import json
import math
import statistics
import sys
from pathlib import Path

FIXTURES = ("perf_10k", "lesce_osm")
OPENING_COUNTS = ("population", "personal_cars", "curb_occupied", "assigned_work")
CORE_WORK = ("active_personal_vehicle_ticks", "offered_person_trips", "completed_person_trips")
OTHER_WORK = ("offered_freight_work", "completed_freight_work",
              "offered_service_work", "completed_service_work")


def positive(value, label):
    if isinstance(value, bool) or not isinstance(value, (float, int)):
        raise ValueError(f"{label} must be numeric")
    if not math.isfinite(value) or value <= 0:
        raise ValueError(f"{label} must be finite and positive")
    return value


def count(value, label):
    if type(value) is not int or value < 0:
        raise ValueError(f"{label} must be a nonnegative integer")
    return value


def metric_count(value, label):
    if isinstance(value, float) and value.is_integer():
        value = int(value)
    return count(value, label)


def digest(value, label):
    if (not isinstance(value, str) or len(value) != 64
            or any(char not in "0123456789abcdef" for char in value)):
        raise ValueError(f"{label} must be a SHA-256 digest")
    return value


def run_order(pairs):
    for fixture in FIXTURES:
        for pair in range(1, pairs + 1):
            builds = ("parent", "candidate") if pair % 2 else ("candidate", "parent")
            for build in builds:
                yield fixture, pair, build
    yield "perf_10k", "absolute", "candidate"


def score(artifacts, fixture, run, build, pinned, cpu, identity):
    stem = artifacts / f"{fixture}-{run}-{build}"
    if int(stem.with_suffix(".status").read_text()) != 0:
        raise ValueError(f"{stem} benchmark failed")
    payload = json.loads(stem.with_suffix(".json").read_text())
    if (identity["fixture"], identity["run"], identity["build"]) != (fixture, run, build):
        raise ValueError(f"{stem} has the wrong run identity")
    if (count(identity["workers"], "workers"), count(identity["cpu"], "cpu")) != (1, cpu):
        raise ValueError(f"{stem} must use the pinned CPU and one simulation worker")
    inputs = pinned["inputs"][build]
    keys = ["runner_sha256", "assets_sha256", "prototypes_sha256"]
    if fixture == "lesce_osm":
        keys.extend(("slot_sha256", "sidecar_sha256"))
        if count(identity["slot_revision"], "slot_revision") != count(inputs["slot_revision"], "slot_revision"):
            raise ValueError(f"{stem} has the wrong slot revision")
    for key in keys:
        if identity[key] != digest(inputs[key], key):
            raise ValueError(f"{stem} has the wrong {key}")
    # Tie the run sidecar to the exact timing report it describes.
    if identity["report_sha256"] != hashlib.sha256(stem.with_suffix(".json").read_bytes()).hexdigest():
        raise ValueError(f"{stem} workload sidecar belongs to another timing report")
    for key in ("opening_sha256", "topology_sha256"):
        if identity[key] != digest(pinned[key], key):
            raise ValueError(f"{stem} has the wrong {key}")
    for key in OPENING_COUNTS:
        expected = count(pinned["opening"][key], key)
        if expected == 0 or count(identity["opening"][key], key) != expected:
            raise ValueError(f"{stem} has the wrong opening {key}")
    for key in ("warmup_ticks", "measured_ticks"):
        expected = count(pinned[key], key)
        if expected == 0 or count(identity[key], key) != expected:
            raise ValueError(f"{stem} has the wrong {key}")
    if fixture == "perf_10k":
        def phase(label):
            samples = [row for row in payload[fixture] if row["label"] == label]
            if len(samples) != 1:
                raise ValueError(f"{stem} must have exactly one {label} sample")
            return samples[0]

        setup, warmup, sample = (phase(label) for label in ("setup", "warmup", "measured"))
        if (warmup["tick"] - setup["tick"], sample["tick"] - warmup["tick"]) != (2_000, 10_000):
            raise ValueError(f"{stem} did not run the stock Paris windows")
        if (identity["warmup_ticks"], identity["measured_ticks"]) != (2_000, 10_000):
            raise ValueError(f"{stem} has the wrong Paris workload windows")
    else:
        sample = payload
        if (sample["slot"], sample["workers"]) != (fixture, 1):
            raise ValueError(f"{stem} is not Lesce on one simulation worker")
        if any(sample[key] != identity[key] for key in ("warmup_ticks", "measured_ticks")):
            raise ValueError(f"{stem} timing report disagrees with its workload windows")

    extra = sample["extra"]
    workload = {}
    for key in CORE_WORK + OTHER_WORK:
        floor = count(pinned["workload_floors"][key], key)
        actual = metric_count(extra[key], key)
        if (key in CORE_WORK and floor == 0) or actual < floor or (floor == 0 and actual != 0):
            raise ValueError(f"{stem} violates the frozen workload floor for {key}")
        workload[key] = actual
    return {
        "median_ticks_per_sec": positive(extra["median_ticks_per_sec"], stem),
        "p99_ms_per_tick": positive(extra["p99_ms_per_tick"], stem),
        "wall_ticks_per_sec": positive(sample["ticks_per_sec"], stem),
        "rss_kib": positive(int(stem.with_suffix(".rss-kib").read_text()), stem),
        "workload": workload,
    }


def equivalent_work(parent, candidate):
    return all(abs(candidate[key] - baseline) * 100 <= baseline * 2
               for key, baseline in parent.items())


def compare(manifest, artifacts):
    pairs = manifest["pairs"]
    if type(pairs) is not int or pairs < 3:
        raise ValueError("at least three complete pairs are required")
    cpu = count(manifest["cpu"], "cpu")
    campaign = digest(manifest["campaign_id"], "campaign_id")
    previous_end = 0
    measurements = {}
    timing_signatures = {fixture: set() for fixture in FIXTURES}
    for sequence, key in enumerate(run_order(pairs), 1):
        fixture, run, build = key
        stem = artifacts / f"{fixture}-{run}-{build}"
        identity = json.loads(stem.with_suffix(".workload.json").read_text())
        if identity["campaign_id"] != campaign:
            raise ValueError(f"{stem} belongs to another campaign")
        start = count(identity["start_ns"], "start_ns")
        end = count(identity["end_ns"], "end_ns")
        if count(identity["sequence"], "sequence") != sequence or not previous_end <= start < end:
            raise ValueError(f"{stem} is out of execution order")
        previous_end = end
        result = score(artifacts, fixture, run, build, manifest["fixtures"][fixture], cpu, identity)
        timing = tuple(result[key] for key in
                       ("median_ticks_per_sec", "p99_ms_per_tick", "wall_ticks_per_sec"))
        if timing in timing_signatures[fixture]:
            raise ValueError(f"{stem} has duplicate timings")
        timing_signatures[fixture].add(timing)
        measurements[key] = result
    verdict = {}
    failed = []
    for fixture in FIXTURES:
        ceilings = manifest["fixtures"][fixture]
        inputs = ceilings["inputs"]
        if inputs["parent"]["assets_sha256"] != inputs["candidate"]["assets_sha256"]:
            raise ValueError(f"{fixture} inputs do not pin a shared assets tree")
        tail_limit = positive(ceilings["p99_ms_per_tick"], fixture)
        rss_limit = positive(ceilings["rss_kib"], fixture)
        parent = []
        candidate = []
        for pair in range(1, pairs + 1):
            master = measurements[fixture, pair, "parent"]
            wave = measurements[fixture, pair, "candidate"]
            if not equivalent_work(master["workload"], wave["workload"]):
                failed.append(f"{fixture} pair {pair} did not match the recorded workload")
            if wave["p99_ms_per_tick"] > tail_limit or wave["rss_kib"] > rss_limit:
                failed.append(f"{fixture} pair {pair} exceeded a frozen tail or RSS ceiling")
            parent.append(master["median_ticks_per_sec"])
            candidate.append(wave["median_ticks_per_sec"])
            if fixture == "perf_10k" and pair == 1:
                absolute = measurements[fixture, "absolute", "candidate"]
                if (absolute["median_ticks_per_sec"] < 150.0
                        or absolute["p99_ms_per_tick"] > tail_limit
                        or absolute["rss_kib"] > rss_limit
                        or not equivalent_work(master["workload"], absolute["workload"])):
                    failed.append("perf_10k independent absolute gate failed")
            verdict.setdefault(fixture, {}).setdefault("wall_ticks_per_sec", []).append({
                "parent": master["wall_ticks_per_sec"], "candidate": wave["wall_ticks_per_sec"],
            })
        ratio = statistics.median(candidate) / statistics.median(parent)
        verdict[fixture].update({
            "parent_median": statistics.median(parent),
            "candidate_median": statistics.median(candidate),
            "ratio": ratio,
            "pairs": [wave / master for master, wave in zip(parent, candidate)],
        })
        if ratio < 0.97:
            failed.append(f"{fixture} throughput is below 97% of master")
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
