-- ADR-0075 §9, §11: Policies, their versions, and the PolicyDecisions the
-- engine records.
--
-- Expand step (ADR-0003): three new tables, nothing existing changes, so a
-- binary from before this migration keeps working against it.

-- A Policy's current version. `project` is NULL for an org-level Policy.
-- `body` is the authored Policy (scarab_policy::policy::Policy) as JSON;
-- `checkpoint` repeats body.checkpoint so `effective` can filter on it.
-- A delete is soft: `deleted` is set and a version row records who and why.
CREATE TABLE policies (
    id           TEXT PRIMARY KEY,
    org          TEXT    NOT NULL,
    project      TEXT,
    name         TEXT    NOT NULL,
    checkpoint   TEXT    NOT NULL CHECK (checkpoint IN ('run.create', 'step.admit', 'gate.resolve')),
    version      BIGINT  NOT NULL CHECK (version >= 1),
    body         JSONB   NOT NULL,
    content_hash TEXT    NOT NULL,
    deleted      BOOLEAN NOT NULL DEFAULT false,
    created_at   BIGINT  NOT NULL,
    updated_at   BIGINT  NOT NULL
);
-- Names are unique per scope among live Policies; a deleted Policy's name can
-- be used again. COALESCE folds the org scope's NULL project into one key,
-- since NULLs are distinct in a plain unique index.
CREATE UNIQUE INDEX policies_scope_name_live_idx
    ON policies (org, COALESCE(project, ''), name) WHERE NOT deleted;
CREATE INDEX policies_effective_idx ON policies (org, checkpoint) WHERE NOT deleted;

-- Every save, the create and the delete included. Append-only.
CREATE TABLE policy_versions (
    policy_id    TEXT    NOT NULL REFERENCES policies (id),
    version      BIGINT  NOT NULL,
    body         JSONB   NOT NULL,
    content_hash TEXT    NOT NULL,
    author       TEXT    NOT NULL,
    reason       TEXT    NOT NULL,
    step_up_ref  TEXT,
    deleted      BOOLEAN NOT NULL DEFAULT false,
    at           BIGINT  NOT NULL,
    PRIMARY KEY (policy_id, version)
);

-- One consultation of the engine. `run_id` is NULL for a run.create that
-- refused the Run (the org's decision log). `step_id` and `attempt` are set
-- for step.admit, which is decided once per Attempt (ADR-0075 §3): the
-- partial unique index holds it to one decision. `input`, `messages` and
-- `results` are the decision's own JSON.
CREATE TABLE policy_decisions (
    id                TEXT PRIMARY KEY,
    org               TEXT    NOT NULL,
    project           TEXT,
    run_id            TEXT,
    step_id           TEXT,
    attempt           BIGINT,
    checkpoint        TEXT    NOT NULL CHECK (checkpoint IN ('run.create', 'step.admit', 'gate.resolve')),
    schema_version    INTEGER NOT NULL,
    input             JSONB   NOT NULL,
    policy_set_digest TEXT    NOT NULL,
    outcome           TEXT    NOT NULL CHECK (outcome IN ('allow', 'deny')),
    -- The deny messages (empty for allow), as the decision reported them.
    messages          JSONB   NOT NULL DEFAULT '[]',
    results           JSONB   NOT NULL,
    at                BIGINT  NOT NULL
);
CREATE INDEX policy_decisions_org_idx ON policy_decisions (org, at DESC, id DESC);
CREATE INDEX policy_decisions_run_idx ON policy_decisions (run_id, at) WHERE run_id IS NOT NULL;
CREATE UNIQUE INDEX policy_decisions_step_admit_idx
    ON policy_decisions (run_id, step_id, attempt) WHERE checkpoint = 'step.admit';
