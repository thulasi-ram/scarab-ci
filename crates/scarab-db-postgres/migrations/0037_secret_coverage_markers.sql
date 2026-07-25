-- ADR-0037 D / ADR-0060: "intentionally unset" markers for the advisory secret
-- coverage matrix. One row silences ONE cell (key × column) of one Project's
-- view, recording that the gap is a decision rather than an oversight.
--
-- Advisory only: nothing on the run path reads this table. It never blocks a
-- deploy and never affects secret resolution — losing it costs annotations, not
-- correctness.
--
-- `environment = ''` is the Project's repo-scope default column (the leftmost
-- one), rather than NULL, so the natural key stays a plain composite PK instead
-- of needing a partial unique index.
CREATE TABLE IF NOT EXISTS secret_unset_markers (
    org         TEXT NOT NULL,
    project     TEXT NOT NULL,
    environment TEXT NOT NULL DEFAULT '',
    key         TEXT NOT NULL,
    PRIMARY KEY (org, project, environment, key)
);
