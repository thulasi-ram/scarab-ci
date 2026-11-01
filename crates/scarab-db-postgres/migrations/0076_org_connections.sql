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
--
-- `account_id` is the forge's immutable id of the account a connection
-- serves (a GitHub installation's `account.id`). Nothing in the database
-- knows it yet; the server records it from the next installation delivery.
ALTER TABLE forge_connections ADD COLUMN IF NOT EXISTS org TEXT;
ALTER TABLE forge_connections ADD COLUMN IF NOT EXISTS installation_id BIGINT;
ALTER TABLE forge_connections ADD COLUMN IF NOT EXISTS account_id BIGINT;

CREATE INDEX IF NOT EXISTS forge_connections_org_idx ON forge_connections (org);

-- The GitHub App installation a `github-install-<id>` connection is. At
-- most 18 digits always fits a BIGINT; a longer id (a config-declared row
-- can be named anything) is left NULL rather than aborting the migration.
UPDATE forge_connections
   SET installation_id = substring(id FROM '^github-install-([0-9]{1,18})$')::BIGINT
 WHERE kind = 'github'
   AND installation_id IS NULL
   AND id ~ '^github-install-[0-9]{1,18}$';

-- A connection whose bindings all have one owner, homed in the Org of that
-- name, serves that Org. A connection with no bindings, or bindings across
-- several owners, stays unassigned: rows are not split and the migration
-- does not refuse.
--
-- Every `github-install-*` row stays unassigned. Its account's login is not
-- in the database, and an old re-sync may have filled it with repositories
-- of other installations: assigning it by its bindings could pin it to an
-- installation that does not cover them. Unassigned, it keeps minting
-- through the per-repo installation lookup, so its Runs keep working. It is
-- assigned by its next installation delivery (when every repository bound
-- through it is the account's and the Org rule allows it) or by a global
-- admin.
UPDATE forge_connections c
   SET org = b.owner
  FROM (SELECT connection_id, min(owner) AS owner
          FROM forge_repos
         GROUP BY connection_id
        HAVING count(DISTINCT owner) = 1
           AND count(DISTINCT org) = 1
           AND min(owner) = min(org)) b
 WHERE b.connection_id = c.id
   AND c.org IS NULL
   AND c.id NOT LIKE 'github-install-%';

-- The Org record. An Org used to exist only as the `org` of a repo binding;
-- now it outlives its last binding. No other table references it yet.
CREATE TABLE IF NOT EXISTS orgs (
    name       TEXT PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Every Org anything is kept for, not only those a binding names: an Org
-- whose last connection went before this upgrade, or one set up with
-- members, secrets or policies before any connection, still exists, so an
-- installation of a same-named account does not join it by name. Every
-- table with an `org` or `scope_org` column (role bindings, API tokens,
-- Environments, Policies, Plugins, libraries, ...) is read, so a table
-- added later is not missed; then the secrets, whose scope names the Org
-- (`org:<org>`, `repo:<org>/<repo>`, `env:<org>/<repo>/<env>`). The
-- secrets table belongs to the secret store and may not exist here.
-- Reserved (`_`-prefixed) and empty names are not Orgs.
DO $$
DECLARE
    col record;
BEGIN
    FOR col IN
        SELECT c.table_name, c.column_name
          FROM information_schema.columns c
          JOIN information_schema.tables t
            ON t.table_schema = c.table_schema AND t.table_name = c.table_name
         WHERE c.table_schema = current_schema()
           AND t.table_type = 'BASE TABLE'
           AND c.column_name IN ('org', 'scope_org')
           AND c.data_type = 'text'
           AND c.table_name <> 'orgs'
    LOOP
        EXECUTE format(
            'INSERT INTO orgs (name)
             SELECT DISTINCT %1$I FROM %2$I
              WHERE %1$I IS NOT NULL AND %1$I <> '''' AND %1$I NOT LIKE ''\_%%''
             ON CONFLICT (name) DO NOTHING',
            col.column_name, col.table_name);
    END LOOP;

    IF to_regclass('secrets') IS NOT NULL THEN
        INSERT INTO orgs (name)
        SELECT DISTINCT o FROM (
            SELECT CASE
                       WHEN scope LIKE 'org:%' THEN substr(scope, 5)
                       WHEN scope LIKE 'repo:%' THEN split_part(substr(scope, 6), '/', 1)
                       WHEN scope LIKE 'env:%' THEN split_part(substr(scope, 5), '/', 1)
                   END AS o
              FROM secrets) s
         WHERE o IS NOT NULL AND o <> '' AND o NOT LIKE '\_%'
        ON CONFLICT (name) DO NOTHING;
    END IF;
END
$$;
