-- ADR-0083 (git-bug 61174ee): a redelivery creates only the Runs a delivery
-- is missing.
--
-- `webhook_delivery_pipelines` records what one delivery decided for each
-- Pipeline file (a Run, a refusal, not run, no match, ...), written in the
-- transaction of the Run's first row or of the refusal, so a redelivery
-- decides only the files no try decided. No foreign keys: a key on `runs`
-- would lock a `runs` row on every insert, and retention removes the rows
-- with their delivery, not with their Run.
--
-- `webhook_deliveries` gains `unfinished` (a delivery a redelivery may
-- still repair), `config_sha` (the commit every try reads `.scarab/` at),
-- `head_scope` and `head_sha` (the ref or pull request the delivery is
-- about and its commit then, for the head rule of a later try) and
-- `try_until` (the lease of the try processing it). The `FALSE` default
-- makes every row written before this migration, and every row a 0.7.1
-- replica writes during a rolling update, read as finished: those
-- deliveries have no outcome rows, and running their trigger again would
-- create their Runs a second time.
--
-- `runs.replaced_by` names the Run a redelivery created in place of one
-- whose creation never finished.
--
-- Locks: a new table takes no lock on another. Since Postgres 11 a constant
-- default is stored in the catalog and a nullable column without a default
-- needs none, so no row is rewritten. Each ALTER takes an ACCESS EXCLUSIVE
-- lock for that moment and waits at most 5 s for it; on a busy database the
-- wait on `runs` can time out, the start fails with `canceling statement due
-- to lock timeout`, and the Pod's restart runs it again.
--
-- Rollback: `crates/scarab-db-postgres/rollback/0104_revert.sql` reverts
-- this migration and 0105.

SET LOCAL lock_timeout = '5s';

CREATE TABLE webhook_delivery_pipelines (
    connection_id  TEXT   NOT NULL,
    delivery_id    TEXT   NOT NULL,
    pipeline       TEXT   NOT NULL,
    forge          TEXT   NOT NULL,
    outcome        TEXT   NOT NULL CHECK (outcome IN
                     ('run', 'refused', 'not_run', 'no_match', 'all_excluded', 'ref_not_allowed')),
    reason_kind    TEXT,
    run_id         TEXT,
    refused_run_id BIGINT,
    at             BIGINT NOT NULL,
    PRIMARY KEY (connection_id, delivery_id, pipeline)
);
CREATE INDEX webhook_delivery_pipelines_delivery_idx
    ON webhook_delivery_pipelines (forge, delivery_id);

ALTER TABLE webhook_deliveries
    ADD COLUMN unfinished BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN config_sha TEXT,
    ADD COLUMN head_scope TEXT,
    ADD COLUMN head_sha   TEXT,
    ADD COLUMN try_until  BIGINT;

ALTER TABLE runs ADD COLUMN replaced_by TEXT;
