# 0008. Step contract: OCI image + convention + built-in kinds

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

The step boundary decides the size of the plugin ecosystem on day one. A typed SDK/ABI gives
richer plugins but starts the catalog at zero; WASM steps are sandboxed but fight the reality
that CI runs `docker build`, `cargo`, `gradle` — native toolchains, not WASM.

## Decision

**A step is any OCI image + command, with a thin filesystem/env I/O convention** — so the
entire container ecosystem is the plugin library on day one, zero SDK required:

- Workspace at `/workspace`; params as `SCARAB_PARAM_*` + `params.json`; secrets as **tmpfs**
  files under `/scarab/secrets/`; results written to `/scarab/results/*.json`; artifacts to
  `/scarab/artifacts/`; logs via stdout/stderr.
- An **optional** injected `scarab` CLI for structured emit (results/annotations/progress) —
  never required.

Plus first-class **built-in step kinds**: `clone` (forge-aware checkout), `invoke` (call
another Pipeline — the recursion primitive), and **`gate`** — a **durable suspend** point
(human approval / timer / external event). Gate is where the wedge pays off: suspend for
seconds or weeks at ~zero cost because state lives in Postgres.

> **Amended by [0058](0058-runtime-service-containers.md):** `service` was originally listed
> here as a step kind too. It is **not** a `needs`-able DAG node — a service is infrastructure
> *for* a Step, not a Step. It is a co-located **Sidecar service** (a field on the Step) or a
> Run-scoped **Shared service** (opt-in via `uses:`). See 0058.

## Consequences

- Massive ecosystem from day one; low barrier.
- `gate` turns approvals/waits from a wart into a native primitive; underpins Environments
  ([0024](0024-environments.md)).
- Fencing tokens are injected into the step env ([0021](0021-double-effect-fencing.md)).

## Alternatives considered

- **Typed plugin SDK/ABI** — richer/safer, but zero starting ecosystem, real authoring barrier.
- **WASM component steps** — great for pure logic/policy as a *secondary* kind; wrong as primary.
