-- ADR-0073 §4, §5: StepUpConfirmations and StepUpConfirmationRequests.

-- A confirmation is a passkey assertion made in one cookie session:
--   window  — covers window-mode operations in that session until expires_at;
--   action  — covers exactly one operation (action_digest), once (consumed_at);
--   request — the record of confirming one StepUpConfirmationRequest.
-- expires_at = confirmed_at + 5 minutes, written once at insert and NEVER
-- updated: using a window does not extend it.
CREATE TABLE step_up_confirmations (
    id            TEXT PRIMARY KEY,
    principal_id  TEXT   NOT NULL,
    session_hash  TEXT   NOT NULL,
    passkey_id    TEXT   NOT NULL,
    kind          TEXT   NOT NULL CHECK (kind IN ('window', 'action', 'request')),
    action_digest BYTEA,
    request_id    TEXT,
    confirmed_at  BIGINT NOT NULL,
    expires_at    BIGINT NOT NULL,
    consumed_at   BIGINT
);
CREATE INDEX step_up_confirmations_session_idx ON step_up_confirmations (session_hash, expires_at);

-- A crown-jewel call a machine made (an API token, or a session presented as
-- Bearer), parked for its human. `sealed` is the KEK envelope of the request
-- body (plus the requester's session id, for a Bearer session), NULLed once
-- the request is terminal; `summary` is already redacted. A request is
-- EXPIRED when state = 'pending' AND expires_at <= now — computed on read,
-- so correctness needs no sweeper.
CREATE TABLE step_up_requests (
    id               TEXT PRIMARY KEY,
    org              TEXT,
    op               TEXT   NOT NULL,
    method           TEXT   NOT NULL,
    path             TEXT   NOT NULL,
    query            TEXT   NOT NULL DEFAULT '',
    sealed           BYTEA,
    action_digest    BYTEA  NOT NULL,
    summary          JSONB  NOT NULL,
    for_human        TEXT   NOT NULL,
    requested_via    TEXT   NOT NULL CHECK (requested_via IN ('api_token', 'bearer_session')),
    api_token_id     TEXT,
    session_hash     TEXT,
    session_role_cap TEXT,
    created_at       BIGINT NOT NULL,
    expires_at       BIGINT NOT NULL,
    state            TEXT   NOT NULL CHECK (state IN ('pending', 'executing', 'executed', 'failed')),
    confirmed_by     TEXT,
    confirmed_at     BIGINT,
    confirmation_id  TEXT,
    executed_at      BIGINT,
    result_status    INTEGER,
    result_summary   TEXT,
    -- The requester column matches requested_via: a request always names
    -- exactly the credential that raised it.
    CHECK ((requested_via = 'api_token' AND api_token_id IS NOT NULL)
        OR (requested_via = 'bearer_session' AND session_hash IS NOT NULL))
);
CREATE INDEX step_up_requests_human_idx ON step_up_requests (for_human, created_at DESC);
CREATE INDEX step_up_requests_token_idx ON step_up_requests (api_token_id, state);
CREATE INDEX step_up_requests_session_idx ON step_up_requests (session_hash, state);
