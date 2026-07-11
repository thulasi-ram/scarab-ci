# 0019. Local execution: executor-local behind the Executor port

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

A fast local developer loop (run a step/pipeline offline, iterate without commit→webhook→run)
is a big DX win but sounds like a second execution path to build and keep in sync.

## Decision

Ship **local execution in v1**, realized as **another `Executor` adapter**
(`scarab-executor-local`, running the same OCI-image steps via kind/local container runtime)
behind the *same* port the k8s executor implements. One brain (`scarab-engine`), two executor
adapters. Same content-addressed workspace, same step contract, same scheduler.

Also ship `scarab lint` / `scarab validate` (config → IR, schema + CEL + DAG checks) for a
fast authoring loop.

## Consequences

- "Second execution path" collapses to a second adapter, kept in sync **by construction**
  (the domain engine drives both).
- Enables offline development and reproducible local runs.
- The local executor is not a security boundary; it is a dev convenience.

## Alternatives considered

- **v1: lint/validate only; local exec fast-follow** — less v1 surface, slower dev loop.
- **Push-to-run only** — leanest, painful authoring iteration.
