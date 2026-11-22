# 0083. A redelivery creates only the Runs a delivery is missing

- **Status:** Proposed (design, to be red-teamed before it is built)
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

## Context

A verified webhook delivery runs in its own task (`finish_delivery`, `scarab-server/src/lib.rs`)
holding one of 64 permits. The task records the delivery id once per delivery
(`delivery_is_fresh` → `ConnectionRegistry::record_delivery`, an
`INSERT ... ON CONFLICT DO NOTHING` into `webhook_deliveries (forge, id, at)`, migration 0024).
A second delivery with the same id is answered 200 `duplicate delivery` and does nothing.
Then `ingest_event` reads `.scarab/` at the event's commit (`read_event_pipelines`), compiles
every file, and for each Pipeline whose trigger matches (`DeliveryTrigger::decide`) either
creates a Run (`admit_and_create_run` → `persist_run_from_ir`) or records a refusal.

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
  `finish_run_creation`. A write that fails ends the Run `Failed`
  (`fail_run_creation`); a server that stops in between leaves the Run `Pending`, and the
  leader's sweep ends it `Failed` after 10 minutes (`sweep_unfinished_creations`,
  `unfinished_creation_bound_ms`, reason "the server stopped while creating this Run"). Such a
  Run cannot be rerun (`RerunError::CreationUnfinished`).
- **The first row may be written with the Policy decision.** When `run.create` Policies apply,
  the Postgres policy store writes the allow decision and the Run's first row in one
  transaction (`record_run_create_decision`, `scarab-db-postgres/src/policies.rs`); the
  default implementation (the in-memory store) writes the decision alone and
  `begin_run_creation` writes the row.
- **A refusal is several writes.** `record_refused_run` inserts into `refused_runs` (no
  unique key), and `enqueue_refusal_status` queues a `failure` commit status. The status's
  idempotency key contains `run_id_for(event, path)`, which is a new random id for every
  webhook event, so two tries queue two posts. A Policy denial also writes a decision row
  first (`record_decision`).
- **Follow-ups already work this way.** An `on: upstream` Run gets an id derived from the
  upstream Run, its generation, its verdict and the Pipeline path (`run_id_for`, git-bug
  149b4db); a redelivered `run_finished` skips a path whose Run exists. This ADR applies the
  same idea to webhook deliveries.
- **Which events reach `ingest_event`.** Push, pull request, tag, release and comment
  (`Event::Comment`). Cron fires, manual and API dispatches, and follow-ups have their own
  paths and are not changed here. There is no check-suite or "re-run" event handling.
- **`webhook_deliveries` is never pruned.** It also answers the connection page's "last
  delivery" (`last_delivery_at`, `max(at)` per forge, with no index on `at`).
- **The in-memory store** (`scarab-testkit`) keeps runs, refused runs, the outbox and
  `webhook_deliveries` under one `Mutex`; one lock is its transaction.

What the forges do, checked against their docs and Forgejo's source (branch `forgejo`):

- Neither forge retries a failed delivery by itself. GitHub: "GitHub does not automatically
  redeliver failed deliveries"; a delivery can be redelivered by hand or through the API for
  3 days. Forgejo's `Deliver` (`services/webhook/deliver.go`) makes one request and records
  the result.
- GitHub's `X-GitHub-Delivery` is "a globally unique identifier (GUID) to identify the
  event", and the deliveries API lists a redelivery as its own entry with that `guid` and
  `redelivery: true`. So a GitHub **Redeliver** sends the same id. The docs do not say this
  in one sentence; it follows from those two, and the build checks it against a recorded
  redelivery before relying on it.
- Forgejo's **Replay** (`ReplayHookTask`, `models/webhook/hooktask.go`) copies the payload
  into a new hook task, and `CreateHookTask` gives it a new UUID; `X-Forgejo-Delivery`,
  `X-Gitea-Delivery` and `X-GitHub-Delivery` all carry that UUID
  (`services/webhook/shared/payloader.go`). A Forgejo Replay is a new delivery id with the
  same body.

So today a GitHub Redeliver is always answered `duplicate delivery`, and a Forgejo Replay
starts every Run again.

## Decision

### 1. The key is (forge, delivery id, Pipeline file path)

