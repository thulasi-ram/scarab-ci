-- Slice 4: first-class Environments + deployment history (ADR-0024, 0011).
--
-- An environment is a deployment target within a project, carrying its
-- protection rules (approvers, allowed refs, wait timer, concurrency, secret
-- scope, OIDC subject) as JSONB. Deployments admitted into it are appended to a
-- history table for audit.
CREATE TABLE environments (
    project    TEXT  NOT NULL,
    name       TEXT  NOT NULL,
    protection JSONB NOT NULL,
    PRIMARY KEY (project, name)
);

CREATE TABLE deployments (
    id          BIGSERIAL PRIMARY KEY,
    project     TEXT   NOT NULL,
    environment TEXT   NOT NULL,
    git_ref     TEXT   NOT NULL,
    run_id      TEXT   NOT NULL,
    approved_by JSONB  NOT NULL,
    at          BIGINT NOT NULL
);

CREATE INDEX deployments_env_idx ON deployments (project, environment, id DESC);
