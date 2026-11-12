-- The trust grants an org gives a library source beside forge credentials
-- (ADR-0018, amendment of 2026-11-12):
--
--   * build_sandbox: Steps from the source may run in a `security.sandbox`;
--   * registry_credential: Steps from the source may receive the Run's
--     registry credential (`registry_auth`).
--
-- Each grant is trusted on its own. `library_trusted_sources` keeps its one
-- meaning, trust with forge credentials, and its key: this migration only
-- adds a table, so a server from before it keeps reading and writing forge
-- trust as it did, and never sees a grant it does not know (ADR-0003,
-- expand only).

CREATE TABLE library_source_grants (
    org         TEXT   NOT NULL,
    source      TEXT   NOT NULL,
    trust_grant TEXT   NOT NULL
        CHECK (trust_grant IN ('build_sandbox', 'registry_credential')),
    trusted_by  TEXT   NOT NULL,
    trusted_at  BIGINT NOT NULL,
    step_up_ref TEXT,
    PRIMARY KEY (org, source, trust_grant)
);
