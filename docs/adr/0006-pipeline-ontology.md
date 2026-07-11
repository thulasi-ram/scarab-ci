# 0006. Pipeline ontology: flat recursive DAG

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Incumbents fix a tier count: GHA `Workflow→Job→Step`, Woodpecker `Pipeline→Workflow→Step`,
Concourse `Pipeline→Job→Step`. That middle tier exists because "job = pod, steps =
sequential-in-pod sharing a filesystem." We dissolved that rationale in
[0004](0004-execution-topology.md): **Step is the Pod boundary**, and the workspace passes
via content-addressing. So parallelism no longer needs a "job" tier (it is emergent from the
DAG), and reuse doesn't need two mechanisms.

## Decision

A **flat recursive DAG**:

- **`Pipeline → Step`**, with dependencies expressed directly between steps (`needs:`).
  Parallelism is emergent (non-dependent steps run concurrently, each its own Pod).
- **Composition = recursion:** a Step can `invoke` another Pipeline. This single concept
  replaces "reusable workflows" *and* "composite actions" *and* Woodpecker's "workflow" tier.
  A "job" is just a named subgraph — sugar, not structure.
- **Matrix is an orthogonal modifier** on any Step (fan-out), not a tier
  ([0023](0023-dag-shape.md)).

## Consequences

- Fewer concepts, strictly more expressive power; maps 1:1 onto the durable DAG state machine
  and the pod-per-step executor.
- Unfamiliar to GHA migrants (no `jobs:` key) — mitigated later by a GHA-import shim.
- `invoke` (same-run nesting) is distinct from `on: upstream` (new run);
  see [0025](0025-cross-pipeline-orchestration.md).

## Alternatives considered

- **Two-tier `Pipeline→Job(DAG of Steps)`** — GHA-familiar, but the tier is vestigial once
  step = pod, and reuse still needs a second mechanism.
- **Three-tier `Pipeline→Workflow→Step`** — most redundant given our model.
