-- ADR-0052: artifacts of record — name-addressed per run, immutable once
-- written, independent TTL (swept as their own class by the ADR-0050
-- sweeper). Blobs live in the object store at object_key; this is metadata.
CREATE TABLE artifacts (
    run_id       TEXT   NOT NULL,
    name         TEXT   NOT NULL,
    size         BIGINT NOT NULL,
    content_type TEXT   NOT NULL,
    object_key   TEXT   NOT NULL,
    created_at   BIGINT NOT NULL,
    PRIMARY KEY (run_id, name)
);
