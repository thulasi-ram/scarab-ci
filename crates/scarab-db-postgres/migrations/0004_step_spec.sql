-- Durable step launch spec (ADR-0004, 0022 expand).
--
-- A step's OCI image + command + env is stored so a resumed run can re-launch
-- the same step after a control-plane crash — resume-across-crash is the normal
-- case (ADR-0011). Nullable add = backward-compatible expand.
ALTER TABLE step_runs ADD COLUMN spec JSONB;
