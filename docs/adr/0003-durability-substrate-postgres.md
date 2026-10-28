# 0003. Durability layer: build our own, DBOS-style on Postgres (Rust)

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

The orchestrator needs a durable backend. Options: DBOS (Postgres-backed durable
execution, but **no Rust SDK** as of 2026; TS/Python/Go/Java/Kotlin only), Restate
(Rust-native engine, but adds a *second* stateful system alongside the Postgres we still
need, and imposes a workflow-as-code model), Temporal (heaviest ops footprint: it needs its
own cluster, which goes against our "shed baggage" value), or building on k8s CRDs/etcd (etcd is
not a database: object-size limits, no history queries, status-write storms; Argo has hit and
documented this scaling limit).

The deciding factor: because [0002](0002-durability-model.md) rejects deterministic step replay,
a general durable-execution engine would be used for ~20% of its power at 100% of its
operational and conceptual cost. The slice we actually need — a crash-safe DAG state machine
with a reliable work queue and idempotent dispatch — is small and well-understood on Postgres.

## Decision

**Build the orchestrator as a durable state machine on Postgres, Rust-native.** This *is* the
DBOS architecture (durable workflows in Postgres, embedded as a library, no separate cluster)
reimplemented in Rust because DBOS has no Rust SDK. Mechanisms:

- **Store:** Postgres, the *only* stateful dependency for control state (blobs go to an
  object store, [0004](0004-execution-topology.md)).
- **Queue:** `SELECT … FOR UPDATE SKIP LOCKED` work claiming.
- **Safety:** transactional **outbox** + **idempotency keys** for exactly-once dispatch.
- **Leadership:** Postgres advisory locks / Kubernetes `Lease`.
- **Activities:** launching k8s Pods is the "activity"; its result is memoized durably.

## Consequences

- Durability becomes *our* code and our core skill rather than a wrapped dependency, so we
  must prove it works (see [0017](0017-testing-strategy.md)).
- Minimal operational footprint (Postgres + object store), which suits self-hosting.
- `sqlx` in the adapter crate `scarab-db-postgres`; the domain core stays pure.
- We own exactly-once correctness. That is the main risk; crash-interleaving tests mitigate it.

## Alternatives considered

- **Restate** — Rust-native + proven, but 2 stateful systems and a partly-unused programming model.
- **Temporal** — most proven, heaviest footprint, weakest Rust SDK.
- **CRD + etcd operator** — hits etcd scaling walls; needs Postgres anyway.
