-- Gate timer auto-release (ADR-0008).
--
-- For a `timer` gate, the wait (seconds) after the run suspends before it
-- auto-releases and resumes. NULL for non-timer gates and ordinary steps.
-- Backward-compatible expand (ADR-0022).
ALTER TABLE step_runs ADD COLUMN gate_timer_seconds BIGINT;