A **delivery outcome** is recorded per Pipeline file a delivery decided, with the key
`(forge, delivery_id, pipeline)`, where `pipeline` is the file path under `.scarab/` as
`read_event_pipelines` lists it (`.scarab/build.yaml`).

- **The path, not the `name:`.** Two files can declare the same `name:`, a file can omit it,
  and a redelivery lists the same paths at the same commit. `build.yaml` and `build.yml` are
  two keys, as they are two Pipelines today.
- **No trigger kind in the key.** One delivery is one event, so one trigger kind.
- **Matrix:** a matrix expands Steps inside one Run at compile time (CONTEXT.md **Matrix**), so
  it adds nothing to the key.
- **Follow-ups (`on: upstream`)** are not started by a webhook delivery; they keep their
  derived Run ids. A redelivered webhook creates its own Runs again only where missing, and
  each of those Runs starts its follow-ups once, when it ends.
- **Comment commands** are `Event::Comment` deliveries and get the same key.

What a delivery decides for a Pipeline, and whether it is recorded:

| Outcome for one Pipeline | Recorded | On a redelivery |
|---|---|---|
| A Run created | yes, `run` + the Run id | skipped |
| Refused at admission (Environment, Policy, yanked library, event field, forge token, sandbox, missing approval gate) | yes, `refused` + the `refused_runs` id | skipped |
| A `when:` that cannot be decided | yes, `refused` | skipped |
| Does not compile | yes, `refused` | skipped |
| Matched, but not run because another file does not compile | yes, `not_run` | skipped |
| Trigger does not match | no | decided again; no write, no post |
| Every Step excluded by its `when:` | no | decided again |
| Ref not allowed by the Environment (`RefNotAllowed`, silent today) | no | decided again |
| File could not be read, or its `invoke:` library could not be read | no | read again |

The rows that are not recorded produce no write and no post today, so deciding them again
costs only the forge reads and the compile. Their answer can change if an admin changed an
Environment between the two tries; the redelivery acts on the settings it reads, as any
new delivery would.

The path-filter note (`record_path_filter_note`), the follow-up record (`record_followups`)
and the cron index (`schedules::index_push`) are not keyed. The first two are written on a
Run by the try that created it. `index_push` writes only when the push is still the default
branch's head, so a redelivery of an old push writes nothing.

### 2. The outcome row is written in the transaction of the Run's first row or of the refusal

New table `webhook_delivery_pipelines`:

```sql
CREATE TABLE webhook_delivery_pipelines (
    forge          TEXT   NOT NULL,
    delivery_id    TEXT   NOT NULL,
    pipeline       TEXT   NOT NULL,   -- `.scarab/<file>`
    outcome        TEXT   NOT NULL CHECK (outcome IN ('run', 'refused', 'not_run')),
    run_id         TEXT,              -- outcome `run`
    refused_run_id BIGINT,            -- outcome `refused`, the `refused_runs` row
    at             BIGINT NOT NULL,
    PRIMARY KEY (forge, delivery_id, pipeline)
);
CREATE INDEX webhook_delivery_pipelines_at_idx ON webhook_delivery_pipelines (at);
```

No foreign keys: a key on `runs` would make every outcome insert lock a `runs` row and tie
the outcome's retention (§7) to the Run's.

Where each outcome row is written:

- **A Run.** In the transaction that inserts the Run's first row (`insert_creating_run`):
  `Db::begin_run_creation` gains an optional `DeliveryPipeline` key, and so does
  `RunCreationRow` for `PolicyStore::record_run_create_decision`, which writes the decision,
  the first row and the outcome row together. The first-row transaction is the only Run
  transaction there is; §4 covers a Run whose later writes never landed.
- **A refusal.** A new `Db::record_delivery_refusal(key, refused, status)` writes, in one
  transaction, the outcome row, the `refused_runs` row (when there is one) and the `failure`
  commit status outbox row (when there is one). `refuse_uncompiled_delivery`,
  `refuse_undecided_when` and the admission-refusal branch of `trigger_runs_from_files` call
  it for webhook deliveries in place of `record_refusal` followed by
  `enqueue_refusal_status`. A `not_run` outcome has a status and no refused row.
- **In-memory store.** The same writes under its one lock.

