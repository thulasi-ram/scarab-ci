-- ADR-0041: a step's named results — the typed `name -> value` map it emitted
-- via the results channel (ADR-0008), captured on successful completion. Distinct
-- from `output_snapshot` (a workspace CAS hash); these are small consumable values
-- a dependent reads through `${{ outputs.<step>.<name> }}` at launch. Empty object
-- for a step that emitted none.
ALTER TABLE step_runs ADD COLUMN results JSONB NOT NULL DEFAULT '{}'::jsonb;
