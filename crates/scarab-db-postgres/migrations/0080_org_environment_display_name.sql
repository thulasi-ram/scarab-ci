-- An Environment's display name (git-bug d002a0c).
--
-- The name stays the key: pipeline YAML, the OIDC subject
-- (`scarab:org/<org>/repo/<repo>/env/<name>/…`) and every table refer to it.
-- `display_name` is what headings, lists and pickers show in its place; NULL
-- shows the name. Never empty: an empty display name is stored as NULL.
--
-- Expand step (ADR-0003): one nullable column. A server built before this
-- migration refuses to boot against a database that has it (sqlx reports
-- migration 80 as unknown); to roll back, delete row 80 from
-- `_sqlx_migrations`. The column is harmless to the old code.
ALTER TABLE org_environments
    ADD COLUMN display_name TEXT NULL CHECK (display_name IS NULL OR display_name <> '');
