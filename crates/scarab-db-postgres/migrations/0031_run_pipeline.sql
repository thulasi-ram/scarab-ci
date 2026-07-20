-- Run pipeline name: the bare `.scarab/<name>` selection a Run executed,
-- stamped at creation for trigger/dispatch runs (beside origin, ADR-0049).
--
-- A display fact for the runs list + run detail ("which pipeline is this run").
-- Not derivable from the stored IR (PipelineIr carries no top-level name), and
-- distinct from supersede_key (a concurrency key, null for deploys). Inline
-- `POST /v1/runs` runs have no named pipeline and leave it NULL.
--
-- NULL on runs created before this migration (no backfill), as origin_* was.
ALTER TABLE runs ADD COLUMN pipeline TEXT;
