-- Slice 2: workspace flows along DAG edges (ADR-0029, 0007, 0004).
--
-- The output workspace a step produces is snapshotted into the CAS; its merkle
-- root hash is recorded here so a dependent can materialize it as its input
-- workspace (implicit-by-default: a step inherits its `needs`' outputs).
-- Nullable = backward-compatible expand (ADR-0022); a step that has not (yet)
-- produced a workspace has NULL.
ALTER TABLE step_runs ADD COLUMN output_snapshot TEXT;
