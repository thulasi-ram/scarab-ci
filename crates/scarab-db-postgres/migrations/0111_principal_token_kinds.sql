-- PrincipalTokens (ADR-0086 §6): an issued API token becomes one kind of
-- PrincipalToken, and a token may be limited to a list of ApiOperations.
--
-- - `kind`: `issued` (a `scarab_pat_` token, every row before this
--   migration) or `oauth` (granted through a consent page; nothing writes
--   one yet). A server reading a kind it does not know drops the row, so the
--   token authenticates nothing.
-- - `operations`: the ApiOperations the token may use. NULL means no list:
--   the token may use every operation its role and scope allow, as every
--   token could before. A token with a list is refused on any other.
-- - `audience`: the one URL the token may be used at. NULL for an issued
--   token, which a server refuses to authenticate when it has one.
-- - `oauth_client`: the client an `oauth` token was granted to.
--
-- A restricted token (a list, a kind other than `issued`, or an audience)
-- is stored under `r1:` + the SHA-256 of a domain and its plaintext, never
-- the plain hash, so a server built before this migration cannot find it
-- during a rolling upgrade and let it do everything its role allows.
--
-- Expand step (ADR-0003): three nullable columns and one NOT NULL column
-- with a constant default, which Postgres 11 and later add in the catalog
-- without rewriting a row. Every existing row reads as an issued token with
-- no list, so it works as it did. The ALTER takes an ACCESS EXCLUSIVE lock on
-- `api_tokens` for that moment and waits at most 5 s for it; on a busy
-- database the start then fails with `canceling statement due to lock
-- timeout` and the Pod's restart runs it again.

SET LOCAL lock_timeout = '5s';

ALTER TABLE api_tokens
    ADD COLUMN kind         TEXT NOT NULL DEFAULT 'issued',
    ADD COLUMN operations   TEXT[],
    ADD COLUMN audience     TEXT,
    ADD COLUMN oauth_client TEXT;

-- How a Run's start arrived when a PrincipalToken made it (ADR-0086 §6):
-- the dispatch's DelegationTrail as JSON, NULL for a session or a forge
-- event. One nullable column, added in the catalog without a rewrite; the
-- same lock timeout applies to `runs`.
ALTER TABLE runs ADD COLUMN origin_via JSONB;
