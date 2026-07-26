-- ADR-0061 s5: a manual **pin** over a Run's Workspace Snapshots — "keep this
-- Run's workspaces" — so a GC sweep skips them while somebody investigates.
--
-- Two tiers, two policies (ADR-0061): the warm workspace service is bounded by
-- SPACE and evicts LRU, carrying no promise; object storage is the cold tier,
-- bounded by TIME under a retention TTL, and that TTL is the only guarantee the
-- product gives about a Workspace Snapshot. A pin extends the *time* bound and
-- deliberately cannot say anything about the space-bounded warm tier — which is
-- exactly why the two tiers are separated.
--
-- The pin lives on the run row rather than in a side table because the grain is
-- the RUN (ADR-0056: a Run has Takes and many Attempts, and the stated use case
-- is an investigation of a whole Run, not of one Attempt), it is at most one fact
-- per run, and `gc_workspace_roots` already joins `runs` — so honouring the pin
-- costs one more disjunct in the mark query and no new join.
--
-- `workspace_pinned_at` is the pin: non-null = pinned (epoch millis, matching
-- every other timestamp column here). `workspace_pinned_by` is its audit half —
-- who claimed the exception; NULL when auth is off. Both nullable expands
-- (ADR-0022), so pre-migration runs are simply unpinned.
ALTER TABLE runs ADD COLUMN workspace_pinned_at BIGINT;
ALTER TABLE runs ADD COLUMN workspace_pinned_by TEXT;

-- The sweeper's mark query filters on "pinned at all", never on the value, and a
-- pin is rare — so a partial index keeps it tiny while still letting the planner
-- find pinned runs without a seq scan as the runs table grows.
CREATE INDEX IF NOT EXISTS runs_workspace_pinned_idx
    ON runs (workspace_pinned_at)
    WHERE workspace_pinned_at IS NOT NULL;
