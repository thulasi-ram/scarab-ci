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

## ~~Amendment (2026-07-24) — skip-if-unchanged deferred; cascade-always is the shipped behavior~~ **WITHDRAWN 2026-07-27 — it was never true**

The withdrawn text said the skip-if-unchanged half of the decision above was
"deliberately not built" and that the engine shipped cascade-always. **That was
wrong when it was written.** Admission has compared each step's recomputed input
signature against the one it last consumed since migration `0013`
(`Scheduler::tick`, "Skip-if-unchanged"), skips the match, emits `StepSkipped`,
and carries the prior output forward. `crates/scarab-db-postgres/tests/restart.rs`
asserts exactly that and passes.

The amendment is left in place, struck through, rather than deleted, because
**how it came to be believed is the interesting part** — and it is a failure mode
this repo will meet again:

1. The code was there and the tests were green.
2. The behaviour in production was cascade-always, because the signature was
   built over each upstream's snapshot **root**, and a root moves with every
   file's mtime ([0061](0061-workspace-data-path.md) s7). A producer that re-runs
   writes byte-identical bytes at a new wall clock, so it could never reproduce
   its own root, so every dependent's signature always changed. Nothing was ever
   skipped.
3. Someone observed the real behaviour, correctly, and wrote it down as a
   *decision*. A second reader found the doc consistent with reality and left it
   alone. The doc comment on `rerun_step` said the same thing ~1000 lines above
   the code that contradicted it.

So the lesson is not "keep docs in sync". It is that **an ADR amendment
describing shipped behaviour must say how it was verified**, because "I observed
cascade-always" and "skip-if-unchanged is not built" are different claims and only
the first was true. The gap between them was a live bug (git-bug `945b1f4`),
recorded as an accepted trade-off for three days.

`force-skip` and `force-cascade` remain genuinely unbuilt, and the "inputs
unchanged" surface exists on the event log (`StepSkipped`) but not yet as a
first-class UI affordance.

## Amendment (2026-07-27) — invalidation compares content identity, not snapshot roots

Skip-if-unchanged asks one question — *did the upstream's content change?* — and
[0061](0061-workspace-data-path.md) s8 makes it ask that question of the right
digest. A Workspace Snapshot now has **two coordinates**:

| | covers | is an address? | answers |
|---|---|---|---|
| **snapshot root** | names, targets, modes, **mtimes** | **yes** — `trees/<hash>` | where are these exact bytes? |
| **content identity** | names, targets, modes | **no** | is this the same content? |

The input signature is built over **identities**. The root stays the one true
address: it is what a dependent materializes, what an Attempt records as its
evidence, and what GC's mark walk starts from — a content identity is a *label*,
never a redirection, and nothing is stored under it.

Two consequences for this ADR's own promises:

- **"Restart the flaky lint step costs nothing downstream" is now true rather
  than aspirational.** It could not be, while a re-run always changed its root.
- **The fallback direction is chosen, and it is cascade.** A snapshot recorded
  before identities existed has none, and the comparison then falls back to its
  root — so such a step re-runs where it might have been skipped. Wasteful, never
  wrong. The reverse default (assume unchanged) would skip a step whose inputs
  really had changed, which is not a slow build but a wrong one.
