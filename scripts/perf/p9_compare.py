#!/usr/bin/env python3
"""Validate and compare the three P9 background performance pairs.

Manifest version 1 records parent and candidate Git SHAs, their executable
SHA-256 hashes, the pinned Lesce SHA-256, host metadata and ordered run entries.
Each run records the fixture, pair, build, exit status, report SHA-256 and the
opening and window workloads captured by the background adapter. Capture those
workloads with the simulation state so topology, sites, installed services,
employment, parking and traffic demand can be compared across builds.
The traffic demand digest covers homes, workplaces and requested destinations.
"""

import argparse
import hashlib
import json
import math
import re
import statistics
import sys
from pathlib import Path

PARENT = "74dea1e05b0c3a32349b675d4534c454eaf7d70b"
LESCE_SHA256 = "04d3a54d97948898dc09823cdf68e60ef6e0d47fd465f0bf346da1031aeced0e"
LESCE_REVISION = "343dc82a2eaf7e35d8cc5c466ce0240f"
FIXTURES = ("perf_10k", "lesce_osm")
PAIRS = (("parent", "candidate"), ("candidate", "parent"), ("parent", "candidate"))
WORKLOAD = {
    "topology", "sites", "population", "employed", "jobs", "buildings",
    "services", "parked_fleet", "traffic_demand",
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def hex_digest(value, digits):
    return isinstance(value, str) and re.fullmatch(f"[0-9a-f]{{{digits}}}", value)


def metric(value, name):
    require(type(value) in (int, float) and math.isfinite(value) and value > 0,
            f"{name} must be a finite, positive number")
    return value


def validate_workload(workload, label):
    require(WORKLOAD <= workload.keys(), f"{label} is missing workload measurements")
    require(type(workload["tick"]) is int and workload["tick"] >= 0, f"{label} has an invalid tick")
    for key in ("topology", "traffic_demand"):
        require(hex_digest(workload[key], 64), f"{label} has an invalid {key} hash")
    for key in WORKLOAD - {"topology", "traffic_demand"}:
        require(type(workload[key]) is int and workload[key] >= 0, f"{label} has an invalid {key} count")


def read_report(path, expected_hash):
    data = path.read_bytes()
    require(hashlib.sha256(data).hexdigest() == expected_hash, f"{path.name} hash mismatch")
    return json.loads(data)


def measured_sample(report, fixture, opening):
    if fixture == "perf_10k":
        samples = report[fixture]
        warmup = [sample for sample in samples if sample["label"] == "warmup"]
        measured = [sample for sample in samples if sample["label"] == "measured"]
        require(len(measured) == len(warmup) == 1, "perf_10k requires one measured and warmup sample")
        sample = measured[0]
        require(sample["tick"] - warmup[0]["tick"] == 10_000
                and warmup[0]["tick"] - opening["tick"] == 2_000,
                "perf_10k does not use the frozen warmup and measured windows")
        require(sample["extra"]["houses"] == 10_000, "perf_10k requires the frozen housing workload")
        return sample
    require(report["slot"] == fixture and report["warmup_ticks"] == 2_000
            and report["measured_ticks"] == report["completed_steps"] == 10_000
            and report["workers"] == 1, "Lesce benchmark options differ from the gate")
    require(report["initial_agents"] == report["initial_population"] == 2_903
            and report["persons_per_agent"] == 1 and report["setup"]["tick"] == 1048,
            "Lesce opening snapshot workload differs from the gate")
    return report


def timings(sample, fixture):
    extra = sample["extra"]
    return {
        "median_ticks_per_sec": metric(extra["median_ticks_per_sec"], "median_ticks_per_sec"),
        "ticks_per_sec": metric(sample["ticks_per_sec"], "ticks_per_sec"),
        "p50_ms_per_tick": metric(
            sample["p50_ms_per_tick"] if fixture == "lesce_osm"
            else 1000 / metric(extra["median_ticks_per_sec"], "median_ticks_per_sec"), "p50"),
        "p95_ms_per_tick": metric(
            sample["p95_ms_per_tick"] if fixture == "lesce_osm" else extra["p95_ms_per_tick"], "p95"),
        "p99_ms_per_tick": metric(extra["p99_ms_per_tick"], "p99"),
        "max_ms_per_tick": metric(sample["max_ms_per_tick"] if fixture == "lesce_osm" else extra["max_ms_per_tick"], "max"),
    }


def compare(manifest, artifacts):
    require(manifest["version"] == 1 and manifest["profile"] == "background",
            "expected a version 1 background manifest")
    require(manifest["parent_sha"] == PARENT and hex_digest(manifest["candidate_sha"], 40)
            and manifest["candidate_sha"] != PARENT, "invalid parent or candidate source pin")
    require(manifest["lesce_sha256"] == LESCE_SHA256
            and manifest["lesce_revision"] == LESCE_REVISION, "invalid Lesce source pin")
    for build in ("parent", "candidate"):
        require(hex_digest(manifest["binary_sha256"][build], 64), f"invalid {build} executable hash")
    for name in ("cpu", "cpuset", "os", "governor", "compiler", "cargo_profile", "scheduler", "flags"):
        require(name in manifest["host"], f"missing host metadata {name}")

    expected = [(fixture, pair, build)
                for pair, order in enumerate(PAIRS, 1)
                for build in order
                for fixture in FIXTURES]
    runs = manifest["runs"]
    require([(run["fixture"], run["pair"], run["build"]) for run in runs] == expected,
            "run list must contain three complete pairs in A/B, B/A, A/B order")
    opening = {}
    windows = {}
    samples = {}
    for run, key in zip(runs, expected):
        fixture, pair, build = key
        require(type(run["exit_status"]) is int and run["exit_status"] == 0, f"{key} did not exit successfully")
        require(run["binary_sha256"] == manifest["binary_sha256"][build], f"{key} executable hash mismatch")
        start = run["opening"]
        checks = run["windows"]
        validate_workload(start, f"{key} opening")
        require(isinstance(checks, list) and checks, f"{key} is missing window measurements")
        previous_tick = start["tick"]
        for window in checks:
            validate_workload(window, f"{key} window")
            require(window["tick"] > previous_tick, f"{key} window ticks are not increasing")
            previous_tick = window["tick"]
        require(start == opening.setdefault(fixture, start), f"{key} opening workload mismatch")
        require(checks == windows.setdefault(fixture, checks), f"{key} window workload mismatch")
        if fixture == "lesce_osm":
            require(start["tick"] == 1048 and start["population"] == start["employed"] == 2_903,
                    "Lesce must open with 2,903 employed residents")
        report = read_report(artifacts / f"{fixture}-{pair}-{build}.json", run["report_sha256"])
        samples[key] = timings(measured_sample(report, fixture, start), fixture)

    result = {}
    for fixture in FIXTURES:
        def ratios(metric_name):
            return [samples[fixture, pair, "candidate"][metric_name] /
                    samples[fixture, pair, "parent"][metric_name] for pair in range(1, 4)]

        tps = statistics.median(ratios("median_ticks_per_sec"))
        p99 = statistics.median(ratios("p99_ms_per_tick"))
        minimum = min(samples[key]["median_ticks_per_sec"] for key in expected if key[0] == fixture)
        result[fixture] = {
            "median_throughput_ratio": tps,
            "median_p99_ratio": p99,
            "runs": [samples[key] for key in expected if key[0] == fixture],
            "passed": tps >= 0.97 and (fixture != "perf_10k" or (minimum >= 150 and p99 <= 1.10)),
        }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--artifacts", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = compare(json.loads(args.manifest.read_text(encoding="utf-8")), args.artifacts)
    except (OSError, ValueError, TypeError, KeyError, AttributeError, OverflowError) as error:
        print(f"P9 performance comparison failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2, allow_nan=False))
    return 0 if all(fixture["passed"] for fixture in result.values()) else 1


if __name__ == "__main__":
    sys.exit(main())
