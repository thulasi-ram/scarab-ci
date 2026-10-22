-- ADR-0071 §1-3, §8, §10 (Deployments MMP slice c-full, sub-slice c2): the
-- org-level Environment becomes what governs a Deploy Run, and every
-- per-Project ADR-0037 environment is migrated so that each Run the migration
-- can see is admitted and governed exactly as before.
--
-- Shape:
--   * the BASELINE lives as typed columns on `org_environments` (it is a fixed
--     set of rules, not a document), with `eligible_all_projects` beside it;
--   * an explicit eligibility list is `environment_project_eligibility`
--     (project-name globs; an empty list admits NO project — ADR-0071 §2);
--   * each Project's tighten-only overlay and its privilege grant are their
--     own tables, keyed (org, environment, project). Their foreign keys are
--     ON DELETE RESTRICT: deleting an Environment must never silently drop a
--     Project's stricter rules or its privileges;
--   * `environments` is RENAMED to `legacy_project_environments` and frozen:
--     nothing admits by it, nothing writes it (the per-repo write API answers
--     410), and it is never dropped — it is the evidence the split report and
--     the golden tests read.
--
-- The backfill is a function so the golden test can call it a second time
-- and prove it a no-op; sqlx runs this whole file in one transaction.

ALTER TABLE org_environments
    ADD COLUMN approvers              TEXT[]  NOT NULL DEFAULT '{}',
    ADD COLUMN allowed_refs           TEXT[]  NOT NULL DEFAULT '{}',
    ADD COLUMN wait_timer_s           INT     NOT NULL DEFAULT 0 CHECK (wait_timer_s >= 0),
    ADD COLUMN require_reason         BOOLEAN NOT NULL DEFAULT false,
    -- NULL = no limit: concurrency is an explicit opt-in (ADR-0071 §9).
    ADD COLUMN deployment_concurrency INT     NULL CHECK (deployment_concurrency > 0),
    ADD COLUMN eligible_all_projects  BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE environment_project_eligibility (
    org             TEXT NOT NULL,
    environment     TEXT NOT NULL,
    project_pattern TEXT NOT NULL CHECK (project_pattern <> ''),
    PRIMARY KEY (org, environment, project_pattern),
    FOREIGN KEY (org, environment) REFERENCES org_environments (org, name) ON DELETE CASCADE
);

ALTER TABLE environments RENAME TO legacy_project_environments;

CREATE TABLE environment_project_overlays (
    org                         TEXT    NOT NULL,
    environment                 TEXT    NOT NULL,
    project                     TEXT    NOT NULL CHECK (project <> ''),
    approvers                   TEXT[]  NOT NULL DEFAULT '{}',
    allowed_refs                TEXT[]  NOT NULL DEFAULT '{}',
    wait_timer_s                INT     NOT NULL DEFAULT 0 CHECK (wait_timer_s >= 0),
    require_reason              BOOLEAN NOT NULL DEFAULT false,
    deployment_concurrency      INT     NULL CHECK (deployment_concurrency > 0),
    -- Applies to this Project's DeploymentGates only (ADR-0071 §3).
    deployment_gate_deadline_ms BIGINT  NULL CHECK (deployment_gate_deadline_ms > 0),
    updated_at                  BIGINT  NOT NULL,
    updated_by                  TEXT    NOT NULL,
    PRIMARY KEY (org, environment, project),
    FOREIGN KEY (org, environment) REFERENCES org_environments (org, name) ON DELETE RESTRICT
);

CREATE TABLE environment_project_privilege_grants (
    org                TEXT    NOT NULL,
    environment        TEXT    NOT NULL,
    project            TEXT    NOT NULL CHECK (project <> ''),
    -- The ADR-0039 whitelist, `[ImageGrant]` exactly as the legacy rules held it.
    privileged_images  JSONB   NOT NULL DEFAULT '[]',
    permit_k8s_overlay BOOLEAN NOT NULL DEFAULT false,
    updated_at         BIGINT  NOT NULL,
    updated_by         TEXT    NOT NULL,
    PRIMARY KEY (org, environment, project),
    FOREIGN KEY (org, environment) REFERENCES org_environments (org, name) ON DELETE RESTRICT
);

