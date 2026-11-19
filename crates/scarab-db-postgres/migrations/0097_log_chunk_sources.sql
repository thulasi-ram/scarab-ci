-- How many bytes of the log source a redacted log chunk stands for
-- (git-bug de2bd9b).
--
-- The log pipeline replaces each secret with `***` before a chunk is stored,
-- so `log_chunks.len` is the length after redaction. A log tail that broke
-- part way re-reads the container's log from the start (the Kubernetes log
-- API takes no byte offset) and skips the part already stored; it needs that
-- part's length as read from the source. A row here records it for a chunk
-- whose length redaction changed; every other chunk's source length is its
-- `len`.
--
-- A row is written before its chunk's `log_chunks` row and counts only when
-- that row exists with the same `len`, so a row left by a write that failed
-- between the two is ignored. Retention deletes the rows with the run's log
-- index.
CREATE TABLE log_chunk_sources (
    run_id      TEXT   NOT NULL,
    step_id     TEXT   NOT NULL,
    attempt_id  TEXT   NOT NULL,
    seq         BIGINT NOT NULL,
    len         BIGINT NOT NULL,
    source_len  BIGINT NOT NULL,
    PRIMARY KEY (run_id, step_id, attempt_id, seq)
);
