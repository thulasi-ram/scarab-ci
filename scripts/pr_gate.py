#!/usr/bin/env python3
"""Merge gate: are this PR's REQUIRED checks green? (test-strategy Phase 0/2)

Usage:
    pr_gate.py <pr-number>            # or `just pr-gate <n>`

Branch protection and rulesets are both 403 on this repo (private, free plan),
so GitHub cannot enforce a required-check set server-side. This script is that
enforcement, run by whoever merges: it reads `.github/required-checks.txt`,
asks GitHub for every check run on the PR's head commit, and exits non-zero
unless every required check passed.

A PR with the `ci:skip` label never passes: its CI jobs did not run. Remove the
label (that starts a run), wait for it, and run the gate again.

Exit codes:  0 green  ·  1 a required check failed, is missing, or was
                         skipped while listed `required`; or the PR has
                         `ci:skip`  ·
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


SKIP_LABEL = "ci:skip"

# Every check run on the PR's head commit, grouped by the workflow run (check
# suite) that made it. `gh pr checks` is not enough: it keeps one run per check
# name, the most recent, and a run where the job was SKIPPED counts as recent.
# With `labeled`/`unlabeled` in the workflows' triggers, a label change starts a
# run whose jobs all skip; through `gh pr checks` that run would hide the result
# of the real run before it, including a failure.
QUERY = """
query($owner: String!, $name: String!, $pr: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $pr) {
      labels(first: 100) { nodes { name } }
      commits(last: 1) { nodes { commit {
        oid
        checkSuites(first: 100) {
          pageInfo { hasNextPage }
          nodes {
            workflowRun { workflow { name } }
            checkRuns(first: 100) {
              pageInfo { hasNextPage }
              nodes { name status conclusion startedAt }
            }
          }
        }
      } } }
    }
  }
}
"""

# Same buckets `gh pr checks` uses, so the verdicts below read the same.
BUCKETS = {
    "SUCCESS": "pass",
    "SKIPPED": "skipping",
    "NEUTRAL": "skipping",
    "CANCELLED": "cancel",
}


def fetch(pr):
    """Return (head sha, label names, [(workflow, name, status, conclusion, startedAt)])."""
    cmd = [
        "gh", "api", "graphql",
        "-F", "owner={owner}", "-F", "name={repo}", "-F", f"pr={pr}",
        "-f", f"query={QUERY}",
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.exit(f"gh api graphql failed (exit {proc.returncode}): {proc.stderr.strip()}")
    pull = json.loads(proc.stdout)["data"]["repository"]["pullRequest"]
    if pull is None:
        sys.exit(f"no pull request #{pr} in this repository")
    labels = [n["name"] for n in pull["labels"]["nodes"]]
    commit = pull["commits"]["nodes"][0]["commit"]
    suites = commit["checkSuites"]
    runs = []
    truncated = suites["pageInfo"]["hasNextPage"]
    for suite in suites["nodes"]:
        truncated |= suite["checkRuns"]["pageInfo"]["hasNextPage"]
        if suite["workflowRun"] is None:  # not a GitHub Actions suite
            continue
        workflow = suite["workflowRun"]["workflow"]["name"]
        for c in suite["checkRuns"]["nodes"]:
            runs.append((workflow, c["name"], c["status"], c["conclusion"], c["startedAt"] or ""))
    # A page we did not read could hold the newest result. Refuse, never guess.
    if truncated:
        sys.exit(f"PR #{pr} has more check runs than one query returns; check it by hand")
    return commit["oid"], labels, runs


def latest_buckets(runs):
    """Return {(workflow, name): bucket}, one verdict per check name.

    A check can run several times on one commit: a re-run, a run started by a
    label change, a run with the `ci:skip` label and one after it was removed.
    The rules:
      - any run still in progress or queued: `pending` (wait for it);
      - else the most recent run that did not skip decides;
      - only when every run skipped is the check `skipping`.
    A skip says nothing about the code, so it never hides a result.
    """
    by_check = {}
    for workflow, name, status, conclusion, started in runs:
        by_check.setdefault((workflow, name), []).append((status, conclusion, started))
    out = {}
    for key, attempts in by_check.items():
        if any(status != "COMPLETED" for status, _, _ in attempts):
            out[key] = "pending"
            continue
        ran = [a for a in attempts if BUCKETS.get(a[1]) != "skipping"]
        if not ran:
            out[key] = "skipping"
            continue
        _, conclusion, _ = max(ran, key=lambda a: a[2])
        out[key] = BUCKETS.get(conclusion, "fail")
    return out


def main(argv):
    if len(argv) != 2 or not argv[1].isdigit():
        sys.exit(__doc__)
    pr = argv[1]

    spec = parse_spec(SPEC)
    head, labels, runs = fetch(pr)
    checks = latest_buckets(runs)
    print(f"PR #{pr} head {head[:12]}")

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

    if SKIP_LABEL in labels:
        print(
            f"\n✗ PR #{pr} has the {SKIP_LABEL} label, so its CI jobs did not run. "
            "Remove the label (that starts a run), wait for it, then run the gate again."
        )
        return 1
    if failed:
        print(f"\n✗ required checks not green: {', '.join(failed)}")
        if any(v.startswith("SKIPPED") for t, _, v in rows if t == "required"):
            print(
                "  A required check skipped in every run on this commit: CI ran only while\n"
                f"  {SKIP_LABEL} was on the PR. If you just removed the label, its run may not\n"
                "  have started yet; wait for it, then run the gate again."
            )
        return 1
    if pending:
        print(f"\n… required checks still running: {', '.join(pending)}")
        return 8
    print(f"\n✓ PR #{pr}: every required check is green")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
