"""Copy the text between two marker lines of a docker build log into a file.

WHY: the build gate is a docker build, and the lockfile cargo resolves inside
it can only leave the container through the build log. This lifts the block
printed between `==== <NAME> BEGIN` and `==== <NAME> END` into the target path.

usage: python extract_marked_block.py <build log> <NAME> <output path>
"""
import sys


def main(log_path: str, name: str, out_path: str) -> None:
    begin, end = f"==== {name} BEGIN", f"==== {name} END"
    lines, inside = [], False
    for raw in open(log_path, encoding="utf-8", errors="replace"):
        line = raw.rstrip("\r\n")
        # Docker prefixes stderr chunks with ANSI color codes; strip them.
        for code in ("\x1b[91m", "\x1b[0m"):
            line = line.replace(code, "")
        if line == begin:
            inside, lines = True, []
            continue
        if line == end:
            inside = False
            continue
        if inside:
            lines.append(line)
    if not lines:
        sys.exit(f"no {name} block in {log_path}")
    with open(out_path, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(lines) + "\n")
    print(f"wrote {len(lines)} lines to {out_path}")


if __name__ == "__main__":
    main(*sys.argv[1:4])
