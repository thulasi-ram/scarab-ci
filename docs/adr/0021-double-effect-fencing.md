# 0021. Double-effect hazard: fencing tokens + idempotency contract

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

The hardest problem in durable CI: **at-least-once execution meets non-idempotent side
effects.** A step runs `docker push` / `terraform apply`, the effect *succeeds*, then the node
dies **before the orchestrator records "step done."** On resume, at-least-once re-runs it →
**double push / double deploy.** Content-addressing makes *recomputation* cheap and safe but
does nothing for *external* effects. If unaddressed, "durable CI" quietly means "occasionally
double-deploys", which is worse than plain stateless CI that never promised more. We deferred
the Concourse commit protocol ([0002](0002-durability-model.md)), so we need a lightweight
answer that is clear about its limits.

## Decision

**Fencing tokens + an explicit idempotency contract:**

- The orchestrator hands each Attempt a **monotonic fence** (`run`/`step`/`attempt` id) via
  env, and threads it into cooperating external systems (idempotency keys, registry digest
  checks, cloud-API generation/conditional writes). This converts a duplicate effect into a
  **no-op** *for systems that cooperate*.
- **Recomputation** is skipped/cheap via content-addressing ([0029](0029-workspace-cas.md)).
- **Limits:** steps with non-idempotent, non-cooperating effects are the **author's
  responsibility**, and the docs say so. The full exactly-once **commit protocol
  remains reserved in the IR** for later.

## Consequences

- Materially reduces double-effect risk without a new top-level concept.
- The at-least-once contract ([0002](0002-durability-model.md)) is stated to users.
- Ties into retry ([0020](0020-retry-and-failure.md)) and restart
  ([0027](0027-restart-semantics.md)).

## Alternatives considered

- **Pull the commit protocol forward now** — near-exactly-once, +1 concept, reverses [0002].
- **Pure at-least-once, docs only** — biggest footgun; real double-deploys.
