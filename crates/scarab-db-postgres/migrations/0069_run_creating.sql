-- A run still being created is left out of the scheduler's work list
-- (`active_runs`). Run creation is a sequence of separate writes — params,
-- concurrency group, steps, `when:` skips — and a scheduler tick between two
-- of them would admit the run half written. `begin_run_creation` inserts the
-- row with `creating = TRUE`; `finish_run_creation` clears it.
--
-- Expand step (ADR-0003): existing rows and rows a binary from before this
-- migration inserts get FALSE, so that binary keeps working against it.
ALTER TABLE runs ADD COLUMN creating BOOLEAN NOT NULL DEFAULT FALSE;
