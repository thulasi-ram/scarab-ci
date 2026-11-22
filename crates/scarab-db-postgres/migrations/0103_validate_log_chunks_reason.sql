-- Validate `log_chunks_reason` again (git-bug 64bc5c6). 0102 dropped it and
-- added it back NOT VALID with the `values_too_large` reason, which undid
-- 0101's validation of that check. Postgres has enforced it on every row
-- written since 0102.
--
-- Its own migration, not part of 0102: sqlx runs each migration in its own
-- transaction, so this VALIDATE takes only a SHARE UPDATE EXCLUSIVE lock on
-- `log_chunks` (which blocks no read or write) and does not scan the table
-- under 0102's ACCESS EXCLUSIVE lock. It scans `log_chunks` once while the
-- server starts.
--
-- Rollback: a server built before this migration does not start on a
-- database that ran it. To go back to 0.7.x, delete rows 101, 102 and 103
-- from `_sqlx_migrations`; the validated check is harmless to 0.7.x, which
-- writes no row that breaks it. `rollback/0099_revert.sql` reverts this
-- migration too.

SET LOCAL lock_timeout = '5s';

ALTER TABLE log_chunks VALIDATE CONSTRAINT log_chunks_reason;
