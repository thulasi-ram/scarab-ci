-- Slice 4: auto-cancel superseded runs (ADR-0011, 0032) — newest commit wins.
--
-- A run carries an optional supersede key = (repo, ref, pipeline). When a newer
-- run with the same key is admitted, older in-flight runs with that key are
-- auto-cancelled. Set only for non-deploy pipelines (a run targeting an
-- Environment leaves it NULL and so is never auto-cancelled). Nullable =
-- backward-compatible expand (ADR-0022).
ALTER TABLE runs ADD COLUMN supersede_key TEXT;

-- Fast lookup of same-key runs.
CREATE INDEX runs_supersede_key_idx ON runs (supersede_key) WHERE supersede_key IS NOT NULL;
