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
-- and the journal of delegated calls. It refuses to run while a plugin that
-- registers an endpoint is installed: uninstall those first.
--
-- To go back further, run 0111_revert.sql next, then 0110_revert.sql, then
-- 0109_revert.sql, then 0106_revert.sql, then 0104_revert.sql, then
-- 0099_revert.sql.

BEGIN;

-- A server before 0112 knows no manifest `extensions` field and could not
-- read an installation that has one. Uninstall every plugin that registers
-- an endpoint first; this refuses while one is installed.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM plugin_installations WHERE manifest ? 'extensions'
    ) THEN
        RAISE EXCEPTION
            'a plugin installation registers an endpoint (its manifest has `extensions`); '
            'uninstall it before reverting 0112';
    END IF;
END
$$;

DROP TABLE IF EXISTS delegated_call_journal;
DROP TABLE IF EXISTS delegated_caller_handles;

DELETE FROM _sqlx_migrations WHERE version = 112;

COMMIT;
