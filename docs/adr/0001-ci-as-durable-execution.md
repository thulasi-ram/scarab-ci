# 0001. CI as durable execution

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

There are many CI systems (GitHub Actions, Woodpecker, Concourse, Tekton, Argo). A new
entrant that is merely "a bit nicer" than an incumbent has no reason to exist. We must be
far better at exactly one thing, and let that constrain every other decision.

The candidate differentiators (forge-centric, k8s-native, better restart/resume UX, durable
execution) are not equally strong. "k8s-native" is an implementation choice (Tekton/Argo own it);
"forge-centric" is positioning (Woodpecker owns it); "restart/resume UX" is a *symptom*.
The only one that is hard to copy, missing from every forge-CI, and that produces the
restart/resume UX as a direct by-product of its structure is **durable execution**.

## Decision

Scarab's main differentiator is **durable execution**: *a pipeline is a resumable, inspectable,
mutable-mid-flight workflow, not a fire-and-forget batch job.* Restart-step, resume,
time-travel, and crash-safe scheduling are **derived** from this, not bolted on. Forge
integration and Kubernetes are *means*, not the point.

## Consequences

- Every subsequent ADR is judged by whether it strengthens durability.
- The hard engineering investment goes into the durable orchestrator and proving its
  correctness (see [0017](0017-testing-strategy.md)).
- Marketing/positioning line: *"Your pipeline is a workflow that survives crashes."*
- We must state the limits of durability plainly (see [0002](0002-durability-model.md),
  [0021](0021-double-effect-fencing.md)) or the claim backfires.

## Alternatives considered

- **Developer-UX as the differentiator** — UX is downstream of durability here; making it primary
  would leave the hard part unproven.
- **k8s purity / forge-centricity as the differentiator** — already owned by incumbents; not defensible.
