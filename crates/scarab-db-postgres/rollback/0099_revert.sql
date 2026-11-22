-- Revert migrations 0099 (log streams and the value ledger), 0100 (the
-- `attempts.started_at` index), 0101 (validating 0099's checks) and 0102
-- (the `values_too_large` reason), so a 0.6.x server starts on this
-- database again. Not a migration: the
-- migrator never runs this file. It also reverts a database that ran 0099
-- and 0100 but not 0101.
--
-- Before running it:
--   1. let Runs finish and stop new ones;
--   2. scale every Scarab server to zero (no server of 0.7 or later may run
--      against the database while this runs, or after it);
--   3. back up the database.
-- Then run it once, e.g. `psql "$DATABASE_URL" -f 0099_revert.sql`, and
-- start 0.6.x.
--
-- What it deletes: the marker rows of logs stored by 0.7 (`kind` other than
-- `output`) and every value ledger. Output rows stay, and 0.6.x reads them
-- as before. Upgrading again runs 0099 to 0102 again.

BEGIN;

DELETE FROM log_chunks WHERE kind <> 'output';

DROP FUNCTION IF EXISTS scarab_append_log_row(
  TEXT, TEXT, TEXT, TEXT, TEXT, BIGINT, TEXT, TEXT, TEXT, BIGINT, BYTEA, BIGINT, TEXT, BIGINT);

DROP TABLE IF EXISTS log_unit_values;
DROP TABLE IF EXISTS log_units;
DROP TABLE IF EXISTS log_streams;
DROP TABLE IF EXISTS log_legacy_units;
DROP TABLE IF EXISTS log_store_epoch;
DROP TABLE IF EXISTS log_prune_pending;

ALTER TABLE log_chunks
  DROP CONSTRAINT IF EXISTS log_chunks_kind,
  DROP CONSTRAINT IF EXISTS log_chunks_reason,
  DROP CONSTRAINT IF EXISTS log_chunks_body,
  DROP CONSTRAINT IF EXISTS log_chunks_fenced,
  DROP COLUMN IF EXISTS kind,
  DROP COLUMN IF EXISTS reason,
  DROP COLUMN IF EXISTS writer_term,
  DROP COLUMN IF EXISTS values_gen,
  DROP COLUMN IF EXISTS digest,
  ALTER COLUMN object_key SET NOT NULL;

DROP INDEX IF EXISTS attempts_started_at_idx;

DELETE FROM _sqlx_migrations WHERE version IN (99, 100, 101, 102);

COMMIT;
