# 0027. Restart semantics: content-addressed smart invalidation

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

The single most wedge-defining UX behavior: when a user restarts step X (or X auto-retries),
X produces a fresh Attempt with a new output; downstream steps consumed X's *old* output. What
happens downstream? This is where content-addressing ([0004](0004-execution-topology.md),
[0029](0029-workspace-cas.md)) pays its dividend.

## Decision

**Smart, content-addressed invalidation by default, with manual overrides:**

- Re-evaluate downstream against X's **new output hash**: **skip** if unchanged (mark
  "inputs unchanged"), **cascade** (re-run) if changed. Minimal recompute; coherent state.
- **Side-effecting steps have no meaningful skip hash** → they **always re-run on upstream
  change** (never silently skipped); ties into fencing ([0021](0021-double-effect-fencing.md)).
- **Manual overrides:** `force-cascade` (re-run all downstream, ignore cache) and
  `force-skip`/isolated (just this step, for those who know better).
- Skipped steps are **surfaced explicitly** in the UI/timeline — "smart" never means "mysterious."

## Consequences

- "Restart the flaky lint step" costs nothing downstream; "restart the build with a fix"
  cascades precisely to what changed — the visible payoff of the wedge.
- Requires reliable per-step input/output hashing (merkle CAS).

## Alternatives considered

- **Cascade always** — simple/safe, but wastes content-addressing (re-deploys on identical builds).
- **Isolated re-run** — fast, but leaves incoherent downstream state; footgun as a default.
