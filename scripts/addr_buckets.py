#!/usr/bin/env python3
"""Attribute Instruments samples to address ranges inside one function.

Symbol-level attribution is useless once a function is `#[inline]`d — everything
lands on the caller. This buckets samples by the sampled program counter instead,
so a loop inside an inlined function can be measured directly.

    # 1. find the ranges (loops are detected from backward branches)
    scripts/addr_buckets.py ranges --binary <exe> --symbol tokenize_windowed

    # 2. attribute samples to them
    scripts/addr_buckets.py report --binary <exe> --symbol tokenize_windowed \\
        --samples samples.xml

Sampled frames carry a runtime address plus the binary's load address; objdump
reports static vmaddrs. `static = runtime - load_addr + text_base`, where
text_base is taken from the disassembly of the symbol itself.
"""
import argparse
import re
import subprocess
import sys
import xml.etree.ElementTree as ET
from collections import defaultdict


def objdump(binary, symbol):
    """-> (full_symbol, [(addr, mnemonic, operands)])"""
    syms = subprocess.run(
        ["nm", binary], capture_output=True, text=True
    ).stdout.splitlines()
    matches = [l.split()[-1] for l in syms if symbol in l]
    if not matches:
        sys.exit(f"no symbol matching {symbol!r} in {binary}")
    if len(matches) > 1:
        print(f"note: {len(matches)} matching symbols, using the last:", file=sys.stderr)
        for m in matches:
            print(f"   {m}", file=sys.stderr)
    sym = matches[-1]

    tool = subprocess.run(
        ["xcrun", "--find", "llvm-objdump"], capture_output=True, text=True
    ).stdout.strip()
    out = subprocess.run(
        [tool, "-d", f"--disassemble-symbols={sym}", "--no-show-raw-insn", binary],
        capture_output=True,
        text=True,
    ).stdout

    insns = []
    for line in out.splitlines():
        m = re.match(r"\s*([0-9a-f]+):\s+(\S+)\s*(.*)", line)
        if m:
            insns.append((int(m.group(1), 16), m.group(2), m.group(3).strip()))
    if not insns:
        sys.exit(f"could not disassemble {sym}")
    return sym, insns


def find_loops(insns):
    """Innermost loops, as (start_addr, end_addr), from backward branches."""
    addr2i = {a: i for i, (a, _, _) in enumerate(insns)}
    edges = []
    for i, (a, op, args) in enumerate(insns):
        if not re.match(r"^b(\.\w+)?$|^cbn?z$|^tbn?z$", op):
            continue
        m = re.search(r"0x([0-9a-f]+)", args)
        if not m:
            continue
        t = int(m.group(1), 16)
        if t < a and t in addr2i:
            edges.append((addr2i[t], i))
    inner = [
        (s, e)
        for s, e in set(edges)
        if not any(s2 > s and e2 < e for s2, e2 in edges)
    ]
    return sorted((insns[s][0], insns[e][0]) for s, e in inner)


def describe(insns, lo, hi):
    body = [x for x in insns if lo <= x[0] <= hi]
    ops = {x[1] for x in body}
    tags = []
    if "bl" in ops:
        tags.append("calls")
    if ops & {"rbit", "clz"}:
        tags.append("trailing_zeros")
    if any(o.startswith("ldrb") or o.startswith("ldrsb") for o in ops):
        tags.append("byte loads")
    if any(o in ("stp", "ldp") for o in ops):
        tags.append("save/restore")
    return ", ".join(tags) or "-"


def parse_samples(path, binary_name):
    """-> [(runtime_addr, load_addr, weight)] for leaf frames in `binary_name`.

    Every value in the export is interned: it appears once carrying `id="N"` and
    thereafter as `<tag ref="N"/>`. That applies to `<binary>` elements too, so a
    frame's binary is usually a bare ref with no name attribute — resolving it is
    what makes the leaf identifiable at all.
    """
    root = ET.parse(path).getroot()
    byid = {el.get("id"): el for el in root.iter() if el.get("id") is not None}

    def deref(el):
        ref = el.get("ref")
        return byid.get(ref, el) if ref is not None else el

    out = []
    for row in root.iter("row"):
        weight, leaf = 1.0, None
        for child in row:
            node = deref(child)
            if node.tag == "weight":
                try:
                    weight = float(node.text)  # nanoseconds
                except (TypeError, ValueError):
                    pass
            elif node.tag == "tagged-backtrace":
                bt = node.find("backtrace")
                frames = list(bt) if bt is not None else []
                if not frames:
                    continue
                frame = deref(frames[0])  # leaf
                b = frame.find("binary")
                if b is None:
                    continue
                b = deref(b)
                if binary_name in (b.get("name") or ""):
                    leaf = (int(frame.get("addr"), 16), int(b.get("load-addr"), 16))
        if leaf:
            out.append((leaf[0], leaf[1], weight))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["ranges", "report"])
    ap.add_argument("--binary", required=True)
    ap.add_argument("--symbol", required=True)
    ap.add_argument("--samples")
    ap.add_argument(
        "--binary-name",
        help="binary name as it appears in the trace's frames; defaults to the "
        "basename of --binary, which is wrong if you disassemble a renamed copy",
    )
    ap.add_argument("--text-base", type=lambda s: int(s, 0), default=0x100000000)
    args = ap.parse_args()

    sym, insns = objdump(args.binary, args.symbol)
    fn_lo, fn_hi = insns[0][0], insns[-1][0]
    loops = find_loops(insns)

    print(f"symbol : {sym[:100]}")
    print(f"range  : 0x{fn_lo:x}..0x{fn_hi:x}  ({len(insns)} instructions)\n")
    print(f"{'range':<28}{'instrs':>8}  contents")
    for lo, hi in loops:
        n = sum(1 for a, _, _ in insns if lo <= a <= hi)
        print(f"  +0x{lo-fn_lo:<5x}..+0x{hi-fn_lo:<5x} {n:>10}  {describe(insns, lo, hi)}")

    if args.mode == "ranges":
        return

    if not args.samples:
        sys.exit("report mode needs --samples")

    binary_name = args.binary_name or args.binary.rsplit("/", 1)[-1]
    samples = parse_samples(args.samples, binary_name)
    if not samples:
        sys.exit(f"no samples with leaf frames in {binary_name!r}")

    buckets = defaultdict(float)
    in_fn = 0.0
    for addr, load, w in samples:
        static = addr - load + args.text_base
        if not (fn_lo <= static <= fn_hi):
            continue
        in_fn += w
        for lo, hi in loops:
            if lo <= static <= hi:
                buckets[(lo, hi)] += w
                break
        else:
            buckets[("other",)] += w

    total = sum(w for _, _, w in samples)
    print(f"\nsamples: {total:,.0f} total, {in_fn:,.0f} inside {args.symbol} "
          f"({100*in_fn/total:.1f}%)\n")
    if not in_fn:
        print("None landed in range — check --text-base against the binary's __TEXT vmaddr.")
        return
    print(f"{'range':<28}{'samples':>10}{'share':>8}  contents")
    for k, w in sorted(buckets.items(), key=lambda kv: -kv[1]):
        label = "other (not in a loop)" if k == ("other",) else f"+0x{k[0]-fn_lo:x}..+0x{k[1]-fn_lo:x}"
        desc = "" if k == ("other",) else describe(insns, k[0], k[1])
        print(f"  {label:<26}{w:>10,.0f}{100*w/in_fn:>7.1f}%  {desc}")


if __name__ == "__main__":
    main()
