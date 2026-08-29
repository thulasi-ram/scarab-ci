# 0068. The infra observer: narrating why a step has no logs

- **Status:** Accepted
- **Date:** 2026-08-29
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0013](0013-history-and-observability.md) (the log tail this sits beside),
  [0047](0047-retry-classification-and-attempt-model.md) (the classes it explains),
  [0059](0059-tick-fault-isolation.md) (the dead-letter it makes legible)

## Context

On the demo instance, a run whose step Pod could not start — the node was too
small, the request too large, the image absent — reported this and only this:

```
step `build`: Infra { never_started: true } — retries exhausted without a verdict
```

…and a Logs pane reading `no output for this try`. A Rust `Debug` string and an
empty box. Kubernetes knew the answer the whole time (`0/3 nodes are available:
3 Insufficient cpu`) and Scarab discarded it at four separate points.

It is tempting to read that as four bugs. It is one: **there is no channel for
infra narration.**

The two channels that exist are both structurally incapable of carrying it.

**Logs are the Pod's stdout.** `Executor::log_stream` is the k8s log endpoint and
nothing else. A Pod that never started never printed, so the log stream is empty
*precisely because of* the condition an operator is trying to diagnose. The
correlation is perfect and backwards: the worse the failure, the less the log
pane says. `log_tail.rs` already detects this (`NOT_READY_GRACE`,
`STUCK_WARN_INTERVAL`) and sends it to `tracing::warn!` — the server's own log,
which no user of the product can see.

**`failure_detail` is terminal and singular.** One string, written once, when a
verdict exists. It cannot describe an attempt that is *currently* wedged, and
the k8s adapter was not populating it for the pod-never-started classes anyway:
`terminal_waiting_class` and `is_unschedulable` both read `.reason` and dropped
`.message`, which is where the entire diagnosis lives.

The in-flight gap is the one that makes the product feel broken. Image-pull and
`Unschedulable` are classified terminal on first observation, so they fail
fast — but a benign-looking wait has **no terminal classifier at all**.
`ContainerCreating` forever, a volume that never mounts, an init container that
hangs: `activeDeadlineSeconds` never fires (`outlived_deadline` needs
`status.start_time`, and an unscheduled Pod has none), so the step sits until the
engine backstop at `default_step_timeout_ms` — an hour — showing nothing.

## Decision

Add a third channel: an **infra observer** that sits outside the step Pod, reads
the backend's account of a launched unit while it is in flight, and appends to
the run's **activity log**.

Outside is forced, not chosen: a Pod that never started cannot report on itself.

### 1. `Executor::infra_condition` — a new evidence port

`InfraCondition { reason, message }` — the backend's stable machine token plus
its human sentence. `Ok(None)` means nothing noteworthy.

**Required on the trait, with no default.** This repo has been bitten twice by
defaulted evidence methods (`artifacts`, 98ea804; `output_identity`, 56220d7):
a decorator that does not override answers the *default* rather than the
executor it wraps, and nothing fails. Both production decorators forward
explicitly, and a test pins that they do.

On Kubernetes the reading is a pure function of the Pod (`pod_infra_condition`),
so it stays fixture-testable and — more importantly — **structurally incapable
of changing a verdict.** When the Pod's own status carries no message, one
best-effort `Event` list fills it in: `FailedScheduling`, `FailedMount`,
`FailedCreatePodSandBox` exist nowhere else. That read needs `list` on `events`
in the Role, and a missing grant degrades the diagnosis rather than failing the
run.

### 2. Emission is not observation

Polling and appending run on deliberately different cadences. One fence is
polled every 30s; an event is written only when the condition **changes** —
once at onset, once when it clears or the attempt ends.

This is load-bearing, not tidiness. The run's event log is walked in full on the
scheduler's hot path: `settle_failed_attempt` scans every event of the run to
count `AttemptStarted` against the retry budget. A per-poll diagnostic would tax
exactly the runs already in trouble, and would get *worse* the longer they
stayed in trouble. A Pod wedged for the full hour costs two rows.

The closing event carries `held_ms` and `observations`, and the UI leads with
duration. The count is an artifact of the kubelet's backoff schedule; "stuck in
`ContainerCreating` for 41m" is the thing an operator acts on.

Change is keyed on `(reason, message)`, not `reason` alone: the same
`FailedScheduling` label carrying first insufficient CPU and later an untolerated
taint is a genuinely different problem, and it is exactly the transition worth
seeing.

### 3. Narrative, never authoritative

Nothing in the engine folds `StepInfraCondition`. Admission, the retry budget and
completion all ignore it and must keep ignoring it. The verdict stays entirely
with `Executor::poll` and the scheduler; the observer only describes.

### 4. The terminal stamp stays

The activity log carries the narrative over time. The **last** word still lands
on `attempts.failure_detail`, via the k8s adapter finally carrying `.message`
into `ExecState::Failed { cause }`. Without it, answering "why did this run die"
would mean scanning the event stream and correlating by fence, and the
dead-letter reason would stay a `Debug` string. `advance` now reads that detail
into the reason it writes.

### 5. Diagnostics are redacted

A backend message quotes registry URLs and auth-failure bodies, and lands in
Postgres and on the API without passing through `LogService::append`. The
`SecretInjectingExecutor` is the only layer holding both the diagnosis and the
redactor, so it scrubs both `cause` and `InfraCondition.message` on the way
through. Messages are also length-bounded before storage.

## Consequences

An operator on the demo box now reads `Unschedulable: 0/3 nodes are available: 3
Insufficient cpu` on the rail while it is happening, on the attempt when it
fails, and in the dead-letter reason when the run dies — instead of an empty log
pane and a `Debug` string.

The Logs tab still shows nothing for these steps, and that is correct: it shows
the Pod's stdout, and there wasn't any. Its empty state now names the cause and
points at it rather than shrugging.

**Not addressed here:** the hour-long silent hang itself. A wedged
`ContainerCreating` is now *visible* within 30 seconds, but it still burns the
full step timeout before failing. The observer is what makes a terminal
classifier for "benign wait that has gone on too long" possible; deciding that
bound is separate work.

**Cost:** one `infra_condition` call per in-flight step per 30s, plus one
`Event` list only when a condition has no message. The observer holds per-fence
state in memory, bounded by the number of currently-wedged steps and retired
when a fence leaves the in-flight set.
