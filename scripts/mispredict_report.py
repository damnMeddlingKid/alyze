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
import re
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


# Path segments that only say *where* the code lives, never what it does.
_NOISE = {
    "alyze", "uax29", "word", "sentence", "properties", "transitions",
    "core", "std", "alloc", "slice", "iter", "ops",
    "wikipedia", "wikipedia_benchmark", "wikipedia_benchmarks", "criterion",
}


def short(name):
    """Best-effort Rust v0 demangle.

    v0 encodes each path segment as <len><ident>, so the identifier must be read
    by *length* — a plain `\\d+(\\w+)` regex swallows the whole rest of the symbol
    because `\\w` matches the digits of the following segment too.
    """
    m = re.search(r"\s*\[(.*?)\]\s*$", name)
    if m:
        name = name[: m.start()]
    if not name.startswith("_R"):
        return name

    segments = []
    for m in re.finditer(r"(\d+)", name):
        n = int(m.group(1))
        cand = name[m.end() : m.end() + n]
        if len(cand) != n or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", cand):
            continue
        # A candidate containing "<digit><letter>" is really several nested
        # segments that happened to fit the length — e.g. a crate hash prefix.
        if re.search(r"\d[A-Za-z]", cand):
            continue
        segments.append(cand)

    for seg in segments:
        if seg not in _NOISE and len(seg) >= 6:
            return seg
    return segments[-1] if segments else name


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

        bfns = (base or {}).get("by_fn", {}).get(remark, {})
        brtotal = sum(bfns.values())

        print(f"\n  {remark} — top frames")
        if base:
            # Share is the number that matters: a lower total with unchanged shares means
            # everything scaled down (or the sample threshold moved), whereas a share that
            # falls while others hold means that call site specifically got better.
            print(
                f"    {'weight':>10} {'share':>7} | {'base':>10} {'share':>7} | "
                f"{'Δshare':>8}  function"
            )
        keys = sorted(set(fns) | set(bfns), key=lambda k: -max(fns.get(k, 0), bfns.get(k, 0)))
        for fn in keys[: args.top]:
            w, b = fns.get(fn, 0.0), bfns.get(fn, 0.0)
            share, bshare = pct(w, rtotal), pct(b, brtotal)
            if base:
                dpp = share - bshare
                mark = "  " if abs(dpp) < 1.0 else ("^^" if dpp > 0 else "vv")
                print(
                    f"    {w:>10,.0f} {share:>6.1f}% | {b:>10,.0f} {bshare:>6.1f}% | "
                    f"{dpp:>+7.1f}pp {mark} {short(fn)[:70]}"
                )
            else:
                print(f"    {w:>10,.0f} {share:>6.1f}%  {short(fn)[:88]}")

    if args.save:
        with open(args.save, "w") as f:
            json.dump(cur, f, indent=1)
        print(f"\nsaved snapshot -> {args.save}")


if __name__ == "__main__":
    main()
