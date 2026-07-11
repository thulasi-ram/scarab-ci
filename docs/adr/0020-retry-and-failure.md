# 0020. Retry & failure taxonomy

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

The most important distinction — which almost no CI models — is **infra failure vs step
failure**. On k8s, infra failures (eviction, OOM, spot reclaim, image-pull backoff) are
*common* and safe to retry; step failures (process exit ≠ 0) mean the user's code failed and
retrying usually just burns minutes and hides flakiness.

## Decision

- **Infra/transient failure → auto-retry with backoff, by default.**
- **Step failure → retry is opt-in** (`retry: { on: failure, max: N }`).
- **Timeouts** per step, per run, and per gate.
- **Poison steps / stuck runs → max-attempts → dead-letter** with diagnostics. The scheduler
  **guarantees forward progress or explicit dead-letter** — never an infinite loop.
- **Cancellation** is graceful: SIGTERM + grace period → SIGKILL.

## Consequences

- Robust against normal k8s churn without wasting cycles on doomed user code.
- Interacts with the double-effect hazard: retries invoke fencing
  ([0021](0021-double-effect-fencing.md)).
- Dead-letter is a first-class terminal `RunStatus`.

## Alternatives considered

- **Opt-in retry only, lean on manual restart** — every spot reclaim breaks a build until a human acts.
- **Uniform auto-retry any failure N times** — re-runs doomed code, hides flakiness, worsens double-effect.
