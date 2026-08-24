#!/usr/bin/env python3
"""Paired comparison across all native and hosted benchmark contenders.

usage: paired_lang.py <fixture> <pairs> <ref-spec> <spec> [<spec> ...]
  spec: go:<cfg>:<variant> | go127:<variant> | rs:<tag>:<variant> |
        zig:<variant> | nim:<variant> | py:<variant> | wf:<variant>
"""
import json
import os
import random
import re
import statistics
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PHASES = ("decode", "transform", "encode", "total")


def safe_label(spec):
    return re.sub(r"[^A-Za-z0-9_.-]+", "_", spec)


def command(spec, fixture):
    parts = spec.split(":")
    source = f"fixtures/{fixture}.png"
    output = f"out/pl_{fixture}_{safe_label(spec)}.pgm"
    if parts[0] == "go" and len(parts) == 3:
        argv = [os.path.join(ROOT, "bin", f"bench_{parts[1]}.exe"), parts[2]]
    elif parts[0] == "go127" and len(parts) == 2:
        argv = [os.path.join(ROOT, "go1.27", "bin", "go127bench.exe"), parts[1]]
    elif parts[0] == "rs" and len(parts) == 3:
        argv = [os.path.join(ROOT, "rust", "bin", f"rsbench_{parts[1]}.exe"), parts[2]]
    elif parts[0] == "zig" and len(parts) == 2:
        argv = [os.path.join(ROOT, "zig", "bin", "zigbench.exe"), parts[1]]
    elif parts[0] == "nim" and len(parts) == 2:
        argv = [os.path.join(ROOT, "nim", "bin", "nimbench.exe"), parts[1]]
    elif parts[0] == "py" and len(parts) == 2:
        argv = [sys.executable, os.path.join(ROOT, "py", "bench_py.py"), parts[1]]
    elif parts[0] in ("wf", "wuffs") and len(parts) == 2:
        argv = [os.path.join(ROOT, "wuffs", "bin", "wuffsbench.exe"), parts[1]]
    else:
        raise ValueError(f"invalid contestant spec: {spec}")
    return argv + [source, output], output


def run(spec, fixture):
    argv, output = command(spec, fixture)
    try:
        result = subprocess.run(argv, capture_output=True, text=True, cwd=ROOT, check=True)
    except subprocess.CalledProcessError as error:
        detail = error.stderr.strip() or error.stdout.strip() or "no process output"
        raise RuntimeError(f"{spec} failed with exit {error.returncode}: {detail}") from error
    return json.loads(result.stdout.strip().splitlines()[-1]), output


def read_pgm(relative_path):
    with open(os.path.join(ROOT, relative_path), "rb") as stream:
        tokens = []
        while len(tokens) < 4:
            line = stream.readline()
            if not line:
                raise ValueError(f"truncated PGM header: {relative_path}")
            tokens.extend(line.split(b"#", 1)[0].split())
        if tokens[0] != b"P5" or tokens[3] != b"255":
            raise ValueError(f"unsupported PGM header: {relative_path}")
        width, height = int(tokens[1]), int(tokens[2])
        pixels = stream.read()
    if len(pixels) != width * height:
        raise ValueError(f"PGM pixel count mismatch: {relative_path}")
    return width, height, pixels


def pixel_diff(reference_path, candidate_path):
    rw, rh, reference = read_pgm(reference_path)
    cw, ch, candidate = read_pgm(candidate_path)
    if (rw, rh) != (cw, ch):
        raise ValueError(f"dimension mismatch: {rw}x{rh} vs {cw}x{ch}")
    differing = 0
    max_delta = 0
    for a, b in zip(reference, candidate):
        delta = abs(a - b)
        if delta:
            differing += 1
            max_delta = max(max_delta, delta)
    return {
        "pixels": len(reference),
        "differing": differing,
        "pct": differing * 100.0 / len(reference),
        "maxAbsDelta": max_delta,
    }


def measurable_ratio(reference_value, candidate_value):
    """Return no ratio when timer quantization made either phase zero."""
    if reference_value <= 0 or candidate_value <= 0:
        return None
    return reference_value / candidate_value


