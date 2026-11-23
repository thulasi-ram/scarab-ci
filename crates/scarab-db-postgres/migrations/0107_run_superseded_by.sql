-- `runs.superseded_by` names the newer Run whose start cancelled this one:
-- newest-wins on the same repository, ref and pipeline (ADR-0032), or the
-- holder of a `cancel-in-progress` concurrency group (ADR-0011). The
-- scheduler writes it just before that cancel, so the runs list can tell a
-- Run replaced by a newer one from a Run a person cancelled; the list shows
-- it only once the run is `cancelled`. NULL for every other run and for
-- runs cancelled before this migration (no backfill).
--
-- Not `replaced_by` (0104): that names the Run a webhook redelivery created
-- in place of one whose creation never finished.
--
-- Expand step (ADR-0003): one nullable column without a default, added in
-- the catalog without rewriting a row. The ALTER takes an ACCESS EXCLUSIVE
-- lock on `runs` for that moment and waits at most 5 s for it; on a busy
-- database the start then fails with `canceling statement due to lock
-- timeout` and the Pod's restart runs it again.

SET LOCAL lock_timeout = '5s';

ALTER TABLE runs ADD COLUMN superseded_by TEXT;
