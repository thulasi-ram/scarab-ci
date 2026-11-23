-- Revert migrations 0106 (the Pipeline file a Run was created from:
-- `runs.pipeline_path`, `pipeline_source`, `pipeline_source_sha`), 0107
-- (`runs.superseded_by`) and 0108 (`runs.origin_pr_head`), so a 0.7.2
-- server starts on this database again. Not a migration: the migrator never
-- runs this file. A second run changes nothing.
--
-- Before running it:
--   1. scale every Scarab server to zero (no server that ran 0106 may run
--      against the database while this runs, or after it);
--   2. back up the database.
-- Then run it, e.g. `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0106_revert.sql`
-- (so psql exits non-zero when a statement fails), and start 0.7.2.
--
-- What is lost: the saved Pipeline files (the Manifest tab of every Run says
-- no file was saved after upgrading again), which Run replaced which, and
-- pull requests' head branches. Upgrading again runs 0106, 0107 and 0108
-- again; Runs from before then have none of the three.
--
-- To go back further, run 0104_revert.sql next, then 0099_revert.sql.

BEGIN;

ALTER TABLE runs
  DROP COLUMN IF EXISTS pipeline_path,
  DROP COLUMN IF EXISTS pipeline_source,
  DROP COLUMN IF EXISTS pipeline_source_sha,
  DROP COLUMN IF EXISTS superseded_by,
  DROP COLUMN IF EXISTS origin_pr_head;

DELETE FROM _sqlx_migrations WHERE version IN (106, 107, 108);

COMMIT;
