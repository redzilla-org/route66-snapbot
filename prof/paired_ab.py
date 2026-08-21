#!/usr/bin/env python3
"""Paired A/B of two build configurations, interleaved and order-randomized.

WHY: the straight matrix run showed base-vs-v3-vs-pgo differences smaller than
the drift of the machine BETWEEN reps (variant D typical transform moved 82.8 ->
88.9 ms for the SAME binary across two sessions). A per-pair comparison, with
the two binaries run back to back and the order coin-flipped each pair, cancels
that drift: the statistic is the median of per-pair ratios, not the ratio of two
independently drifting medians. A sign test over the pairs says whether the
direction is consistent at all.

usage: paired_ab.py <cfgA> <cfgB> <variant> <fixture> <pairs>
"""
import json, os, random, subprocess, sys, statistics

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run(spec, variant, fixture):
    # A spec is "cfg" or "cfg:variant" -- the same paired protocol answers both
    # "which build config is faster" and "which variant is faster", and the
    # variant question needs the SAME cancellation of between-run machine drift.
    cfg = spec
    if ":" in spec:
        cfg, variant = spec.split(":", 1)
    exe = os.path.join(ROOT, "bin", f"bench_{cfg}.exe")
    out = os.path.join(ROOT, "out", f"{fixture}_{variant}_ab.pgm")
    r = subprocess.run([exe, variant, os.path.join(ROOT, "fixtures", fixture + ".png"), out],
                       capture_output=True, text=True, cwd=ROOT, check=True)
    return json.loads(r.stdout.strip().splitlines()[-1])


def main():
    a, b, variant, fixture, pairs = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
    random.seed(1234)
    ratios = {ph: [] for ph in ("decode", "transform", "total")}
    for _ in range(pairs):
        order = [a, b] if random.random() < 0.5 else [b, a]
        res = {c: run(c, variant, fixture) for c in order}
        for ph in ratios:
            ratios[ph].append(res[a][ph]["med"] / res[b][ph]["med"])
    for ph, rs in ratios.items():
        wins = sum(1 for r in rs if r > 1.0)  # A slower than B
        print(f"{fixture} {variant} {ph}: median {a}/{b} ratio = {statistics.median(rs):.4f} "
              f"(mean {statistics.mean(rs):.4f}, {wins}/{len(rs)} pairs where {b} is faster, "
              f"range {min(rs):.3f}-{max(rs):.3f})")


if __name__ == "__main__":
    main()
