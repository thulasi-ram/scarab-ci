# 0030. Operational defaults

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Several operational concerns have clear, low-controversy answers that follow from earlier
decisions. Recorded together for traceability; each may be split into its own ADR if it grows.

## Decision

- **Step isolation** (follows from soft multi-tenancy, [0005](0005-tenancy-and-k8s-only.md)):
  plain Pods hardened with **Pod Security Standards (restricted)** — non-root, read-only
  rootfs where feasible, seccomp `RuntimeDefault`, **NetworkPolicy default-deny egress**
  (allow-list per step), no service-account token unless requested. **gVisor/Kata pluggable
  via `runtimeClass`**; hostile-tenant isolation deferred. Rootless BuildKit gets privilege via
  user namespaces, not host access.
- **Retention/GC:** configurable per-org/repo TTLs (sane defaults: logs ~30d, artifacts ~90d
  or count-capped, caches LRU by size budget, run *metadata* retained long for audit/lineage);
  durable GC sweeper + object-store lifecycle rules.
- **Notifications:** a `Notifier` **port** with adapters (Slack/email/generic webhook),
  distinct from forge status/checks (the `Forge` port). Fast-follow, not v1-core.
- **Log masking:** best-effort redaction of known secret values from live + stored streams
  (defense-in-depth atop fork-PR lockout, [0014](0014-secrets.md)).
- **Install/distribution:** canonical **Helm chart** first; optional install **Operator** later.
- **Multi-cluster:** *designed-for, not built* — the `Executor` port + version-tagged work
  claiming accommodate a future remote agent; no agent in v1.
- **DR/backup:** Postgres **PITR** (WAL archiving) + object-store versioning/replication +
  a documented restore runbook.
- **API limits:** per-token + per-tenant rate limits at the API edge; execution fairness is
  already covered by per-project concurrency caps ([0011](0011-durable-scheduler.md)).

## Consequences

- Secure, operable defaults out of the box; each item has a clear later-ADR home if it deepens.

## Alternatives considered

- Deferring these entirely — leaves obvious gaps; recording the defaults now avoids drift.
