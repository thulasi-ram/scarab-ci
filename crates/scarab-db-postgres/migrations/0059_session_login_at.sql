-- ADR-0073 §2: when a session's login completed, and how. The FIRST passkey
-- may be enrolled only within five minutes of a fresh BROWSER login, so a
-- stolen session alone can never enroll an attacker's passkey.
--
--   created_at — unix-ms the login completed. 0 = a session minted before
--                this column existed, which is never "fresh".
--   login_kind — 'browser' (the OAuth callback) | 'api' (POST /v1/auth/login,
--                the CLI exchange — never fresh). '' = pre-upgrade.
--
-- Forward-only (expand-contract, ADR-0003): an old binary never names these
-- columns and its INSERTs take the defaults.
ALTER TABLE sessions ADD COLUMN created_at BIGINT NOT NULL DEFAULT 0,
                     ADD COLUMN login_kind TEXT   NOT NULL DEFAULT '';
