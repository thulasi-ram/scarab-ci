#!/usr/bin/env python3
"""Merge gate: are this PR's REQUIRED checks green? (test-strategy Phase 0/2)

Usage:
    pr_gate.py <pr-number>            # or `just pr-gate <n>`

Branch protection and rulesets are both 403 on this repo (private, free plan),
so GitHub cannot enforce a required-check set server-side. This script is that
enforcement, run by whoever merges: it reads `.github/required-checks.txt`,
asks `gh pr checks` for the PR's check runs, and exits non-zero unless every
required check passed.

Exit codes:  0 green  ·  1 a required check failed, is missing, or was
                         skipped while listed `required`  ·
             8 a required check is still pending  ·  2 usage/tooling error

Advisory checks are always reported and never affect the exit code — that is
the whole point of a probation tier.
"""

import json
import subprocess
import sys
from pathlib import Path

TIERS = ("required", "required-if-run", "advisory")
SPEC = Path(__file__).resolve().parent.parent / ".github" / "required-checks.txt"


def parse_spec(path):
    """Return [(tier, workflow, job)] from the required-checks file."""
    out = []
    for lineno, raw in enumerate(path.read_text().splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) != 2 or parts[0] not in TIERS or "/" not in parts[1]:
            sys.exit(
                f"{path}:{lineno}: expected `<{'|'.join(TIERS)}> <workflow>/<job>`, got: {raw!r}"
            )
        tier, ref = parts
        workflow, job = ref.split("/", 1)
        out.append((tier, workflow, job))
    return out


def fetch_checks(pr):
    """Return {(workflow, name): bucket} for the PR's check runs."""
    cmd = ["gh", "pr", "checks", str(pr), "--json", "name,state,bucket,workflow"]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    # `gh pr checks` exits 8 when checks are pending and 1 when some failed —
    # both are states we want to REPORT, not crash on. Only a missing/erroring
    # gh (no JSON on stdout) is fatal.
    if not proc.stdout.strip():
        sys.exit(f"gh pr checks failed (exit {proc.returncode}): {proc.stderr.strip()}")
    return {(c["workflow"], c["name"]): c["bucket"] for c in json.loads(proc.stdout)}


def main(argv):
    if len(argv) != 2 or not argv[1].isdigit():
        sys.exit(__doc__)
    pr = argv[1]

    spec = parse_spec(SPEC)
    checks = fetch_checks(pr)

    failed, pending, rows = [], [], []
    for tier, workflow, job in spec:
        bucket = checks.get((workflow, job))
        ref = f"{workflow}/{job}"
        # "It did not run" arrives in two shapes and they mean the same thing.
        # A workflow-level `paths:` filter makes the check ABSENT (bucket None,
        # as `kind/cluster-tests` is on an unrelated PR); a job-level
        # `if: needs.changes.outputs.… == 'true'` makes it SKIPPED. Only the
        # tier decides whether that is acceptable — never the mechanism. This
        # used to let `skipping` pass at ANY tier, so a required check that was
        # skipped by mistake reported green.
        if bucket is None or bucket == "skipping":
            how = "skipped" if bucket == "skipping" else "MISSING"
            if tier == "required-if-run":
                verdict = f"{how} (filtered) — OK"
            else:
                verdict = f"{how.upper()} (did not run)"
                if tier == "required":
                    failed.append(ref)
        elif bucket == "pass":
            verdict = "pass"
        elif bucket in ("pending",):
            verdict = "PENDING"
            if tier != "advisory":
                pending.append(ref)
        else:  # fail, cancel
            verdict = bucket.upper()
            if tier != "advisory":
                failed.append(ref)
        rows.append((tier, ref, verdict))

    width = max(len(r[1]) for r in rows)
    for tier, ref, verdict in rows:
        print(f"  {tier:<16} {ref:<{width}}  {verdict}")

    # Anything the PR ran that the spec doesn't mention: not a gate, but worth
    # surfacing so a newly added job isn't silently ungated forever.
    known = {(w, j) for _, w, j in spec}
    unlisted = sorted(f"{w}/{n}" for (w, n) in checks if (w, n) not in known)
    if unlisted:
        print(f"\nnot in {SPEC.name} (ungated): {', '.join(unlisted)}")

    if failed:
        print(f"\n✗ required checks not green: {', '.join(failed)}")
        return 1
    if pending:
        print(f"\n… required checks still running: {', '.join(pending)}")
        return 8
    print(f"\n✓ PR #{pr}: every required check is green")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
