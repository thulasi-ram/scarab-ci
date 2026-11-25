-- Revert migration 0109 (`org_leak_check_limits`: each org's limits on leak
-- checks), so a 0.7.5 server starts on this database again. Not a migration:
-- the migrator never runs this file. A second run changes nothing.
--
-- Before running it:
--   1. scale every Scarab server to zero (no server that ran 0109 may run
--      against the database while this runs, or after it);
--   2. back up the database.
-- Then run it, e.g. `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0109_revert.sql`
-- (so psql exits non-zero when a statement fails), and start 0.7.5.
--
-- What is lost: the leak-check limits org Owners set. Upgrading again runs
-- 0109 again, and every org starts on the defaults (20 an hour, 40 a day).
-- The `secret.match_checked` events stay in `org_security_events`.
--
-- To go back further, run 0106_revert.sql next, then 0104_revert.sql, then
-- 0099_revert.sql.

BEGIN;

DROP TABLE IF EXISTS org_leak_check_limits;

DELETE FROM _sqlx_migrations WHERE version = 109;

COMMIT;
