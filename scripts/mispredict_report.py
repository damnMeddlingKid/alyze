#!/usr/bin/env python3
"""Aggregate CPU-Counters bottleneck samples by remark and by function.

Reads the `CountingModeSamples` table exported from an Instruments CPU Counters
trace. Each row is one sampled bottleneck event carrying a remark name, a
backtrace, and a weight; summing weight per remark gives the mispredict /
front-end / dependency split, and summing per leaf frame says where.

Usage:
    mispredict_report.py <samples.xml> [--only SUBSTR] [--save out.json]
                                       [--baseline prev.json] [--top N]

`--only` keeps samples whose backtrace mentions SUBSTR (default: alyze), which
drops dyld / libobjc / malloc startup noise.
"""
import argparse
import json
import sys
import xml.etree.ElementTree as ET
from collections import defaultdict

# CountingModeSamples column order
TIME, PROCESS, THREAD, REMARK, BACKTRACE, WEIGHT = range(6)


def parse(path, only):
    ids = {}
    by_remark = defaultdict(float)
    by_fn = defaultdict(lambda: defaultdict(float))
    kept = dropped = 0

    for _, row in ET.iterparse(path, events=("end",)):
        if row.tag != "row":
            continue

        vals, frames = [], []
        for node in row:
            ref = node.get("ref")
            if ref is not None:
                vals.append(ids.get(ref))
                if node.tag == "tagged-backtrace":
                    frames = ids.get("frames:" + ref, [])
                continue
            if node.tag == "tagged-backtrace":
                frames = [f.get("name") or "?" for f in node.iter("frame")]
                bins = [
                    (f.find("binary").get("name") if f.find("binary") is not None else "")
                    for f in node.iter("frame")
                ]
                frames = [f"{n} [{b}]" for n, b in zip(frames, bins)]
                v = node.get("fmt")
            else:
                v = node.get("fmt") or node.text
            i = node.get("id")
            if i is not None:
                ids[i] = v
                if node.tag == "tagged-backtrace":
                    ids["frames:" + i] = frames
            vals.append(v)
        row.clear()

        if len(vals) <= WEIGHT or vals[REMARK] is None:
            continue
        try:
            weight = float(vals[WEIGHT])
        except (TypeError, ValueError):
            weight = 1.0

        if only and not any(only in f for f in frames):
            dropped += 1
            continue
        kept += 1

        remark = vals[REMARK]
        by_remark[remark] += weight
        if frames:
            by_fn[remark][frames[0]] += weight

    return {
        "by_remark": dict(by_remark),
        "by_fn": {k: dict(v) for k, v in by_fn.items()},
        "kept": kept,
        "dropped": dropped,
    }


def pct(part, whole):
    return (100.0 * part / whole) if whole else 0.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("samples")
    ap.add_argument("--only", default="alyze")
    ap.add_argument("--save")
    ap.add_argument("--baseline")
    ap.add_argument("--top", type=int, default=10)
    args = ap.parse_args()

    cur = parse(args.samples, args.only)
    total = sum(cur["by_remark"].values()) or 1.0

    base = None
    if args.baseline:
        with open(args.baseline) as f:
            base = json.load(f)
    base_total = sum(base["by_remark"].values()) if base else 0.0

    print(f"samples kept {cur['kept']}  (dropped {cur['dropped']} outside '{args.only}')\n")
    print(f"{'remark':<46}{'weight':>14}{'share':>8}" + ("        vs baseline" if base else ""))
    print("-" * (68 + (19 if base else 0)))
    for remark, w in sorted(cur["by_remark"].items(), key=lambda kv: -kv[1]):
        line = f"{remark[:45]:<46}{w:>14,.0f}{pct(w, total):>7.1f}%"
        if base:
            b = base["by_remark"].get(remark, 0.0)
            delta = pct(w - b, b) if b else float("inf")
            arrow = "→" if abs(delta) < 2 else ("↑" if delta > 0 else "↓")
            line += f"   {b:>12,.0f} {arrow}{delta:+7.1f}%" if b else f"   {'(new)':>12}"
        print(line)
    print("-" * (68 + (19 if base else 0)))
    line = f"{'TOTAL':<46}{total:>14,.0f}"
    if base:
        d = pct(total - base_total, base_total) if base_total else 0.0
        line += f"{'':>8}   {base_total:>12,.0f} {d:+7.1f}%"
    print(line)

    for remark, fns in sorted(cur["by_fn"].items(), key=lambda kv: -sum(kv[1].values())):
        rtotal = sum(fns.values())
        if rtotal < total * 0.02:
            continue
        print(f"\n  {remark} — top frames")
        for fn, w in sorted(fns.items(), key=lambda kv: -kv[1])[: args.top]:
            print(f"    {w:>12,.0f} {pct(w, rtotal):>6.1f}%  {fn[:96]}")

    if args.save:
        with open(args.save, "w") as f:
            json.dump(cur, f, indent=1)
        print(f"\nsaved snapshot -> {args.save}")


if __name__ == "__main__":
    main()
