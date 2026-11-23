-- Revert migrations 0104 (webhook delivery outcomes, the delivery row's
-- `unfinished`, `config_sha`, `head_scope`, `head_sha`, `try_until` and
-- `try_token`, and `runs.replaced_by`) and 0105 (the
-- `webhook_deliveries.at` index), so a 0.7.1 server starts on this
-- database again. Not a migration: the migrator never runs this file. A
-- second run changes nothing.
--
-- Before running it:
--   1. scale every Scarab server to zero (no server that ran 0104 may run
--      against the database while this runs, or after it);
--   2. back up the database.
-- Then run it, e.g. `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0104_revert.sql`
-- (so psql exits non-zero when a statement fails), and start
-- 0.7.1.
--
-- What changes for 0.7.1: it reads every recorded delivery as done,
-- including ones a newer server left unfinished, which is its behaviour
-- today. A Run a redelivery replaced becomes an ordinary `Failed` Run whose
-- creation did not finish. Upgrading again runs 0104 and 0105 again, with
-- no outcome rows: deliveries from before the upgrade read as finished.
--
-- To go back to 0.6.x, run this file first and then 0099_revert.sql.

BEGIN;

DROP TABLE IF EXISTS webhook_delivery_pipelines;

DROP INDEX IF EXISTS webhook_deliveries_at_idx;

ALTER TABLE webhook_deliveries
  DROP COLUMN IF EXISTS unfinished,
  DROP COLUMN IF EXISTS config_sha,
  DROP COLUMN IF EXISTS head_scope,
  DROP COLUMN IF EXISTS head_sha,
  DROP COLUMN IF EXISTS try_until,
  DROP COLUMN IF EXISTS try_token;

ALTER TABLE runs DROP COLUMN IF EXISTS replaced_by;

DELETE FROM _sqlx_migrations WHERE version IN (104, 105);

COMMIT;
