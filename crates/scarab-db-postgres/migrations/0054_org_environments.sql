-- ADR-0071 (MMP step 1): the minimal org-level Environment and its
-- EnvironmentDeploymentSources.
--
-- An org Environment is a NAME plus what the Deployment record needs from it:
-- the EnvironmentDeploymentDeadline the read fold stalls by, and the sources
-- allowed to report into it. It coexists with the per-Project `environments`
-- table (0012, 0016) by exact name; there is deliberately no foreign key either way,
-- and nothing here feeds Run admission — per-Project Environments keep
-- governing until the full migration (ADR-0071 §10).
--
-- No column default for the deadline: the one default lives in code
-- (`scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS`), and the server
-- applies it both when a write omits a deadline and when a name has no row
-- here. The 1 minute..7 day bound is the domain type's; the CHECK only keeps
-- out what no binary could ever read.
CREATE TABLE org_environments (
    org                    TEXT   NOT NULL,
    name                   TEXT   NOT NULL CHECK (name <> ''),
    description            TEXT   NOT NULL DEFAULT '',
    deployment_deadline_ms BIGINT NOT NULL CHECK (deployment_deadline_ms > 0),
    created_at             BIGINT NOT NULL,
    updated_at             BIGINT NOT NULL,
    updated_by             TEXT   NOT NULL,
    PRIMARY KEY (org, name)
);

-- One row per (installation, vendor grouping) an Environment accepts reports
-- from. The PRIMARY KEY is the routing rule: within an Org a source maps into
-- exactly ONE Environment (ADR-0071 §5), so a second Environment claiming it
-- is refused by the database, not by a read-then-write race. `vendor_grouping`
-- rather than `grouping`, which is a Postgres keyword.
CREATE TABLE environment_deployment_sources (
    org             TEXT NOT NULL,
    environment     TEXT NOT NULL,
    installation    TEXT NOT NULL CHECK (installation <> ''),
    vendor_grouping TEXT NOT NULL CHECK (vendor_grouping <> ''),
    PRIMARY KEY (org, installation, vendor_grouping),
    FOREIGN KEY (org, environment) REFERENCES org_environments (org, name) ON DELETE CASCADE
);

-- Reading one Environment's sources (and the cascade on delete).
CREATE INDEX environment_deployment_sources_env_idx
    ON environment_deployment_sources (org, environment);
