#!/usr/bin/env python3
"""Repeat the Rust variant matrix N times, interleaved, and print the
median-of-medians plus the spread for every variant/fixture/phase.

WHY interleaved repeats: each rsbench invocation already uses a 250 ms timed window
after 3 warmups and reports its own median, but this host's run-to-run noise is
large enough (5-15% on transform) to swamp small differences if a single
invocation is trusted. Interleaving the variants inside each repeat also stops a
thermal or background-load drift from being attributed to whichever variant
happened to run during it.

Usage: python rust/bench_rs.py --reps 5 --tag base --variants C,D,DS,...
The tag names the binary under rust/bin/ so that different BUILD FLAGS (baseline,
+lto/panic, +target-cpu=native) can be measured as separate, comparable steps.
"""
import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = {"typical": "fixtures/typical.png", "big": "fixtures/big.png"}
PHASES = ["decode", "transform", "encode", "total"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--tag", default="base")
    ap.add_argument("--variants", default="C,D,DS,D1,D2,D3,F64,F32")
    ap.add_argument("--bin", default=None, help="path to the rsbench binary")
    ap.add_argument("--phases", action="store_true", help="also split transform into stretch/scale")
    args = ap.parse_args()

    binary = Path(args.bin) if args.bin else ROOT / "rust" / "bin" / f"rsbench_{args.tag}.exe"
    if not binary.exists():
        sys.exit(f"missing binary: {binary}")
    variants = args.variants.split(",")
    env = None
    phase_keys = list(PHASES)
    if args.phases:
        import os
        env = dict(os.environ, RSBENCH_PHASES="1")
        phase_keys += ["stretch", "scale"]

    acc = {}
    for r in range(args.reps):
        for v in variants:
            for fx, path in FIXTURES.items():
                out = f"out/{fx}_rs{v}.pgm"
                res = subprocess.run(
                    [str(binary), v, path, out], cwd=ROOT, capture_output=True, text=True, env=env
                )
                if res.returncode != 0:
                    sys.exit(f"{v}/{fx} failed: {res.stderr}")
                j = json.loads(res.stdout)
                for ph in phase_keys:
                    acc.setdefault((fx, v, ph), []).append(j[ph]["med"])
        print(f"rep {r + 1}/{args.reps} done", file=sys.stderr)

    for fx in FIXTURES:
        print(f"\n== {args.tag} / {fx} ==")
        print(f"{'var':<5}" + "".join(f"{ph:<22}" for ph in phase_keys))
        for v in variants:
            row = f"{v:<5}"
            for ph in phase_keys:
                s = sorted(acc[(fx, v, ph)])
                row += f"{statistics.median(s):.2f} [{s[0]:.1f}-{s[-1]:.1f}]".ljust(22)
            print(row)

    # Machine-readable copy, so a later comparison across build-flag tags does not
    # have to re-run the matrix.
    dump = {f"{fx}|{v}|{ph}": vals for (fx, v, ph), vals in acc.items()}
    (ROOT / "out" / f"rust_{args.tag}.json").write_text(json.dumps(dump, indent=1))


if __name__ == "__main__":
    main()
