-- Slice 2: drive a real DAG durably (ADR-0006, 0011, 0022).
--
-- Two backward-compatible expands (ADR-0022):
--   * runs.ir — the compiled Pipeline IR, stored on the run so a resumed run is
--     self-describing (the "what to run" travels with the run, not the code).
--     Nullable: slice-1 runs and API callers that don't supply it stay valid.
--   * step_runs.needs — this step's dependency edges (a JSONB array of step ids).
--     Dependency-aware admission reads these to promote a step to `ready` only
--     once all its needs have `succeeded`. Defaulted to '[]' so existing rows
--     (and root steps) are unblocked.
ALTER TABLE runs ADD COLUMN ir JSONB;
ALTER TABLE step_runs ADD COLUMN needs JSONB NOT NULL DEFAULT '[]'::jsonb;
