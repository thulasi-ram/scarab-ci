-- ADR-0046: a Project IS the governed repo — there is no separate governed
-- "Repo" entity. Re-key the governance tables by (org, project), where the
-- project name is its repo's forge name (1:1 in v1). Pure renames; the values
-- are unchanged.
ALTER TABLE environments RENAME COLUMN repo TO project;
ALTER TABLE deployments RENAME COLUMN repo TO project;
ALTER TABLE runs RENAME COLUMN deploy_repo TO deploy_project;
