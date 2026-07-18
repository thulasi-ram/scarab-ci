# 0052. Artifacts: a dedicated per-run store, convention-emitted

- **Status:** Accepted
- **Date:** 2026-07-17
- **Deciders:** thulasi.ram (architect)
- **Implements:** [0007](0007-data-passing-model.md) (Artifact concept);
  relates to [0050](0050-retention-and-gc.md), [0049](0049-identity-and-access.md)

## Context

ADR-0007 defines **Artifact** (output of record — binaries, reports, coverage,
images; retained by TTL, downloadable, UI-visible) but there is **no store, no
API, nothing built** (2026-07-16 audit). Artifacts differ from the workspace
(CAS) in lifetime and access: independent ~90d TTL (vs workspace
collected-on-terminal), downloadable by URL, UI-visible, immutable.

## Decision

### A dedicated per-run artifact store — not the workspace CAS

Artifacts are stored as object-store blobs plus a **metadata row**
(`name, size, content_type, run, created_at, ttl`), **name-addressed per run**.
They do **not** reuse the workspace CAS — mixing two lifecycles in one store would
complicate the mark-sweep GC (ADR-0050). The blob bytes *may* be content-addressed
internally for dedup, but an artifact's identity is `(run, name)`.

### Emission — convention + optional globs

Following ADR-0008's filesystem convention: a step writes to **`/scarab/artifacts/`**
and the executor **auto-collects** post-step (like results). An optional
**`artifacts:` glob** names/selects what to publish (ADR-0007 "explicit-on-demand").
No mandatory upload ceremony.

### API + retention

- `GET /v1/runs/{id}/artifacts` (list) and `GET .../artifacts/{name}` (download,
  via a **presigned object-store URL**), **Project-scoped** (ADR-0049).
- Independent per-class **TTL** (~90d or count-capped, ADR-0030/0050).
- **Immutable** once written; provenance/signing (SLSA/cosign, ADR-0015) rides
  later on top of the artifact record.

## Consequences

- An artifact metadata table + object layout; list/download endpoints; a UI
  artifact list on the run page.
- Retention handled by the ADR-0050 sweeper as its own class, decoupled from CAS
  GC.

## Alternatives considered

- **Reuse the workspace CAS:** free dedup, but two lifecycles in one store
  complicate GC and conflate "data on DAG edges" with "output of record".
  Rejected.
- **Explicit-upload-only (GHA `upload-artifact`):** more ceremony; convention +
  optional globs is lower-friction and matches ADR-0007. Rejected.
