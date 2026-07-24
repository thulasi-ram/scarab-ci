# 0017. Correctness & testing strategy

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

The wedge is durability, which argues for proving crash-safety rigorously. But heavy upfront
testing slows iteration and couples tests to implementation. The team's philosophy: classical,
lean, grow-from-bugs (see memory `testing-philosophy`).

## Decision

**Classical (Detroit-school), lean, regression-driven:**

- Exercise **real collaborators in-process**; **mock only true externals** at the adapter
  boundary (forge HTTP, cloud, object store). Treat Postgres as a real collaborator
  (testcontainers), not a mock.
- **Minimal in v1** — enough integration tests per layer to catch glaring bugs; **grow the
  suite from real bugs** (every fix leaves a regression test). E2e per layer + a few genuine
  cross-layer e2e. Don't fight test infra; keep velocity.
- **Wedge exception:** 2–3 targeted **crash/resume** integration tests (kill the engine
  mid-DAG against real Postgres; assert exactly-once resume) guard the one path that must not
  break. *Not* a full simulation framework in v1.

The hexagonal ports ([0016](0016-code-architecture.md)) keep the door open: `FakeClock` /
`InMemoryDb` / `FakeExecutor` in `scarab-testkit` are ordinary test doubles that *also* enable
deterministic simulation testing later, at low marginal cost, if/when the wedge demands it.

## Consequences

- Fast iteration; test surface tracks real risk.
- The durability path still gets deliberate crash coverage.
- DST is available but not mandated up front.

## Alternatives considered

- **Full DST harness in v1** — proves the wedge, but upfront harness cost against velocity.
- **e2e-first, defer rigor** — fastest, weakest durability assurance for a durability product.
