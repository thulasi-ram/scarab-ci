-- ADR-0072 (MMP, slice d1): PluginInstallations, their kill switch, and the
-- side tables report ingest needs.
--
-- What a plugin REPORTS lands in the Deployment record (`deployment_inputs`,
-- 0053) through the same append as everything else. What it takes to accept
-- a report — idempotency receipts, per-target source sequences, the unmapped
-- bookkeeping, ownership — lives HERE, beside the record and never in it: the
-- input log holds only facts the fold reads, so it stays vendor-neutral.

-- The name ledger. An installation name is unique per Org and NEVER reused
-- there: a row is written at install and only ever retired, so the PRIMARY KEY
-- itself refuses a second installation under a name — live or uninstalled. A
-- reused name would inherit the old installation's Deployment history
-- (`deployment_inputs.target_source` is the name) and any Environment source
-- still naming it.
CREATE TABLE plugin_installation_names (
    org             TEXT   NOT NULL,
    name            TEXT   NOT NULL CHECK (name <> ''),
    installation_id TEXT   NOT NULL UNIQUE,
    installed_at    BIGINT NOT NULL,
    -- Set when uninstalled; the row stays as the tombstone.
    retired_at      BIGINT,
    retired_by      TEXT,
    PRIMARY KEY (org, name)
);

-- One live installation per (org, name). `id` is server-minted (10 hex) and
-- unique across Orgs: it names the installation's ServiceAccount, so a
-- reinstall gets a new id and the old tokens map to nothing.
CREATE TABLE plugin_installations (
    org             TEXT   NOT NULL,
    name            TEXT   NOT NULL,
    id              TEXT   NOT NULL UNIQUE,
    plugin          TEXT   NOT NULL,
    -- `<repository>@sha256:<digest>`: what the admin approved, with `manifest`.
    image           TEXT   NOT NULL,
    manifest        JSONB  NOT NULL,
    manifest_sha256 TEXT   NOT NULL,
    config          JSONB  NOT NULL DEFAULT '{}',
    -- The kill switch. Read on EVERY broker request and locked by every
    -- ingest, so a kill takes effect on the next request, not at token expiry.
    state           TEXT   NOT NULL CHECK (state IN ('enabled', 'killed')),
    -- What the runtime (slice d2) last observed. Never consulted to accept a
    -- call: that is `state`'s alone.
    runtime_status  TEXT   NOT NULL DEFAULT 'pending'
        CHECK (runtime_status IN
            ('pending', 'probing', 'running', 'probe_failed', 'error', 'stopped')),
    runtime_message TEXT   NOT NULL DEFAULT '',
    probe_passed_at BIGINT,
    approved_by     TEXT   NOT NULL,
    approved_at     BIGINT NOT NULL,
    killed_by       TEXT,
    killed_at       BIGINT,
    PRIMARY KEY (org, name),
    FOREIGN KEY (org, name) REFERENCES plugin_installation_names (org, name)
);

-- One row per (installation, DeploymentTarget) a source has reported on: the
-- cursor (last accepted sequence, ADR-0070 §11), where the target routes now,
-- its declared version scheme, its last status, and its owners — the current
-- DeploymentTargetProjectOwnership snapshot (not in the fold).
--
-- LOCK ORDER on ingest: installation row → cursor row → the Org's
-- `deployment_input_counters` row (taken by the append). Request appends take
-- only the counter, and every ingest of an installation holds its
-- installation row first, so cursor rows are never contended across
-- transactions and no cycle can form.
CREATE TABLE deployment_source_targets (
    installation_id    TEXT   NOT NULL REFERENCES plugin_installations (id) ON DELETE CASCADE,
    target_id          TEXT   NOT NULL,
    org                TEXT   NOT NULL,
    vendor_grouping    TEXT   NOT NULL,
    -- NULL = no Environment lists the grouping (unmapped).
    environment        TEXT,
    last_sequence      BIGINT NOT NULL CHECK (last_sequence > 0),
    -- NULL until a status report declares one (an ownership report has none).
    version_scheme     TEXT,
    last_version       TEXT,
    last_status        TEXT CHECK (last_status IN ('in_progress', 'live', 'failed')),
    last_label         TEXT,
    owner_projects     TEXT[] NOT NULL DEFAULT '{}',
    owners_reported_at BIGINT,
    updated_at         BIGINT NOT NULL,
    PRIMARY KEY (installation_id, target_id)
);

-- The Environment view: every reported target routed into one Environment.
CREATE INDEX deployment_source_targets_env_idx
    ON deployment_source_targets (org, environment);

-- Idempotency receipts: a repeat key with the same content is a duplicate and
-- answers with the first copy's outcome; with other content it is refused.
-- Pruned after 7 days by the retention sweeper — a repeat older than that is
-- still dropped, as stale, by the cursor.
CREATE TABLE plugin_report_receipts (
    installation_id TEXT   NOT NULL REFERENCES plugin_installations (id) ON DELETE CASCADE,
    idempotency_key TEXT   NOT NULL,
    content_sha256  TEXT   NOT NULL,
    outcome         TEXT   NOT NULL CHECK (outcome IN ('accepted', 'stale', 'unmapped')),
    -- The Deployment-record seq an accepted status landed at.
    input_seq       BIGINT,
    received_at     BIGINT NOT NULL,
    PRIMARY KEY (installation_id, idempotency_key)
);

CREATE INDEX plugin_report_receipts_received_at_idx ON plugin_report_receipts (received_at);

-- Groupings an installation reported on that no Environment listed: dropped
-- from the record (ADR-0071 §5) and counted here, so an admin can see what is
-- waiting to be mapped. Read filtered by the CURRENT sources, so a grouping
-- disappears the moment an Environment lists it.
CREATE TABLE deployment_source_unmapped (
    installation_id TEXT   NOT NULL REFERENCES plugin_installations (id) ON DELETE CASCADE,
    vendor_grouping TEXT   NOT NULL,
    org             TEXT   NOT NULL,
    reports         BIGINT NOT NULL,
    first_at        BIGINT NOT NULL,
    last_at         BIGINT NOT NULL,
    last_target_id  TEXT   NOT NULL,
    PRIMARY KEY (installation_id, vendor_grouping)
);
