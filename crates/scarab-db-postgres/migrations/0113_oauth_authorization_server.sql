-- The OAuth authorization server for Plugin endpoints (ADR-0086 §7).
--
-- - `oauth_client_metadata_hosts`: per org, the hosts Scarab may fetch a
--   client's metadata document from. A client is identified only by such a
--   document's URL; there is no client registration.
-- - `oauth_authorization_codes`: a code, by its SHA-256, bound to the
--   client, the redirect URI, the PKCE challenge, the resource (one
--   endpoint URL), the user, the operations consented to and the role then
--   held. Single use (`consumed_at`) and short-lived.
-- - `oauth_grants`: one consent turned into tokens. The refresh secret is
--   never stored: it is an HMAC of the grant id and `generation` under a key
--   every replica shares. `rotated_at` is when `generation` last moved, so a
--   retry of the same refresh within 30 s gets the same successor, and a
--   later reuse revokes the grant. `access_token_id` names the grant's
--   current access token (an `oauth` row of `api_tokens`).
--
-- Expand step (ADR-0003): three new tables; no existing row is read or
-- written. The lock timeout matches the other migrations of this release.

SET LOCAL lock_timeout = '5s';

CREATE TABLE oauth_client_metadata_hosts (
    org      TEXT   NOT NULL,
    host     TEXT   NOT NULL,
    added_by TEXT   NOT NULL,
    added_at BIGINT NOT NULL,
    PRIMARY KEY (org, host)
);

CREATE TABLE oauth_authorization_codes (
    code_hash         TEXT    PRIMARY KEY,
    org               TEXT    NOT NULL,
    installation_id   TEXT    NOT NULL,
    resource          TEXT    NOT NULL,
    client_id         TEXT    NOT NULL,
    client_host       TEXT    NOT NULL,
    redirect_uri      TEXT    NOT NULL,
    code_challenge    TEXT    NOT NULL,
    principal_subject TEXT    NOT NULL,
    operations        TEXT[]  NOT NULL,
    role              TEXT    NOT NULL,
    created_at        BIGINT  NOT NULL,
    expires_at        BIGINT  NOT NULL,
    consumed_at       BIGINT
);

CREATE INDEX oauth_authorization_codes_expires_idx ON oauth_authorization_codes (expires_at);

CREATE TABLE oauth_grants (
    id                TEXT    PRIMARY KEY,
    org               TEXT    NOT NULL,
    installation_id   TEXT    NOT NULL,
    resource          TEXT    NOT NULL,
    client_id         TEXT    NOT NULL,
    client_host       TEXT    NOT NULL,
    principal_subject TEXT    NOT NULL,
    operations        TEXT[]  NOT NULL,
    role              TEXT    NOT NULL,
    created_at        BIGINT  NOT NULL,
    last_used_at      BIGINT  NOT NULL,
    -- At most 90 days after consent.
    expires_at        BIGINT  NOT NULL,
    generation        BIGINT  NOT NULL DEFAULT 0,
    rotated_at        BIGINT,
    access_token_id   TEXT,
    revoked_at        BIGINT,
    revoked_reason    TEXT
);

CREATE INDEX oauth_grants_principal_idx ON oauth_grants (principal_subject);
CREATE INDEX oauth_grants_installation_idx ON oauth_grants (installation_id);
