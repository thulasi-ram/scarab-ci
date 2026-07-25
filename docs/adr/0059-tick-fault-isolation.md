# 0059. Per-run tick fault isolation + bounded per-run failures

- **Status:** Accepted (implemented 2026-07-25 — see the amendment at the bottom for how the open questions resolved)
- **Date:** 2026-07-21
- **Deciders:** thulasi.ram (architect)
- **Relates:** [0058](0058-runtime-service-containers.md) (its "Fix B" introduced *partial* per-run isolation for `reconcile_services`, and "Fix A" the deadline-bound this generalizes), [0011](0011-durable-scheduler.md) (the durable scheduler tick), [0047](0047-retry-classification-and-attempt-model.md) (retry classification + dead-letter), [0001](0001-ci-as-durable-execution.md) / CONTEXT §7.1 (the "forward progress **or** explicit dead-letter" invariant), [0051](0051-multi-replica-operation.md) (leader/lease), [0056](0056-run-takes-and-attempt-grain-evidence.md) (Takes)

## Context

The scheduler tick (`Scheduler::tick_all`) is the durable heartbeat of the fleet: each cycle it
walks every active Run and drives it forward (reconcile shared services → admit → advance), plus a
few cross-run reconcile passes (`reconcile`, `reconcile_cancellations`, `reconcile_supersessions`).

[0058](0058-runtime-service-containers.md)'s **Fix B** made the *service-reconcile* step per-run
fault-isolated: a `reconcile_services` error for one Run is collected into `tick_all`'s returned
`Vec<(RunId, SchedulerError)>` and the loop `continue`s, instead of aborting the whole tick. That
closed the concrete stall found while dogfooding — a shared-service RBAC/launch error was failing
`reconcile_services` every tick, and the `?` propagation aborted the tick for **all** Runs, not just
the offending one.

But it is a **point fix, not the general property**, and two gaps remain:

1. **`admit` and `advance` still abort the whole tick.** In the same loop `self.admit(run).await?`
   and the later `self.advance(run).await?` still propagate with `?`, as do the cross-run
   `reconcile*` passes. A per-Run error in admission or advancement therefore still starves *every
   other* Run that cycle — the exact fleet-level stall Fix B set out to prevent, merely relocated to
   a different call site.

2. **Swallowed per-Run errors retry forever, unbounded.** A collected `reconcile_services` error is
   logged by the driver and retried next tick with no bound. For a *transient* error (a db blip, a
   since-fixed control-plane bug) that is precisely the resilience we want — it self-heals on a later
   tick. But a *persistent* per-Run error (e.g. a `teardown_service` that always fails) now hot-loops
   indefinitely with no dead-letter — brushing the "forward progress **or** explicit dead-letter"
   invariant (CONTEXT §7.1). This is the same class of gap [0058](0058-runtime-service-containers.md)'s
   Fix A closed for service *launch* errors, by bounding them with the service-startup deadline.

The tick is the forward-progress engine for the whole fleet; its fault-isolation model deserves to be
a deliberate, documented invariant rather than an accident of which call sites happen to use `?`.

## Decision (proposed)

**1. Per-Run fault isolation is a tick invariant.** The per-Run portion of a tick —
reconcile-services, admit, advance for a given Run — MUST be fault-isolated: an error attributable to
one Run is collected and the tick continues to the next Run. **One poison Run can never stall
another Run's progress.** `admit` and `advance` get the same collect-and-continue treatment
`reconcile_services` already has; the returned `Vec<(RunId, SchedulerError)>` carries them all for the
`scarab-server` driver to log (the engine stays pure — it returns the errors, it does not log them).

**2. Genuinely tick-global failures stay fatal (the outer `Result`).** `db.active_runs()` and
cross-run passes *not* attributable to a single Run legitimately abort the tick and retry next cycle —
they are infrastructural, not per-Run poison. Where a cross-run pass *can* attribute a failure to one
Run's data, it should isolate that Run rather than abort.

**3. Persistent per-Run failures are bounded → dead-letter.** A per-Run error that recurs across a
bounded window MUST eventually dead-letter that Run with a distinct diagnostic, restoring §7.1 at the
per-Run grain. Reuse an existing bound rather than inventing one — a **wall-clock deadline**
(consistent with Fix A's `service_ready_timeout_ms` + the `Clock` port, and tick-frequency
independent) is the leading candidate, with the outbox `MAX_DELIVERY_ATTEMPTS` count model as the
alternative.

## Consequences

- "One Run cannot stall the scheduler" becomes a real, testable guarantee — provable by the
  crash/poison-interleaving tests the durability wedge already licenses (CONTEXT §8).
- A small **durable per-Run failure signal** is needed (timestamp of first consecutive failure, or a
  counter) to drive the bound — new Run state.
