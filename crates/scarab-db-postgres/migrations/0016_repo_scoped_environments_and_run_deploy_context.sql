-- ADR-0037: environments are scoped to the owning repo (org, repo, name) rather
-- than an opaque project key — the key a run knows from its trigger event. And a
-- run durably records its *deploy context* (repo + environment + git ref) so
-- admission can find the environment's protection rules at gate-approval time
-- without archaeology over the stored IR blob.
--
-- The environment/deployment tables are new as of Slice 4 (0012) and carry no
-- data worth migrating, so we reshape them outright.
DROP TABLE IF EXISTS deployments;
DROP TABLE IF EXISTS environments;

CREATE TABLE environments (
    org        TEXT  NOT NULL,
    repo       TEXT  NOT NULL,
    name       TEXT  NOT NULL,
    protection JSONB NOT NULL,
    PRIMARY KEY (org, repo, name)
);

CREATE TABLE deployments (
    id          BIGSERIAL PRIMARY KEY,
    org         TEXT   NOT NULL,
    repo        TEXT   NOT NULL,
    environment TEXT   NOT NULL,
    git_ref     TEXT   NOT NULL,
    run_id      TEXT   NOT NULL,
    approved_by JSONB  NOT NULL,
    at          BIGINT NOT NULL
);

CREATE INDEX deployments_env_idx ON deployments (org, repo, environment, id DESC);

-- A run's deploy context. Set only for deploy runs (the pipeline declared an
-- `environment:`); NULL for ordinary CI runs.
ALTER TABLE runs ADD COLUMN deploy_org         TEXT;
ALTER TABLE runs ADD COLUMN deploy_repo        TEXT;
ALTER TABLE runs ADD COLUMN deploy_environment TEXT;
ALTER TABLE runs ADD COLUMN deploy_git_ref     TEXT;
