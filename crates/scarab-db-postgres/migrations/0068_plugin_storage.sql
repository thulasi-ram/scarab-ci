-- ADR-0074 §7: plugins bring their own storage. A PluginInstallation may name
-- a PersistentVolumeClaim an operator created in the plugin namespace; the
-- plugin runtime mounts it read-write at `storage_mount_path`. Scarab never
-- creates, backs up or reads the claim.
--
-- Expand step (ADR-0003): both columns are nullable and existing rows get
-- none, so a binary from before this migration keeps working against it.
ALTER TABLE plugin_installations
    ADD COLUMN storage_claim      TEXT,
    ADD COLUMN storage_mount_path TEXT,
    ADD CONSTRAINT plugin_installations_storage_whole
        CHECK ((storage_claim IS NULL) = (storage_mount_path IS NULL));
