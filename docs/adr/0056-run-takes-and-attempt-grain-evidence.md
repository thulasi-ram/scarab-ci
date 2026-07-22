# 0056. Run Takes and attempt-grain evidence

- **Status:** Accepted
- **Date:** 2026-07-20
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0052](0052-artifacts.md) (artifact keying); complements [0027](0027-restart-semantics.md), [0047](0047-retry-classification-and-attempt-model.md)

## Context

Restart-step mutates a run in place: the UI shows only the latest state, and a
2026-07-20 audit found the backend *destroys* prior-attempt evidence in three
places — `step_runs.results` and `step_runs.output_snapshot` are latest-wins
columns (migrations 0018/0007), and `artifacts` is `PRIMARY KEY (run_id, name)`
with `ON CONFLICT … DO UPDATE` (`scarab-db-postgres/src/lib.rs:1244`), directly
contradicting its own migration comment ("immutable once written"). A failed
attempt's failure report is silently overwritten by the retry that recovered
from it. The overwritten workspace root also becomes a dangling CAS reference —
sweepable garbage — so old-attempt evidence *races the GC*. Meanwhile the event
log records no restart fact at all: `restart_step` (`scheduler.rs:116`) emits
nothing, so a manual intervention is indistinguishable from engine activity and
carries no principal (an audit gap independent of any UI).

GHA's prior art is a run-level `run_attempt` with a version dropdown, but its
rerun unit is the whole run. Scarab's restart is **per-step** (ADR-0027 smart
invalidation), so "version 2 of the run" is not a natural object — after
restarting only `build`, the run is a patchwork (`clone@1, build@2, test@2`).
The version unit had to be defined, not borrowed.

## Decision

### The **Take**: a derived lens, never a stored version

A **Take** is the span of a run between two **human interventions**
(restart-step presses). A restart closes the current Take and opens the next.
Auto-retries, crash re-adoptions, and dead-letters are *contents* of a Take,
never boundaries — they surface in the per-step attempt strip and Activity
feed, not as version entries (a `retry: max 5` flake must not mint five
versions). "Attempt" stays reserved for the per-step unit (CONTEXT §4.3);
"epoch" was rejected for the Unix-timestamp collision, "revision" for implying
the pipeline definition changed.

**No `take` column exists anywhere.** Take N = "N−1 restart events precede this
point"; a Take's view = a pure event-log replay up to its boundary event; its
per-step attempt frontier indexes the attempt-keyed evidence stores. The engine
(scheduler, admission, fencing) never learns Takes exist. Server-side Take
resolution can be lifted behind an endpoint later as a pure function of the
event log — deferred, no migration pain.

### Two new events

- **`RunRestartRequested { target, invalidated, by }`** — emitted at the top of
  `restart_step`, before any re-arming. The Take boundary anchor; carries the
  resolved invalidation set (deterministic record, not re-derivation) and the
  acting principal (closes the who-restarted-this audit gap).
- **`AttemptReadopted { step, attempt }`** — a control-plane crash re-adopting
  a live Pod (same attempt, same fence, per ADR-0047) is currently invisible.
  This event exists purely for visibility: the durability wedge, made legible
  in the Activity feed. It is *not* a new attempt and must not render as one.

### Evidence moves to the attempt grain — the fence unit is the storage unit

The attempt is already the fence unit `{run, step, attempt}` (ADR-0021/0047);
it becomes the evidence unit. Logs (`log_chunks`) already comply.

- **`attempts.results JSONB`** and **`attempts.output_snapshot TEXT`** — written
  on attempt completion in the same transaction that records the verdict. The
  existing `step_runs` columns remain as the **latest-successful
  denormalization** feeding the hot path (`${{ outputs.* }}` interpolation,
  workspace inheritance), whose semantics are unchanged.
- **`attempts.consumed JSONB`** — a map `upstream step → attempt id` stamped at
  attempt launch: which upstream attempt's outputs/workspace this attempt
  actually consumed. Recorded, not inferred — after a mid-run restart, `test@2`
  consumed `build@2` while a smart-skipped `deploy` still stands on `build@1`;
  that mixed frontier is now a durable fact, not timestamp archaeology.
- **Artifacts become immutable per attempt**: key
  `(run_id, name, step_id, attempt_id)`. The ADR-0052 name-addressed
  of-record URL contract is unchanged; resolution gains one rule — it returns
  the **latest successful** attempt's version (a downstream consumer must never
  silently receive a failed attempt's partial file). Failed-attempt versions
  are retained, UI-tagged (`build@1 ✗`), and swept with the run's TTL class.
- **GC liveness**: every attempt's `output_snapshot` is a live CAS root, not
  just the step's current one (ADR-0050 sweeper).

### Take-view semantics

- **Snapshot-at-boundary**: a closed Take's view is the run frozen at the
  instant of the restart press. An attempt straddling the boundary (started in
  Take 1, finished in Take 2) shows as *running* in Take 1 — the state a
  bystander actually observed — with a "finished in Take 2 →" affordance.
  Outcome-absorbing attribution was rejected: it reintroduces take
  bookkeeping and can render a state that never existed at any wall-clock
  moment.
