# 0023. DAG shape: static matrix v1, dynamic reserved

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Underneath "matrix" is an **invariant** decision: is a run's DAG fully known at submit time,
or can it grow at runtime? A static DAG is a powerful simplifying invariant (bounded state for
the engine + DST, fully drawable UI, complete validation). Dynamic fan-out (a step emits N
children at runtime — monorepo per-package builds, dynamic sharding) is powerful but mutates
the DAG mid-run and expands the state space.

## Decision

- **Static matrix in v1:** cartesian product computed at **submit** → fixed node set, fully
  validatable and visualizable, bounded state.
- **Reserve runtime growth:** shape the IR + engine state model to allow a future runtime
  **`expand`** event, so **dynamic `for_each` fan-out** (`invoke … for_each: <CEL over a prior
  step's Result>`) is a fast-follow, not a rewrite.
- **Fan-in join policies from the start:** `all-success` (default), `all-complete`,
  `any-success`; the join receives legs' Results/artifacts aggregated.

## Consequences

- v1 engine, DST, and UI stay simple (bounded DAG) without cornering dynamic later.
- Dynamic fan-out reuses the `invoke` recursion primitive ([0006](0006-pipeline-ontology.md)).

## Alternatives considered

- **Dynamic fan-out in v1** — unlocks monorepo/sharding now, but mutable-DAG engine +
  growing-state DST + expanding-graph UI before the core is battle-tested.
- **Static only, forbid growth** — simplest forever, forfeits dynamic; reversing later is cross-cutting.
