-- Drops the tables that only the environment migration report and the legacy
-- approval history used. The code that read and wrote them is gone.
--
--   * environment_migration_report (0057): the names migration 0057 asked an
--     org admin to review, and who acknowledged each. Its API and the web UI
--     banner are removed.
--   * legacy_project_environments (0012 `environments`, renamed by 0057): the
--     per-Project environments 0057 migrated to org Environments. Nothing
--     admits by them since 0057; the report read them, and an identity link
--     rewrote approver ids in them for the report. The overlays, privilege
--     grants and eligibility 0057 made from them stay.
--   * deployments (0012): one row per Environment approval that released a
--     gate. No API returned them. Each Run keeps its approvals in its own
--     event history (EnvironmentApprovalMet).
--
-- Contract step (ADR-0003): a binary from before this migration writes
-- `deployments` on every gate release and reads the report tables, so it
-- cannot run against a database migrated past here.

DROP TABLE IF EXISTS environment_migration_report;
DROP TABLE IF EXISTS legacy_project_environments;
DROP TABLE IF EXISTS deployments;
