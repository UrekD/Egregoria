#!/usr/bin/env python3
"""Measure an alternating PathPark campaign using two instrumented release runtimes."""

import argparse
import hashlib
import json
import math
import os
import secrets
import subprocess
import sys
import time
from pathlib import Path

from pathpark_compare import CORE_WORK, FIXTURES, OTHER_WORK, compare, metric_count, run_order

LESCE_SOURCE = "ba6bf6be50435cd00402f024f60adfafef6dbf8851ca5768c1beb322612150db"


def read(path):
    return json.loads(path.read_text())


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def canonical_digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def tree_digest(root):
    files = sorted(path for path in root.rglob("*") if path.is_file())
    if not files:
        raise ValueError(f"empty runtime input: {root}")
    return canonical_digest([(str(path.relative_to(root)), sha256(path)) for path in files])


def inputs(runtime, slots):
    report_file = slots / "lesce_osm.import-report.json"
    report = read(report_file)
    if (report["source"]["sha256"] != LESCE_SOURCE
            or not report["headless"]["freight_completed"] or not report["opening"]):
        raise ValueError(f"{slots} is missing the checked, pinned Lesce opening")
    revision = report["revision"]
    sidecar = slots / f"lesce_osm.{revision}.import-report.json"
    if sidecar.read_bytes() != report_file.read_bytes():
        raise ValueError(f"{slots} report sidecars disagree")
    return {
        "runner_sha256": sha256(runtime / "scenarios"),
        "assets_sha256": tree_digest(runtime / "assets"),
        "prototypes_sha256": tree_digest(runtime / "base_mod"),
        "slot_sha256": sha256(slots / "lesce_osm.zip"),
        "sidecar_sha256": sha256(sidecar),
        "slot_revision": int(revision, 16),
    }


def opening_identity(raw):
    return {
        "opening_sha256": canonical_digest([raw["cohort"], raw["assignments_after_warmup"]]),
        "topology_sha256": canonical_digest(raw["topology"]),
        "opening": raw["counts"],
    }


def run(args, fixture, run_id, build, pins, campaign_id, sequence):
    runtime = getattr(args, f"{build}_runtime")
    slots = getattr(args, f"{build}_slots")
    warmup, ticks = (2_000, 10_000) if fixture == "perf_10k" else (args.warmup, args.ticks)
    if inputs(runtime, slots) != pins[build]:
        raise ValueError(f"{build} runtime inputs changed after pinning")
    stage = "practice-" if sequence is None else ""
    stem = args.artifacts / f"{stage}{fixture}-{run_id}-{build}"
    report = stem.with_suffix(".json")
    opening = stem.with_suffix(".opening.json")
    env = os.environ | {"RAYON_NUM_THREADS": "1", "EGR_PATHPARK_OPENING": str(opening)}
    command = ["/usr/bin/time", "-f", "%M", "-o", str(stem.with_suffix(".rss-kib")),
               str(runtime / "scenarios")]
    if fixture == "perf_10k":
        command += [fixture, "--json", str(report)]
        if run_id == "absolute":
            command.append("--gate")
    else:
        command += ["bench-slot", "--root", str(slots), "--slot", fixture,
                    "--warmup", str(warmup), "--ticks", str(ticks), "--sim-threads", "1"]
    print(f"{stem.name}", flush=True)
    start = time.monotonic_ns()
    output = stem.with_suffix(".stdout.log") if fixture == "perf_10k" else report
    with output.open("wb") as stdout, stem.with_suffix(".stderr.log").open("wb") as stderr:
        status = subprocess.run(command, cwd=runtime, env=env, stdout=stdout, stderr=stderr).returncode
    end = time.monotonic_ns()
    stem.with_suffix(".status").write_text(f"{status}\n")
    if status:
        raise ValueError(f"{stem} failed with status {status}")
    after = inputs(runtime, slots)
    if after != pins[build]:
        raise ValueError(f"{build} runtime inputs changed during measurement")
    measured_opening = read(opening)
    if (measured_opening["workers"], measured_opening["warmup_ticks"],
            measured_opening["measured_ticks"]) != (1, warmup, ticks):
        raise ValueError(f"{stem} used different measurement settings")
    if fixture == "lesce_osm" and int(measured_opening["slot_revision"], 16) != after["slot_revision"]:
        raise ValueError(f"{stem} loaded a different slot revision")
    workload = opening_identity(measured_opening) | {
        "fixture": fixture, "run": run_id, "build": build, "workers": 1, "cpu": args.cpu,
        "campaign_id": campaign_id, "sequence": sequence, "start_ns": start, "end_ns": end,
        "warmup_ticks": warmup, "measured_ticks": ticks, "report_sha256": sha256(report),
    } | after
    write(stem.with_suffix(".workload.json"), workload)
    payload = read(report)
    sample = next(row for row in payload[fixture] if row["label"] == "measured") \
        if fixture == "perf_10k" else payload
    return workload, sample["extra"], int(stem.with_suffix(".rss-kib").read_text())


