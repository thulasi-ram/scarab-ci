-- git-bug a543fef (perf half): the depot-expiry pre-epoch reachability floor
-- probes `EXISTS (SELECT 1 FROM runs WHERE created_at < epoch AND ...)` on
-- every pass (default cadence 300s), and `runs.created_at` had no index, so
-- each probe was a full `runs` scan. Pre-epoch runs are a fixed, shrinking
-- set: with the index the probe is a narrow range scan that only narrows
-- further as the floor drains.
CREATE INDEX runs_created_at_idx ON runs (created_at);
