-- ADR-0069: identity links. Until now a Principal was nothing but a string —
-- whatever the one configured provider's claims happened to produce (`sub`,
-- else `login`, else `id`) — and every binding, token and approver entry keyed
-- on it. A GitHub login is released on rename and claimable by a stranger, who
-- then inherited all of it.
--
-- principals: the Scarab-side record. `id` IS the subject every existing table
-- already stores, so nothing historical is rewritten. Rows are NEVER deleted:
-- an id, once minted, can never be minted again (a freed id would be claimed by
-- whoever produced the same string next). claimed_at NULL = never signed in as
-- (an import pre-creates these, ADR-0069 §5); merged_into = folded into
-- another Principal (§6).
--
-- principal_identities: who the Principal is on each provider, keyed by the
-- provider's IMMUTABLE id. `handle` (the login, the email) is refreshed on
-- every sign-in and never a key. At most one identity per provider per
-- Principal, so "unlink my GitHub" is unambiguous.
--
-- rbac_bindings.imported_via: which provider an import-origin row was seeded
-- through, so a re-sync prunes only what it can attribute. NULL on every
-- pre-upgrade row.
--
-- Forward-only, expand-only (ADR-0003): no column dropped, no history touched.
CREATE TABLE principals (
    id           TEXT PRIMARY KEY,
    display_name TEXT,
    created_at   BIGINT NOT NULL,
    claimed_at   BIGINT,
    merged_into  TEXT REFERENCES principals (id)
);

CREATE TABLE principal_identities (
    provider       TEXT    NOT NULL,
    external_id    TEXT    NOT NULL,
    principal_id   TEXT    NOT NULL REFERENCES principals (id),
    handle         TEXT    NOT NULL,
    email          TEXT,
    email_verified BOOLEAN NOT NULL DEFAULT FALSE,
    linked_at      BIGINT  NOT NULL,
    last_login_at  BIGINT,
    PRIMARY KEY (provider, external_id),
    UNIQUE (principal_id, provider)
);

ALTER TABLE rbac_bindings ADD COLUMN imported_via TEXT;
