-- Org-scoped forge connections (git-bug 19b0a45).
--
-- A connection serves one Org: the forge owner or account whose repositories
-- it binds. Scarab's Org IS that owner, so every binding through a connection
-- has org == repo owner == connection org. NULL = unassigned: only a global
-- admin may act on it.
--
-- Expand-only, safe while old and new servers overlap in a rollout:
-- * an old server's INSERT omits `org` and `installation_id` and gets NULL,
--   which the new server reads as an unassigned connection;
-- * an old server ignores both columns and the `orgs` table, and the new
--   server also reads the Orgs of bindings, so a repo an old server binds
--   still names an Org.
--
-- Rollback: a server built before this migration refuses to boot against a
-- database that has it (sqlx reports migration 76 as unknown). To roll
-- back, restore the database from before the upgrade, or delete row 76
-- from `_sqlx_migrations`; the added columns and table are harmless to the
-- old code. A connection an old server creates after that stays unassigned
-- until a global admin assigns it, because this backfill runs once.
--
-- `installation_id` is filled for every `github-install-<id>` row, assigned
-- or not. The server pins token minting to it only once the connection
-- serves an Org: an unassigned row may hold repositories of other
-- installations, and those keep the per-repo installation lookup.
ALTER TABLE forge_connections ADD COLUMN IF NOT EXISTS org TEXT;
ALTER TABLE forge_connections ADD COLUMN IF NOT EXISTS installation_id BIGINT;

CREATE INDEX IF NOT EXISTS forge_connections_org_idx ON forge_connections (org);

-- The GitHub App installation a `github-install-<id>` connection is.
UPDATE forge_connections
   SET installation_id = substring(id FROM '^github-install-([0-9]+)$')::BIGINT
 WHERE kind = 'github'
   AND installation_id IS NULL
   AND id ~ '^github-install-[0-9]+$';

-- A connection whose bindings all have one owner, homed in the Org of that
-- name, serves that Org. A connection with no bindings, or bindings across
-- several owners, stays unassigned: rows are not split and the migration
-- does not refuse.
UPDATE forge_connections c
   SET org = b.owner
  FROM (SELECT connection_id, min(owner) AS owner
          FROM forge_repos
         GROUP BY connection_id
        HAVING count(DISTINCT owner) = 1
           AND count(DISTINCT org) = 1
           AND min(owner) = min(org)) b
 WHERE b.connection_id = c.id
   AND c.org IS NULL;

-- The Org record. An Org used to exist only as the `org` of a repo binding;
-- now it outlives its last binding. No other table references it yet.
CREATE TABLE IF NOT EXISTS orgs (
    name       TEXT PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO orgs (name)
SELECT org FROM forge_repos WHERE org NOT LIKE '\_%'
UNION
SELECT org FROM forge_connections WHERE org IS NOT NULL AND org NOT LIKE '\_%'
ON CONFLICT (name) DO NOTHING;
