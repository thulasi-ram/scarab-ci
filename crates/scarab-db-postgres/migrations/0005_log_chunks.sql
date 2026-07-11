-- Per-step log-chunk INDEX (ADR-0013).
--
-- Log bodies are NEVER stored in Postgres — they live chunked + compressed in
-- the object store. This table holds only the offset index: which object key
-- holds each chunk, and where it sits (uncompressed) in the step's stream, so
-- the UI/API can seek and replay without bloating the database.
CREATE TABLE log_chunks (
    run_id      TEXT   NOT NULL,
    step_id     TEXT   NOT NULL,
    attempt_id  TEXT   NOT NULL,
    seq         BIGINT NOT NULL,
    byte_offset BIGINT NOT NULL,
    len         BIGINT NOT NULL,
    object_key  TEXT   NOT NULL,
    at          BIGINT NOT NULL,
    PRIMARY KEY (run_id, step_id, attempt_id, seq)
);
