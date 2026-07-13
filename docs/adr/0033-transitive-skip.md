# 0033. Transitive skip: a guarded-off step skips its descendants

- **Status:** Accepted
- **Date:** 2026-07-13
- **Deciders:** thulasi.ram (architect)

## Context

Step-level `when:` guards ([0009](0009-dsl-ir-yaml-cel.md)) decide whether a step
runs. The open question ([0027](0027-restart-semantics.md) neighbourhood, deferred
from slice 4) was what happens to a guarded-off step's **descendants**.

The earlier pure-lowering `select_steps` *removed* a `when:false` step and
*dropped* its dependents' edges onto it, so a dependent became a root and ran
anyway. That is surprising: a `notify` step that exists only to run after
`deploy` would still run on a branch where `deploy` is guarded off. It also
diverged from the runtime cascade the engine already had (`dep_dead`: a step with
a terminal-non-success dependency is `Skipped`).

## Decision

**A `when:false` step is `Skipped`, not removed; the full DAG (edges intact) is
preserved, and any step with a `Skipped` need is transitively skipped** — the
GitHub-Actions default `success()` join, consistent with Scarab's stated
GHA-ergonomics ([0007](0007-data-passing-model.md)).

- Run creation keeps every compiled step and marks the guarded-off ones
  `Skipped` (pure `excluded_steps` reports which; the server records the
  transition). Edges are **not** dropped.
- The existing scheduler cascade (`dep_dead`) does the propagation: a `Pending`
  step with any `Skipped` (or otherwise terminal-non-success) need becomes
  `Skipped`. No new cascade logic.
- **A `Skipped` step is not a failure.** Run settlement fails a run only on a
  `Failed`/`Cancelled` step; a run whose only non-successes are skips
  **succeeds**.
- An `always()`-style opt-out (run a descendant even when a need was skipped) is
  a later feature; the default is skip.

## Consequences

- Conditional pipelines behave like GHA: guard off a `deploy` and its `notify`
  tail skips with it, the run still green.
- Compile-time `when:` and the runtime cascade are now one mechanism (mark
  `Skipped`), not two divergent ones.
- `select_steps` (remove + drop edges) is replaced by `excluded_steps` (report
  ids); pruning never drops edges, which is exactly what makes transitive skip
  work.

## Alternatives considered

- **Non-transitive (keep the old behaviour)** — descendants run as roots. Simpler
  but surprising, and splits skip semantics between compile-time and runtime.
- **Skip only when *all* needs were skipped** — lets a step run if any need
  survived; rejected as more surprising than the GHA `success()` default and
  harder to reason about. Revisit alongside `always()`/`if:` conditions.
