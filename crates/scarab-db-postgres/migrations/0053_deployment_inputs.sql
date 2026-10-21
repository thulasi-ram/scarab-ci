-- ADR-0070: the Deployment record, persisted as its INPUT LOG.
--
-- What is stored is every DeploymentRequest and DeploymentStatusReport,
-- append-only — never a Deployment or a status. Reads refold the log with the
-- pure `scarab-deployment` crate, so the rules live in exactly one place and a
-- stored row can never disagree with them. Same shape as the `events` table
-- (0001): the log is the truth, everything else is derived.
--
-- `seq` is per Org and IS the commit order. A Deployment's public id is the
-- seq of the input that created it, which is stable only if no reader ever
-- sees seq N+1 without seq N. A BIGSERIAL hands out numbers at INSERT time
-- and can commit them out of order; this counter row cannot: an append
-- upserts it (`ON CONFLICT … DO UPDATE … RETURNING`, the 0034
-- repo_run_counters pattern) and holds its row lock until commit, so appends
-- within one Org commit strictly in seq order. Orgs never contend.
--
-- CORE STAMPS TIME. `at` is the time the input was RECEIVED, taken inside
-- that same upsert as GREATEST(the caller's `received_at`, the Org's
-- `last_at`) — so it never goes backwards within an Org, and seq order and
-- time order are one order, which the fold requires
-- (`DeploymentLedger::apply`). `received_at` is the control plane's clock,
-- the same one a request's `effective_at` and a reader's `now` come from
-- (ADR-0070 §8); the upsert refuses one more than 60 s ahead of the database
-- clock, which would otherwise ratchet `last_at` into the future for good.
-- The `at` inside a caller's input is not stored as `at`: a source's clock
-- (Argo's, a plugin's) is not the control plane's, and the fold must never
-- decide "before"/"after" by clock skew. A report's own timestamp is kept as
-- `report_source_at`, display-only. A request's `effective_at` is clamped to
-- no later than the stamped `at`.
--
-- The legacy `deployments` table (0012, 0016) is untouched: it stays readable as
-- approval history (ADR-0070 §1, ADR-0071 §9). Nothing here references it.
CREATE TABLE deployment_input_counters (
    org      TEXT   PRIMARY KEY,
    last_seq BIGINT NOT NULL,
    -- Unix-ms of the Org's last stamped `at`; the floor for the next one.
    last_at  BIGINT NOT NULL
);

CREATE TABLE deployment_inputs (
    org           TEXT   NOT NULL,
    seq           BIGINT NOT NULL,
    -- The DeploymentTarget, which carries its Environment.
    environment   TEXT   NOT NULL,
    target_source TEXT   NOT NULL,
    target_id     TEXT   NOT NULL,
    version       TEXT   NOT NULL,
    kind          TEXT   NOT NULL CHECK (kind IN ('request', 'report')),
    -- Unix-ms receipt time, stamped by the append (see above).
    at            BIGINT NOT NULL,
    -- request only: the requesting Deploy Run step, and when it started.
    requester_run  TEXT,
    requester_step TEXT,
    effective_at   BIGINT,
    -- report only. The CHECK spells out INV-DEP-6 at rest: a source can only
    -- assert these three.
    report_status TEXT CHECK (report_status IN ('in_progress', 'live', 'failed')),
    report_label  TEXT,
    report_link   TEXT,
    -- report only: the source's own timestamp. Shown, never compared.
    report_source_at BIGINT,
    PRIMARY KEY (org, seq),
    CHECK (
        (kind = 'request'
            AND requester_run IS NOT NULL AND requester_step IS NOT NULL
            AND effective_at IS NOT NULL
            AND report_status IS NULL AND report_label IS NULL AND report_link IS NULL
            AND report_source_at IS NULL)
        OR
        (kind = 'report'
            AND report_status IS NOT NULL AND report_label IS NOT NULL
            AND report_source_at IS NOT NULL
            AND requester_run IS NULL AND requester_step IS NULL AND effective_at IS NULL)
    ),
    -- A step cannot have started after its success was recorded: the append
    -- clamps `effective_at` to the stamp, and the table holds it to that.
    CHECK (kind <> 'request' OR effective_at <= at)
);

-- Reads fold one Environment at a time, in seq order.
CREATE INDEX deployment_inputs_env_idx ON deployment_inputs (org, environment, seq);
