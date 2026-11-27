-- A plugin gate's progress note: why its open wait is not answered yet, as
-- the releasing plugin last said (git-bug 9f76ee1). Shown on the Run page
-- while the gate waits. Overwritten by each note, never journalled: it is a
-- status line, not a verdict.
--
-- Expand step (ADR-0003): two nullable columns without a default, which
-- Postgres 11 and later add in the catalog without rewriting a row. The
-- ALTER takes an ACCESS EXCLUSIVE lock on `attempts` for that moment and
-- waits at most 5 s for it; on a busy database the start then fails with
-- `canceling statement due to lock timeout` and the Pod's restart runs it
-- again.

SET LOCAL lock_timeout = '5s';

ALTER TABLE attempts
    ADD COLUMN gate_note    TEXT,
    ADD COLUMN gate_note_at BIGINT;
