-- A library is named by one name, as `invoke: lib:<name>` writes it: parts
-- of letters, digits, `.`, `_` and `-`, joined by `::` or `/`
-- (`acme::scarab-libs::deploy-helm`). Before this migration it was two
-- columns, `publisher` and `name`, and Pipelines wrote
-- `lib:<publisher>/<name>`.
--
-- Every stored name becomes `<publisher>/<name>`, so an installation made
-- before this migration keeps the name Pipelines already invoke it by
-- (`thulasi-ram/clone`), and keeps its id, version and yanks. The publisher
-- columns go, and an installation is keyed by `(org, name)`.
--
-- Two columns of the old rule held at most 100 bytes each, so a joined name
-- is at most 201 bytes, inside the rule's 255.
--
-- Contract step (ADR-0003): a binary from before this migration names the
-- publisher column and does not run against this schema. Nothing else holds
-- a publisher: library_org_defaults (0071) was dropped by 0090, and the
-- trust tables (0072, 0095) are keyed by source.

ALTER TABLE library_installations DROP CONSTRAINT library_installations_pkey;

UPDATE library_versions SET name = publisher || '/' || name;
UPDATE library_installations SET name = publisher || '/' || name;

ALTER TABLE library_versions DROP COLUMN publisher;
ALTER TABLE library_installations DROP COLUMN publisher;

ALTER TABLE library_installations ADD PRIMARY KEY (org, name);
