# 0005. Tenancy & deployment; Kubernetes as the only backend

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Deployment shape sets the security boundary and decides whether an agent is needed. Options
span self-hosted in-cluster (simple), hosted brain + BYO customer cluster (needs an agent),
and hard multi-tenant SaaS (hostile-tenant isolation — a company, not a v1). Separately, the
"Woodpecker baggage" we want to shed was clarified to mean its **multiple execution
backends** (Docker + local + …), not that it does too much.

## Decision

**Self-hosted, single control-plane install, with soft multi-tenancy:**

- Forge orgs/repos/teams are first-class **projects** under RBAC; isolation is
  **namespace-per-run**. In-cluster **direct dispatch** (no agent).
- The **executor is a cleanly separable component**, so a future remote agent (BYO cluster)
  is an addition, not a rewrite. Hard hostile-tenant SaaS is deferred indefinitely.

**Kubernetes is the ONLY execution substrate** (explicit non-goal: Docker-socket, local,
SSH, or any other backend). One substrate, done well.

## Consequences

- Simplest credible isolation now; SaaS future not architecturally foreclosed.
- No multi-backend abstraction to build or maintain — less code, sharper product.
- Step isolation posture (hardened Pods, NetworkPolicy) in [0030](0030-operational-defaults.md);
  gVisor/Kata pluggable via `runtimeClass`.

## Alternatives considered

- **Hosted brain + BYO exec cluster now** — mandates an agent + secure channel up front.
- **Hard multi-tenant SaaS** — enormous security surface; wrong first bite.
- **Multi-backend (à la Woodpecker)** — the exact baggage we are shedding.
