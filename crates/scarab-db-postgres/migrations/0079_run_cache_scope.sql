-- A Run's cache scope (git-bug efcec77): which Runs may write what this Run's
-- Cache restores. Stamped once at creation. `cache_scope` is `env:<name>` for
-- an Environment-governed Run, else the Run's symbolic ref (`refs/heads/x`,
-- `refs/pull/7/head`); it is folded into every cache key the Run reads first
-- and the only scope it writes. `cache_fallback_scope` is the default
-- branch's scope (`refs/heads/main`), read on a miss; NULL for an
-- Environment-governed Run, a Run on the default branch, or a default branch
-- unknown at creation. Both NULL (no ref and no Environment, or a Run created
-- before this column) turns the Run's Cache off.
--
-- Cache keys fold the scope now, so every `cache_entries` row written before
-- this migration is unreachable: the first builds after the upgrade are cold.
--
-- Expand step (ADR-0003): nullable columns only. A server built before this
-- migration refuses to boot against a database that has it (sqlx reports
-- migration 79 as unknown); to roll back, delete row 79 from
-- `_sqlx_migrations`. The columns are harmless to the old code.
ALTER TABLE runs ADD COLUMN cache_scope TEXT;
ALTER TABLE runs ADD COLUMN cache_fallback_scope TEXT;
