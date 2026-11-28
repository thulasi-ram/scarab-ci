-- Revert migration 0113 (`oauth_authorization_server`: client metadata
-- hosts, authorization codes and OAuth grants), so a server built before it
-- starts on this database again. Not a migration: the migrator never runs
-- this file. A second run changes nothing.
--
-- Before running it:
--   1. scale every Scarab server to zero (no server that ran 0113 may run
--      against the database while this runs, or after it);
--   2. back up the database.
-- Then run it, e.g. `psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f 0113_revert.sql`
-- (so psql exits non-zero when a statement fails).
--
-- What is lost: every connected app. The access tokens grants minted
-- (`oauth` rows of `api_tokens`) are revoked here, so none outlives its
-- grant; each user consents again after the next upgrade.
--
-- To go back further, run 0112_revert.sql next, then 0111_revert.sql, then
-- 0110_revert.sql, then 0109_revert.sql, then 0106_revert.sql, then
-- 0104_revert.sql, then 0099_revert.sql.

BEGIN;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'api_tokens' AND column_name = 'kind'
    ) THEN
        UPDATE api_tokens
           SET revoked_at = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
         WHERE revoked_at IS NULL AND kind = 'oauth';
    END IF;
END
$$;

DROP TABLE IF EXISTS oauth_grants;
DROP TABLE IF EXISTS oauth_authorization_codes;
DROP TABLE IF EXISTS oauth_client_metadata_hosts;

DELETE FROM _sqlx_migrations WHERE version = 113;

COMMIT;
