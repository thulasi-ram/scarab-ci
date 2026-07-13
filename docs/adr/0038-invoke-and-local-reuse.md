# 0038. `invoke` is compile-time inlining; local reuse via `.scarab/lib`, third-party reuse via OCI images

- **Status:** Accepted
- **Date:** 2026-07-14
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0025](0025-cross-pipeline-orchestration.md) (sharpens what `invoke` *is*)
- **Refines:** [0006](0006-pipeline-ontology.md) (composition = recursion), [0008](0008-step-contract.md) (OCI image = the plugin library)

## Context

[0006](0006-pipeline-ontology.md)/[0025](0025-cross-pipeline-orchestration.md) blessed `invoke`
(nest a Pipeline in the same Run) as the single composition/reuse primitive, but never said what
it *is* at run time, and it was never built — `StepSpec` today has only image-steps and `gate`.
Meanwhile users want "reusable steps / modules" and a "plugin ecosystem." Those words collide
with two existing bets: [0008](0008-step-contract.md) already declared *the OCI registry is the
plugin library, zero SDK*, and [0006](0006-pipeline-ontology.md) declared *a "job" is a named
subgraph — sugar, not structure*. The open question is whether reuse needs new machinery (a
registry, an SDK, a runtime sub-pipeline object, a macro system) or whether it falls out of
primitives already chosen. It falls out — once one distinction is made explicit: **reuse across a
trust boundary is a different problem from reuse inside your own repo.**

## Decision

Reuse splits by trust boundary, and each side uses a primitive we already have:

1. **Local reuse (no trust boundary) = `invoke` a Pipeline under `.scarab/lib/`.** `invoke` is a
   built-in Step kind that is **resolved by compile-time inlining, not a runtime object**: at
   `compile_yaml` the referenced Pipeline's Steps are flattened into the caller's DAG. This is
   [0006](0006-pipeline-ontology.md)'s "sugar, not structure" taken literally — there is no new
   durable object; restart, resume, and time-travel work unchanged because the result is just
   more Steps in one flat DAG. `invoke` is **local-only, forever**: a repo-relative path resolved
   inside the repo tree and read **at the caller's ref** (atomic git versioning — no registry, no
   semver). A Library pipeline lives under `.scarab/lib/`, is referenced not triggered, and is out
   of trigger discovery both by convention (subdir; discovery is flat) and by carrying no matching
   `on:`.

2. **Third-party reuse (across a trust boundary) = an OCI image Step** ([0008](0008-step-contract.md)),
   isolated by the container (its own Pod, the rootless posture of `scarab-executor-k8s`) and
   digest-pinned. Reuse across a trust boundary is therefore **step-granular** — an image is one
   Step. To reuse a third-party *subgraph*, you **vendor** it (commit it into your `.scarab/lib/`),
   at which point it is local, git-versioned, and diff-reviewable. `invoke` never reaches across the
   network. Governance of *privileged* images per Environment is deferred to its own ADR (builds on
   [0037](0037-environment-governance.md)).

**Corollaries of compile-time inlining** (all enforced at compile):

- **Termination:** cycles (`A invoke B invoke A`) are rejected and a hard depth cap applies — the
  same "always terminates" discipline [0009](0009-dsl-ir-yaml-cel.md) chose CEL for.
- **Id-namespacing:** inlined step ids are prefixed by the invoke-step id (`deploy/build`; nested
  `deploy/db/migrate`; under matrix `deploy[svc=api]/build`), and `needs` edges rewrite across the
  seam. `/` is reserved in step ids.
- **Path safety:** repo-relative only; must resolve inside the repo tree (no absolute paths, no
  `../` escape, no cross-repo). Cross-repo *causation* remains `on: upstream` (a new Run).
- **No isolation seam — and that is correct.** A flattened lib Step runs in the caller's full
  secret + Environment scope ([0037](0037-environment-governance.md)). This is safe *because*
  `invoke` is structurally never the third-party path: untrusted code always arrives as an
  (isolated, digest-pinned, per-Environment-governable) OCI image. The hazardous combination
  (untrusted code + full scope + no sandbox) cannot be expressed.

- **Vendored code is committed, pinned to the ref — never git-ignored + fetched at run time.**
  Run-time fetch would violate read-at-ref ([0009](0009-dsl-ir-yaml-cel.md)), defeat compile-time
  inlining (source absent at compile → dynamic DAG, refused by [0023](0023-dag-shape.md)), break
  self-describing Runs ([0022](0022-upgrades-and-versioning.md)), and erase the auditability that
  is the whole point of vendoring. It is remote `invoke` in disguise. A git submodule (a pinned SHA
  in the tree) is the only reproducible "don't copy the bytes" variant and is a possible later
  ergonomic, not v1.

## Consequences

- **No new subsystem, no marketplace, no SDK.** The OCI registry is the plugin ecosystem
  ([0008](0008-step-contract.md)); `.scarab/lib` is the module ecosystem (local); vendoring bridges
  them. Maximally consistent with the wedge ("fewer concepts, strictly more power").
- **`invoke` must actually be built** (it does not exist): the step kind, compile-time inlining with
  the corollaries above, and an explicit `inputs:`/`outputs:` interface validated at compile
  (explicit over ambient — consistent with the secret-scope stance in [0037](0037-environment-governance.md)).
- **`matrix` × `invoke` is free:** because `matrix` ([0023](0023-dag-shape.md)) is an orthogonal
  modifier already implemented for any Step, applying it to an invoke-step fans out N copies of the
  whole referenced subgraph — "run this reusable subgraph once per coordinate" — which covers the
  common case people reach a templating language for. Variable graph *shape* per input is **not**
  covered and remains deferred to a possible `data → IR` frontend ([0009](0009-dsl-ir-yaml-cel.md) pt 4).
- **Third-party subgraph reuse costs a commit** (vendoring). Accepted: it buys an airtight,
  reviewable supply chain and keeps `invoke` local-only.

## Alternatives considered

- **A runtime sub-pipeline object** (a StepRun owning child StepRuns) — matches a naive reading of
  [0025](0025-cross-pipeline-orchestration.md)'s "nesting," but introduces a new durable object shape
  that [0006](0006-pipeline-ontology.md) explicitly called "sugar, not structure." Rejected; the
  real runtime lineage boundary is `on: upstream`.
- **Remote `invoke`** (`invoke: github.com/org/repo//lib@sha`) — convenient, but reopens the trust
  seam: un-vendored third-party *composition* flattens into your Run with full scope. Rejected in
  favour of vendoring; cross-repo needs are met by `on: upstream` (causation) + vendoring (composition).
- **A macro / AST-rewriting plugin** — arbitrary code mutating the IR at compile, run in the control
  plane. Rejected: reopens the Turing-tarpit [0009](0009-dsl-ir-yaml-cel.md) closed, is an RCE surface
  on the durable brain, and breaks self-describing Runs. The legitimate need underneath (programmatic
  DAG generation) is reserved for a blessed, total `data → IR` frontend, not a plugin.
- **A typed plugin SDK / marketplace** — already rejected by [0008](0008-step-contract.md) (zero
  starting ecosystem, authoring barrier); nothing here revisits that.
