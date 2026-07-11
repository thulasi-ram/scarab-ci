# 0024. Environments: first-class with protection rules

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

An Environment (staging/prod) is the natural convergence point for deployment governance, and
several primitives we built already meet there: `gate` (approval/timer), env-scoped secrets,
env OIDC subject, concurrency groups, RBAC approvers, and the forge deployments API.

## Decision

**Environment is a first-class entity** (in `scarab-projects`) carrying **protection rules**:

- required **approvers** + **wait timer** → realized via `gate` ([0008](0008-step-contract.md))
- **allowed refs/branches**
- **concurrency policy** → concurrency group ([0011](0011-durable-scheduler.md))
- **env secret scope** → ([0014](0014-secrets.md))
- **OIDC subject template** → ([0015](0015-supply-chain-oidc.md))
- **deployment history** + forge deployments link

One coherent governance/security surface — a credible deploy system, not just a build runner.

## Consequences

- The security features get an auditable home; policy attaches in one place.
- Secrets/OIDC/gates/concurrency are designed to plug into Environment.
- Timing: implemented in roadmap Slice 4; the *concept* is modeled now.

## Alternatives considered

- **Thin named scope** (secrets + OIDC + optional gate only) — fewer concepts, governance
  reinvented per team.
- **No Environment concept** — maximal minimalism, no cohesive deploy-safety surface.
