-- ADR-0072 §10 (amended 2026-11-10): an installation records where its
-- manifest was read, and an upgrade records the version it replaced.
--
--   * plugin_installations.manifest_source: the source the approved
--     manifest was read from, `<host>/<owner>/<repo>//<path>@<commit>`, as
--     the server normalized it. An upgrade reads the same file at a new
--     commit by default. NULL for an installation made before this
--     migration: its source was never recorded, and the UI says so.
--   * plugin_installations.manifest_connection: the ForgeConnection the
--     source was read through; NULL = read anonymously, or not recorded.
--     (`manifest_sha256` is in the row since 0055.)
--   * plugin_installation_versions: one row per upgrade, holding what the
--     installation ran before it — image, manifest, its hash and source, who
--     had approved it — and who replaced it, when, and under which step-up.
--     It goes with its installation on uninstall, like the other rows keyed
--     by the installation id.
--
-- Expand step (ADR-0003): two nullable columns and a new table. A binary from
-- before this migration names its columns and never reads the table, so it
-- keeps working against it; it refuses to boot only because sqlx sees a
-- migration it does not know. To roll back, delete row 91 from
-- `_sqlx_migrations`, drop the table and the two columns.

ALTER TABLE plugin_installations
    ADD COLUMN manifest_source     TEXT,
    ADD COLUMN manifest_connection TEXT;

CREATE TABLE plugin_installation_versions (
    id                  BIGSERIAL PRIMARY KEY,
    installation_id     TEXT   NOT NULL REFERENCES plugin_installations (id) ON DELETE CASCADE,
    image               TEXT   NOT NULL,
    manifest            JSONB  NOT NULL,
    manifest_sha256     TEXT   NOT NULL,
    manifest_source     TEXT,
    manifest_connection TEXT,
    approved_by         TEXT   NOT NULL,
    approved_at         BIGINT NOT NULL,
    replaced_by         TEXT   NOT NULL,
    replaced_at         BIGINT NOT NULL,
    step_up_ref         TEXT
);

CREATE INDEX plugin_installation_versions_installation_idx
    ON plugin_installation_versions (installation_id, id);