A primary-key violation is not an error to the caller. The port answers `Claimed` with the
outcome already recorded, and nothing of the losing write remains: no first row, no Policy
decision row from that transaction, no refused row, no status. `trigger_runs_from_files`
then counts that Pipeline as decided by another try.

The key is passed only on the webhook path (`ingest_event` → `trigger_runs_from_files`).
Dispatch, cron and follow-up callers pass none and behave as today.

### 3. A try reads the outcomes first and decides only what is missing

`trigger_runs_from_files` reads the delivery's recorded outcomes once
(`Db::delivery_outcomes(forge, delivery_id)`), before compiling. A recorded path is not
compiled, decided or created again; it is listed in the answer as `earlier`. This is the
check the follow-up path already makes with `run_status(run_id_for(..))`.

The compile rule changes scope: "a file that does not compile stops the delivery" becomes "a
file that does not compile stops every Pipeline of the delivery not yet decided". A
redelivery that finds `c.yaml` readable but broken records `c.yaml` as `refused` and posts
`not_run` only on matching Pipelines with no outcome. `a.yaml` and `b.yaml`, created by the
first try, are not touched. On a first try nothing is decided yet, so the rule is today's.

The read is an optimisation. Two tries that both pass it are separated by the primary key
(§2).

### 4. A Run whose creation did not finish is replaced by a redelivery

An outcome `run` can point at a Run whose first row exists and whose creation never finished
(`runs.creating = TRUE`): the server stopped, or a later write failed. A redelivery looks
at it:

