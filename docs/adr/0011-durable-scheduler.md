# 0011. Scheduling: durable admission control

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

There are **two levels of scheduling**. Kubernetes schedules *pods you have already decided
to launch* (node fit, resource requests). It **cannot** express "only one prod deploy at a
time," "cancel the older run when a new commit lands," or "fair-share so one busy repo
doesn't starve others." That decision (*which runs/steps are admitted at all*) is
**admission control**. Much of the CI UX lives there, and so does much of what durability buys us.

## Decision

Build our own **durable admission scheduler** in Postgres, feeding k8s placement:

- **Concurrency groups** — named mutex/queue (`concurrency: deploy-prod`), policy = *queue*
  or *cancel-in-progress* (a durable lock/queue row).
- **Auto-cancel superseded** — a new commit on a ref cancels the older in-flight run (on by
  default for non-deploy pipelines).
- **Cancellation** — mark cancelled → executor SIGTERMs with grace → kill → record terminal
  state. Because it is durable, a run cannot get stuck half-cancelled.
- **Fairness & backpressure** — per-project concurrency caps + global max-in-flight; the rest
  wait durably in queue. Optional priority.

Then: **Scarab admission → k8s placement.** Build the mechanism to support all of it (v1
ships the full set per the grilling decision).

## Consequences

- CI UX (serialize prod, auto-cancel, fair-share) that k8s alone cannot provide.
- The queue/groups are durable rows — trivial given [0003](0003-durability-substrate-postgres.md).
- Cancellation and gates ([0008](0008-step-contract.md)) share the same durable machinery.

## Alternatives considered

- **Phased subset** — FIFO + global cap first; rejected in favor of full v1 scope.
- **Lean on k8s (ResourceQuota/PriorityClass)** — can't express serialize-prod/auto-cancel/fair-share.
