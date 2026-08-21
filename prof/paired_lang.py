#!/usr/bin/env python3
"""Paired, order-randomized A/B ACROSS LANGUAGES on the same fixture.

WHY: the Go, Rust and Python matrices were each measured in their own session,
and this host drifts enough between sessions (~7%) to invent or erase a 1.2x
transform gap. Cross-language claims therefore have to be re-measured with the
contenders interleaved back to back and the order coin-flipped, exactly as the
within-Go comparisons were. Statistic = median of per-pair ratios + sign count.

usage: paired_lang.py <fixture> <pairs> <specA> <specB>
   spec: go:<cfg>:<variant> | rs:<tag>:<variant> | py:<variant>
"""
import json, os, random, statistics, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def cmd(spec, fixture):
    kind = spec.split(":")[0]
    src = f"fixtures/{fixture}.png"
    out = f"out/pl_{fixture}_{spec.replace(':','_')}.pgm"
    if kind == "go":
        _, cfg, v = spec.split(":")
        return [os.path.join(ROOT, "bin", f"bench_{cfg}.exe"), v, src, out]
    if kind == "rs":
        _, tag, v = spec.split(":")
        return [os.path.join(ROOT, "rust", "bin", f"rsbench_{tag}.exe"), v, src, out]
    _, v = spec.split(":")
    return [sys.executable, os.path.join(ROOT, "py", "bench_py.py"), v, src, out]


def run(spec, fixture):
    r = subprocess.run(cmd(spec, fixture), capture_output=True, text=True, cwd=ROOT, check=True)
    return json.loads(r.stdout.strip().splitlines()[-1])


def main():
    fixture, pairs, a, b = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
    random.seed(99)
    acc = {ph: [] for ph in ("decode", "transform", "total")}
    for _ in range(pairs):
        order = [a, b] if random.random() < 0.5 else [b, a]
        res = {s: run(s, fixture) for s in order}
        for ph in acc:
            acc[ph].append(res[a][ph]["med"] / res[b][ph]["med"])
    print(f"-- {fixture}: {a} vs {b} ({pairs} pairs) --")
    for ph, rs in acc.items():
        faster = sum(1 for r in rs if r > 1.0)
        print(f"  {ph:<10} median {a}/{b} = {statistics.median(rs):.3f}  "
              f"({faster}/{len(rs)} pairs where {b} is faster, range {min(rs):.3f}-{max(rs):.3f})")


if __name__ == "__main__":
    main()
