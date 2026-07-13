-- ADR-0037/0015: a deploy run records whether it is locked out of secrets (a
-- fork PR), so the launch path can enforce the lockout durably without the
-- transient trigger event. Part of the run's deploy context; NULL/false for
-- ordinary runs.
ALTER TABLE runs ADD COLUMN deploy_locked_out BOOLEAN NOT NULL DEFAULT false;
