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
