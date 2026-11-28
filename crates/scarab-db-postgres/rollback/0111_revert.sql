-- Revert migration 0111 (`principal_token_kinds`: each API token's kind,
-- operation list, audience and OAuth client), so a 0.8.3 server starts on
-- this database again. Not a migration: the migrator never runs this file. A
-- second run changes nothing.
--
-- Before running it:
--   1. scale every Scarab server to zero (no server that ran 0111 may run
--      against the database while this runs, or after it);
--   2. back up the database.
-- Then run it, e.g. `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0111_revert.sql`
-- (so psql exits non-zero when a statement fails), and start 0.8.3.
--
-- What is lost: each token's operation list, and the record of which token
-- started each Run. A 0.8.3 server would let a
-- token that had a list use every operation its role and scope allow, so
-- this revokes every token that has a list or is not an issued token before
-- it drops the columns. Their owners mint new ones.
--
-- To go back further, run 0110_revert.sql next, then 0109_revert.sql, then
-- 0106_revert.sql, then 0104_revert.sql, then 0099_revert.sql.

BEGIN;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'api_tokens' AND column_name = 'operations'
    ) THEN
        UPDATE api_tokens
           SET revoked_at = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
         WHERE revoked_at IS NULL
           AND (operations IS NOT NULL OR kind <> 'issued' OR audience IS NOT NULL);
    END IF;
END
$$;

ALTER TABLE api_tokens DROP COLUMN IF EXISTS kind;
ALTER TABLE api_tokens DROP COLUMN IF EXISTS operations;
ALTER TABLE api_tokens DROP COLUMN IF EXISTS audience;
ALTER TABLE api_tokens DROP COLUMN IF EXISTS oauth_client;
ALTER TABLE runs DROP COLUMN IF EXISTS origin_via;

DELETE FROM _sqlx_migrations WHERE version = 111;

COMMIT;
