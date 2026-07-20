-- ADR-0057 (amendment): per-repo human run number.
--
-- A run carries two identifiers: the opaque, time-sortable UUIDv7 `id` (the PK,
-- URLs, CAS paths) and this `run_number` — a per-repo sequential `#N` that is the
-- human handle. Nullable: untenanted inline runs (`POST /v1/runs` with no repo)
-- and runs created before this migration carry no number, degrading in the UI
-- exactly as pre-stamping origin does. No backfill.
ALTER TABLE runs ADD COLUMN run_number BIGINT;

-- Uniqueness is scoped per repo; nulls are excluded so untenanted/pre-migration
-- runs don't collide. A correctness backstop behind the counter allocation below.
CREATE UNIQUE INDEX runs_repo_number_idx
    ON runs (tenant_org, tenant_project, run_number)
    WHERE run_number IS NOT NULL;

-- Monotonic per-repo counter. `last_number` is the most recently assigned number
-- for the repo; allocation is one atomic upsert (INSERT … ON CONFLICT DO UPDATE
-- … RETURNING), so concurrent run creations for the same repo serialize on this
-- row rather than racing a MAX(run_number)+1 scan. Gaps are acceptable (a
-- rolled-back run creation burns a number — as on every forge).
CREATE TABLE repo_run_counters (
    org         TEXT   NOT NULL,
    project     TEXT   NOT NULL,
    last_number BIGINT NOT NULL,
    PRIMARY KEY (org, project)
);