def main():
    if len(sys.argv) < 5:
        raise SystemExit(__doc__)
    fixture, pairs = sys.argv[1], int(sys.argv[2])
    specs = sys.argv[3:]
    if len(specs) != len(set(specs)):
        raise SystemExit("contestant specs must be unique")
    reference = specs[0]
    random.seed(20260823)
    os.makedirs(os.path.join(ROOT, "out"), exist_ok=True)

    absolute = {spec: {phase: [] for phase in PHASES} for spec in specs}
    ratios = {
        spec: {phase: [] for phase in PHASES}
        for spec in specs[1:]
    }
    outputs = {}
    for pair in range(pairs):
        # Shuffling each outer run distributes transient host effects without
        # changing the owner's best-observed-run comparison statistic.
        order = list(specs)
        random.shuffle(order)
        results = {}
        for spec in order:
            results[spec], outputs[spec] = run(spec, fixture)
        for spec in specs:
            for phase in PHASES:
                absolute[spec][phase].append(results[spec][phase]["min"])
        for spec in specs[1:]:
            for phase in PHASES:
                ratios[spec][phase].append(
                    measurable_ratio(
                        results[reference][phase]["min"],
                        results[spec][phase]["min"],
                    )
                )
        print(f"  pair {pair + 1}/{pairs} done", file=sys.stderr, flush=True)

    # Headline values are the fastest complete observations across all outer
    # runs. Ratios compare those independent minima directly, as requested.
    aggregate_min = {
        spec: {phase: min(absolute[spec][phase]) for phase in PHASES}
        for spec in specs
    }
    # Medians describe host noise only. They are deliberately kept out of every
    # winner ratio so a diagnostic summary cannot silently replace the owner's
    # fastest-complete-iteration ranking rule.
    diagnostic_median = {
        spec: {
            phase: statistics.median(absolute[spec][phase])
            for phase in PHASES
        }
        for spec in specs
    }
    best_observed_ratios = {
        spec: {
            phase: measurable_ratio(
                aggregate_min[reference][phase], aggregate_min[spec][phase]
            )
            for phase in PHASES
        }
        for spec in specs[1:]
    }
    correctness = {
        spec: pixel_diff(outputs[reference], outputs[spec])
        for spec in specs[1:]
    }
    print(f"\n== {fixture}: paired, interleaved, {pairs} pairs ==")
    print("fastest observed process minima across all runs (ms):")
    label_width = max(20, max(len(spec) for spec in specs) + 2)
    for spec in specs:
        row = f"  {spec:<{label_width}}"
        for phase in PHASES:
            values = absolute[spec][phase]
            row += (
                f"{phase}={aggregate_min[spec][phase]:7.2f} "
                f"[{min(values):.1f}-{max(values):.1f}]  "
            )
        print(row)

    print("\nnoise diagnostic: median of outer-run process minima (ms; not ranked):")
    for spec in specs:
        row = f"  {spec:<{label_width}}"
        for phase in PHASES:
            row += f"{phase}={diagnostic_median[spec][phase]:7.2f}  "
        print(row)

    print(f"\nbest-observed ratios vs {reference} (>1 means candidate is faster):")
    for spec in specs[1:]:
        for phase in PHASES:
            ratio = best_observed_ratios[spec][phase]
            result = "n/a" if ratio is None else f"{ratio:.4f}"
            print(f"  {reference}/{spec} {phase:<10} {result}")

    print(f"\npaired-run diagnostics vs {reference} (>1 means candidate is faster):")
    for spec in specs[1:]:
        for phase in PHASES:
            values = ratios[spec][phase]
            # A zero process minimum is below this host timer's useful
            # resolution. Excluding it preserves honest sign counts and JSON.
            measured = [value for value in values if value is not None]
            if not measured:
                print(
                    f"  {reference}/{spec} {phase:<10} "
                    f"n/a (0/{len(values)} measurable pairs)"
                )
                continue
            wins = sum(value > 1.0 for value in measured)
            print(
                f"  {reference}/{spec} {phase:<10} "
                f"{wins}/{len(measured)} candidate-faster pairs, "
                f"ratio range {min(measured):.3f}-{max(measured):.3f}; "
                f"{len(measured)}/{len(values)} measurable"
            )

    print(f"\ncorrectness vs {reference}:")
    for spec, diff in correctness.items():
        print(
            f"  {spec:<{label_width}} differing={diff['differing']}/{diff['pixels']} "
            f"({diff['pct']:.4f}%) maxAbsDelta={diff['maxAbsDelta']}"
        )

    payload = {
        "fixture": fixture,
        "pairs": pairs,
        "reference": reference,
        "sample_statistic": "process_phase_minimum_ms",
        "headline_statistic": "minimum_observed_across_runs",
        "absolute_ms": absolute,
        "aggregate_min_ms": aggregate_min,
        "diagnostic_median_ms": diagnostic_median,
        "ratios": ratios,
        "best_observed_ratios": best_observed_ratios,
        "correctness": correctness,
    }
    with open(os.path.join(ROOT, "out", f"paired_lang_{fixture}.json"), "w") as stream:
        json.dump(payload, stream, indent=2)


if __name__ == "__main__":
    main()
