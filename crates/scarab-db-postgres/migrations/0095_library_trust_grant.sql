-- What an org's trust in a library source grants (ADR-0018, amendment of
-- 2026-11-12). Until now a row meant one thing: Steps from the source may
-- receive a long-lived forge credential. A row now names its grant:
--
--   * forge_credentials: that meaning, kept for every existing row;
--   * build_sandbox: Steps from the source may run in a `security.sandbox`.
--
-- Each grant is trusted on its own, so the key becomes (org, source, grant):
-- trusting a source with forge credentials does not trust it with the
-- sandbox, and the reverse.
--
-- Contract step (ADR-0003): a binary from before this migration inserts with
-- `ON CONFLICT (org, source)`, which no longer names a unique constraint, and
-- deletes every grant of a source when it means one. It cannot run against a
-- database migrated past here.

ALTER TABLE library_trusted_sources
    ADD COLUMN trust_grant TEXT NOT NULL DEFAULT 'forge_credentials'
        CHECK (trust_grant IN ('forge_credentials', 'build_sandbox'));

ALTER TABLE library_trusted_sources DROP CONSTRAINT library_trusted_sources_pkey;

ALTER TABLE library_trusted_sources ADD PRIMARY KEY (org, source, trust_grant);
