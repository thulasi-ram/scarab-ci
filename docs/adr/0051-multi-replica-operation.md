# 0051. Multi-replica operation: tail lease, replica-agnostic SSE, shared OIDC key

- **Status:** Accepted
- **Date:** 2026-07-17
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0011](0011-durable-scheduler.md) (leader lease),
  [0013](0013-history-and-observability.md) (log pipeline),
  [0048](0048-fail-closed-startup.md) / [0015](0015-supply-chain-oidc.md) (OIDC key)

## Context

The chart pins `replicaCount: 1`. Most of the control plane is already
replica-safe (admission + timers leader-gated via the Postgres lease;
reconcile/advance use SKIP-LOCKED claiming + optimistic transitions). Three
things break at 2+ replicas (2026-07-16 audit): the **log tailer dedups
in-memory per-process** → the same stdout is ingested by every replica; the
**live-SSE path serves from an in-process broadcast** → a client on replica B
sees no live tail for a step replica A is tailing; and the **OIDC signing key is
regenerated per boot** → each replica publishes a different JWKS.

> **Amended 2026-08-03 by [0066](0066-the-depot-is-a-cache.md) — this ADR is about the CONTROL PLANE
> only.** Every mechanism below (the tail lease, replica-agnostic SSE, the shared OIDC key, the
> leader-gated sweeper) converges replicas **through Postgres**, which is what the Consequences mean by
> *"converged replicas share work via Postgres; no service-to-service RPC"*. The **Data Depot** has
> **no durable core** — it never connects to Postgres — so none of it applies there, and
> `replicaCount: 2+` becoming honest for the server says nothing about the Depot.
>
> **The Depot's multi-replica story is [0066](0066-the-depot-is-a-cache.md)**, and it is a different
> shape for a structural reason: the Depot is **definitionally a cache**, so replicas hold nothing
> unique and HA is *"run more replicas"* rather than a coordination protocol. What it needs instead is
> **fence affinity** — a headless Service, a replica chosen at launch and stamped into the Pod spec,
> hashed by **Run** so every Step of a Run lands on one replica. That is a correctness requirement
> rather than an optimisation the moment N > 1, and 0066 also records the prerequisite: git-bug
> `e140121`, a Depot outage of 20–30 s dead-letters Runs today, which makes any replica count
> meaningless until it is fixed.

## Decision

### One tailer per running step — a claim-to-tail lease

A replica claims "I tail step X" via a **SKIP-LOCKED lease** (the exact pattern
already used for step/outbox claiming); only the claimer tails; on lease expiry
another replica picks it up. This **dedupes** ingestion *and* **distributes** the
log I/O (the heaviest per-step work) across replicas — rather than concentrating
it on the leader.

### Live-SSE reads from the durable index — replica-agnostic

The SSE live path serves new chunks by reading the **durable PG log index** (which
the tailer writes to), so **any replica can serve live logs** regardless of which
one tails. The in-process broadcast is demoted to a **same-replica low-latency
fast-path**, not the source of truth. (Replay already reads durably.)

### Shared, persistent OIDC signing key

The issuer key is loaded from a **persistent source** (SecretProvider/config),
shared across replicas → a **consistent JWKS**. Per-boot regeneration is removed
(also required by ADR-0048's "OIDC enabled ⇒ persistent key or refuse boot").

### GC sweeper is leader-gated

The retention sweeper (ADR-0050) runs under the same leader lease as admission —
one sweeper cluster-wide.

## Consequences

- A tail-lease table + reclaim loop; the SSE handler polls the durable index for
  live chunks; the OIDC key comes from SecretProvider/config.
- `replicaCount: 2+` becomes **honest**; the chart can raise it with guidance
  (converged replicas share work via Postgres; no service-to-service RPC).

## Alternatives considered

- **Leader-pin all tailing:** simplest and correct, but makes the leader a log-I/O
  bottleneck on top of admission/timers/GC. Rejected for the per-step lease.
- **Sticky SSE sessions** (route a client to the tailing replica): fragile,
  breaks on replica churn. Rejected.
- **Postgres `LISTEN/NOTIFY`** for live fan-out: lower latency than polling the
  index, but more moving parts; can be added later if poll latency matters.
