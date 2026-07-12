-- Slice 4: fairness, backpressure, priority (ADR-0011, 0032) — no repo starves
-- another.
--
-- A run belongs to a `project` (org/repo) used to cap per-project concurrency,
-- and carries an integer `priority` (higher admits first, default 0). Excess
-- runs wait durably in Pending. Both are backward-compatible expands (ADR-0022).
ALTER TABLE runs ADD COLUMN project TEXT;
ALTER TABLE runs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
