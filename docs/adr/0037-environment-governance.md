# 0037. Environment governance: scoped secrets, approval-as-gate, admission at point-of-use

- **Status:** Accepted
- **Date:** 2026-07-13
- **Deciders:** thulasi.ram (architect)

Refines [0024](0024-environments.md) (which modeled the concept) and wires it to
[0008](0008-step-contract.md) gates, [0014](0014-secrets.md) secret scopes, and
[0034](0034-external-gate-token.md) gate release. Closes the stored-but-not-enforced
gaps left after Slice 4.

## Context

[0024] made Environment a first-class entity carrying protection rules, but the
implementation left real gaps: `ProtectionRules.approvers`/`wait_timer`/`secret_scope`/
`oidc_subject` are **stored and never read** at runtime; deploy admission trusts a
**caller-supplied** `approvals` list (`DeployRequest.approvals`) rather than any
authenticated state; secret lookup is **exact-scope only** (no `org→repo→env`
inheritance despite [0014]); and env-scoped secret **resolution is never wired into
the live run path** (`resolve_step_secrets` is test-only). A `manual` gate, meanwhile,
is a genuine durable suspend point but records **no approver identity**.

The value of Environment is not any single rule. It is that **one reference —
`environment: prod` — pulls in a governed bundle (approval + secrets + allowed-refs +
OIDC subject), defined once by an admin and reused across pipelines.** That removes the
per-pipeline wiring taxation, and — because rules live in an admin-owned entity, not the
pipeline YAML — it gives separation of duties: a pipeline author *requests* a deploy; the
environment *governs* it, regardless of what the YAML says (the GHA model).

## Decision

### A. Environment is the governed bundle; protection is optional

- Environment stays a first-class entity in `scarab-projects`, edited only with
  `Administer` RBAC. A pipeline **references** it by name (`PipelineIr.environment`,
  pipeline-level: **one run targets one environment** — step-level multi-env deferred).
- **Protection rules are optional per environment.** An environment with no rules
  (typical `staging`) is a frictionless label: secrets are scoped to it, but no
  approval, no ref restriction. An environment with reviewers (`prod`) is gated. Same
  entity, opt-in governance — low-stakes targets pay nothing.

### B. Approval is a gate; admission runs at the point of use

- **Approval = gate release, recorded on the run's event log.** No separate approvals
  table. A new event `GateApproved { step, by }` records each approver's authenticated
  `Principal.subject` (fixes the dropped identity in `approve_gate`).
- A `manual` gate **accumulates** distinct approver subjects. The run stays `Suspended`
  until every name in the environment's `approvers` has approved; then `GateReleased`
  fires and the run resumes (exactly-once, as today). `min_approvals`/N-of-M is a later
  additive threshold.
- **Admission moves into the engine.** `allowed_refs` is checked at **run creation**
  (reject a disallowed ref before any pod). `approvers` is satisfied by accumulated gate
  approvals — not by a request body. The standalone `POST …/deploy` admission path is
  **retired**; `deploy_environment` is demoted to read-only deployment history.
- `admits(git_ref, approvals)` keeps its signature; `approvals` is now sourced from
  `GateApproved` events on the run, never from the caller.

### C. Secrets: scoped, inherited, resolved at the point of use

- Lookup walks the scope chain **`env → repo → org`, most-specific wins** (implements
  [0014]'s inheritance; today's store is exact-match only). A value shared across
  environments lives **once** at repo/org scope; only values that genuinely differ are
  stored per-environment.
- Pipelines reference secrets **by key, environment-agnostically** (`${{ secrets.X }}`).
  Resolution picks the value by the run's target environment — no `_staging`/`_prod`
  naming convention baked into the pipeline.
- Env-scoped secrets are **resolved on the live run path** and injected only under an
  admitted deployment (wires `resolve_step_secrets`): registered with the log redactor
  before any step output streams, and subject to the fork-PR lockout (`fork_policy`).
- A referenced key that resolves to nothing at **every** scope is a submit/deploy-time
  error (see D), not a silent empty.

### D. Secret completeness is usage-driven, not parity-driven

- The **actionable** check: at submit/deploy, for the target environment, resolve every
  key the pipeline *references* through the scope chain; flag any that resolve to
  nothing. Bounded by pipeline usage — independent of environment count, zero false
  positives from intentional divergence.
- Cross-environment parity (keys × environments) is **advisory UI only**: it shows
  **effective** status per cell — *set here / inherited / unset* (post-inheritance, so
  inherited keys never read as missing) — with an explicit **"intentionally unset"**
  marker to silence a cell. It never blocks a deploy.

### E. Management API surface (`Administer` for writes)

```
PUT    /v1/environments/{project}/{name}            upsert env + protection rules
GET    /v1/environments/{project}                   list environments
GET    /v1/environments/{project}/{name}            get one (rules + effective secret status)
DELETE /v1/environments/{project}/{name}            remove
GET    /v1/environments/{project}/{name}/deployments deployment history (was the deploy handler)
GET    /v1/projects/{project}/secrets/matrix        keys × envs effective-status read model (UI)
```

Secret CRUD stays the existing scoped endpoints ([0014]). Gate approval stays
`POST /v1/runs/{id}/gates/{step}/approve` (RBAC `Write` **and** membership in the
environment's `approvers`); `external` release stays token-authed ([0034]).

## Consequences

- Deploy approval becomes durable, auditable, and unforgeable: it is authenticated gate
  state on the event log, not a caller assertion. Restart-safe by construction.
- Prod credentials exist **only** under the gated environment and cannot be relocated by
  editing pipeline YAML — the separation-of-duties boundary is real.
- Pipelines become portable across environments; the target, not the pipeline, selects
  secret values.
- New surface: `GateApproved` event + gate accumulation; a scope-chain resolver; the
  live-path secret wiring; four env-management endpoints + one read model. Retires one
  handler's admission role.
- Pipeline-level environment means no staging→prod promotion *within one run*; that
  requires step-level environment, deferred to a future ADR.

## Alternatives considered

- **Dedicated approvals table** — easier to *read*, but a run must still durably *pause*
  for approval, which only the gate does; a table beside the gate is a dual-write
  consistency hazard. Rejected in favor of gate-as-record.
- **Keep caller-supplied approvals** (authenticated per entry) — smallest change, but
  never unifies with the gate and leaves approval off the durable log.
- **Flat secret store with naming conventions** (`key_staging`/`key_prod`) — pushes
  environment selection into pipeline string names, breaking portability.
- **Cross-environment parity as a blocking check** — explodes at N environments and
  fires on intentional divergence; demoted to advisory.
- **Step-level environment** (GHA-style, many envs per run) — enables single-run
  promotion but needs a transitive-`needs` ancestry walk to scope approvals per step;
  deferred.

## Resolved

- **Naming.** `env:` (step OS-env vars) and `environment:` (deploy target) collide in the
  DSL. GHA ships the identical collision and it is tolerated in practice; **we keep
  `environment`** for the deploy target. Revisit only if the collision proves confusing.
