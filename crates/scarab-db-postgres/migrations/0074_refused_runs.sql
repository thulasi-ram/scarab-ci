-- A Run that was never created (ADR-0076): a trigger or dispatch refused at
-- Run creation because its Pipeline does not compile, uses a yanked library
-- version, or a run.create Policy or other admission check denied it. Kept so
-- the Runs list can show the refusal where the Run would have been; before
-- this, a push refusal reached only the forge's delivery log.
--
-- Expand step (ADR-0003): a new table only.
CREATE TABLE refused_runs (
    id           BIGSERIAL PRIMARY KEY,
    org          TEXT   NOT NULL,
    project      TEXT   NOT NULL,
    -- The pipeline file, `.scarab/<name>.yaml`.
    pipeline     TEXT   NOT NULL,
    trigger_kind TEXT   NOT NULL,
    actor        TEXT,
    git_ref      TEXT,
    sha          TEXT,
    -- The trigger's headline: a push's commit subject, a dispatch reason.
    title        TEXT,
    -- `compile`, `yanked`, `policy`, `environment`, `event_field`,
    -- `forge_token`.
    reason_kind  TEXT   NOT NULL,
    message      TEXT   NOT NULL,
    -- For `compile`: each error with its fields. `[]` otherwise.
    errors       JSONB  NOT NULL DEFAULT '[]',
    created_at   BIGINT NOT NULL
);

CREATE INDEX refused_runs_tenant_idx ON refused_runs (org, project, created_at DESC);
