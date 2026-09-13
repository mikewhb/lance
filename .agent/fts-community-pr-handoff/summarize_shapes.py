#!/usr/bin/env python3
"""Time-weighted per-shape summary for the SBG 943-query set.

usage: summarize_shapes.py <base_root> <base_tag> <cand_root> <cand_tag> <reps>
  roots contain <tag>-r<N>/results.json ; engine name is derived from the tag.
Prints median-over-reps of the time-weighted sum per shape (and per query count),
plus the delta. Count mismatches are reported.
"""

import json
import os
import statistics
import sys

SHAPES = [
    "union",
    "phrase",
    "intersection",
    "intersection_union",
    "term",
    "two-phase-critic",
]


def load(root, tag, rep):
    path = f"{root}/{tag}-r{rep}/results.json"
    if not os.path.exists(path):
        return None
    data = json.load(open(path))
    ((engine, rows),) = [(k, v) for k, v in data["results"]["TOP_10"].items()]
    return {
        r["query"]: (min(r["duration"]), r["count"], r.get("tags", [])) for r in rows
    }


def shape_of(tags):
    for s in SHAPES:
        if s in tags:
            return s
    return "other"


def rep_sum(d, shape):
    if shape == "all":
        return sum(v[0] for v in d.values())
    return sum(v[0] for v in d.values() if shape_of(v[2]) == shape)


def rep_n(d, shape):
    if shape == "all":
        return len(d)
    return sum(1 for v in d.values() if shape_of(v[2]) == shape)


def collect(root, tag, reps):
    out = {}
    for r in range(1, reps + 1):
        d = load(root, tag, r)
        if d is not None:
            out[r] = d
    return out


def main():
    base_root, base_tag, cand_root, cand_tag, reps = sys.argv[1:6]
    reps = int(reps)
    B = collect(base_root, base_tag, reps)
    C = collect(cand_root, cand_tag, reps)
    if not B:
        print("no baseline data")
        return
    shape = "all"
    shapes = SHAPES + ["all"]
    header = f"{'shape':>18} {'n':>4} "
    header += f"{'base(us)':>12} "
    if C:
        header += f"{'cand(us)':>12} {'delta':>9}"
    print(header)
    print("-" * len(header))
    for shape in shapes:
        base_vals = [rep_sum(B[r], shape) for r in B]
        n = rep_n(B[min(B)], shape)
        if n == 0:
            continue
        line = f"{shape:>18} {n:>4} {statistics.median(base_vals):>12.0f} "
        if C:
            cand_vals = [rep_sum(C[r], shape) for r in C]
            b, c = statistics.median(base_vals), statistics.median(cand_vals)
            line += f"{c:>12.0f} {(c - b) / b * 100:>+8.1f}%"
        print(line)
    # per-rep raw for stability
    print("\nper-rep time-weighted sums (us):")
    for shape in shapes:
        base_vals = [round(rep_sum(B[r], shape)) for r in B]
        line = f"  {shape:>18} base={base_vals}"
        if C:
            line += f" cand={[round(rep_sum(C[r], shape)) for r in C]}"
        print(line)
    # counts
    if C:
        bad = 0
        for r in C:
            for r2 in B:
                for q, v in C[r].items():
                    if q in B[r2] and v[1] != B[r2][q][1]:
                        bad += 1
        print(f"\ncount mismatches (cand vs base): {bad}")


if __name__ == "__main__":
    main()
