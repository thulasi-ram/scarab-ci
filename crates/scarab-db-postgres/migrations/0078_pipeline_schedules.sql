-- The cron schedule index and fired slots (git-bug 5d5fc8a).
--
-- `pipeline_schedules`: one row per `on: cron:` expression a Pipeline file on
-- a Project's default branch declares, with its next slot. The server writes
-- it from a push to the live default branch and from each fire; it decides
-- only when the server checks the forge. What fires is read from the forge at
-- fire time.
--
-- `schedule_fires`: one row per claimed slot. The primary key is the claim:
-- two replicas, or two schedules of one Pipeline naming the same minute,
-- make one Run per Pipeline per minute.
--
-- Expand step (ADR-0003): new tables only. A server built before this
-- migration refuses to boot against a database that has it (sqlx reports
-- migration 78 as unknown); to roll back, delete row 78 from
-- `_sqlx_migrations`. The tables are harmless to the old code.
CREATE TABLE pipeline_schedules (
    org          TEXT   NOT NULL,
    project      TEXT   NOT NULL,
    -- The Pipeline file, `.scarab/<name>.yaml`.
    pipeline     TEXT   NOT NULL,
    -- The cron expression as written (trimmed).
    schedule     TEXT   NOT NULL,
    -- The next slot, a whole minute in Unix milliseconds.
    next_fire_at BIGINT NOT NULL,
    -- The default-branch commit the row was read from.
    source_sha   TEXT   NOT NULL,
    PRIMARY KEY (org, project, pipeline, schedule)
);

CREATE INDEX pipeline_schedules_due_idx ON pipeline_schedules (next_fire_at);

CREATE TABLE schedule_fires (
    org       TEXT   NOT NULL,
    project   TEXT   NOT NULL,
    pipeline  TEXT   NOT NULL,
    schedule  TEXT   NOT NULL,
    slot_at   BIGINT NOT NULL,
    fired_at  BIGINT NOT NULL,
    -- `pending`, `run`, `refused`, `skipped` or `missed`.
    outcome   TEXT   NOT NULL,
    run_id    TEXT,
    PRIMARY KEY (org, project, pipeline, slot_at)
);

CREATE INDEX schedule_fires_fired_idx ON schedule_fires (fired_at);
