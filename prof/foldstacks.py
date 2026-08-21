#!/usr/bin/env python3
"""Turn `go tool pprof -traces` output into Brendan-Gregg folded stacks and a
self-contained flamegraph SVG.

WHY this exists: `go tool pprof` ships a call-GRAPH renderer (-svg) but no
flamegraph renderer outside the interactive -http server, and the deliverable
here has to be a static file. -traces gives one leaf-to-root stack per sample
group with a value, which is exactly the folded format modulo ordering, so the
conversion is mechanical and needs no extra dependency.

usage: foldstacks.py <traces.txt> <out.folded> <out.svg> <title>
"""
import sys, html
from collections import defaultdict


def parse(path):
    """Read -traces output; return {folded_stack_string: sample_ms}."""
    folded = defaultdict(float)
    cur, val = [], None
    for line in open(path, encoding="utf-8", errors="replace"):
        line = line.rstrip("\n")
        if line.startswith("-----------+"):
            if cur and val is not None:
                # -traces prints leaf-first; folded format is root-first.
                folded["".join([";".join(reversed(cur))])] += val
            cur, val = [], None
            continue
        s = line.strip()
        if not s:
            continue
        # A new sample group starts with "<value><unit>   <leaf frame>".
        parts = s.split(None, 1)
        if val is None and len(parts) == 2 and parts[0][:1].isdigit():
            v = parts[0]
            for unit, mult in (("ms", 1.0), ("s", 1000.0), ("us", 0.001)):
                if v.endswith(unit):
                    try:
                        val = float(v[: -len(unit)]) * mult
                    except ValueError:
                        val = None
                    break
            if val is not None:
                cur = [parts[1]]
                continue
        if val is not None:
            cur.append(s)
    if cur and val is not None:
        folded[";".join(reversed(cur))] += val
    return folded


def build_tree(folded):
    root = {"name": "root", "value": 0.0, "children": {}}
    for stack, v in folded.items():
        root["value"] += v
        node = root
        for frame in stack.split(";"):
            node = node["children"].setdefault(
                frame, {"name": frame, "value": 0.0, "children": {}}
            )
            node["value"] += v
    return root


PALETTE = ["#d94a38", "#e0703a", "#e69138", "#d9a441", "#c8b04a"]


def color(name, i):
    # Runtime/stdlib frames get a cooler tint so product code stands out.
    if name.startswith("runtime.") or name.startswith("hash/") or name.startswith("compress/"):
        return "#8fa3b0"
    if name.startswith("main."):
        return PALETTE[i % len(PALETTE)]
    return "#b9926a"


def render(root, title, out):
    H = 18
    rows = []

    def walk(node, depth, x):
        rows.append((depth, x, node["value"], node["name"]))
        cx = x
        for i, ch in enumerate(sorted(node["children"].values(), key=lambda n: -n["value"])):
            walk(ch, depth + 1, cx)
            cx += ch["value"]

    walk(root, 0, 0.0)
    maxdepth = max(r[0] for r in rows) + 1
    W = 1400
    height = maxdepth * H + 40
    total = root["value"] or 1.0
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{height}" '
        f'font-family="Consolas,monospace" font-size="11">',
        f'<rect width="{W}" height="{height}" fill="#ffffff"/>',
        f'<text x="8" y="14" font-size="13" fill="#222">{html.escape(title)} '
        f'(total {total:.0f} ms of CPU samples)</text>',
    ]
    for i, (depth, x, v, name) in enumerate(rows):
        w = v / total * (W - 16)
        if w < 0.4:
            continue
        px = 8 + x / total * (W - 16)
        py = height - (depth + 1) * H
        pct = 100 * v / total
        label = name if w > 60 else ""
        parts.append(
            f'<g><title>{html.escape(name)} — {v:.0f} ms ({pct:.2f}%)</title>'
            f'<rect x="{px:.2f}" y="{py}" width="{w:.2f}" height="{H-1}" '
            f'fill="{color(name,i)}" stroke="#fff" stroke-width="0.5"/>'
            f'<text x="{px+3:.2f}" y="{py+H-6}" fill="#111">'
            f'{html.escape(label[:int(w/6)])}</text></g>'
        )
    parts.append("</svg>")
    open(out, "w", encoding="utf-8").write("\n".join(parts))


def main():
    traces, foldout, svgout, title = sys.argv[1:5]
    folded = parse(traces)
    with open(foldout, "w", encoding="utf-8") as f:
        for stack, v in sorted(folded.items(), key=lambda kv: -kv[1]):
            f.write(f"{stack} {v:.0f}\n")
    render(build_tree(folded), title, svgout)
    print(f"{foldout}: {len(folded)} folded stacks; {svgout} written")


if __name__ == "__main__":
    main()