- **Creation finished** (any status): the Pipeline is decided; skipped.
- **Still creating and younger than the unfinished-creation bound** (10 minutes, the
  scheduler's `unfinished_creation_bound_ms`, moved to a constant both read): another try may
  be writing it now. The Pipeline is left alone, and the answer is 503 (§6).
- **Creation unfinished and older than the bound, or already ended `Failed` by the sweep or
  by `end_run_creation`:** the try ends it with `fail_run_creation` (a compare-and-set from
  `Pending`; a conflict means it is already `Failed`) and creates a new Run. The new Run's
  first-row transaction moves the outcome row to it:
  `UPDATE webhook_delivery_pipelines SET run_id = $new, at = $now WHERE <key> AND run_id = $old`,
  and must update one row. Zero rows means another try replaced it first: the transaction
  rolls back and the Pipeline counts as `Claimed`.

The old Run stays in the Runs list as `Failed`, with its reason. Its `failure` post is
queued by `fail_run_creation` before the new Run exists, so the commit's last status for that
check is the new Run's.

A creation that fails for a reason that repeats (a write that always fails) gives one more
`Failed` Run per redelivery. Redeliveries are made by hand, so that is bounded by the
operator.

### 5. Two replicas, one delivery

`RunningDelivery` (the in-process set) is removed: it existed because 0.7.1 forgot ids, and
the database now decides across replicas.

Two tries of one delivery at once, on one replica or two:

- Both may read the forge and compile. Reads have no effect beyond the forge's rate limit.
- For each Pipeline, both may reach §2's write; one inserts, the other gets `Claimed`.
- Side effects in the winning transaction happen once: the Run's first row, the `run.create`
  allow decision (written with it), the refused row, the refusal or `not_run` status.
- Side effects after the first row belong to the one Run that exists: its later writes, its
  path-filter note, its follow-up record, and every commit status, check or notification,
  which are outbox rows a Run transition writes, keyed by Run and generation.
- Side effects before the transaction, which can happen twice:
  - a `run.create` **deny** decision is written by `consult_run_create` before the refusal
    transaction, so a race (or a server that stopped between the two) leaves two decision
    rows for one refusal in the org's decision log. The log records consultations, and both
    consultations happened; the refusal itself is recorded once.
  - `tracing` lines.

The 64-permit semaphore and the shutdown drain stay as they are.

### 6. The delivery-level record stays as a fast path, with an `unfinished` mark

`webhook_deliveries` keeps one row per delivery and gains
`unfinished BOOLEAN NOT NULL DEFAULT FALSE`.

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
| Some created, a file unreadable | 202 `incomplete: true`, id kept | see owner question 1 |
| A Pipeline whose Run another try is creating (§4) | 503 on the same replica only | 503 `this delivery is still running; redeliver later`, on any replica, after the other Pipelines are decided |
| No free permit in 10 s | 503, nothing recorded | same |
| No delivery id, registry wired | 400 | same |

The OpenAPI response lists on both webhook endpoints change their 502 and 503 text.

### 7. Retention

Both tables are pruned by the retention sweeper (`retention.rs`), under its leader lease, in
a new `sweep_webhook_deliveries`: rows of both tables whose `at` is older than the retention
period, in batches of 1,000 per pass. The period is an operator setting beside the other
`retention_*_days` (owner question 3 for its default).

It has to outlive GitHub's 3-day redelivery window. Pruning `webhook_deliveries` does not
weaken the replay guard of ADR-0046: the delivery id is outside the HMAC, so a captured
delivery can be replayed with a new id today. `last_delivery_at` reads `max(at)`, which the
newest rows keep.

`webhook_deliveries` has grown since 0024 without pruning, so the first passes on an old
database delete many rows, 1,000 at a time.

### 8. Migration

- **0104** (in a transaction, `SET LOCAL lock_timeout = '5s'` as 0101 does): `CREATE TABLE
  webhook_delivery_pipelines` with its index (a new table, no lock on another), and `ALTER
  TABLE webhook_deliveries ADD COLUMN unfinished BOOLEAN NOT NULL DEFAULT FALSE`. Since
  Postgres 11 a constant default is stored in the catalog and no row is rewritten; the
  statement takes an ACCESS EXCLUSIVE lock on `webhook_deliveries` for that moment, waiting
  at most 5 s for in-flight delivery inserts. A start that times out fails with `canceling
  statement due to lock timeout` and the Pod's restart retries.
- **0105** (`-- no-transaction`, as 0100): `CREATE INDEX CONCURRENTLY IF NOT EXISTS
  webhook_deliveries_at_idx ON webhook_deliveries (at)`, added to `CONCURRENT_INDEXES` so an
  invalid index is rebuilt at start. Writes go on while it builds. The sweeper and
  `last_delivery_at` use it.
- **No backfill.** Runs carry no delivery id, so outcome rows cannot be derived for past
  deliveries; the `FALSE` default makes them read as finished (§6).
- **Rollback:** `crates/scarab-db-postgres/rollback/0104_revert.sql`, in the format of
  `0099_revert.sql`: drop `webhook_delivery_pipelines`, the index and the `unfinished`
  column, and `DELETE FROM _sqlx_migrations WHERE version IN (104, 105)`. A 0.7.1 server
  then reads every recorded delivery as done, including unfinished ones; that is its
  behaviour today.

### 9. What changes for operators and forges

- A **GitHub Redeliver** of a delivery that left a Pipeline undecided (a forge read failed,
  the server stopped, another replica was mid-way) creates what is missing and nothing else.
  A Redeliver of a finished delivery is still `duplicate delivery`.
- A delivery that stopped part-way on one replica can be redelivered to any replica.
- A **Forgejo Replay** depends on owner question 2.
- The webhook answers change as in §6. Neither forge retries on its own, so the change shows
  in the forge's delivery list and in anything that watches it.
- Two migrations and a rollback file; a new retention setting.
- Whether this ships as 0.7.2 or 0.8.0 is the owner's call.

## Consequences

- Windows 1, 2 and 3 of the Context are closed for every Pipeline a redelivery reaches, on
  GitHub within its 3-day redelivery window.
- A delivery is processed again on redelivery unless it finished, so an unfinished one costs
  its forge reads again. A finished one costs one primary-key lookup, as today.
- `refused_runs` gets at most one row per (delivery, Pipeline) on the webhook path; a
  refusal's commit status is queued once per (delivery, Pipeline).
- `RunningDelivery`, `claim_delivery`, `forget_delivery` and `refuse_unread_delivery`'s
  delete go.
- The org decision log can hold two deny decisions for one refusal after a race (§5).

## Not solved

- **Run creation stays many writes.** A Run whose creation stops part-way is detected and
  replaced (§4); it is not prevented. Making `persist_run_from_ir` one transaction would mean
  a transactional `Db` port across about twenty methods.
- **No repair without a redelivery.** Neither forge retries, so an unfinished delivery stays
  unfinished until someone redelivers it (GitHub: within 3 days). A server-side sweep that
  re-runs unfinished deliveries would have to store their signed payloads; not decided here.
- **A delivery Scarab never received** (the server was down for longer than the forge waits)
  has no row at all; the forge shows it failed.
- **Comment deliveries read `.scarab/` at `HEAD`** (`config_ref`), and tag and release
  deliveries at the tag. A redelivery can read a newer commit than the first try, and the
  Pipelines it creates are those at that commit. Pre-existing.
- **Forgejo Replay** gets a new id (see owner question 2).
- **Follow-up refusals** (a chain loop or cap) are recorded again when a `run_finished` is
  delivered twice; their status is not, through the derived id. Pre-existing, not on the
  webhook path.
- **Installation events** are not keyed; applying one twice is idempotent today.

## Rejected options

- **Keep the delivery-level id and delete it on any failure.** 0.7.1 does this for "nothing
  created". It cannot cover a partial delivery or a server that stopped, which are the cases
  left.
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

## Owner questions

1. **The answer to a delivery that created some Runs while a Pipeline could not be read.**
   Today 202 with `"incomplete": true`; the forge shows it delivered and nobody redelivers it.
   Options: (a) keep 202 `incomplete`; (b) 502 with the same body (`run_ids`,
   `incomplete: true`), so the forge's list shows it failed and a Redeliver creates the rest.
   Stakes: (b) is a red delivery for a push that did start Runs; (a) leaves the missing
   Pipeline unnoticed now that a redelivery could repair it. Recommendation: (b).
2. **What a Forgejo Replay means.** It sends the same body with a new delivery id. Options:
   (a) keep the id: a Replay starts every Run again, as today, unlike a GitHub Redeliver;
   (b) key Forgejo deliveries by the SHA-256 of the body instead of `X-Forgejo-Delivery`: a
   Replay repairs like a GitHub Redeliver, and two webhooks (repository and organisation)
   that send the same event to Scarab start its Runs once instead of twice. Stakes: (b)
   removes "Replay to run again" for Forgejo users, who have Rerun in Scarab; it relies on
   Forgejo sending the stored payload byte for byte (the source copies it; not tested against
   a live Forgejo). Recommendation: (b).
3. **How long delivery records are kept.** Options: 7 days, 30 days, 90 days, or never (as
   today for `webhook_deliveries`). Stakes: shorter keeps the tables small; it must stay over
   GitHub's 3 days; longer keeps an operator able to ask "what did delivery X do" from the
   database. Recommendation: 30 days.

## Test plan

Classical, against the in-memory store and the compose Postgres (`just test rust webhook`);
the forge is the existing `FakeForge`.

- **Partial delivery repaired** (window 1): `c.yaml`'s read fails, `a.yaml` and `b.yaml` start
  Runs; the redelivery of the same id starts `c.yaml` only, and its answer lists `a.yaml` and
  `b.yaml` under `earlier`. A third delivery is `duplicate delivery` with zero forge calls.
- **Server stopped between rows** (window 2): run the trigger with a store hook that stops
  the try after `begin_run_creation` of `b.yaml`; the redelivery within the bound answers 503
  and creates nothing; with the clock past the bound it ends the old Run `Failed`, creates a
  new one, and the outcome row names the new Run.
- **Two tries at once** (window 3): two `ingest_event` calls of one delivery, interleaved
  with the testkit's `interleave_point` at the outcome write (constructed, not raced): one
  Run per Pipeline, one `refused_runs` row and one refusal status per refused Pipeline.
- **Postgres:** the same three against the compose database, plus the primary-key race:
  two connections insert one key; the loser's transaction leaves no `runs` row and no
  `policy_decisions` row.
- **Compile rule on a redelivery:** a redelivery that finds `c.yaml` broken posts `not_run`
  only on Pipelines with no outcome.
- **Old rows:** a `webhook_deliveries` row inserted without `unfinished` (as 0.7.1 writes it)
  is a duplicate.
- **Retention:** rows past the period are deleted from both tables; `last_delivery_at` still
  answers.
- **Migration:** 0104 and 0105 apply on a database holding 0.7.1 rows; `0104_revert.sql`
  brings it back and a second run of it changes nothing.
