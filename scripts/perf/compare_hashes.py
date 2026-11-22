#!/usr/bin/env python3
"""Compare two ordered JSONL checkpoint dumps from scenarios --hashes."""

import argparse
import json
import sys
from itertools import zip_longest


def checkpoint(row):
    return f"scenario={row['scenario']} run={row['run']} tick={row['tick']}"


def compare(left, right, projection_only=False):
    count = 0
    for left_line, right_line in zip_longest(left, right):
        count += 1
        a = json.loads(left_line) if left_line is not None else None
        b = json.loads(right_line) if right_line is not None else None
        if left_line is None:
            return f"{checkpoint(b)} resource=<checkpoint> is missing from the first dump"
        if right_line is None:
            return f"{checkpoint(a)} resource=<checkpoint> is missing from the second dump"
        if checkpoint(a) != checkpoint(b):
            return f"{checkpoint(a)} resource=<checkpoint> differs from {checkpoint(b)}"

        if projection_only:
            old, new = a["projection"], b["projection"]
            if any(type(value) is not int or not 0 <= value < 2**64 for value in (old, new)):
                raise ValueError(f"{checkpoint(a)} has an invalid u64 projection")
            if old != new:
                return f"{checkpoint(a)} projection: {old} != {new}"
            continue

        left_hashes, right_hashes = a["hashes"], b["hashes"]
        if left_hashes != right_hashes:
            for resource in sorted(left_hashes.keys() | right_hashes.keys()):
                old = left_hashes.get(resource, "<missing>")
                new = right_hashes.get(resource, "<missing>")
                if old != new:
                    return f"{checkpoint(a)} resource={resource}: {old} != {new}"

    matched = "projections" if projection_only else "hash dumps"
    print(f"{matched} match ({count} checkpoint lines)")
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--projection-only", action="store_true", help="compare gameplay projections only"
    )
    parser.add_argument("left")
    parser.add_argument("right")
    args = parser.parse_args()
    try:
        with open(args.left, encoding="utf-8") as left, open(args.right, encoding="utf-8") as right:
            difference = compare(left, right, args.projection_only)
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        print(f"could not compare hash dumps: {error}", file=sys.stderr)
        return 1
    if difference is not None:
        print(f"first difference: {difference}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
