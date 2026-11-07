-- git-bug 1d06f2d: two approvals that meet an Environment's rules at the same
-- moment each wrote a row to the approval history. A row now names the gate
-- wait it records: the gate Step and the Attempt that opened the wait. One
-- row per wait; a rerun of the gate opens a new Attempt and gets its own.
-- Rows written before this migration have no wait and are left as they are
-- (NULLs never collide in a unique index).
--
-- Expand step (ADR-0003): nullable columns and an index. A server built
-- before this migration refuses to boot against a database that has it; to
-- roll back, delete row 85 from `_sqlx_migrations`.
ALTER TABLE deployments ADD COLUMN gate_step TEXT;
ALTER TABLE deployments ADD COLUMN gate_attempt TEXT;
CREATE UNIQUE INDEX deployments_one_per_gate_wait_idx
    ON deployments (run_id, gate_step, gate_attempt);
