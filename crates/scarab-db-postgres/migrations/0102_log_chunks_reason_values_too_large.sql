-- ADR-0082 §3: a `not_stored` marker gets a third reason,
-- `values_too_large` (a unit whose secret values add up to more than
-- 256 KiB, the most a log is redacted with). `log_chunks_reason` lists the
-- reasons, so it is dropped and added again with the new one.
--
-- NOT VALID, so the ADD scans no row under the ALTER's ACCESS EXCLUSIVE
-- lock; Postgres checks every row written after it. The rows already
-- stored hold only `no_ledger`, `ledger_purged` or NULL, and 0101 has just
-- validated them. A later release can VALIDATE the check under a lock that
-- blocks no write, as 0101 did.
--
-- Rollback: a server built before this migration does not start on a
-- database that ran it (the migrator finds version 102, which it does not
-- know, and stops). To go back to 0.7.x, delete rows 101 and 102 from
-- `_sqlx_migrations`. The wider check is harmless to 0.7.x; it shows a
-- `values_too_large` marker with the text of a `not_stored` marker that has
-- no reason it knows. `rollback/0099_revert.sql` reverts this migration too.

SET LOCAL lock_timeout = '5s';

ALTER TABLE log_chunks
  DROP CONSTRAINT log_chunks_reason,
  ADD CONSTRAINT log_chunks_reason
    CHECK (reason IN ('no_ledger', 'ledger_purged', 'values_too_large')) NOT VALID;
