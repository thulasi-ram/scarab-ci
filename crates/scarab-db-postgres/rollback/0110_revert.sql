-- Revert migration 0110 (`gate_wait_note`: a plugin gate's progress note on
-- its open wait), so a 0.8.2 server starts on this database again. Not a
-- migration: the migrator never runs this file. A second run changes nothing.
--
-- Before running it:
--   1. scale every Scarab server to zero (no server that ran 0110 may run
--      against the database while this runs, or after it);
--   2. back up the database.
-- Then run it, e.g. `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0110_revert.sql`
-- (so psql exits non-zero when a statement fails), and start 0.8.2.
--
-- What is lost: the notes plugins posted on open gate waits. A plugin posts
-- its note again when its reason next changes.
--
-- To go back further, run 0109_revert.sql next, then 0106_revert.sql, then
-- 0104_revert.sql, then 0099_revert.sql.

BEGIN;

ALTER TABLE attempts DROP COLUMN IF EXISTS gate_note;
ALTER TABLE attempts DROP COLUMN IF EXISTS gate_note_at;

DELETE FROM _sqlx_migrations WHERE version = 110;

COMMIT;
