# 0083. A redelivery creates only the Runs a delivery is missing

- **Status:** Proposed (design; red-teamed once, one owner decision pending, see
  [Owner decision pending](#owner-decision-pending-a-late-redelivery))
- **Date:** 2026-11-22
- **Deciders:** thulasi.ram (architect), pending review
- **Builds on:** [0010](0010-forge-integration.md) (forge integration),
  [0046](0046-forge-auth-and-multi-adapter.md) (delivery-id replay guard),
  [0050](0050-retention-and-gc.md) (retention sweeper),
  [0051](0051-multi-replica-operation.md) (several server replicas),
  [0075](0075-policy-engine.md) (`run.create` decision written with the Run's first row),
  [0076](0076-libraries-and-plugins-through-invoke.md) (refused Runs),
  [0081](0081-push-changed-paths-come-from-the-forge-compare.md) §7 (work inside a delivery)
- **Tickets:** git-bug 61174ee; the review that filed it is the 2026-11-22 batch judge
  ruling, "Webhook: forge read fails", ticket 2

> **2026-11-22, after the design red team.** Revised: every decision is recorded, no-match
> included, because a pull request's paths are asked of the forge live and a second try can
> decide differently (§1); a refusal stores its reason kind, so a recorded compile refusal
> still stops the Pipelines not yet decided (§3); a comment, tag or release delivery reads
> `.scarab/` at the commit its first try resolved (§1); a replaced Run is marked, its commit
> statuses are dropped and its creation can no longer finish (§4); a delivery and its
> outcomes are pruned together, with a 7-day floor (§7); the GitHub redelivery id is quoted
> from GitHub's docs; the Forgejo body-digest key is rejected; what a late redelivery may do
> is an open owner decision.

## Context

A verified webhook delivery runs in its own task (`finish_delivery`, `scarab-server/src/lib.rs`)
holding one of 64 permits. The task records the delivery id once per delivery
(`delivery_is_fresh` → `ConnectionRegistry::record_delivery`, an
`INSERT ... ON CONFLICT DO NOTHING` into `webhook_deliveries (forge, id, at)`, migration 0024).
A second delivery with the same id is answered 200 `duplicate delivery` and does nothing.
Then `ingest_event` reads `.scarab/` (`read_event_pipelines`), compiles every file, and for
each Pipeline whose trigger matches (`DeliveryTrigger::decide`) either creates a Run
(`admit_and_create_run` → `persist_run_from_ir`) or records a refusal.

0.7.1 added, around that:

- a delivery that recorded nothing because the forge could not be read deletes its id
  (`forget_delivery`) and answers 502, so a redelivery is processed;
- a delivery that created some Runs while a Pipeline file could not be read keeps its id and
  answers 202 with `"incomplete": true`;
- a redelivery of an id whose task is still running **on the same server** is answered 503
  (`RunningDelivery`, an in-process set);
- a delivery that finds no free permit within 10 s is answered 503 with nothing recorded.

Three windows still lose Runs, because the id means "this delivery is done" from the moment
it is recorded:

1. **Partial delivery.** Runs of `a.yaml` and `b.yaml` are created, `c.yaml` could not be read.
   The id stays recorded; a redelivery is a duplicate; `c.yaml` never runs.
2. **Server death.** The server stops after the id is recorded and before (or while) the Runs
   are created. A redelivery is a duplicate.
3. **Another replica.** A redelivery reaches replica B while replica A still runs the first
   try. B finds the id recorded and answers `duplicate delivery`; if A then fails, nothing
   repairs it.

What the code does today, checked for this ADR:

- **Run creation is not one transaction.** `persist_run_from_ir` writes the Run's first row
  (`Db::begin_run_creation`, `runs.creating = TRUE`, out of the scheduler's work list), then
  about twenty separate writes (tenant, number, origin, IR, events, Steps, ...), then
  `finish_run_creation` (`UPDATE runs SET creating = FALSE WHERE id = $1`, with no other
  condition). A write that fails ends the Run `Failed` (`fail_run_creation`); a server that
  stops in between leaves the Run `Pending`, and the leader's sweep ends it `Failed` after 10
  minutes (`sweep_unfinished_creations`, `unfinished_creation_bound_ms`, reason "the server
  stopped while creating this Run"). `creating` stays `TRUE` on such a Run, and it cannot be
  rerun (`RerunError::CreationUnfinished`).
- **The first row may be written with the Policy decision.** When `run.create` Policies apply,
  the Postgres policy store writes the allow decision and the Run's first row in one
  transaction (`record_run_create_decision`, `scarab-db-postgres/src/policies.rs`); the
  default implementation (the in-memory store) writes the decision alone and
  `begin_run_creation` writes the row.
- **A refusal is several writes.** `record_refused_run` inserts into `refused_runs` (no
  unique key; best effort, a failed write is logged), and `enqueue_refusal_status` queues a
  `failure` commit status. The status's idempotency key contains `run_id_for(event, path)`,
  which is a new random id for every webhook event, so two tries queue two posts. A Policy
  denial and a missing-approval-gate refusal each write a decision row first
  (`consult_run_create`).
- **Commit statuses are ordered per Run only.** A post that fails stays on the outbox and is
  retried; `status_post_superseded` compares a post with its own Run's generation and status,
  never with another Run's.
- **Follow-ups already work this way.** An `on: upstream` Run gets an id derived from the
  upstream Run, its generation, its verdict and the Pipeline path (`run_id_for`, git-bug
  149b4db); a redelivered `run_finished` skips a path whose Run exists.
- **Which events reach `ingest_event`.** Push, pull request, tag, release and comment
  (`Event::Comment`). Cron fires, manual and API dispatches, and follow-ups have their own
  paths and are not changed here. There is no check-suite or "re-run" event handling.
- **Where `.scarab/` is read** (`config_ref`): a push at `after`, a pull request at its head
  commit, a tag or release at `refs/tags/<tag>`, a comment at `HEAD`. The last two move.
- **A pull request's changed paths are asked live,** by pull request number, not at the
  event's commit (`pull_request_files`), and a push's compare can time out; an unknown list
  counts as true (ADR-0081 §6). Two tries of one delivery can decide a `when:` differently.
- **`webhook_deliveries` is never pruned.** It also answers the connection page's "last
  delivery" (`last_delivery_at`, `max(at)` per forge, with no index on `at`).
- **The in-memory store** (`scarab-testkit`) keeps runs, refused runs, the outbox and
  `webhook_deliveries` under one `Mutex`; one lock is its transaction.

What the forges do:

- **GitHub.** "If you request a redelivery, the `X-GitHub-Delivery` header will be the same as
  in the original delivery" (GitHub docs, *Best practices for using webhooks*). "You can
  redeliver webhook deliveries that occurred in the past 3 days", and "GitHub does not
  automatically redeliver failed deliveries" (*Redelivering webhooks*).
- **Forgejo** (source, branch `forgejo`). `Deliver` (`services/webhook/deliver.go`) makes one
  request and records the result; nothing retries it. **Replay** (`ReplayHookTask`,
  `models/webhook/hooktask.go`) copies the payload into a new hook task, and `CreateHookTask`
  gives it a new UUID; `X-Forgejo-Delivery`, `X-Gitea-Delivery` and `X-GitHub-Delivery` all
  carry that UUID (`services/webhook/shared/payloader.go`). A Replay is a new delivery id.

So today a GitHub Redeliver is always answered `duplicate delivery`, and a Forgejo Replay
starts every Run again.

## Decision

### 1. The key is (forge, delivery id, Pipeline file path), and every decision is recorded

A **delivery outcome** is recorded per Pipeline file a delivery decided, with the key
`(forge, delivery_id, pipeline)`, where `pipeline` is the file path under `.scarab/` as
`read_event_pipelines` lists it (`.scarab/build.yaml`).

- **The path, not the `name:`.** Two files can declare the same `name:`, a file can omit it,
  and every try lists `.scarab/` at the same commit (below). `build.yaml` and `build.yml` are
  two keys, as they are two Pipelines today.
- **No trigger kind in the key.** One delivery is one event, so one trigger kind.
- **Matrix:** a matrix expands Steps inside one Run at compile time (CONTEXT.md **Matrix**), so
  it adds nothing to the key.
- **Follow-ups (`on: upstream`)** are not started by a webhook delivery; they keep their
  derived Run ids. Each Run a delivery creates starts its follow-ups once, when it ends.
- **Comment commands** are `Event::Comment` deliveries and get the same key.

**One commit per delivery.** The first try resolves the commit it reads `.scarab/` at and
stores it on the delivery row (`webhook_deliveries.config_sha`). Push and pull request
deliveries store the event's commit. A comment delivery (`HEAD`) and a tag or release
delivery (`refs/tags/<tag>`) resolve the ref once with `ForgePort::latest_commit`. Every later
try reads `.scarab/` at the stored commit. Without this, a comment's redelivery after a
rename on `HEAD` (`deploy.yaml` → `release.yaml`) would find no outcome for the new path and
run `/deploy` a second time. If the ref cannot be resolved, nothing is stored and the try is
answered as an unreadable forge (§6).

**Every decision is recorded.** A redelivery must not decide a Pipeline again: a pull
request's paths are asked live, a compare can time out the second time, and an unknown list
counts as true, so a Pipeline the first try did not match could become a Run on the second.

| Outcome for one Pipeline | `outcome` | Side effects in the same transaction |
|---|---|---|
| A Run created | `run`, with the Run id | the Run's first row |
| Refused at admission (Environment, Policy, yanked library, event field, forge token, sandbox, missing approval gate) | `refused`, with `reason_kind` and the `refused_runs` id | refused row, `failure` status |
| A `when:` that cannot be decided | `refused`, `reason_kind = when` | refused row, `failure` status |
| Does not compile | `refused`, `reason_kind = compile` | refused row, `failure` status when its `on:` may name the event |
| Matched, not run because another file does not compile | `not_run` | `failure` status naming the broken file |
| Trigger does not match | `no_match` | none |
| Every Step excluded by its `when:` | `all_excluded` | none |
| Ref not allowed by the Environment (silent today) | `ref_not_allowed` | none |
| File, or its `invoke:` library, could not be read | not recorded | none; read again by the next try |
| An error from the store or from admission | not recorded | none; the delivery stays unfinished |

`reason_kind` takes the values `refused_runs.reason_kind` takes today, plus `when`.

The last row covers an error returned out of the loop in `trigger_runs_from_files`, which
leaves the Pipelines sorted after it undecided too. One that repeats, such as a concurrency
group that cannot be rendered (`concurrency_group_key`), repeats on every redelivery; see
Not solved.

Not keyed: the path-filter note (`record_path_filter_note`) and the follow-up record
(`record_followups`), written on a Run after its creation finished by the try that created
it. If that try stops in between, a redelivery skips the Pipeline and neither is written; both
are best effort today, and the follow-up drain reads `.scarab/` itself when the record is
missing. The cron index (`schedules::index_push`) writes only when the push is still the
default branch's head, so a redelivery of an old push writes nothing.

### 2. The outcome row is written in the transaction of the Run's first row or of the refusal

New table `webhook_delivery_pipelines`:

```sql
CREATE TABLE webhook_delivery_pipelines (
    forge          TEXT   NOT NULL,
    delivery_id    TEXT   NOT NULL,
    pipeline       TEXT   NOT NULL,   -- `.scarab/<file>`
    outcome        TEXT   NOT NULL CHECK (outcome IN
                     ('run', 'refused', 'not_run', 'no_match', 'all_excluded', 'ref_not_allowed')),
    reason_kind    TEXT,              -- outcome `refused`
    run_id         TEXT,              -- outcome `run`
    refused_run_id BIGINT,            -- outcome `refused`, the `refused_runs` row
    at             BIGINT NOT NULL,
    PRIMARY KEY (forge, delivery_id, pipeline)
);
```

No foreign keys: a key on `runs` would make every outcome insert lock a `runs` row, and
retention (§7) removes outcomes with their delivery, not with their Run.

Where each outcome row is written:

- **A Run.** In the transaction that inserts the Run's first row (`insert_creating_run`):
  `Db::begin_run_creation` gains an optional `DeliveryPipeline` key, and so does
  `RunCreationRow` for `PolicyStore::record_run_create_decision`, which writes the decision,
  the first row and the outcome row together. That is the only Run transaction there is; §4
  covers a Run whose later writes never landed.
- **A refusal or `not_run`.** A new `Db::record_delivery_refusal(key, reason_kind, refused,
  status)` writes, in one transaction, the outcome row, the `refused_runs` row (when there is
  one) and the `failure` commit status outbox row (when there is one).
  `refuse_uncompiled_delivery`, `refuse_undecided_when` and the admission-refusal branch of
  `trigger_runs_from_files` call it for webhook deliveries in place of `record_refusal`
  followed by `enqueue_refusal_status`.
- **`no_match`, `all_excluded`, `ref_not_allowed`.** `Db::record_delivery_outcome(key,
  outcome)`, one insert.
- **In-memory store.** The same writes under its one lock.

A primary-key violation is not an error to the caller. The port answers `Claimed` with the
outcome already recorded, and nothing of the losing write remains. With the Postgres stores
that includes the Policy allow decision. The first outcome written wins, whatever the
second try decided.

A failed outcome write (other than a key violation) leaves that Pipeline undecided and the
delivery unfinished, like an unreadable file. Today a refused row that fails to write is only
logged; on the webhook path it now keeps the delivery open for a redelivery.

The key is passed only on the webhook path (`ingest_event` → `trigger_runs_from_files`).
Dispatch, cron and follow-up callers pass none and behave as today.

### 3. A try reads the outcomes first and decides only what is missing

`trigger_runs_from_files` reads the delivery's recorded outcomes once
(`Db::delivery_outcomes(forge, delivery_id)`), before compiling. A recorded path is not
compiled, decided or created again; Runs and refusals among them are listed in the answer as
`earlier`. This is the check the follow-up path already makes with
`run_status(run_id_for(..))`.

**The compile rule across tries.** ADR-0009's rule is that a file that does not compile stops
every Run of the delivery. Across tries it reads: if any file of the delivery is broken,
recorded (`refused` with `reason_kind = compile`) or found by this try, every Pipeline of the
delivery not yet decided is decided as the first try would have decided it with that file
broken: `not_run` with its `failure` status where its trigger matches, `no_match` where it
does not. Example: the first try records `c.yaml` as `compile`, `a.yaml` and `b.yaml` as
`not_run`, and cannot read `d.yaml`. The redelivery reads `d.yaml`; because `c.yaml`'s
compile refusal is recorded, `d.yaml` becomes `not_run` (if it matches), never a Run.

Runs a first try created before a later try found a broken file (possible only when the
broken file was unreadable on the first try) are not touched; the broken file is recorded as
`compile` and the Pipelines still undecided become `not_run`. That is the 0.7.1 behaviour of
a partial delivery, carried forward.

The read is an optimisation. Two tries that both pass it are separated by the primary key
(§2).

### 4. A Run whose creation did not finish is replaced by a redelivery

An outcome `run` can point at a Run whose first row exists and whose creation never finished
(`runs.creating = TRUE`): the server stopped, the try stalled, or a later write failed. A
redelivery looks at it:

- **Creation finished** (any status): the Pipeline is decided; skipped.
- **Still creating, `Pending`, and younger than the unfinished-creation bound** (10 minutes,
  the scheduler's `unfinished_creation_bound_ms`, moved to a constant both read): another
  try may be writing it now. The Pipeline is left alone, and the answer is 503 (§6).
- **Creation unfinished and older than the bound, or already ended `Failed`:** the try ends
  it with `fail_run_creation` (a compare-and-set from `Pending`; a conflict means it is
  already `Failed`) and creates a new Run. The new Run's first-row transaction also:
  - moves the outcome row: `UPDATE webhook_delivery_pipelines SET run_id = $new WHERE <key>
    AND run_id = $old`, which must update one row; zero rows means another try replaced it
    first, the transaction rolls back and the Pipeline counts as `Claimed`;
  - marks the old Run replaced: new column `runs.replaced_by` set to the new Run's id.

What the mark does:

- **Statuses.** `status_post_superseded` answers "superseded" for every post of a Run whose
  `replaced_by` is set, so a `failure` post of the old Run still on the outbox (the forge was
  failing, the usual reason for a redelivery) is dropped instead of landing after the new
  Run's `success`. A post the forge already accepted before the replacement committed is
  older than every post of the new Run, which is created after it.
- **A stalled try cannot finish the old Run.** `finish_run_creation` becomes a compare-and-set:
  `UPDATE runs SET creating = FALSE WHERE id = $1 AND creating AND status = 'pending' AND
  replaced_by IS NULL`. Zero rows means the Run was ended or replaced while the try stalled;
  the try writes no path-filter note or follow-up record for it and reports the Pipeline as
  decided by another try. Without this, a try paused past the bound would clear `creating` on
  a `Failed` Run and a human Rerun would give a second Run for one (delivery, Pipeline).
- **Rerun.** `ensure_creation_finished` also refuses a Run with `replaced_by` set, and the
  refusal names the replacement.

The bound only decides how long a redelivery waits with 503 before replacing. It compares the
Run's `created_at`, taken on the creating server, with the redelivering server's clock, so
clock skew between replicas lengthens or shortens that wait; the compare-and-set finish keeps
a wrong guess from producing two live Runs.

The old Run stays in the Runs list as `Failed`, with its reason and a link to its replacement.
A creation that fails for a reason that repeats gives one more `Failed` Run per redelivery;
redeliveries are made by hand, so that is bounded by the operator.

### 5. Two replicas, one delivery

`RunningDelivery` (the in-process set) is removed: it existed because 0.7.1 forgot ids, and
the database now decides across replicas.

Two tries of one delivery at once, on one replica or two:

- Both may read the forge and compile. Reads have no effect beyond the forge's rate limit.
- For each Pipeline, both may reach §2's write; one inserts, the other gets `Claimed`.
- Side effects in the winning transaction happen once: the Run's first row, the `run.create`
  allow decision (written with it on Postgres), the refused row, the refusal or `not_run`
  status.
- Side effects after the first row belong to the one Run that exists: its later writes, its
  path-filter note, its follow-up record, and every commit status, check or notification,
  which are outbox rows a Run transition writes, keyed by Run and generation.
- Side effects before the transaction, which can happen twice:
  - a `run.create` **deny** decision and the decision row of a **missing-approval-gate**
    refusal (ADR-0078) are written by `consult_run_create` before the refusal transaction, so
    a race, or a server that stopped between the two, leaves two decision rows for one
    refusal in the org's decision log. The log records consultations, and both happened; the
    refusal itself is recorded once.
  - with a policy store that does not write the Run's first row (the default trait
    implementation, used by the in-memory store; production pairs both on one Postgres), the
    loser's **allow** decision is written alone and names a Run id that is never created.
  - `tracing` lines.

The 64-permit semaphore and the shutdown drain stay as they are.

### 6. The delivery-level record stays as a fast path, with an `unfinished` mark

`webhook_deliveries` keeps one row per delivery and gains
`unfinished BOOLEAN NOT NULL DEFAULT FALSE` and `config_sha TEXT` (§1).

- On arrival a try inserts the row with `unfinished = TRUE`, or finds it.
- When the try ends with every Pipeline decided (no unreadable file, no Pipeline left to
  another try, no error), it sets `unfinished = FALSE`. So do the answers that act on no
  Pipeline: installation events, unsupported or ignored events, a repository not enabled or
  unreachable.
- A delivery whose row exists with `unfinished = FALSE` is answered 200 `duplicate delivery`
  with no forge read, as today.
- A delivery whose row is unfinished runs the trigger again (§3).

The default is `FALSE` so that every row written before this migration, and every row a
0.7.1 replica writes during a rolling update, reads as finished: those deliveries have no
outcome rows, and running their trigger again would create their Runs a second time.

`forget_delivery` and its 502 path go: nothing is ever deleted to let a redelivery in.

The answers:

| Case | 0.7.1 | This ADR |
|---|---|---|
| Finished delivery again | 200 `duplicate delivery` | same |
| Every Pipeline decided, some by this try | 202 `run_ids` | 202 `run_ids`, plus `earlier: [{pipeline, run_id \| refused}]` when an earlier try decided some |
| Every Pipeline decided by earlier tries | (could not happen) | 200 `duplicate delivery`, and the row is marked finished |
| Nothing created, forge unreadable | 502, id deleted | 502, row left unfinished |
| Some created, a Pipeline left undecided (unreadable, failed outcome write) | 202 `incomplete: true`, id kept | see owner question 1 |
| A Pipeline whose Run another try is creating (§4) | 503 on the same replica only | 503 `this delivery is still running; redeliver later`, on any replica, after the other Pipelines are decided |
| No free permit in 10 s | 503, nothing recorded | same |
| No delivery id, registry wired | 400 | same |

The OpenAPI response lists on both webhook endpoints change their 502 and 503 text.

### 7. Retention

The retention sweeper (`retention.rs`) prunes deliveries under its leader lease, in a new
`sweep_webhook_deliveries`: it selects up to 1,000 `webhook_deliveries` rows whose `at` is
older than the retention period and, in one transaction, deletes them and every
`webhook_delivery_pipelines` row with their `(forge, delivery_id)`. A delivery and its
outcomes go together, so no later try of a delivery can find some of its outcomes and not
others.

The period is an operator setting beside the other `retention_*_days` (owner question 2 for
its default). A value under **7 days** is refused at start with an error naming the floor:
under GitHub's 3-day redelivery window a Redeliver would find no outcomes and create every
Run again, and 7 days leaves room for clock and sweep timing.

Pruning `webhook_deliveries` does not weaken the replay guard of ADR-0046: the delivery id is
outside the HMAC, so a captured delivery can be replayed with a new id today.
`last_delivery_at` reads `max(at)`, which the newest rows keep.

`webhook_deliveries` has grown since 0024 without pruning, so the first passes on an old
database delete many rows, 1,000 deliveries at a time.

### 8. Migration

- **0104** (in a transaction, `SET LOCAL lock_timeout = '5s'` as 0101 does):
  - `CREATE TABLE webhook_delivery_pipelines` (a new table, no lock on another);
  - `ALTER TABLE webhook_deliveries ADD COLUMN unfinished BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN config_sha TEXT`;
  - `ALTER TABLE runs ADD COLUMN replaced_by TEXT`.

  Since Postgres 11 a constant default is stored in the catalog, and a nullable column
  without a default needs none, so no row is rewritten. Each `ALTER` takes an ACCESS
  EXCLUSIVE lock for that moment and waits at most 5 s for it. `runs` is written by every
  scheduler tick, so on a busy database the wait can time out; the start then fails with
  `canceling statement due to lock timeout` and the Pod's restart retries, as for 0101.
- **0105** (`-- no-transaction`, as 0100): `CREATE INDEX CONCURRENTLY IF NOT EXISTS
  webhook_deliveries_at_idx ON webhook_deliveries (at)`, added to `CONCURRENT_INDEXES` so an
  invalid index is rebuilt at start. Writes go on while it builds. The sweeper and
  `last_delivery_at` use it.
- **No backfill.** Runs carry no delivery id, so outcome rows cannot be derived for past
  deliveries; the `FALSE` default makes them read as finished (§6).
- **Rollback:** `crates/scarab-db-postgres/rollback/0104_revert.sql`, in the format of
  `0099_revert.sql`: drop `webhook_delivery_pipelines`, the index, the two
  `webhook_deliveries` columns and `runs.replaced_by`, and `DELETE FROM _sqlx_migrations
  WHERE version IN (104, 105)`. A 0.7.1 server then reads every recorded delivery as done,
  including unfinished ones, which is its behaviour today, and a replaced Run becomes an
  ordinary `Failed` Run whose creation did not finish.

### 9. What changes for operators and forges

- A **GitHub Redeliver** of a delivery that left a Pipeline undecided (a forge read failed,
  the server stopped, another replica was mid-way) creates what is missing and nothing else.
  A Redeliver of a finished delivery is still `duplicate delivery`.
- A delivery that stopped part-way on one replica can be redelivered to any replica.
- A **Forgejo Replay** has a new delivery id, so it starts the delivery's Runs again, as
  today.
- The webhook answers change as in §6. Neither forge retries on its own, so the change shows
  in the forge's delivery list and in anything that watches it.
- **During a rolling update** from 0.7.1, a delivery a new replica answered 502 and left
  unfinished is answered `duplicate delivery` if its redelivery reaches a 0.7.1 replica,
  where 0.7.1 alone would have deleted the id and processed it. Redelivering again once the
  rollout ends repairs it.
- Two migrations, a rollback file and a new retention setting with a 7-day floor.
- Whether this ships as 0.7.2 or 0.8.0 is the owner's call.

## Owner decision pending: a late redelivery

A redelivery can arrive up to 3 days after its push (GitHub), and §3 lets it create a Run the
first try left undecided. Two things can then go wrong, and neither can happen in 0.7.1,
where a redelivery is processed only when the first try recorded nothing:

- **It cancels a newer Run.** A push Run's `supersede_key` is `repo:ref:pipeline`, and a Run
  created later cancels the older non-terminal Runs with that key (`created_at <`). Day 0:
  push P1 to `main`, `c.yaml` unreadable. Day 2: push P2, its `c.yaml` Run is running. A
  Redeliver of P1 creates a `c.yaml` Run at P1's commit, which is the newest by `created_at`
  and cancels P2's.
- **It deploys an older commit.** A Pipeline that targets an Environment has no supersede
  key; the same Redeliver starts a deploy of P1's commit after P2's is out (inferred from the
  same flow).

Options, for a push whose commit is no longer the head of its ref when the redelivery decides
a Pipeline (the head read once per try, only when a Pipeline is left to create):

- **A.** Refuse: record `not_run` with the reason "no longer the head of `<ref>`", and post
  its `failure` status on the old commit.
- **B.** Create the Run without the supersede key, so it cancels nothing; an Environment
  Pipeline still deploys the older commit.
- **C.** As this ADR stands: create the Run as the first try would have.

This ADR is not Accepted until the owner picks one.

## Consequences

- Windows 1, 2 and 3 of the Context are closed for every Pipeline a redelivery reaches, on
  GitHub within its 3-day redelivery window, subject to the pending decision for late
  redeliveries.
- A delivery is processed again on redelivery unless it finished, so an unfinished one costs
  its forge reads again. A finished one costs one primary-key lookup, as today.
- `webhook_delivery_pipelines` holds one row per Pipeline file per delivery, matched or not,
  until the delivery is pruned.
- `refused_runs` gets at most one row per (delivery, Pipeline) on the webhook path; a
  refusal's commit status is queued once per (delivery, Pipeline).
- `RunningDelivery`, `claim_delivery`, `forget_delivery` and `refuse_unread_delivery`'s
  delete go. `finish_run_creation` becomes a compare-and-set.
- The org decision log can hold two decision rows for one refusal after a race (§5).

## Not solved

- **Run creation stays many writes.** A Run whose creation stops part-way is detected and
  replaced (§4); it is not prevented. Making `persist_run_from_ir` one transaction would mean
  a transactional `Db` port across about twenty methods.
- **No repair without a redelivery.** Neither forge retries, so an unfinished delivery stays
  unfinished until someone redelivers it (GitHub: within 3 days). A server-side sweep that
  re-runs unfinished deliveries would have to store their signed payloads; not decided here.
- **A delivery Scarab never received** (the server was down for longer than the forge waits)
  has no row at all; the forge shows it failed.
- **An admission error that repeats** (a concurrency group that cannot be rendered) stops
  every try at the same Pipeline, and the files sorted after it stay undecided. Recording it
  as a refusal is a change to admission, not to delivery handling.
- **Comment deliveries** now read `.scarab/` at one commit for every try (§1). Whether the
  Run's checkout uses that commit is unchanged by this ADR.
- **Forgejo Replay** starts the Runs again (§9).
- **The id is outside the HMAC** on both forges, so the delivery record is no replay guard
  against a sender who changes the id. A body digest is covered by the signature; using one as
  a second guard is a separate question from this ADR.
- **The key has no connection or repository.** One webhook secret per forge kind serves the
  whole server, so ids share one space. If secrets become per connection (git-bug 19b0a45),
  the key needs the connection.
- **Follow-up refusals** (a chain loop or cap) are recorded again when a `run_finished` is
  delivered twice; their status is not, through the derived id. Pre-existing, not on the
  webhook path.
- **Installation events** are not keyed; applying one twice is idempotent today.

## Rejected options

- **Keep the delivery-level id and delete it on any failure.** 0.7.1 does this for "nothing
  created". It cannot cover a partial delivery or a server that stopped, which are the cases
  left.
- **Record only Runs and refusals, and decide the rest again** (this ADR's first draft). A pull
  request's paths are asked live and an unknown list counts as true, so a Pipeline the first
  try did not match could become a Run on the second.
- **Derive webhook Run ids from (forge, delivery id, path)**, as follow-ups do, and let the
  `runs` primary key be the unique constraint. Run ids would then be computable by anyone who
  sees delivery ids in the forge's delivery list, against the rule that Run ids in URLs are
  not guessable; a UUIDv7 needs a time that a redelivery can reproduce; and refusals have no
  Run id to key on.
- **Write the outcome row at `finish_run_creation`** instead of with the first row. Two
  replicas would then both create a whole Run and the loser would have to undo one, posting a
  `failure` for it.
- **Key by the Pipeline's `name:`.** Not unique and optional (§1).
- **Make the whole delivery one transaction.** Forge reads and compiles would run inside an
  open transaction for up to tens of seconds.
- **Key Forgejo deliveries by the SHA-256 of the body**, so a Replay repairs instead of
  running again. Two events with the same body become one: a tag deleted and pushed again at
  the same commit can send the same bytes, and the second push would be answered `duplicate
  delivery` with no sign to the user. It also removes Replay as a way to run a delivery again,
  and during a rolling update a 0.7.1 replica keys by UUID while a new one keys by digest.
  Forgejo keeps one key per delivery id.

## Owner questions

1. **The answer to a delivery that created some Runs while a Pipeline was left undecided.**
   Today 202 with `"incomplete": true`; the forge shows it delivered and nobody redelivers it.
   Options: (a) keep 202 `incomplete`; (b) 502 with the same body (`run_ids`,
   `incomplete: true`), so the forge's list shows it failed and a Redeliver creates the rest.
   Stakes: (b) is a red delivery for a push that did start Runs; (a) leaves the missing
   Pipeline unnoticed now that a redelivery could repair it. Recommendation: (b).
2. **How long delivery records are kept.** Options: 7 days (the floor), 30 days, 90 days.
   "Never", today's behaviour for `webhook_deliveries`, is no longer offered: the outcome
   table holds a row per Pipeline file per delivery. Stakes: shorter keeps the tables small;
   longer keeps an operator able to ask "what did delivery X do" from the database.
   Recommendation: 30 days.
3. **A late redelivery:** see [Owner decision pending](#owner-decision-pending-a-late-redelivery).

## Test plan

Classical, against the in-memory store and the compose Postgres (`just test rust webhook`);
the forge is the existing `FakeForge`.

- **Partial delivery repaired** (window 1): `c.yaml`'s read fails, `a.yaml` and `b.yaml` start
  Runs; the redelivery of the same id starts `c.yaml` only, and its answer lists `a.yaml` and
  `b.yaml` under `earlier`. A third delivery is `duplicate delivery` with zero forge calls.
- **A recorded no-match stays one** (red team B1): the first try decides `a.yaml` `no_match`
  on known pull request paths and leaves `c.yaml` unread; on the redelivery the `FakeForge`
  answers the pull request's files with an error; `a.yaml` is not created.
- **A recorded compile refusal stops later files** (red team B2): first try `c.yaml` broken,
  `d.yaml` unreadable; the redelivery reads `d.yaml`, which matches; it is `not_run`, with no
  Run.
- **One commit per comment delivery** (red team A5): the first try reads at the commit `HEAD`
  resolved to; between tries `HEAD` moves to a commit where the file is renamed; the
  redelivery reads at the stored commit and creates nothing new.
- **Server stopped between rows** (window 2): a store hook stops the try after
  `begin_run_creation` of `b.yaml`; the redelivery within the bound answers 503 and creates
  nothing; with the clock past the bound it ends the old Run `Failed`, creates a new one, the
  outcome row names the new Run and the old Run's `replaced_by` names it too.
- **A stalled try cannot finish a replaced Run** (red team A3): the stopped try of the last
  case resumes; `finish_run_creation` updates no row, the old Run stays `Failed` and a Rerun of
  it is refused.
- **A replaced Run's queued status is dropped** (red team A4): the old Run's `failure` post
  fails once and stays queued; the new Run posts `success`; the retried old post is dropped.
- **Two tries at once** (window 3): two `ingest_event` calls of one delivery, interleaved
  with the testkit's `interleave_point` at the outcome write (constructed, not raced): one
  Run per Pipeline, one `refused_runs` row and one refusal status per refused Pipeline.
- **Postgres:** the same cases against the compose database, plus the primary-key race:
  two connections insert one key; the loser's transaction leaves no `runs` row and no
  `policy_decisions` row.
- **Old rows:** a `webhook_deliveries` row inserted without `unfinished` (as 0.7.1 writes it)
  is a duplicate.
- **Retention:** a delivery past the period is deleted with all its outcomes; a period of 6
  days is refused at start; `last_delivery_at` still answers.
- **Migration:** 0104 and 0105 apply on a database holding 0.7.1 rows; `0104_revert.sql`
  brings it back and a second run of it changes nothing.
