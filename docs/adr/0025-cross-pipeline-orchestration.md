# 0025. Cross-pipeline orchestration: nest vs trigger

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

There are two different needs that many CIs blur: composing sub-pipelines *within* one run,
and triggering *separate* pipelines (possibly cross-repo, cross-team) on completion.

## Decision

**Two clean primitives:**

- **`invoke` = nest** — a sub-pipeline **in the same Run** (shared lineage, one durable
  lifecycle, one permission scope). Composition/reuse ([0006](0006-pipeline-ontology.md)).
- **`on: upstream` = trigger** — a **new Run**, causally linked (`on: { upstream: { pipeline,
  status } }`). Works **cross-repo** via the same event mechanism, gated by RBAC. The
  causation edge is recorded in the event log ([0013](0013-history-and-observability.md)) for
  lineage/audit ("why did this run start").
- **Monorepo** needs no cross-repo machinery: **CEL path/branch/tag trigger filters**
  (`on: { push, paths: ["services/api/**"] }`).

Mental model: **nest = one run; trigger = new run.**

## Consequences

- Tidy model that composes with everything; lineage is first-class.
- Cross-repo triggering is an RBAC-gated event, not a special subsystem.

## Alternatives considered

- **invoke-only (no cross-run)** — crams unrelated pipelines/teams into one run + one perm scope.
- **Concourse-style resource-triggered cross-pipeline graph** — revives deferred resource
  complexity; heavy for v1.
