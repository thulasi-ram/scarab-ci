-- ADR-0047 outbox poison handling: count failed deliveries and permanently
-- park a message that keeps failing (dead_lettered_at set => never claimed
-- again, retained for diagnosis). Fixes the infinite redelivery of a
-- permanently-failing message.
ALTER TABLE outbox ADD COLUMN delivery_attempts BIGINT NOT NULL DEFAULT 0;
ALTER TABLE outbox ADD COLUMN dead_lettered_at BIGINT;
