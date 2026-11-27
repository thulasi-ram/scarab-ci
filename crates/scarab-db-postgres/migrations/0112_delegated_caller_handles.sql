-- PluginApi (ADR-0086 §3, §4): the DelegatedCallerHandles a Plugin calls
-- core with, and the journal of every call it makes through one.
--
-- A handle is a row, not a token, because a Plugin's calls may reach any
-- server replica. `handle_hash` is the SHA-256 of the opaque value the
-- Plugin presents, never the value. `operations` is what the handle may use:
-- the endpoint operation's `may`, narrowed to the caller token's operation
-- list. Each call takes one from `calls_left`, and each write one from
-- `writes_left`, in one conditional UPDATE that refuses at zero.
--
-- The journal records each call with its caller, the PrincipalToken behind
-- it, the operation and the result. It is separate from
-- `plugin_broker_journal`, whose rows name a connection slot.
--
-- Expand step (ADR-0003): two new tables; no existing row is read or
-- written. The lock timeout matches the other migrations of this release.

SET LOCAL lock_timeout = '5s';

CREATE TABLE delegated_caller_handles (
    id              TEXT    PRIMARY KEY,
    handle_hash     TEXT    NOT NULL UNIQUE,
    installation_id TEXT    NOT NULL,
    org             TEXT    NOT NULL,
    token_id        TEXT    NOT NULL,
    operations      TEXT[]  NOT NULL,
    writes_left     INTEGER NOT NULL CHECK (writes_left >= 0),
    calls_left      INTEGER NOT NULL CHECK (calls_left >= 0),
    created_at      BIGINT  NOT NULL,
    heartbeat_at    BIGINT  NOT NULL,
    expires_at      BIGINT  NOT NULL
);

-- The sweep deletes expired handles; an uninstall finds an installation's.
CREATE INDEX delegated_caller_handles_expires_idx ON delegated_caller_handles (expires_at);
CREATE INDEX delegated_caller_handles_installation_idx
    ON delegated_caller_handles (installation_id);

CREATE TABLE delegated_call_journal (
    id              BIGSERIAL PRIMARY KEY,
    installation_id TEXT    NOT NULL,
    org             TEXT    NOT NULL,
    handle_id       TEXT    NOT NULL,
    caller_subject  TEXT    NOT NULL,
    token_id        TEXT    NOT NULL,
    token_kind      TEXT    NOT NULL,
    oauth_client    TEXT,
    operation       TEXT    NOT NULL,
    method          TEXT    NOT NULL,
    path            TEXT    NOT NULL,
    request_id      TEXT    NOT NULL,
    started_at      BIGINT  NOT NULL,
    -- NULL until the call answers; a row left NULL is a call whose replica
    -- stopped before it answered.
    status          INTEGER,
    finished_at     BIGINT
);

CREATE INDEX delegated_call_journal_installation_idx
    ON delegated_call_journal (installation_id, started_at DESC);
CREATE INDEX delegated_call_journal_started_idx ON delegated_call_journal (started_at);
