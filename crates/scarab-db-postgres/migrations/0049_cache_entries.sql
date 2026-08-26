-- Keyed directory Cache (ADR-0065 s1): the control-plane mapping
-- (project, key, dir) → tree_root, written best-effort on step success and
-- read at launch to mint restore hints.
--
-- ADVISORY ONLY: under replica-local warm + eviction a row means "a drain
-- recently saved this tree warm somewhere" — a hint, never a promise. A
-- restore that 404s (evicted, or on the wrong replica) degrades to a miss,
-- which is slower and never wrong. Nothing durable references these rows;
-- deleting them all costs only cold builds. Provenance (run/step/attempt)
-- is the instrumentation for the ADR-0065 evidence bar (warm lifetime vs key
-- lifetime); `saved_at` refreshes on every upsert.
CREATE TABLE cache_entries (
    project   TEXT   NOT NULL,
    key       TEXT   NOT NULL,
    dir       TEXT   NOT NULL,
    tree_root TEXT   NOT NULL,
    saved_at  BIGINT NOT NULL,
    run_id    TEXT   NOT NULL,
    step_id   TEXT   NOT NULL,
    attempt   TEXT   NOT NULL,
    PRIMARY KEY (project, key, dir)
);
