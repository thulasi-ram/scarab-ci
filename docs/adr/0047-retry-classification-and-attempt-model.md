# 0047. Retry classification, attempt/fence model, and dead-letter (implements 0020)

- **Status:** Accepted
- **Date:** 2026-07-16
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0020](0020-retry-and-failure.md) (retry & failure taxonomy)

## Context

ADR-0020 set the *policy* (infra auto-retry, step retry opt-in, timeouts,
dead-letter, forward-progress guarantee) but it is entirely **unimplemented**
(2026-07-16 audit): `finish_attempt` with the `Infra` branch is dead code
(`engine/src/lib.rs:588-618`), the scheduler hardcodes every failure as
`FailureKind::Step` (`scheduler.rs:635`), `retry` is not expressible in the IR,
there are no step/run/gate timeouts, and `DeadLettered` is a legal state that no
code produces. This ADR is the concrete, buildable model, and it refines two
places where ADR-0020's policy was too coarse.

## Decision

### Classification lives at the Executor port

A step is an opaque black box (ADR-0002), so only the **executor adapter** can
classify a failure — it alone observes the *execution conditions* around the box.
The `Executor` port surfaces a **`FailureClass { Infra, Step, Timeout }`** derived
from observable state (k8s Pod status; local: process signal vs exit code), not a
bare exit code. The pure engine is a **consumer** of the class — it never
inspects k8s. The k8s mapping extends the `pod_state` logic c9660bd already
touched.

### Retry is gated on *"did the process start?"* + author assertion (amends 0020)

ADR-0020's "infra auto-retry by default" is too coarse — auto-retrying *any*
infra failure silently assumes every step is retry-safe. Split it:

- **Never-started infra** — `ImagePullBackOff`, `CreateContainerConfigError`,
  unschedulable, evicted-while-`Pending`. The main process never ran ⇒ **no side
  effect possible** ⇒ **auto-retry, bounded, no author assertion needed.** (This
  is the common transient churn ADR-0020 wanted to self-heal.)
- **Post-start infra + Step + Timeout + Lost** — a side effect *may* have
  occurred. Retry **only when the author configured `retry:`/max-attempts**,
  which *is* the author's assertion "this step is idempotent/fenced, re-run it."
  That is the boundary of Scarab's responsibility.
- **All retries consume the attempt budget** (including `Lost`).

### Attempt / fence model — retry mints a new fence; re-adoption reuses it

- **Re-adoption** (control-plane restarted, Pod still exists): the deterministic
  fence-named Pod is *adopted* — same Attempt, same fence, supervision resumes,
  **no re-execution, no budget consumed.** This is how a run survives a
  control-plane crash without re-running in-flight steps.
- **Retry** (terminal Infra/Step/Timeout, or `Lost`): a **new Attempt with a new
  monotonic fence.** The higher fence is the *safety mechanism* — it fences off a
  zombie (a "Lost" Pod that was only partitioned and comes back): the zombie
  presents a stale fence and a cooperating sink rejects it. Reusing the fence
  would make zombie and retry indistinguishable.
- **`Lost` is conservatively post-start** (can't prove the vanished Pod never
  ran) → assertion-gated retry, new fence, counts against budget.

### Safety contract — at-least-once; fences cover only cooperating sinks

- Max-attempts is a **liveness** bound (forward progress), **not** a safety one.
- Side-effect safety is **at-least-once** (CONTEXT §2): the monotonic fence
  protects **cooperating** sinks (generation/idempotency-key checks — k8s, a
  registry keyed by digest, a DB fencing column). **Non-cooperating** effects
  (a bare `POST`, email, Slack, some `terraform` backends) **transcend the
  token** and *will* double-fire on retry. No token can prevent that.
- Therefore `retry:` carries an **honest warning at the opt-in point**: *"retry
  re-runs the whole step at-least-once; enable only if the step is idempotent or
  fenced against a cooperating sink."* Never over-promise safety.

### `retry:` IR surface

`retry: { on: failure, max: N }` (ADR-0020's syntax), currently not expressible
(`scarab-pipeline` has no `retry`). `on` selects post-start-infra/step/timeout;
never-started infra retries independently of it.

### Timeouts

- **Step:** kubelet **`activeDeadlineSeconds`** is primary — enforced by the
  kubelet, so a hung Pod dies even if the control plane is down (surfaces as
  `DeadlineExceeded` → `Timeout` class); plus an engine-side backstop, and the
  local executor's own kill-timer. **Default 1h, per-step overridable
  (`timeout:`), globally configurable** — a default is mandatory (it closes the
  "hung Pod wedges the run forever" hole). Not auto-retried (post-start,
  assertion-gated).
- **Run:** **no default wall-clock timeout** — a run suspended weeks on a gate is
  the *wedge*, not a hang. Forward progress rests on step timeouts + gate expiry.
  A run budget is **opt-in** and counts **active time only** (excludes
  gate-suspended duration).
- **Gate:** the existing auto-release *timer* (auto-approve after a wait) plus an
  **optional gate expiry** (fail if unapproved by a deadline) — distinct.
  **Default indefinite** (gates may wait forever).

### Dead-letter semantics — developer signal vs operator signal

- **`Failed`** — code produced a failing *verdict*: `Step` (incl. an exhausted
  opt-in Step-retry) or `Timeout` (reason `TimedOut`). Shown to the developer.
- **`DeadLettered`** — the system could not obtain a verdict: `Infra` retries
  exhausted, or a poison outbox message. Alerts the operator.

### Outbox poison

Add `delivery_attempts` + a max to the outbox; on exhaustion, **dead-letter the
message** and transition its run to `DeadLettered` with diagnostics. Fixes
today's infinite redelivery of a permanently-failing message
(`migrations/0001_initial.sql:67-78` has no attempt counter / DLQ).

## Consequences

- `Executor` port terminal state carries `FailureClass` (was bare exit code);
  both adapters classify.
- IR gains `retry`; migrations for attempt/retry counters and outbox delivery
  attempts; the engine wires the dead `finish_attempt` path.
- `DeadLettered` finally gets produced; the ADR-0020 forward-progress guarantee
  becomes real.

## Alternatives considered

- **Uniform auto-retry any failure (ADR-0020 literal):** silently assumes all
  steps are retry-safe; re-runs doomed user code. Refined away.
- **Opt-in retry for *all* classes incl. never-started:** honest but poor
  ergonomics — a registry blip would fail every build until authors add `retry:`.
  Rejected; never-started is unconditionally safe to auto-heal.
