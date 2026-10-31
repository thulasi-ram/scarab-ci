-- The `event` values a Run's templates may read (ADR-0076 §9): the trigger's
-- allowlisted fields that passed their check, fixed at Run creation. NULL for
-- a Run created before they were stored.
ALTER TABLE runs ADD COLUMN event_facts JSONB;

-- The library sources an org trusts with long-lived forge credentials
-- (ADR-0076 §9), keyed by the source without its commit so trust carries
-- across upgrades from the same source. The first-party clone library is
-- trusted without a row.
CREATE TABLE library_trusted_sources (
    org         TEXT   NOT NULL,
    source      TEXT   NOT NULL,
    trusted_by  TEXT   NOT NULL,
    trusted_at  BIGINT NOT NULL,
    step_up_ref TEXT,
    PRIMARY KEY (org, source)
);
