-- ADR-0061 s5: a manual **pin** over a Run's Workspace Snapshots — "keep this
-- Run's snapshots" — so a GC sweep skips them while somebody investigates.
--
-- The columns say `snapshots_`, not `workspace_`: CONTEXT.md §4.2 splits the two
-- terms, and what a pin holds is the *immutable, content-addressed* tree an
-- Attempt owns as evidence — never the mutable Pod-local Workspace, which died
-- with its Pod long before any sweeper could care. Plural because the pin is one
-- fact over the Run's whole SET of snapshots (grain = Run, see below), not over
-- one of them.
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
-- `snapshots_pinned_at` is the pin: non-null = pinned (epoch millis, matching
-- every other timestamp column here). `snapshots_pinned_by` is its audit half —
-- who claimed the exception; NULL when auth is off. Both nullable expands
-- (ADR-0022), so pre-migration runs are simply unpinned.
ALTER TABLE runs ADD COLUMN snapshots_pinned_at BIGINT;
ALTER TABLE runs ADD COLUMN snapshots_pinned_by TEXT;

-- The sweeper's mark query filters on "pinned at all", never on the value, and a
-- pin is rare — so a partial index keeps it tiny while still letting the planner
-- find pinned runs without a seq scan as the runs table grows.
CREATE INDEX IF NOT EXISTS runs_snapshots_pinned_idx
    ON runs (snapshots_pinned_at)
    WHERE snapshots_pinned_at IS NOT NULL;
