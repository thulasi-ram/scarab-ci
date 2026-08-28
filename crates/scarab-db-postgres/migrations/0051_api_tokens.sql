-- ADR-0049 amendment: issued API tokens — the credential a machine can hold.
-- Until now the only bearer credential the server understood was a session id:
-- obtainable only by a browser completing an OAuth redirect, and dead in 24h.
--
-- token_hash is the SHA-256 of the plaintext, never the plaintext. This
-- deployment dumps Postgres to R2 nightly (deploy/demo-oracle/postgres.yaml);
-- a plaintext column would replicate every live credential into object storage
-- on a schedule, and a backup outlives a token by a long way. Lookup is BY the
-- hash (UNIQUE, hence indexed), so verification never compares secrets in
-- application code.
--
-- scope_org + scope_project mirror rbac_bindings exactly, project='' meaning an
-- org-scoped token: a token is a NARROWING of its owner's authority, not a
-- second authorization model, so it is keyed the way bindings are.
--
-- Forward-only (expand-contract, ADR-0003).
CREATE TABLE api_tokens (
    id            TEXT PRIMARY KEY,
    token_hash    TEXT   NOT NULL UNIQUE,
    name          TEXT   NOT NULL,
    owner_subject TEXT   NOT NULL,
    scope_org     TEXT   NOT NULL,
    scope_project TEXT   NOT NULL DEFAULT '',
    role          TEXT   NOT NULL,
    -- NOT NULL on purpose. values.yaml already records why, about the workspace
    -- results token: it "carries no verb and never expires", and that pairing is
    -- exactly why it must never be reused for anything else. This credential
    -- carries both, and the schema is where "both" stops being optional.
    expires_at    BIGINT NOT NULL,
    created_by    TEXT   NOT NULL,
    created_at    BIGINT NOT NULL,
    last_used_at  BIGINT,
    revoked_at    BIGINT
);

-- The listing endpoint is per-org, and so is every reachability question.
CREATE INDEX api_tokens_scope_idx ON api_tokens (scope_org, scope_project);

-- Offboarding walks by owner ("what does this person still hold?").
CREATE INDEX api_tokens_owner_idx ON api_tokens (owner_subject);