- **Read-only, with one exception**: all mutating actions are disabled while
  viewing a closed Take; **debug-pod** is allowed (it reproduces a finished
  attempt in a throwaway Pod and mutates nothing durable — debugging "what did
  attempt 1 see" is the point of time travel).
- **Takes are linear, never a tree.** The run has exactly one live frontier;
  restart is only pressed from the latest Take. "Go back and try differently"
  is **Re-run** (a new run), which may later grow a from-this-Take prefill.

### Read surface

`?attempt=` (already on logs) extends to the results / workspace / artifact
read endpoints. There is **no `?take=` parameter** — the client derives
frontiers from the event log it already fetches.

## Consequences

- Execution semantics untouched: viewing is pure read; the only engine-adjacent
  change is emitting two events. The UI (Take dropdown + merged step pane with
  an attempt strip scoping logs/results/outputs/workspace) rides on top and is
  freely revisable.
- Migrations: three `attempts` columns; artifacts re-key + provenance backfill
  (existing rows get a synthetic attempt attribution or NULL-tolerant reads).
- Storage growth is bounded by attempt budgets (ADR-0047 max-attempts) and
  swept by the existing TTL classes (ADR-0050).
- The migration comment "immutable once written" becomes true.

## Alternatives considered

- **GHA-style run-level attempt counter** — wrong unit for per-step restart;
  forces whole-run re-execution semantics Scarab deliberately avoids (0027).
- **Stored `take_version` column** — derivable from the event log; storing it
  couples the engine to a read-model concept and invites drift.
- **Latest-wins evidence + provenance tag only** — cheaper, but keeps
  destroying failure evidence (the artifact a retry produced *is* the record of
  what it recovered from). Rejected as contrary to the durability thesis: the
  system never destroys evidence; the UI was the only thing hiding it.
- **Auto-retries as Take boundaries** — noise; retries are the engine doing its
  job within a chapter, already legible per step.

## Amendment (2026-07-20): end-user language, retry vs rerun, superseded/shadowed

The original decision defined the *internal* model (Take = derived lens; Attempt
= evidence grain) but left the **end-user** framing open, and a grilling session
found the two history controls (a `Take` dropdown + a per-step attempt strip)
were presented as confusing **peers**. This amendment fixes the user-facing model
and sharpens the re-execution vocabulary. It changes **no execution semantics**;
`superseded`, `shadowed`, and the `cascade` cause are all derived client-side
from the existing event log, exactly as Takes are.

### One mental model: the run is a timeline; the pipeline is its fixed shape

The pipeline is *space* (what runs after what); the run is *time* (what actually
happened). Every step **try** (Attempt) is kept, immutable, read-only, until the
run's TTL. "Take"/"attempt" are **internal words** and are never surfaced
verbatim; the user sees a **row per rerun** and a per-step **tries** strip.

### Two "agains", split on step state — not on who pressed

The discriminating axis is **what state the step was in**, not machine-vs-human:

- **auto-retry** — the *engine* re-executing a **not-yet-succeeded** step within
  its budget (ADR-0047). Same history row, **no fork**. The only "retry".
- **rerun** — the *single human action*. Re-running a **terminal** step —
  **failed or succeeded** — which **always forks** the run into a new Take (a new
  history row) and cascades to descendants.

A separate **"manual retry"** concept was **rejected**: a human re-running a
failed step is just a `rerun` of a terminal step (one human control on the
screen, not two). This *confirms and generalizes* the original "every restart
mints a Take": a rerun of a dead-lettered step is a genuine new version — the
"before" is *the run that died*, the "after" is *the run revived and continued* —
so `restart_step` correctly emits `RunRestartRequested` on **every** press,
regardless of the target's prior status. (Auto-retry emits no boundary.)

### Attempt causes gain `cascade`

The read model distinguishes the rerun's **target** (`rerun`) from descendants
**dragged along** by smart invalidation (`cascade`, ADR-0027). Prior
`attemptCauses` tagged both as `restart`, hiding "you did one thing; the rest
followed". Full set: `initial · auto-retry · rerun · cascade` (plus
`readopted` as a visibility flag, never a new attempt).

### Two new non-success outcomes (derived, not stored)

An Attempt ends `running → succeeded | failed | superseded | cancelled`:

- **superseded** — an Attempt **cut short while running** because a human reran an
  ancestor (its input was being replaced, so it could not honestly finish; a
  fresh Attempt replaces it). Distinct from **cancelled** (a *deliberate* stop
  with **no** replacement, `cancel_run_request`) and **failed** (ran and errored).
  Derived client-side: `AttemptStarted` with no matching `AttemptFinished`, whose
  step is re-armed by a later `RunRestartRequested`.
- **shadowed** — a **finished-successful** Attempt that is **no longer the
  of-record latest** (a newer Attempt from a rerun/cascade took its role). Not a
  terminal state but a flag on a succeeded try; the of-record = latest-successful
  resolution already decided above *is* the shadowing rule.

### UI: one component, two zoom levels (supersedes the peer-controls layout)

