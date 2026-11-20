-- Each step of a run gets a key: 12 random characters of lowercase base32
-- (`a`-`z`, `2`-`7`), unique within the run. API paths name a step by its
-- key, so a step id that holds `/` (an invoked library's `amd64/image`) or
-- `[`, `,` and `=` (a matrix step) never has to pass through a proxy as a
-- percent-encoded path segment. The key never changes: a rerun, a retry and a
-- new Take keep the row and so keep its key.
--
-- `scarab_step_key()` draws the bytes from two version 4 UUIDs
-- (`gen_random_uuid()`, a CSPRNG built into Postgres 13 and later). The first
-- 6 bytes of a version 4 UUID are all random; each byte's low 5 bits pick one
-- character, and 256 is a multiple of 32, so every character is uniform.
-- The server mints keys the same way in Rust for its in-memory store.
--
-- The DEFAULT mints the key. Adding the column with a volatile default
-- computes it once per existing row, so every step made before this
-- migration gets its own key here. The store relies on the same default for
-- new rows, and so does a binary from before this migration, which inserts
-- without naming the column.
--
-- Expand step (ADR-0003): one function, one column and one index added,
-- nothing dropped.

CREATE FUNCTION scarab_step_key() RETURNS TEXT LANGUAGE sql VOLATILE AS $$
    SELECT string_agg(
               substr('abcdefghijklmnopqrstuvwxyz234567', (get_byte(b, i) & 31) + 1, 1),
               '' ORDER BY i)
    FROM (SELECT substring(uuid_send(gen_random_uuid()) FROM 1 FOR 6)
              || substring(uuid_send(gen_random_uuid()) FROM 1 FOR 6) AS b) AS bytes,
         generate_series(0, 11) AS i
$$;

ALTER TABLE step_runs
    ADD COLUMN step_key TEXT NOT NULL DEFAULT scarab_step_key();

-- Two steps of one run drawing the same 60 bits is not expected; if it
-- happens, mint again until every key of a run is distinct, so the unique
-- index below can be built.
DO $$
BEGIN
    LOOP
        UPDATE step_runs AS s
        SET step_key = scarab_step_key()
        WHERE EXISTS (
            SELECT 1 FROM step_runs AS o
            WHERE o.run_id = s.run_id
              AND o.step_key = s.step_key
              AND o.step_id < s.step_id
        );
        EXIT WHEN NOT FOUND;
    END LOOP;
END
$$;

CREATE UNIQUE INDEX step_runs_run_id_step_key ON step_runs (run_id, step_key);
