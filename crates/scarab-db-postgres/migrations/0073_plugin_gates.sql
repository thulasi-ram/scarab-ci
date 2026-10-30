-- Plugin gates (ADR-0076 §7).
--
-- The Principal that may release a gate: `plugin:<installation>` for a
-- `gate: plugin` node, written once at Run creation. NULL for every other
-- gate and for every Step.
ALTER TABLE step_runs ADD COLUMN gate_releaser TEXT;

-- A plugin gate's inputs, rendered when the gate is reached and stored on
-- that wait's Attempt, once. NULL for every other Attempt.
ALTER TABLE attempts ADD COLUMN gate_inputs JSONB;

-- A plugin lists the open waits that name it.
CREATE INDEX step_runs_gate_releaser_idx ON step_runs (gate_releaser)
    WHERE gate_releaser IS NOT NULL;
