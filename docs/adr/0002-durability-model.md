# 0002. Durability model: durable DAG orchestrator + at-least-once steps

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

"Durable execution" engines (DBOS, Temporal) derive their power from **deterministic replay**
of workflow code, with side effects wrapped in idempotent, memoized activities. But a CI
step is **arbitrary side-effecting user shell in a container** (`docker push`,
`terraform apply`). You cannot transparently replay the *interior* of such a step. Believing
otherwise ("resume a `docker build` from the middle") would poison the design.

## Decision

Draw the durability boundary explicitly:

- **Durable & exactly-once:** the DAG state machine (which steps ran, exit codes, output
  manifests) and the append-only event log.
- **Not durable / not replayable:** the step interior. A step is an **opaque black box,
  re-executed wholesale** on restart.
- **Execution contract: at-least-once** per step. Idempotency of *external effects* is a
  **step-author contract** for which we provide tooling (see
  [0021](0021-double-effect-fencing.md)), not a magic guarantee.

Therefore: **Resume** = re-drive the DAG from the last durably-recorded state (finished steps
stay finished, zero user work). **Restart step** = re-run the black-box container.

We explicitly **do not** adopt Concourse's "resource" (put/get) model as a core concept; the
output-commit protocol is reserved in the IR for later, not built now.

## Consequences

- The engine is a *bounded* state machine, not a general workflow runtime — which is why we
  can own it on Postgres ([0003](0003-durability-substrate-postgres.md)).
- Restart/skip semantics ([0027](0027-restart-semantics.md)) hang off the recorded step
  outputs, not off replay.
- Documentation must state the at-least-once contract prominently.

## Alternatives considered

- **Output-commit / resource protocol now** — more exactly-once power, +1 major concept; deferred.
- **Full workflow-as-code replay (Temporal-style)** — wrong impedance for opaque shell steps.
