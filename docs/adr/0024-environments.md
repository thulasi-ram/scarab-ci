# 0024. Environments: a built-in entity with protection rules

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

An Environment (staging/prod) is the natural convergence point for deployment governance, and
several features we built already meet there: `gate` (approval/timer), env-scoped secrets,
env OIDC subject, concurrency groups, RBAC approvers, and the forge deployments API.

## Decision

**Environment is its own entity** (in `scarab-projects`) carrying **protection rules**:

- required **approvers** + **wait timer** → realized via `gate` ([0008](0008-step-contract.md))
- **allowed refs/branches**
- **concurrency policy** → concurrency group ([0011](0011-durable-scheduler.md))
- **env secret scope** → ([0014](0014-secrets.md))
- **OIDC subject template** → ([0015](0015-supply-chain-oidc.md))
- **deployment history** + forge deployments link

Governance and security live in one place, which makes Scarab a credible deploy system and
not only a build runner.

## Consequences

- The security features get an auditable home; policy attaches in one place.
- Secrets/OIDC/gates/concurrency are designed to plug into Environment.
- Timing: implemented in roadmap Slice 4; the *concept* is modeled now.

## Alternatives considered

- **Thin named scope** (secrets + OIDC + optional gate only) — fewer concepts, governance
  reinvented per team.
- **No Environment concept** — as minimal as it gets, but no single place for deploy safety.
