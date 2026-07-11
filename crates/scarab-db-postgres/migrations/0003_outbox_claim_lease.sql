-- Outbox claim-lease columns (ADR-0003 reliable dispatch).
--
-- A drainer claims pending rows with a visibility timeout: `claimed_until` hides
-- a row from other drainers until it expires, and `claimed_by` records who holds
-- it. Marking `dispatched_at` (separately, after the effect) retires the row.
-- Crash after claim but before dispatch → the claim lapses → the row is
-- redelivered (at-least-once); the consumer's fence neutralizes the duplicate.
--
-- Both are nullable adds — a backward-compatible expand (ADR-0022).
ALTER TABLE outbox ADD COLUMN claimed_by    TEXT;
ALTER TABLE outbox ADD COLUMN claimed_until BIGINT;
