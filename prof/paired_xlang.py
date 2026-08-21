#!/usr/bin/env python3
"""Cross-language paired A/B: Go and Rust binaries interleaved, order shuffled.

WHY a sibling of paired_ab.py rather than a reuse of it: paired_ab.py resolves
every spec to bin/bench_<cfg>.exe, so it can only ever pit one Go build against
another. The cross-language question needs the SAME statistic -- median of
per-pair ratios plus a sign count -- over binaries that live in two different
trees and are produced by two different toolchains. The methodology is copied
verbatim; only the binary resolution is generalized.

WHY paired at all: the Go side documented real machine drift BETWEEN sessions
(the same binary, same variant, moved 82.8 -> 88.9 ms on transform). Two medians
collected in two sessions are therefore not comparable at the few-percent level.
Running the contestants back to back inside one pair, with the order coin-flipped
per pair, cancels drift that is slow relative to a pair; the sign count says
whether the direction is consistent at all, which is what distinguishes a real
difference from a tie whose median happens not to be exactly 1.0.

Every config in a pair is run back to back within that pair, so all of them share
the same slice of machine weather and every pairwise ratio is drift-cancelled.

usage: paired_xlang.py <pairs> <fixture> <ref-spec> <spec> [<spec> ...]
  spec := <label>=<path-to-exe>:<variant>
"""
import json
import os
import random
import statistics
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PHASES = ("decode", "transform", "encode", "total")


def parse(spec):
    label, rest = spec.split("=", 1)
    exe, variant = rest.rsplit(":", 1)
    return label, os.path.join(ROOT, exe), variant


def run(label, exe, variant, fixture):
    # Both harnesses share an argv contract (variant, input, output) and both
    # print one JSON object of per-phase min/med/mean over 15 timed iterations
    # after 3 warmups. Distinct output paths keep the configs from racing on
    # the same file and from being credited with each other's write cost.
    # RELATIVE paths on purpose: the Rust harness echoes its input path into the
    # JSON without escaping, so an absolute Windows path emits invalid JSON.
    out = f"out/xl_{fixture}_{label}.pgm"
    r = subprocess.run([exe, variant, f"fixtures/{fixture}.png", out],
                       capture_output=True, text=True, cwd=ROOT, check=True)
    return json.loads(r.stdout.strip().splitlines()[-1])


def main():
    pairs = int(sys.argv[1])
    fixture = sys.argv[2]
    specs = [parse(s) for s in sys.argv[3:]]
    ref = specs[0]
    random.seed(20260820)

    # ratios[label][phase] = list of per-pair (ref_time / label_time); >1 means
    # the reference is SLOWER, i.e. the labelled config is faster in that pair.
    ratios = {lab: {ph: [] for ph in PHASES} for lab, _, _ in specs[1:]}
    abs_ms = {lab: {ph: [] for ph in PHASES} for lab, _, _ in specs}

    for p in range(pairs):
        order = list(specs)
        random.shuffle(order)
        res = {lab: run(lab, exe, var, fixture) for lab, exe, var in order}
        for lab, _, _ in specs:
            for ph in PHASES:
                abs_ms[lab][ph].append(res[lab][ph]["med"])
        for lab, _, _ in specs[1:]:
            for ph in PHASES:
                ratios[lab][ph].append(res[ref[0]][ph]["med"] / res[lab][ph]["med"])
        print(f"  pair {p + 1}/{pairs} done", file=sys.stderr, flush=True)

    print(f"\n== {fixture}: paired, interleaved, {pairs} pairs, order shuffled per pair ==")
    print("absolute medians of the per-pair medians (ms):")
    for lab, _, _ in specs:
        row = "  " + lab.ljust(14)
        for ph in PHASES:
            s = sorted(abs_ms[lab][ph])
            row += f"{ph}={statistics.median(s):7.2f} [{s[0]:.1f}-{s[-1]:.1f}]  "
        print(row)
    print(f"\nper-pair ratios vs reference {ref[0]} (>1 means {ref[0]} is slower):")
    for lab, _, _ in specs[1:]:
        for ph in PHASES:
            rs = ratios[lab][ph]
            wins = sum(1 for r in rs if r > 1.0)
            print(f"  {ref[0]}/{lab} {ph:<10} median {statistics.median(rs):.4f} "
                  f"(mean {statistics.mean(rs):.4f})  {wins}/{len(rs)} pairs where {lab} is faster  "
                  f"range {min(rs):.3f}-{max(rs):.3f}")

    dump = {"fixture": fixture, "pairs": pairs, "ref": ref[0],
            "abs_ms": abs_ms, "ratios": ratios}
    with open(os.path.join(ROOT, "out", f"xlang_{fixture}.json"), "w") as f:
        json.dump(dump, f, indent=1)


if __name__ == "__main__":
    main()
