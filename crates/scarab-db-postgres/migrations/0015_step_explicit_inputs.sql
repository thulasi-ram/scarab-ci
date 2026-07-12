-- Explicit workspace inputs (ADR-0007).
--
-- The subset of a step's `needs` whose output workspace it consumes, stored as a
-- JSON array of step ids. NULL = implicit-by-default (inherit every need's
-- workspace). Used to compute a precise restart skip-if-unchanged signature
-- (ADR-0027). Backward-compatible expand (ADR-0022).
ALTER TABLE step_runs ADD COLUMN explicit_inputs JSONB;
