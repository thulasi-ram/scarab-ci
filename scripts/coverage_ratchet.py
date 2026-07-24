#!/usr/bin/env python3
"""Per-crate line-coverage summary + ratchet (test-strategy Phase 0).

Usage:
    coverage_ratchet.py <lcov.info> <baseline.toml>          # compare (CI)
    coverage_ratchet.py <lcov.info> <baseline.toml> --write  # regenerate baseline

Reads an lcov file produced by `cargo llvm-cov nextest --lcov`, aggregates
LH/LF per workspace crate (paths under `crates/<name>/`; anything else —
including `.workspaces/` worktrees — is ignored), prints a markdown table,
and appends it to $GITHUB_STEP_SUMMARY when that is set.

Ratchet: exits 1 if any crate's line coverage drops more than SLACK
percentage points below the committed baseline. `--write` regenerates the
baseline instead; a human commits it deliberately (see `just coverage`).

The baseline is a deliberately trivial TOML subset: comments, a `[crates]`
header, and `name = <float>` lines. No TOML library needed to read or write.
"""

import sys
from pathlib import Path

SLACK = 0.5  # percentage points a crate may drop before the ratchet fails


def parse_lcov(path):
    """Return {crate: (lines_hit, lines_found)} aggregated over source files."""
    per_crate = {}
    crate = None
    for line in Path(path).read_text().splitlines():
        if line.startswith("SF:"):
            src = line[3:].replace("\\", "/")
            crate = None
            if "/.workspaces/" in src or src.startswith(".workspaces/"):
                continue  # gitignored worktrees — never part of the signal
            parts = src.split("/")
            if "crates" in parts:
                i = parts.index("crates")
                if i + 1 < len(parts):
                    crate = parts[i + 1]
        elif crate and line.startswith("LF:"):
            hit, found = per_crate.get(crate, (0, 0))
            per_crate[crate] = (hit, found + int(line[3:]))
        elif crate and line.startswith("LH:"):
            hit, found = per_crate.get(crate, (0, 0))
            per_crate[crate] = (hit + int(line[3:]), found)
    return per_crate


def pct(hit, found):
    return 100.0 * hit / found if found else 0.0


def read_baseline(path):
    """Return {crate: percent} from the trivial-TOML baseline file."""
    baseline = {}
    for line in Path(path).read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if not line or line.startswith("["):
            continue
        name, _, value = line.partition("=")
        baseline[name.strip().strip('"')] = float(value.strip())
    return baseline


def write_baseline(path, coverage):
    lines = [
        "# Per-crate line-coverage baseline (percent), consumed by",
        "# scripts/coverage_ratchet.py: CI fails if any crate drops more than",
        f"# {SLACK} percentage points below its value here. Regenerate",
        "# deliberately with `just coverage` and commit the result.",
        "",
        "[crates]",
    ]
    lines += [f"{name} = {p:.1f}" for name, p in sorted(coverage.items())]
    Path(path).write_text("\n".join(lines) + "\n")


def main():
    args = [a for a in sys.argv[1:] if a != "--write"]
    write = "--write" in sys.argv
    if len(args) != 2:
        sys.exit(__doc__)
    lcov_path, baseline_path = args

    per_crate = parse_lcov(lcov_path)
    coverage = {c: pct(h, f) for c, (h, f) in per_crate.items()}

    if write:
        write_baseline(baseline_path, coverage)
        print(f"wrote {baseline_path}")

    baseline = read_baseline(baseline_path)
    failures = []
    rows = ["| crate | lines | coverage | baseline | delta |", "|---|---|---|---|---|"]
    for name in sorted(set(coverage) | set(baseline)):
        cur = coverage.get(name)
        base = baseline.get(name)
        if cur is None:
            rows.append(f"| {name} | — | — | {base:.1f}% | crate gone |")
            continue
        hit, found = per_crate[name]
        delta = "new" if base is None else f"{cur - base:+.1f}pp"
        rows.append(f"| {name} | {hit}/{found} | {cur:.1f}% | "
                    f"{'—' if base is None else f'{base:.1f}%'} | {delta} |")
        if base is not None and cur < base - SLACK:
            failures.append(f"{name}: {cur:.1f}% < baseline {base:.1f}% - {SLACK}pp")

    table = "### Per-crate line coverage\n\n" + "\n".join(rows) + "\n"
    print(table)

    import os
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(table)

    if failures and not write:
        for f_ in failures:
            print(f"::error::coverage ratchet: {f_}")
        sys.exit(1)


if __name__ == "__main__":
    main()
