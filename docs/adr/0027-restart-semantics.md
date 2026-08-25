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
wrong when it was written**, by twelve days. Admission has compared each step's
recomputed input signature against the one it last consumed since `7ea905d`
(2026-07-12, migration `0013`) — see `Scheduler::tick`, "Skip-if-unchanged": it
skips the match, emits `StepSkipped`, and carries the prior output forward.
`crates/scarab-db-postgres/tests/restart.rs` asserts exactly that, and has passed
throughout.

The amendment is left in place, struck through, rather than deleted, because the
**chronology is the instructive part**, and it is not the one you would guess:

| date | what happened |
|---|---|
| 2026-07-12 | `7ea905d` **ships skip-if-unchanged**, in admission, with tests. |
| 2026-07-24 | `17dace7` declares it "deliberately not built", in this ADR *and* in a doc comment on `rerun_step` — while it was working. |
| 2026-07-27 | `dd67e12` ([0061](0061-workspace-data-path.md) s7) puts mtimes in the tree-hash preimage, and thereby **makes the false claim true in effect**: a re-run can no longer reproduce its own root, so nothing is ever skipped. |

So the amendment was not a stale doc that reality drifted away from. It was
**wrong on the day it was written**, and three days later an unrelated slice
retroactively made it accurate — which is the worst possible way for a false
claim to survive review, because by the time anyone checked the behaviour, the
behaviour agreed with it.

The mechanism of the original error is worth naming, because it is cheap to
repeat: `rerun_step` genuinely does not compare output hashes. It re-arms the
invalidation set to `Pending` and stops. The comparison lives in **admission**,
which decides per step whether a `Pending` step runs. Reading the rerun entry
point and finding no comparison is a correct reading of that function and a wrong
conclusion about the system. The fix is not "keep docs in sync" but: **an ADR
amendment asserting that something is not built must name where it looked** — and
"not built" is a claim about a behaviour, so it needs a behavioural check, not a
reading of one function.

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
