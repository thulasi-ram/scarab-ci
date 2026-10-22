-- ADR-0072 §5 (MMP, slice d3): PluginConnections — an org admin's binding of
-- a credential to the ONE destination it may be used against — and the
-- CredentialBroker's journal of every request it proxied or refused.

-- One binding per (installation, manifest connection slot). The destination
-- is part of the authority: the broker forwards a Plugin's request to
-- `base_url` and nowhere else, whatever the request names.
--
-- The credential's bytes are NOT here: they live in the SecretProvider
-- (ADR-0014) at scope Org{"_plugin_connections"}, key `credential_ref`. Every
-- re-bind mints a new key, so a destination is never re-aimed while keeping a
-- credential an admin entered for another. The rows go with their
-- installation (CASCADE); uninstall returns their keys so the caller deletes
-- the secrets.
CREATE TABLE plugin_connections (
    installation_id TEXT   NOT NULL REFERENCES plugin_installations (id) ON DELETE CASCADE,
    slot            TEXT   NOT NULL CHECK (slot <> ''),
    base_url        TEXT   NOT NULL CHECK (base_url LIKE 'http://%' OR base_url LIKE 'https://%'),
    -- Only `bearer` exists in the MMP; another scheme is a migration away.
    auth_scheme     TEXT   NOT NULL CHECK (auth_scheme = 'bearer'),
    credential_ref  TEXT   NOT NULL UNIQUE,
    -- The ONLY trust anchors for `base_url` when set; NULL = platform roots.
    -- There is deliberately no skip-verify column.
    ca_pem          TEXT,
    updated_by      TEXT   NOT NULL,
    updated_at      BIGINT NOT NULL,
    PRIMARY KEY (installation_id, slot)
);

-- The broker's journal (ADR-0072 §5 step 4), append-only. A request the
-- broker refused is one `refused` row (`denied_reason` set). A request it
-- forwarded is two rows sharing `request_id`: `head` when the upstream's
-- response head arrived — written through a queue slot reserved before
-- forwarding, so nothing is forwarded unjournaled and a long watch stream is
-- on record from its start — and `end` when the body ended (`bytes`;
-- `denied_reason` NULL, or `cut: …` for a stream cut by the kill switch or a
-- re-bind, or `upstream body error`). Written asynchronously, off the
-- request path. Deliberately NOT a foreign key: the journal outlives an
-- uninstall, for audit, until retention (14 days, owner default 9) prunes it.
CREATE TABLE plugin_broker_journal (
    id              BIGSERIAL PRIMARY KEY,
    request_id      TEXT   NOT NULL,
    phase           TEXT   NOT NULL CHECK (phase IN ('refused', 'head', 'end')),
    installation_id TEXT   NOT NULL,
    slot            TEXT   NOT NULL,
    method          TEXT   NOT NULL,
    -- The path after the slot, as sent (bounded).
    path            TEXT   NOT NULL,
    -- What was forwarded: the matched path and the query, re-encoded
    -- canonically (bounded). On a row refused for its query, `query` is the
    -- query as sent.
    upstream_path   TEXT,
    query           TEXT,
    rule            TEXT,
    status          INT,
    bytes           BIGINT,
    started_at      BIGINT NOT NULL,
    duration_ms     BIGINT,
    denied_reason   TEXT
);

-- An installation's newest entries; the retention sweep's cutoff.
CREATE INDEX plugin_broker_journal_installation_idx
    ON plugin_broker_journal (installation_id, id DESC);
CREATE INDEX plugin_broker_journal_started_at_idx
    ON plugin_broker_journal (started_at);
