-- Revert migration 0114 (`org_fork_run_approval`: each org's Fork Run
-- approval setting), so a server built before it starts on this database
-- again. Not a migration: the migrator never runs this file. A second run
-- changes nothing.
--
-- Before running it:
--   1. set Fork Run approval to `off` in every org, and release
--      or reject the Fork Run approval gates still waiting (a Run created
--      while the setting was on keeps its gate after the revert);
--   2. scale every Scarab server to zero (no server that ran 0114 may run
--      against the database while this runs, or after it);
--   3. back up the database.
-- Then run it, e.g. `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0114_revert.sql`
-- (so psql exits non-zero when a statement fails).
--
-- What is lost: each org's setting. Upgrading again runs 0114 again, and
-- every org starts on the default, `agent_started`. A Run that has the gate keeps it:
-- 0.8.4 and later read it, an earlier server cannot load such a Run.
--
-- To go back further, run the revert script of every other migration above
-- 0111 this database ran (0113, 0112), newest first, then 0111_revert.sql,
-- then 0110_revert.sql, then 0109_revert.sql, then 0106_revert.sql, then
-- 0104_revert.sql, then 0099_revert.sql.

BEGIN;

-- The DROP waits at most 5 seconds for its lock on `org_fork_run_approval`,
-- as migration 0114 does for its own: a server still running against the
-- database holds it, and the script then stops with `canceling statement
-- due to lock timeout` and changes nothing. Scale the servers to zero and
-- run it again.
SET LOCAL lock_timeout = '5s';

DROP TABLE IF EXISTS org_fork_run_approval;

DELETE FROM _sqlx_migrations WHERE version = 114;

COMMIT;
