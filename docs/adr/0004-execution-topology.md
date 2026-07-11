# 0004. Execution topology: pod-per-step + content-addressed workspace

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

How does a step physically run on k8s? The wedge is *durable, individually-restartable*
steps. Pod-per-*pipeline* (Woodpecker: one long-lived pod, steps are execs sharing a
filesystem) makes "restart one step" awkward and pins the whole pipeline to one node,
killing cross-node parallelism and scale-to-zero. Pod-per-*step* makes a step a 1:1
re-creatable k8s object — but then the workspace must survive across pods.

## Decision

**Pod-per-step**, with the workspace passed as **content-addressed snapshots in object
storage** (per-file merkle CAS, [0029](0029-workspace-cas.md)):

- A step ≈ pure: `inputs (workspace-hash + params) → outputs (new workspace-hash + artifacts)`.
- **Restart step** = recreate that Pod (clean, addressable). **Skip-if-unchanged** caching
  and **smart invalidation** ([0027](0027-restart-semantics.md)) fall out of the same
  content-addressing.
- **Direct dispatch, no polling agents** — the executor creates k8s objects itself.
- **Control-plane / data-plane split via the outbox:** the durable brain records "step S
  should run"; a separate executor-reconciler observes it, creates the Pod, watches it,
  writes back terminal state. Postgres is the bus — no brain↔executor RPC.
- **Bare Pods (or Job `backoffLimit=0`); the orchestrator owns retries** — single source of
  truth for retry logic ([0020](0020-retry-and-failure.md)).

## Consequences

- Requires an **object store** (S3/MinIO) — the second (and last) stateful dependency; needed
  anyway for logs/artifacts/cache.
- Per-step snapshot/restore latency, mitigated by node-local CAS caching + lazy fetch.
- Cross-node parallelism and scale-to-zero for free.
- The executor is a clean seam → a future remote agent (BYO cluster) is an added adapter,
  not a rewrite ([0005](0005-tenancy-and-k8s-only.md)).

## Alternatives considered

- **Pod-per-pipeline + shared volume** — fast sequential, but fights restart-step, node-pinned.
- **Pod-per-stage (hybrid)** — adds a "stage" concept; restart granularity coarsens to a stage.