-- ADR-0071 §10 step 8: names an admin should look at — merged across Projects,
-- or migrated with eligibility "all projects". Read and acknowledged by the
-- c3 API; no foreign key, so the record outlives the Environment it describes.
CREATE TABLE environment_migration_report (
    org                  TEXT    NOT NULL,
    environment          TEXT    NOT NULL,
    -- The Projects whose per-Project environments of this name were merged.
    merged_projects      TEXT[]  NOT NULL,
    -- No Project defined the name; it was only referenced (or only existed as
    -- a c-min org Environment), so it got eligibility "all projects".
    referenced_only      BOOLEAN NOT NULL,
    -- Projects whose Runs named it without defining it.
    referencing_projects TEXT[]  NOT NULL,
    created_at           BIGINT  NOT NULL,
    acknowledged_at      BIGINT  NULL,
    acknowledged_by      TEXT    NULL,
    PRIMARY KEY (org, environment)
);

-- The ADR-0071 §10 backfill. Every write is INSERT … ON CONFLICT DO NOTHING
-- or an UPDATE guarded to rows it has not touched yet, so a second call on
-- the state the first produced changes nothing (the golden test proves it).
-- It never removes or loosens anything; it is meant to be called by this
-- migration and the golden test only.
CREATE FUNCTION adr0071_backfill() RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    now_ms BIGINT := (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT;
BEGIN
    -- Step 1: one org Environment per distinct (org, name) a Project defined
    -- or a Run named, with the permissive baseline the column defaults give
    -- and `scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS` (one hour) — the
    -- deadline such a name already folded by. An existing c-min Environment
    -- keeps its description, deadline and sources.
    INSERT INTO org_environments
        (org, name, description, deployment_deadline_ms, created_at, updated_at, updated_by)
    SELECT n.org, n.name, '', 3600000, now_ms, now_ms, 'migration:adr-0071'
    FROM (
        SELECT org, name FROM legacy_project_environments
        UNION
        SELECT deploy_org, deploy_environment FROM runs
        WHERE deploy_org IS NOT NULL AND deploy_org <> ''
          AND deploy_environment IS NOT NULL AND deploy_environment <> ''
    ) n
    ON CONFLICT (org, name) DO NOTHING;

    -- Step 2: each Project's old rules become its overlay, verbatim —
    -- approvers (in order, duplicates and all), allowed refs (a bare `main`
    -- stays a bare `main`: it never matched a symbolic ref and must not start
    -- to), wait timer and require_reason (absent on pre-ADR-0057 rows = off).
    -- Step 3: concurrency is never carried (NULL = no limit); the UI stored 1
    -- for every environment and nothing ever read it.
    -- The wait timer is clamped into INT: it was a u32 nobody enforced
    -- (git-bug 2b94989), and an out-of-range one must not abort the migration.
    INSERT INTO environment_project_overlays
        (org, environment, project, approvers, allowed_refs, wait_timer_s, require_reason,
         deployment_concurrency, deployment_gate_deadline_ms, updated_at, updated_by)
    SELECT l.org, l.name, l.project,
           ARRAY(SELECT a FROM jsonb_array_elements_text(
                     COALESCE(l.protection->'approvers', '[]'::jsonb)) WITH ORDINALITY t(a, i)
                 ORDER BY i),
           ARRAY(SELECT r FROM jsonb_array_elements_text(
                     COALESCE(l.protection->'allowed_refs', '[]'::jsonb)) WITH ORDINALITY t(r, i)
                 ORDER BY i),
           LEAST(COALESCE((l.protection->>'wait_timer')::BIGINT, 0), 2147483647)::INT,
           COALESCE((l.protection->>'require_reason')::BOOLEAN, false),
           NULL, NULL, now_ms, 'migration:adr-0071'
    FROM legacy_project_environments l
    WHERE l.project <> ''
    ON CONFLICT (org, environment, project) DO NOTHING;

    -- Step 4 (secrets) moves no data: the EnvironmentProjectSecretScope keeps
    -- the old scope's stored key byte for byte (scarab-secrets-postgres).

    -- Step 5: privileges become that Project's grant — only where there is a
    -- privilege to carry; no grant decides exactly as an empty whitelist did.
    INSERT INTO environment_project_privilege_grants
        (org, environment, project, privileged_images, permit_k8s_overlay, updated_at, updated_by)
    SELECT l.org, l.name, l.project,
           COALESCE(l.protection->'privileged_images', '[]'::jsonb),
           COALESCE((l.protection->>'permit_k8s_overlay')::BOOLEAN, false),
           now_ms, 'migration:adr-0071'
    FROM legacy_project_environments l
    WHERE l.project <> ''
      AND (jsonb_array_length(COALESCE(l.protection->'privileged_images', '[]'::jsonb)) > 0
           OR COALESCE((l.protection->>'permit_k8s_overlay')::BOOLEAN, false))
    ON CONFLICT (org, environment, project) DO NOTHING;

    -- Step 6a: a name no Project in the Org defined — only referenced by Runs,
    -- or only ever a c-min org Environment — gets eligibility "all projects":
    -- before this migration admission treated it as a deploy with no rules,
    -- from any Project. Guarded to rows with no list and the flag still off.
    UPDATE org_environments o
    SET eligible_all_projects = true
    WHERE NOT o.eligible_all_projects
      AND NOT EXISTS (SELECT 1 FROM legacy_project_environments l
                      WHERE l.org = o.org AND l.name = o.name)
      AND NOT EXISTS (SELECT 1 FROM environment_project_eligibility e
                      WHERE e.org = o.org AND e.environment = o.name);

    -- Step 6b: a defined name admits exactly the Projects that defined it plus
    -- the Projects whose Runs named it without a definition. Project names are
    -- forge repo names, which cannot hold `*`, so each is an exact pattern.
    INSERT INTO environment_project_eligibility (org, environment, project_pattern)
    SELECT l.org, l.name, l.project
    FROM legacy_project_environments l
    WHERE l.project <> ''
    UNION
    SELECT r.deploy_org, r.deploy_environment, r.deploy_project
    FROM runs r
    WHERE r.deploy_project IS NOT NULL AND r.deploy_project <> ''
      AND EXISTS (SELECT 1 FROM legacy_project_environments l
                  WHERE l.org = r.deploy_org AND l.name = r.deploy_environment)
    ON CONFLICT (org, environment, project_pattern) DO NOTHING;

    -- Step 7: the legacy `deployments` table stays as legacy approval
    -- history, untouched (ADR-0070 §1).

    -- Step 8: the split report — every name merged from more than one
    -- Project, and every name migrated with eligibility "all projects".
    INSERT INTO environment_migration_report
        (org, environment, merged_projects, referenced_only, referencing_projects, created_at)
    SELECT o.org, o.name,
           ARRAY(SELECT DISTINCT l.project FROM legacy_project_environments l
                 WHERE l.org = o.org AND l.name = o.name ORDER BY 1),
           NOT EXISTS (SELECT 1 FROM legacy_project_environments l
                       WHERE l.org = o.org AND l.name = o.name),
           ARRAY(SELECT DISTINCT r.deploy_project FROM runs r
                 WHERE r.deploy_org = o.org AND r.deploy_environment = o.name
                   AND r.deploy_project IS NOT NULL AND r.deploy_project <> ''
                   AND NOT EXISTS (SELECT 1 FROM legacy_project_environments l
                                   WHERE l.org = o.org AND l.name = o.name
                                     AND l.project = r.deploy_project)
                 ORDER BY 1),
           now_ms
    FROM org_environments o
    WHERE (SELECT count(*) FROM legacy_project_environments l
           WHERE l.org = o.org AND l.name = o.name) <> 1
    ON CONFLICT (org, environment) DO NOTHING;
END;
$$;

SELECT adr0071_backfill();
