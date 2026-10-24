-- ADR-0073 §1: passkeys (WebAuthn credentials) and the challenges their
-- ceremonies answer.

-- Each Principal's WebAuthn user handle: 32 random bytes, no PII, minted the
-- first time it is needed and never changed.
CREATE TABLE passkey_users (
    principal_id TEXT  PRIMARY KEY,
    user_handle  BYTEA NOT NULL UNIQUE
);

-- One row per registered credential. `sign_count` is the last counter
-- accepted; `clone_suspected_at` set means the counter went backwards once
-- and the credential is never offered or accepted again.
CREATE TABLE passkeys (
    id                   TEXT PRIMARY KEY,
    principal_id         TEXT    NOT NULL,
    credential_id        BYTEA   NOT NULL UNIQUE,
    public_key_cose      BYTEA   NOT NULL,
    alg                  INTEGER NOT NULL,
    sign_count           BIGINT  NOT NULL,
    label                TEXT    NOT NULL,
    transports           TEXT[]  NOT NULL DEFAULT '{}',
    aaguid               BYTEA,
    backup_eligible      BOOLEAN NOT NULL DEFAULT FALSE,
    backup_state         BOOLEAN NOT NULL DEFAULT FALSE,
    created_at           BIGINT  NOT NULL,
    created_session_hash TEXT    NOT NULL,
    last_used_at         BIGINT,
    clone_suspected_at   BIGINT
);
CREATE INDEX passkeys_principal_idx ON passkeys (principal_id);

-- WebAuthn challenges: single use, two minutes, bound to the session that
-- asked. An action/request challenge keeps the action digest and the summary
-- the human saw, so the signature it receives commits to that action.
CREATE TABLE webauthn_challenges (
    id            TEXT PRIMARY KEY,
    principal_id  TEXT   NOT NULL,
    session_hash  TEXT   NOT NULL,
    purpose       TEXT   NOT NULL CHECK (purpose IN ('register', 'window', 'action', 'request')),
    nonce         BYTEA  NOT NULL,
    action_digest BYTEA,
    request_id    TEXT,
    label         TEXT,
    summary       JSONB,
    created_at    BIGINT NOT NULL,
    expires_at    BIGINT NOT NULL,
    consumed_at   BIGINT
);
-- The retention sweeper reaps by expiry.
CREATE INDEX webauthn_challenges_expires_idx ON webauthn_challenges (expires_at);