- The driver's per-tick log enumerates the isolated per-Run failures; a Run that dead-letters from
  *persistent tick failure* needs a diagnostic clearly distinct from a step dead-letter.
- More error plumbing inside `tick_all`; the outer-`Result`-vs-inner-`Vec` split must stay legible so
  "fatal tick failure" and "isolated per-Run failure" never blur.

## Open questions (to grill)

- **Bound: wall-clock deadline vs consecutive-tick count?** Wall-clock is frequency-independent and
  matches Fix A; a count is simpler but couples the bound to the tick interval.
- **Where does the per-Run failure signal live durably** — new column(s) on the run row, or reuse the
  dead-letter / outbox machinery?
- **Do the cross-run reconcile passes** (`reconcile`, `reconcile_cancellations`,
  `reconcile_supersessions`) need per-*item* isolation too, or is abort-and-retry acceptable for them?
- **Multi-replica ([0051](0051-multi-replica-operation.md)):** is the failure signal per-replica or
  durable/shared, and how does isolation interact with the per-step lease?
- **Classification:** should a persistent per-Run tick failure reuse [0047](0047-retry-classification-and-attempt-model.md)'s
  `FailureClass` / dead-letter path, or is it a distinct "run stuck" terminal outcome?

## Alternatives considered

- **Fail-fast whole tick (the pre-Fix-B status quo)** — simplest, but one bad Run halts the entire
  fleet. Unacceptable for a multi-tenant scheduler.
- **Per-Run isolation without a bound (Fix B's shape, generalized to admit/advance)** — resilient to
  transient errors but hot-loops on persistent ones and never dead-letters; violates §7.1.
- **Isolate only `reconcile_services` (today's state)** — the point fix; leaves `admit`/`advance`
  able to stall the fleet, and leaves persistent per-Run errors unbounded.

## Amendment (2026-07-25) — built; open questions resolved

Implemented in `scarab-engine/src/scheduler.rs` (`tick_all`, `isolate_run_failure`,
`TickHealth`) with acceptance in `crates/scarab-server/tests/tick_isolation.rs`. How each open
question landed:

**Bound: wall-clock, not a tick count.** `tick_failure_deadline_ms`, default 5 min — the same
number and shape as [0058](0058-runtime-service-containers.md)'s `service_ready_timeout_ms`, and
independent of tick frequency, so re-tuning the interval can't silently re-tune the bound.

**The failure signal is IN-MEMORY per replica** (`TickHealth`, threaded like
[`Supervision`](0056-run-takes-and-attempt-grain-evidence.md)) — deliberately *not* durable, which
reverses this ADR's own Consequences note. A durable marker is only safe if a matching "recovered"
marker is written on the healthy path too; skip it and a stale marker from a long-healed episode
makes the next single blip look like a deadline-crossing streak and dead-letters a healthy Run.
Writing the recovery marker instead puts an append on every clean tick of every Run — a permanent
cost on the hot path to sharpen a bound that only matters for Runs already broken. In-memory fails
the other way: a restart or failover resets the streak, so a poison Run dead-letters *later* than
the bound, never sooner. A control plane restarting faster than 5 minutes is a louder problem than
a late dead-letter. Revisit only if a real poison Run is observed surviving repeated failovers.

**Cross-run passes: per-*item* isolation for the teardown drains.** `reconcile_cancellations` and
`reconcile_supersessions` now isolate per outbox message, because they could genuinely wedge the
fleet *permanently*: a `SUPERSEDE_TEARDOWN` payload that no longer deserializes made `BadPayload`
abort the tick — for every Run — on that cycle and every future one. Failed *processing* now rides
ADR-0047's existing `MAX_DELIVERY_ATTEMPTS` ceiling (`abandon_poison_message`), which retires the
message with a `TeardownAbandoned` diagnostic and, like `settle_teardown`, never touches the Run's
own state. `db.active_runs()` and the outbox claim itself stay tick-fatal — infrastructural, per
decision 2.

**Classification: reuse the existing dead-letter path.** `dead_letter_run` with a reason naming the
tick and the bound, so it reads distinctly from a step dead-letter without a new terminal state.
This forced one substantive change: dead-lettering now treats its *step-cleanup* preamble as
best-effort. A Run dead-lettered **because its own reads keep failing** was otherwise
undead-letterable — `steps_of_run` failed, the dead-letter propagated, the bound never fired and the
hot-loop returned. An explicit verdict the operator can see beats a tidy one they never get; a
cleanup failure is appended to the reason instead, and leftover non-terminal step rows are inert
once the Run is terminal (nothing launches from them, and fencing protects durable state anyway).

**Multi-replica:** the signal is per-replica, as above; the pre-existing leader lease already means
one replica does admission at a time, so streaks don't interleave in practice.
