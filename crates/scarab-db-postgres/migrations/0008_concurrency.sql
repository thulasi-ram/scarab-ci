-- Slice 4: named concurrency groups (ADR-0011, 0032) — serialize/limit runs the
-- way Kubernetes cannot.
--
-- A run may belong to a named group with a policy (`queue` or
-- `cancel-in-progress`). At most one run per group holds the group's single slot
-- at a time; others queue (or the older is cancelled). Nullable columns = a
-- backward-compatible expand (ADR-0022); a run without a group is ungated.
ALTER TABLE runs ADD COLUMN concurrency_group TEXT;
ALTER TABLE runs ADD COLUMN concurrency_policy TEXT;

-- The durable slot: one row per active group naming its current holder. The
-- holder is cleared (row deleted) when the run settles, letting the next queued
-- run acquire. ON DELETE CASCADE cleans up if the holder run is ever purged.
CREATE TABLE concurrency_slots (
    group_key TEXT PRIMARY KEY,
    holder    TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE
);
