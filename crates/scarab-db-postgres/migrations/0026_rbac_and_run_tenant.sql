-- ADR-0049 C2: Scarab-native RBAC + run tenancy.
--
-- rbac_bindings: Principal × {Org|Project} × Role. project='' means an
-- org-scoped binding (PG primary keys cannot hold NULL). role NULL is a
-- NATIVE REVOKE TOMBSTONE: the subject was explicitly stripped here and a
-- later forge import must not resurrect the grant. origin records who wrote
-- the row — 'native' rows are authoritative; 'import' rows are seeds a
-- re-sync may refresh.
CREATE TABLE rbac_bindings (
    subject TEXT NOT NULL,
    org     TEXT NOT NULL,
    project TEXT NOT NULL DEFAULT '',
    role    TEXT,
    origin  TEXT NOT NULL DEFAULT 'native',
    PRIMARY KEY (subject, org, project)
);

CREATE INDEX rbac_bindings_org_idx ON rbac_bindings (org);

-- Runs get their owning tenant, stamped from the trigger's repo at creation
-- (the audit's cross-tenant list_runs leak needs a queryable owner). NULL =
-- an untenanted run (inline dev submission) — visible only to global roles.
ALTER TABLE runs ADD COLUMN tenant_org TEXT;
ALTER TABLE runs ADD COLUMN tenant_project TEXT;

CREATE INDEX runs_tenant_idx ON runs (tenant_org, tenant_project);