def freeze(args, pins, campaign_id):
    fixtures = {}
    for fixture in FIXTURES:
        practices = [run(args, fixture, num, "parent", pins, campaign_id, None)
                     for num in range(1, 4)]
        opening = {key: practices[0][0][key] for key in
                   ("opening", "opening_sha256", "topology_sha256", "warmup_ticks", "measured_ticks")}
        for run_opening, _, _ in practices:
            if any(run_opening[key] != value for key, value in opening.items()):
                raise ValueError(f"{fixture} parent practice openings differ")
        # Both stock scenarios lack bus routes. Lesce has an exercised freight terminal.
        active = set(CORE_WORK)
        if fixture == "lesce_osm":
            active.update(("offered_freight_work", "completed_freight_work"))
        floors = {}
        for key in CORE_WORK + OTHER_WORK:
            samples = [metric_count(extra[key], key) for _, extra, _ in practices]
            if key in active:
                if min(samples) == 0:
                    raise ValueError(f"{fixture} parent did not exercise {key}")
                floors[key] = math.ceil(min(samples) * 0.98)
            else:
                if any(samples):
                    raise ValueError(f"{fixture} parent unexpectedly exercised {key}")
                floors[key] = 0
        tail = max(extra["p99_ms_per_tick"] for _, extra, _ in practices) * 1.03
        if fixture == "perf_10k" and args.paris_p99_ceiling is not None:
            tail = min(tail, args.paris_p99_ceiling)
        fixtures[fixture] = opening | {
            "inputs": pins, "workload_floors": floors, "p99_ms_per_tick": tail,
            "rss_kib": math.ceil(max(rss for _, _, rss in practices) * 1.03),
        }
    return {"campaign_id": campaign_id, "pairs": args.pairs, "cpu": args.cpu, "fixtures": fixtures}


def campaign(args):
    if args.pairs < 3 or args.warmup < 1 or args.ticks < 1:
        raise ValueError("at least three pairs and positive Lesce windows are required")
    os.sched_setaffinity(0, {args.cpu})
    if os.sched_getaffinity(0) != {args.cpu}:
        raise ValueError("could not pin the campaign to a single CPU")
    pins = {build: inputs(getattr(args, f"{build}_runtime"), getattr(args, f"{build}_slots"))
            for build in ("parent", "candidate")}
    if pins["parent"]["assets_sha256"] != pins["candidate"]["assets_sha256"]:
        raise ValueError("both release runtimes must use the same assets")
    if pins["parent"]["runner_sha256"] == pins["candidate"]["runner_sha256"]:
        raise ValueError("parent and candidate require separate release runners")
    args.artifacts.mkdir()
    campaign_id = secrets.token_hex(32)
    manifest = freeze(args, pins, campaign_id)
    write(args.artifacts / "manifest.json", manifest)
    for sequence, (fixture, pair, build) in enumerate(run_order(args.pairs), 1):
        run(args, fixture, pair, build, pins, campaign_id, sequence)
    verdict = compare(manifest, args.artifacts)
    write(args.artifacts / "verdict.json", verdict)
    return 0 if verdict["qualified"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for build in ("parent", "candidate"):
        parser.add_argument(f"--{build}-runtime", required=True, type=lambda path: Path(path).resolve(),
                            help="directory containing the release scenarios binary, assets and base_mod")
        parser.add_argument(f"--{build}-slots", required=True, type=lambda path: Path(path).resolve(),
                            help="directory containing a checked slot commissioned by that build")
    parser.add_argument("--artifacts", required=True, type=lambda path: Path(path).resolve())
    parser.add_argument("--cpu", required=True, type=int)
    parser.add_argument("--pairs", type=int, default=3)
    parser.add_argument("--warmup", type=int, default=2_000)
    parser.add_argument("--ticks", type=int, default=24_000)
    parser.add_argument("--paris-p99-ceiling", type=float)
    try:
        return campaign(parser.parse_args())
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"PathPark campaign failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
