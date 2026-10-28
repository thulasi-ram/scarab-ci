# 0005. Tenancy & deployment; Kubernetes as the only backend

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Deployment shape sets the security boundary and decides whether an agent is needed. Options
span self-hosted in-cluster (simple), hosted brain + BYO customer cluster (needs an agent),
and hard multi-tenant SaaS (hostile-tenant isolation: a company in itself, not a v1). Separately, the
"Woodpecker baggage" we want to shed was clarified to mean its **multiple execution
backends** (Docker + local + …), not that it does too much.

## Decision

**Self-hosted, single control-plane install, with soft multi-tenancy:**

- Forge orgs/repos/teams are modelled directly as **projects** under RBAC; isolation is
  **namespace-per-run**. In-cluster **direct dispatch** (no agent).
- The **executor is a cleanly separable component**, so a future remote agent (BYO cluster)
  is an addition, not a rewrite. Hard hostile-tenant SaaS is deferred indefinitely.

**Kubernetes is the ONLY execution backend** (explicit non-goal: Docker-socket, local,
SSH, or any other backend). We support one backend and do it well.

## Consequences

- Simplest credible isolation now; the architecture does not rule out SaaS later.
- No multi-backend abstraction to build or maintain — less code and a more focused product.
- Step isolation settings (hardened Pods, NetworkPolicy) are in [0030](0030-operational-defaults.md);
  gVisor/Kata pluggable via `runtimeClass`.

## Alternatives considered

- **Hosted brain + BYO exec cluster now** — mandates an agent + secure channel up front.
- **Hard multi-tenant SaaS** — enormous attack surface; the wrong place to start.
- **Multi-backend (à la Woodpecker)** — the exact baggage we are shedding.
