-- Revert migration 0112 (`delegated_caller_handles`: PluginApi's handles and
-- its journal of delegated calls), so a server built before it starts on
-- this database again. Not a migration: the migrator never runs this file.
-- A second run changes nothing.
--
-- Before running it:
--   1. scale every Scarab server to zero (no server that ran 0112 may run
--      against the database while this runs, or after it);
--   2. back up the database.
-- Then run it, e.g. `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0112_revert.sql`
-- (so psql exits non-zero when a statement fails).
--
-- What is lost: every open handle (a Plugin's in-flight calls are refused)
-- and the journal of delegated calls.
--
-- To go back further, run 0111_revert.sql next, then 0110_revert.sql, then
-- 0109_revert.sql, then 0106_revert.sql, then 0104_revert.sql, then
-- 0099_revert.sql.

BEGIN;

DROP TABLE IF EXISTS delegated_call_journal;
DROP TABLE IF EXISTS delegated_caller_handles;

DELETE FROM _sqlx_migrations WHERE version = 112;

COMMIT;
