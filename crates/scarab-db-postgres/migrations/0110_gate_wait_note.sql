-- A plugin gate's progress note: why its open wait is not answered yet, as
-- the releasing plugin last said (git-bug 9f76ee1). Shown on the Run page
-- while the gate waits. Overwritten by each note, never journalled: it is a
-- status line, not a verdict.
ALTER TABLE attempts ADD COLUMN gate_note TEXT;
ALTER TABLE attempts ADD COLUMN gate_note_at BIGINT;
