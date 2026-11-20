-- Each step of a run gets a key: 12 random characters of lowercase base32
-- (`a`-`z`, `2`-`7`), unique within the run. API paths name a step by its
-- key, so a step id that holds `/` (an invoked library's `amd64/image`) or
-- `[`, `,` and `=` (a matrix step) never has to pass through a proxy as a
-- percent-encoded path segment. The key never changes: a rerun, a retry and a
-- new Take keep the row and so keep its key.
--
-- `scarab_step_key()` is one expression, so Postgres inlines it into the
-- statement that evaluates the DEFAULT: 9 random bytes (the first 6 of one
-- version 4 UUID and the first 3 of another; `gen_random_uuid()` is a CSPRNG
-- built into Postgres 13 and later, and those bytes of a version 4 UUID are
-- all random) are base64-encoded to exactly 12 characters, and `translate`
-- maps each of the 64 base64 characters to the key alphabet character at its
-- index mod 32. 64 is twice 32, so every character is uniform: 60 random
-- bits. The server's in-memory store mints keys with the same alphabet and
-- length from the OS CSPRNG. On Postgres 16 this costs about 5 us a call,
-- against 9 us for a body that aggregated one character at a time.
--
-- The DEFAULT mints the key. Adding the column with a volatile default
-- rewrites the table and computes it once per existing row, so every step
-- made before this migration gets its own key here; the rewrite holds an
-- ACCESS EXCLUSIVE lock on `step_runs`, and its time grows with the table
-- (about 7 us a row on Postgres 16, index rebuild included). The store relies
-- on the same default for new rows, and so does a binary from before this
-- migration, which inserts without naming the column.
--
-- Expand step (ADR-0003): one function, one column and one index added,
-- nothing dropped.

CREATE FUNCTION scarab_step_key() RETURNS TEXT LANGUAGE sql VOLATILE AS $$
    SELECT translate(
        encode(substring(uuid_send(gen_random_uuid()) FROM 1 FOR 6)
               || substring(uuid_send(gen_random_uuid()) FROM 1 FOR 3), 'base64'),
        'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/',
        'abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwxyz234567')
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
