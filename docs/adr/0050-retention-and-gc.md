# 0050. Retention & GC: mark-sweep CAS, lifecycle-keyed eligibility

- **Status:** Proposed
- **Date:** 2026-07-17
- **Deciders:** thulasi.ram (architect)
- **Implements:** [0030](0030-operational-defaults.md) (retention/GC defaults);
  builds on [0029](0029-workspace-cas.md), [0007](0007-data-passing-model.md)

## Context

ADR-0030 committed the *what* (per-org TTLs, a durable sweeper, object-store
lifecycle) but not the *how*, and the audit found **nothing built** — runs,
events, log chunks, workspace CAS objects, and artifacts grow forever. The hard
part is the workspace CAS: it is content-addressed and **shared across runs**
(the dedup win, ADR-0029), so an object cannot be deleted while any live run
references it. Note the CAS holds the **Workspace** (correctness-critical
intra-run data on DAG edges), *not* the separate evictable **Cache** concept
(ADR-0007) — so CAS GC is a correctness concern, not best-effort eviction.

## Decision

### CAS GC = mark-sweep (not refcount)

Periodically walk the workspace roots of all *reachable* runs, mark reachable
objects, sweep the rest — the git/Nix/BuildKit model.

- **Robust over precise:** a missed mark only *delays* collection; refcounting a
  distributed CAS risks a missed increment *deleting live data* (corruption).
- **Grace window:** never sweep objects younger than a grace period, so an
  in-flight run whose root is not yet recorded is not collected.

### Eligibility is keyed on run *lifecycle*, not wall-clock alone

- The mark set = **all non-terminal runs + terminal runs still within TTL**. A
  **non-terminal run — including one suspended on a gate for weeks — is never
  GC-eligible**, regardless of age. This protects the durable-gate wedge: a
  resumed run always finds its workspace intact.
- A terminal run's workspace CAS becomes collectable once the run is terminal
  **and** past its TTL.

### Per-class retention

- **Logs** ~30d, **artifacts** ~90d (or count-capped) — per-run, time-based
  (ADR-0030 defaults; configurable per-org/Project).
- **Workspace CAS** — collected by mark-sweep once its run is terminal+aged.
- **Run metadata** (state row + event-log summary) — **retained long** for
  audit/lineage, even after logs/workspace/artifacts are pruned.

### Sweeper authoritative; S3 lifecycle is a backstop

The durable **sweeper** is authoritative for reference-coupled data (workspace
CAS, run metadata) — only it knows run reachability. S3 **lifecycle rules** are
an *optional cost backstop* for purely per-run time-based blobs (logs/artifacts),
never the primary (S3 cannot see run references).

## Consequences

- A durable GC sweeper loop (leader-gated, like admission) + a CAS reachability
  walk; per-class TTL config on Org/Project; retention columns/indexes.
- Bounded storage growth without corrupting live or suspended runs.
- Object-store lifecycle rules become an optional deployment-time cost tuning.

## Alternatives considered

- **Refcount CAS GC:** precise/incremental, but distributed refcount bugs delete
  live data. Rejected for mark-sweep's self-healing robustness.
- **Wall-clock-only eligibility:** simple, but would collect a suspended run's
  workspace and break resume. Rejected — eligibility must read run lifecycle.
