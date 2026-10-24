-- git-bug 51d6bcb: session ids and CSRF tokens at rest only as hashes.
--
-- The session id is the bearer secret behind the `scarab_session` cookie and a
-- CLI's `Bearer`; the CSRF token is its double-submit partner. Stored in the
-- clear, any database read — a backup, a replica, a leaked dump, a read-only
-- SQL injection — rode any live session (and, with ADR-0073, any session
-- inside a StepUpConfirmation window or a fresh-login window).
--
-- From here on:
--   id   = lowercase hex SHA-256 of the session id (the same digest the
--          step-up tables call `session_hash`); the store looks sessions up
--          by it and never writes the id itself.
--   csrf = lowercase hex SHA-256 of the CSRF token, or '' for none (which
--          matches nothing).
--
-- Existing rows are converted IN PLACE with Postgres's built-in sha256()
-- (PG 11+, no extension), so every signed-in browser and CLI keeps its
-- session: the cookie it holds hashes to the new key, its CSRF cookie to the
-- new csrf.
--
-- Not expand-contract (ADR-0003), deliberately: the point is that no
-- plaintext survives, so the CHECKs below make an OLD binary's INSERT of a
-- clear-text id or token FAIL rather than quietly re-plant one during a
-- rolling upgrade (its logins error until the rollout finishes; its lookups
-- by clear id simply miss — fail closed, never open).
UPDATE sessions
   SET id   = encode(sha256(convert_to(id, 'UTF8')), 'hex'),
       csrf = CASE WHEN csrf = '' THEN ''
                   ELSE encode(sha256(convert_to(csrf, 'UTF8')), 'hex') END;

ALTER TABLE sessions
    ADD CONSTRAINT sessions_id_is_a_sha256 CHECK (id ~ '^[0-9a-f]{64}$'),
    ADD CONSTRAINT sessions_csrf_is_a_sha256 CHECK (csrf = '' OR csrf ~ '^[0-9a-f]{64}$');
