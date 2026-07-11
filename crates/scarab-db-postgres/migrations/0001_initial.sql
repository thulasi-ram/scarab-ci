-- Scarab durable core — foundational schema (ADR-0003, 0013, 0022).
--
-- State tables are the source of truth (queryable current state); the `events`
-- table is the append-only, versioned log derived-but-durable via the `outbox`
-- (ADR-0013). Timestamps are stored as BIGINT unix-millis to mirror the pure
-- domain's `Timestamp(i64)` — the domain never depends on a date/time crate.
--
-- Version tolerance is baked in from commit one (ADR-0022): every run is
-- self-describing ({ir_version, event_schema_version}) and every event carries
-- its own schema `version` stamp for upcast-on-read.

-- A durable instance of a pipeline for a specific event/commit.
CREATE TABLE runs (
    id                    TEXT   PRIMARY KEY,
    status                TEXT   NOT NULL,
    -- Self-describing version stamps (ADR-0022): the engine advertises a
    -- supported window and parks runs outside it rather than corrupting them.
    ir_version            INTEGER NOT NULL,
    event_schema_version  INTEGER NOT NULL,
    created_at            BIGINT NOT NULL,
    updated_at            BIGINT NOT NULL
);

-- The per-run projection of a single step in the DAG.
CREATE TABLE step_runs (
    run_id      TEXT   NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    step_id     TEXT   NOT NULL,
    status      TEXT   NOT NULL,
    created_at  BIGINT NOT NULL,
    updated_at  BIGINT NOT NULL,
    PRIMARY KEY (run_id, step_id)
);

-- Ready steps are claimed with FOR UPDATE SKIP LOCKED, so index the hot column.
CREATE INDEX idx_step_runs_status ON step_runs (status);

-- One execution of a step. Restart-step mints a new attempt; the monotonic
-- attempt is the fencing unit (ADR-0021).
CREATE TABLE attempts (
    run_id      TEXT   NOT NULL,
    step_id     TEXT   NOT NULL,
    attempt_id  TEXT   NOT NULL,
    started_at  BIGINT NOT NULL,
    -- NULL while running/succeeded; 'infra' | 'step' once a failure is recorded.
    failure     TEXT,
    PRIMARY KEY (run_id, step_id, attempt_id),
    FOREIGN KEY (run_id, step_id) REFERENCES step_runs(run_id, step_id) ON DELETE CASCADE
);

-- Append-only, versioned event log. `seq` gives a total order for SSE tailing,
-- timeline, audit, and time-travel. Rows are never updated or deleted in normal
-- operation (only GC'd by retention sweeps).
CREATE TABLE events (
    seq      BIGSERIAL PRIMARY KEY,
    run_id   TEXT   NOT NULL,
    version  INTEGER NOT NULL,
    at       BIGINT NOT NULL,
    payload  JSONB  NOT NULL
);

CREATE INDEX idx_events_run_seq ON events (run_id, seq);

-- Transactional outbox for exactly-once side-effect dispatch (ADR-0003). A
-- transition and the intent to act on it are written in one transaction; a
-- dispatcher drains pending rows. `idempotency_key` is UNIQUE so a retried
-- enqueue collapses to a single effect.
CREATE TABLE outbox (
    id               BIGSERIAL PRIMARY KEY,
    run_id           TEXT   NOT NULL,
    kind             TEXT   NOT NULL,
    payload          JSONB  NOT NULL,
    idempotency_key  TEXT   NOT NULL UNIQUE,
    created_at       BIGINT NOT NULL,
    dispatched_at    BIGINT
);

-- Pending work only: partial index keeps the dispatcher's scan tiny.
CREATE INDEX idx_outbox_pending ON outbox (id) WHERE dispatched_at IS NULL;

-- Time-bounded single-owner leases (ADR-0003 leadership / step ownership).
CREATE TABLE leases (
    resource    TEXT   PRIMARY KEY,
    owner       TEXT   NOT NULL,
    expires_at  BIGINT NOT NULL
);
