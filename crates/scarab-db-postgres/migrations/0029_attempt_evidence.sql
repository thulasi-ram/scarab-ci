-- ADR-0056: evidence moves to the attempt grain — the fence unit
-- {run, step, attempt} becomes the storage unit, so a restart never destroys
-- a prior attempt's results or workspace root (which previously made the old
-- CAS root unreachable and therefore GC-sweepable).
--
-- `step_runs.results` / `step_runs.output_snapshot` remain as the
-- latest-evidence denormalization feeding the hot path (`${{ outputs.* }}`
-- interpolation, workspace inheritance); `evidence_attempt` stamps which
-- attempt that denormalized row came from — the consumption-provenance
-- source read at a dependent's launch instant.
--
-- `consumed` records, per attempt, the map `upstream step -> attempt id` it
-- actually built on, stamped at launch (recorded, not inferred).
ALTER TABLE attempts ADD COLUMN results JSONB;
ALTER TABLE attempts ADD COLUMN output_snapshot TEXT;
ALTER TABLE attempts ADD COLUMN consumed JSONB;
ALTER TABLE step_runs ADD COLUMN evidence_attempt TEXT;
