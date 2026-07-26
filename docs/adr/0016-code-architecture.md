# 0016. Code architecture: hexagonal + adapter crates + converged binary

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

The wedge is durability correctness, and we want deterministic simulation testing
([0017](0017-testing-strategy.md)). "Swappability" is not unique to hexagonal — the real
distinctions are *which* edges are abstracted (time, executor, crash — not just the DB),
*dependency direction*, and whether purity is enforced **by the compiler** vs by discipline.

## Decision

- **Hexagonal with compiler-enforced purity via crate boundaries.** Pure **domain crates**
  (`scarab-engine`, `-pipeline`, `-forge`, `-identity`, `-secrets`, `-storage`, `-projects`)
  list **no infra crates** in `Cargo.toml` — so `use sqlx::` in the domain is a *compile
  error*, not a review comment. Read that as *no infra*, **not** *no dependencies*: the boundary
  is narrowed by [0031](0031-pure-computation-deps.md), which admits an **I/O-free library**
  when the capability it provides belongs to the domain rather than to a backend. Three members
  today, all in the allowed block of the root `Cargo.toml` with a justification beside each:
  `cel-interpreter` (0031's original case — evaluating a conditional is a domain rule); `sha2`
  ([0029](0029-workspace-cas.md) defines a snapshot **as** the SHA-256 merkle root of its
  canonical tree bytes, so the content address is a domain fact and the digest belongs where
  that fact is defined); and `tracing` (a *facade* — see 0031's amendment). What actually writes
  stays infra: `tracing-subscriber` is installed only by the composition root, and anything
  keyed by a deployment secret (`hmac`) belongs to a boundary. **Adapters are separate vendor
  crates**
  (`scarab-forge-github`, `scarab-db-postgres`, `scarab-storage-s3`, `scarab-executor-k8s/-local`,
  `scarab-secrets-postgres`) holding all infra deps. Ports are `async-trait`s (dyn-safe) so
  fakes (`scarab-testkit`) slot in.
- **Layered *inside* a domain** (services/api modules); the port/adapter line is the enforced
  boundary.
- **Domain-first (vertical-slice) layout:** top-level is bounded contexts; a thin
  `scarab-server` composition root mounts each domain's `api` into one axum app + OpenAPI + SSE.
- **One converged binary, roles splittable:** `scarab-server --role converged|api|scheduler|
  executor|webhook`. Postgres (outbox) is the coordination bus; scale by running roles as
  separate replicas.

## Consequences

- DST is cheap: fakes are just alternate adapter crates ([0017](0017-testing-strategy.md)).
- Single-binary simplicity + horizontal scale without a microservices tax.
- More crates and trait indirection than a flat monolith — accepted for the correctness payoff.

## Alternatives considered

- **Feature-gated adapters inside domain crates** — fewer crates, but domain crate pulls
  optional infra; purity by discipline, not compiler.
- **Modular monolith / layered everywhere** — faster to write, leaky boundaries, DST harder.
