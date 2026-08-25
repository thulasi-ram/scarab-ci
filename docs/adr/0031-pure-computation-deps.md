# 0031. Purity means no I/O, not no dependencies: pure-computation crates allowed

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0016](0016-code-architecture.md) (clarifies its "compiler-enforced purity" boundary)

## Context

[0016](0016-code-architecture.md) and CONTEXT.md invariant 4 forbid infra in the pure
domain crates, illustrated by an *allowlist* of four crates
(`serde`/`serde_json`/`thiserror`/`async-trait`). Slice 2 needs **CEL** for `when:`,
interpolation, and matrix predicates ([0009](0009-dsl-ir-yaml-cel.md)). A CEL evaluator
(e.g. `cel-interpreter`) is **pure computation** — total, typed, sandboxed, no I/O — yet a
literal four-crate allowlist would ban it, forcing an awkward port+adapter around what is
really just a function `(&str, &Context) -> Value`.

## Decision

**The purity boundary is *no I/O and no infra*, not *no dependencies*.** A crate may live in a
pure domain crate (`scarab-engine`/`-pipeline`/`-forge`/`-identity`/`-secrets`/`-storage`/
`-projects`) iff it performs **no I/O and pulls no infra**: specifically it must not touch the
network, filesystem, clock/RNG, processes, or an async runtime, and must not (transitively)
depend on `sqlx`, `kube`, `reqwest`, `object_store`, `axum`, `tokio`, cloud SDKs, or similar.
Pure, total, deterministic computation libraries (CEL evaluation, hashing, parsing such as
`serde_yaml`) are allowed.

- The compiler still enforces the important edge: an **infra** crate named in a pure crate's
  manifest remains a build-time bug — that property is unchanged.
- Adding a pure-computation dependency is a deliberate act: it must be justified in the PR/commit
  and, ideally, be `#![forbid(unsafe_code)]`-friendly and free of ambient I/O.
- Determinism for DST ([0017](0017-testing-strategy.md)) is preserved: no wall-clock/RNG means
  fakes still fully control the world.

### Amendment (2026-07-27): facades, not just computation

"Pure, total, deterministic *computation*" covers CEL, hashing and parsing, but it does not
describe a **facade** — a crate that performs no I/O because, with nothing installed behind it,
it does nothing at all. `tracing` is the case: without a subscriber every macro compiles away,
and the subscriber is installed by the composition root. It touches no network, filesystem,
clock, process or runtime, so it passes the I/O test on the merits while failing the
"computation" wording on a technicality.

**A no-op-by-default facade is admissible on the same terms as pure computation**, with one
addition: the *backend* behind it is still infra and stays out. So `tracing` is allowed in a pure
crate; `tracing-subscriber`, which actually writes, is not.

The motivating case is `scarab-storage`'s `TieredCas` ([0061](0061-workspace-data-path.md)): a
warm-tier write failure must **succeed** the caller, because the cold tier is the durability
promise — so the entire observable behaviour of that branch is a log line. Denying the domain a
facade would have forced the choice between swallowing the failure silently (the "silent facade"
this codebase refuses) and inventing a port + adapter whose only implementation writes to
stderr. Neither is architecture; both are ceremony.

The same reasoning does **not** extend to metrics or error-reporting clients that buffer, batch,
or open a socket even nominally — those are backends wearing a facade's name, and the I/O test
catches them.

## Consequences

- CEL lives in `scarab-pipeline` directly (no port/adapter ceremony for a pure function).
- The "allowlist of four" in older docs is illustrative, not exhaustive; this ADR is the rule.
- Reviewers judge new domain deps by the I/O/infra test above, not by a fixed list.

## Alternatives considered

- **Keep the literal four-crate allowlist; wrap CEL behind a port + adapter** — preserves the
  letter of 0016 but adds indirection and a dyn boundary around pure computation; rejected as
  ceremony without benefit.
- **Feature-gate CEL inside the crate** — reintroduces optional-infra-in-domain, the very thing
  0016 rejected.