The DAG, the selected step's evidence (logs/results/outputs/files), and a
**version-aware Artifacts footer** merge into a single `PIPELINE` component:

- **zoom out** = an **always-present version dropdown** on the component header
  (rich rows: cause + time + result; single-version state reads "latest · live").
- **zoom in** = the selected step's **tries** strip, chips labelled by cause
  (auto-retry / you reran / ⟵ cascade) and outcome (✗ / ⊘ superseded / shadowed).

The two are **zoom levels, not peers**. Selecting a non-latest version turns the
**whole component read-only** (tinted + a "👁 …" banner; `Rerun`/`Cancel`
disabled, `New run`/`Debug pod` live). **Activity stays separate** — the
unfiltered cross-version event log where reruns are witnessed and versions are
derived. User-facing verb is **Rerun** (not "Restart").

### Follow-up (the one real engine gap)

`restart_step` re-arms an in-flight descendant Running→Pending but does **not**
tear down its Pod; fencing keeps results *correct* (the superseded Pod's late
verdict is rejected), but the Pod can run on as an **orphan** wasting resources.
Tracked separately — a superseded in-flight attempt should trigger an
executor teardown (SIGTERM + grace), and MAY emit an explicit `AttemptSuperseded`
event for audit rather than relying solely on the derived read.

## Amendment (2026-07-22): reinstate a human Retry; rerun validation; per-Take attempt scoping

The 2026-07-20 amendment **rejected** a human "manual retry" ("one human control,
not two"), collapsing every human re-execution into a Take-forking `rerun`.
Dogfooding reversed that call. Two forces the earlier reasoning under-weighted:

1. **Version proliferation.** A flaky step a human nudges three times minted
   three Takes — three near-identical version rows for what a user reads as "the
   same run, tried again." The Take dropdown became noise.
2. **The user's own mental split.** "Retry this failed step" and "redo this (even
   a green) step" are *different intents*. Forcing both to fork erased the
   cheaper one. Giving the user the choice **is** the control the earlier
   amendment thought it was removing.

This reverses the "manual retry rejected" point **only**; the Take model, the
derived-lens principle, and `superseded`/`shadowed`/`cascade` are unchanged.

### Two human controls, split on step state (supersedes "one human control")

- **Retry** — a human re-executing a **Failed** step. Produces **another Attempt
  in the *current* Take — no fork.** It reopens the settled run
  (`Failed → Running`) and re-arms the target **plus its dependent cascade**
  (ADR-0027) — a Retry of `b` must re-arm the now-`Skipped` `c`, or a recovered
  `b` would leave the pipeline stuck. It emits **`StepRetryRequested { step, by }`**
  — an attribution/audit fact and Activity-feed entry, but **not** a Take
  boundary (`deriveTakes` ignores it; only `RunRestartRequested` splits Takes).
  Offered on Failed steps only.
- **Rerun** — a human re-executing **any terminal step whose deps all Succeeded**.
  **Forks a new Take** (`RunRestartRequested`) as before. Offered on
  Succeeded/Failed steps; on a Failed step both controls appear (retry-in-place
  vs redo-as-new-version — the user picks).

A human Retry's target Attempt carries the `retry` cause (same strip treatment as
an auto-retry — both are "another try in this version"); its human origin is
witnessed by the `StepRetryRequested` event in Activity, not by a distinct chip.
Dependent Attempts it drags along carry `cascade`, as with rerun.

### Rerun validation — reject, don't silently skip (refines ADR-0027)

A rerun whose **target's `needs` are not all `Succeeded`** can never run (admission
would `dep_dead`-skip it). `restart_step` now **rejects** such a call up front —
`RestartError::DependencyNotSatisfied { step, blocker }` → **409**, e.g. "cannot
rerun `c`: dependency `b` has not Succeeded" — instead of forking a Take that
just re-skips. The gate is purely on **deps**, never the target's own status:

- `c` `Skipped` because dep `b` **Failed** → deps not Succeeded → **reject**.
- a step `Skipped` by its own `when:` (deps **did** Succeed) → **allowed**; the
  rerun replays and the still-false condition **skips it again**. Rerun does not
  concern itself with conditions — it never force-overrides a `when:`.

### Per-Take attempt scoping (fixes the read model, not the engine)

The original "a Take's view = replay **up to** its boundary" made `replayTake`
count Attempts **cumulatively from run birth** — so the latest Take showed every
Take's attempts summed (`a`=3 where it should read 1). Corrected: a step's
attempt count / frontier / tries-strip is scoped to the Take's **own window**
(previous boundary → this boundary), not from birth. Status still carries
forward cumulatively (a step not re-run in a Take keeps its prior verdict).
Durable Attempt ids stay globally monotonic (`a1..aN`, needed for `?attempt=`
reads); the strip displays the **per-Take positional index**, so Take 2's `a`
reads "attempt 1" though its durable id is `a3`.

A step **not re-armed** in the viewed Take (a partial rerun left it untouched)
shows **0 attempts** for that Take and renders as a **muted/desaturated** version
of its carried-forward status — present and green, but visibly "not part of this
rerun" — rather than fabricating an attempt it never had.
