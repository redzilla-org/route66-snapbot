#!/usr/bin/env python3
"""Run the variant matrix across several BUILD CONFIGURATIONS of the same source.

WHY a driver instead of the existing bench_all.ps1: that script varies the
VARIANT against one binary. Task here is the orthogonal axis -- identical source,
different compiler settings (GOAMD64, PGO) -- so the loop has to vary the binary
and hold the variant fixed. Each bench.exe invocation is itself 15 timed
iterations after 3 warmups; repeating the invocation is what exposes machine
noise, and the reported number is the median of the per-invocation medians.

usage: bench_configs.py <reps>   (writes out/buildconfig.json)
"""
import json, os, subprocess, sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CONFIGS = ["base", "v3", "pgo", "v3pgo"]
VARIANTS = (os.environ.get("VARIANTS") or "D,E").split(",")
FIXTURES = {"typical": "fixtures/typical.png", "big": "fixtures/big.png"}
PHASES = ["decode", "transform", "encode", "total"]


def run(cfg, variant, fixture):
    exe = os.path.join(ROOT, "bin", f"bench_{cfg}.exe")
    out = os.path.join(ROOT, "out", f"{fixture}_{variant}_{cfg}.pgm")
    r = subprocess.run(
        [exe, variant, os.path.join(ROOT, FIXTURES[fixture]), out],
        capture_output=True, text=True, cwd=ROOT, check=True,
    )
    return json.loads(r.stdout.strip().splitlines()[-1])


def main():
    reps = int(sys.argv[1]) if len(sys.argv) > 1 else 3
    acc = defaultdict(list)
    for rep in range(reps):
        for cfg in CONFIGS:
            for v in VARIANTS:
                for fx in FIXTURES:
                    j = run(cfg, v, fx)
                    for ph in PHASES:
                        acc[(fx, v, cfg, ph)].append(j[ph]["med"])
        print(f"rep {rep+1}/{reps} done", flush=True)

    res = {}
    for k, vals in acc.items():
        s = sorted(vals)
        res["|".join(k)] = {"med": s[len(s) // 2], "min": s[0], "max": s[-1], "n": len(s)}
    with open(os.path.join(ROOT, "out", os.environ.get("OUTJSON", "buildconfig.json")), "w") as f:
        json.dump(res, f, indent=1)

    for fx in FIXTURES:
        for v in VARIANTS:
            print(f"\n== {fx} variant {v} ==")
            print(f"{'cfg':<8}" + "".join(f"{p:<22}" for p in PHASES))
            for cfg in CONFIGS:
                row = f"{cfg:<8}"
                for ph in PHASES:
                    d = res[f"{fx}|{v}|{cfg}|{ph}"]
                    row += f"{d['med']:8.2f} [{d['min']:.1f}-{d['max']:.1f}]".ljust(22)
                print(row)


if __name__ == "__main__":
    main()
