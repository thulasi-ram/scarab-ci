# 0007. Data-passing model: Workspace / Result / Artifact / Cache

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

A step may emit four genuinely different kinds of data with different lifecycles. The classic
mistake (Argo's single "artifact"; Concourse's everything-is-a-"resource") collapses them and
breeds confusion.

## Decision

Keep **four distinct concepts**:

| Concept | Scope | Lifetime | Purpose |
|---|---|---|---|
| **Workspace** | intra-run, on DAG edges | ephemeral | the filesystem steps build on |
| **Result** | intra-run, on DAG edges | ephemeral | small typed values (version, bool) for params/conditionals |
| **Artifact** | output of record | retained (TTL), downloadable | binaries, reports, images |
| **Cache** | cross-run | best-effort, evictable | `~/.cargo`, `node_modules` (keyed) — **not** correctness-critical |

**Workspace is implicit-by-default, explicit-on-demand:** by default a step inherits the
merged content-addressed workspace of its `needs` (GHA ergonomics). It *may* declare explicit
`inputs:`/`outputs:` to get precise cache keys, restrict what flows (safe fan-out), or remap
(Concourse precision). Explicit I/O is exactly what powers skip-if-unchanged
([0027](0027-restart-semantics.md)).

## Consequences

- Clear retention/UI/caching semantics per concept; no reinvention by users.
- Workspace mechanism is per-file merkle CAS ([0029](0029-workspace-cas.md)).
- More concepts to learn than a 2-concept minimal model — deemed worth it.

## Alternatives considered

- **Workspace + Result only** — folds artifacts/caches into "DIY object store"; users rebuild
  retention/cache-keys badly.
- **Explicit I/O everywhere (Tekton)** — precise but verbose ceremony on every step.
