-- git-bug 64bc5c6: validate three of the checks migration 0099 added to
-- `log_chunks` NOT VALID. 0099 added them NOT VALID so that it scanned no
-- row under its ACCESS EXCLUSIVE lock; Postgres has enforced them on every
-- row written since. Rows stored before 0099 pass all three: their `kind` is
-- the column default `output`, their `reason` is NULL, and their
-- `object_key` was NOT NULL then.
--
-- VALIDATE CONSTRAINT takes a SHARE UPDATE EXCLUSIVE lock on `log_chunks`,
-- which does not block reads or writes, and scans the table once per check
-- while the server starts. On a large `log_chunks` that start takes longer.
--
-- `log_chunks_fenced` (`writer_term IS NOT NULL`) stays NOT VALID for good:
-- rows from before 0099 have no `writer_term`.
--
-- Rollback: a server built before this migration does not start on a
-- database that ran it (the migrator finds version 101, which it does not
-- know, and stops). To go back to 0.7.x, delete row 101 from
-- `_sqlx_migrations`; the validated checks are harmless to it, since 0.7.x
-- writes no row that breaks them. To go back to 0.6.x,
-- `crates/scarab-db-postgres/rollback/0099_revert.sql` reverts this
-- migration too.

SET LOCAL lock_timeout = '5s';

ALTER TABLE log_chunks VALIDATE CONSTRAINT log_chunks_kind;
ALTER TABLE log_chunks VALIDATE CONSTRAINT log_chunks_reason;
ALTER TABLE log_chunks VALIDATE CONSTRAINT log_chunks_body;
