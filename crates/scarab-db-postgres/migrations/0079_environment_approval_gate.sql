-- The Environment approval gate's settings on an Environment's baseline
-- (ADR-0078 §3, git-bug cd43bf3). A Project overlay has none: these are the
-- Environment's alone.
--
-- - missing_gate_mode: what Run creation does when a Step that uses the
--   Environment's authority has no `manual` gate upstream: `add` one gate to
--   the Run (the default) or `refuse` the Run.
-- - added_gate_expiry_s: the expiry of the gate the Environment adds; NULL
--   waits until someone approves or rejects it.
-- - required_approvals: how many of the baseline's approvers must approve;
--   NULL means every one of them, the rule before this migration.
-- - max_approval_age_s: a Step that uses the Environment's authority is
--   refused at launch once its approval gate released longer ago than this.
--
-- Expand-only: every column has a default or is nullable, so an old server's
-- writes leave them at their defaults while old and new servers overlap.
--
-- Rollback: a server built before this migration refuses to boot against a
-- database that has it (sqlx reports migration 79 as unknown). Delete row 79
-- from `_sqlx_migrations`; the added columns are harmless to the old code.
ALTER TABLE org_environments
    ADD COLUMN missing_gate_mode   TEXT NOT NULL DEFAULT 'add'
        CHECK (missing_gate_mode IN ('add', 'refuse')),
    ADD COLUMN added_gate_expiry_s INT CHECK (added_gate_expiry_s > 0),
    ADD COLUMN required_approvals  INT CHECK (required_approvals > 0),
    ADD COLUMN max_approval_age_s  INT NOT NULL DEFAULT 86400 CHECK (max_approval_age_s > 0);
