-- Restart skip-if-unchanged (ADR-0027).
--
-- The input signature a step consumed on its last run: a deterministic digest of
-- its `needs`' output snapshots (see `scarab_engine::input_signature`). On a
-- restart, a re-armed step whose recomputed signature equals this stored one —
-- and which produced a content-addressed output — is skipped rather than re-run,
-- carrying its prior output forward. Nullable = backward-compatible expand
-- (ADR-0022); NULL means "not run yet" or "cleared to force a re-run" (the
-- explicit restart target).
ALTER TABLE step_runs ADD COLUMN input_signature TEXT;
