-- Each library installation gets an id, so the settings page's URL names it
-- by id and not by `<publisher>~<name>`.
--
--   * library_installations.id: 10 random lowercase hex, unique across orgs,
--     as a plugin installation's id is. It comes from the random bytes of a
--     version 4 UUID (`gen_random_uuid()`, built into Postgres 13 and later),
--     the first 5 of which are random. An upgrade keeps the row and its id; a
--     removal deletes the row, so a new install gets a new id.
--
-- The DEFAULT mints the id. Adding the column with a volatile default
-- computes it once per existing row, so every installation made before this
-- migration gets its own id here. The store relies on the same default for
-- new rows, and so does a binary from before this migration, which inserts
-- without naming the column.
--
-- Expand step (ADR-0003): one column added, nothing dropped.

ALTER TABLE library_installations
    ADD COLUMN id TEXT NOT NULL
        DEFAULT substr(replace(gen_random_uuid()::text, '-', ''), 1, 10);

ALTER TABLE library_installations
    ADD CONSTRAINT library_installations_id_key UNIQUE (id);
