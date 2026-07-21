-- ADR-0058: Run-scoped shared services.
--
-- A pipeline-level `services:` entry (a Postgres/Redis a set of opt-in steps
-- reach over the network) is provisioned as a standalone Pod + k8s Service at
-- Run start and torn down at Run/Take terminal. Unlike a per-Step sidecar it is
-- a durable, Run-scoped, NON-DAG resource with its own lifecycle status — this
-- table is that durable projection.
--
-- Keyed {run, take, name}. The `take` is a stored generation integer: a Rerun
-- (ADR-0056) opens a new Take and provisions a FRESH instance keyed by the new
-- take, so it never sees the prior Take's writes. This is a deliberate, narrow
-- departure from ADR-0056's "no take column anywhere" rule — a live k8s object
-- cannot be re-derived from event replay, and ADR-0058 explicitly keys the
-- instance on {run, take}. Expand-only (a new table); nothing is renamed/dropped.
CREATE TABLE run_services (
    run_id     TEXT   NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    take       BIGINT NOT NULL,
    name       TEXT   NOT NULL,
    -- Lifecycle: starting → ready → running → torn-down | failed.
    status     TEXT   NOT NULL,
    -- The executor handle once launched (NULL before launch).
    handle     TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (run_id, take, name)
);

-- The scheduler folds a run's services every tick (find current take, gate
-- opt-in steps on readiness, drive teardown), so index the run_id lookup.
CREATE INDEX idx_run_services_run ON run_services (run_id);
