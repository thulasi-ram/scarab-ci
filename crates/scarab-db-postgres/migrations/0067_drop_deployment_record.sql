-- ADR-0074: deployments leave core. Drops what migrations 0053 to 0058 added
-- for the Deployment record; the code that read and wrote it is gone (kept at
-- git tag `archive/deployment-record`).
--
-- Kept on purpose:
--   * the plugin tables 0055 and 0056 also created — plugin_installation_names,
--     plugin_installations, plugin_connections, plugin_broker_journal — which
--     the plugin runtime, the broker read proxy and the kill switch still use;
--   * org_environments and the rest of ADR-0071's governance (0054, 0057):
--     only their deadline and per-target concurrency columns go;
--   * the older `deployments` table (0012), which records gate approvals and
--     predates ADR-0070.
--
-- Contract step (ADR-0003): a binary from before ADR-0074 names these tables
-- and columns, so it cannot run against a database migrated past here.

-- 0058: the Run's resolved DeploymentRequests.
DROP TABLE IF EXISTS run_deployment_requests;

-- 0053 (+ 0058's evidence column and target index): the Deployment record's
-- input log and its per-Org sequence counter.
DROP TABLE IF EXISTS deployment_inputs;
DROP TABLE IF EXISTS deployment_input_counters;

-- 0054: EnvironmentDeploymentSources.
DROP TABLE IF EXISTS environment_deployment_sources;

-- 0055: what the report ingest kept per installation.
DROP TABLE IF EXISTS deployment_source_targets;
DROP TABLE IF EXISTS deployment_source_unmapped;
DROP TABLE IF EXISTS plugin_report_receipts;

-- 0054, 0057: EnvironmentDeploymentDeadline (the baseline's and an overlay's
-- tighter gate deadline) and the per-DeploymentTarget concurrency limit.
ALTER TABLE org_environments
    DROP COLUMN IF EXISTS deployment_deadline_ms,
    DROP COLUMN IF EXISTS deployment_concurrency;
ALTER TABLE environment_project_overlays
    DROP COLUMN IF EXISTS deployment_concurrency,
    DROP COLUMN IF EXISTS deployment_gate_deadline_ms;
