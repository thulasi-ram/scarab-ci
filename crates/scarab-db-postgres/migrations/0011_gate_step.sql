-- Slice 4: Gate step kind (ADR-0008, 0011) — durable suspend for approvals,
-- timers, and external events at near-zero cost.
--
-- A gate step carries a `gate_kind` (`manual` | `timer` | `external`) and runs
-- no Pod: when its dependencies are satisfied the run suspends
-- (RunStatus::Suspended) until the gate is released, then the DAG resumes.
-- NULL = an ordinary (executed) step. Backward-compatible expand (ADR-0022).
ALTER TABLE step_runs ADD COLUMN gate_kind TEXT;
