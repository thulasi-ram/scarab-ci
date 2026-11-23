-- The Pipeline file a Run was created from, kept so the Run page's Manifest
-- tab shows the YAML the Run compiled, not the file as it reads today.
--
-- `pipeline_path` is the repo-relative path (`.scarab/ci.yaml`); `pipeline`
-- keeps the display name. `pipeline_source` is the file as the forge
-- returned it. The compiler refuses a file over 1 MiB, so no stored source
-- is larger. `pipeline_source_sha` is the commit the file was read at; NULL
-- when it was read at a symbolic ref no commit was resolved for (a tag or
-- release delivery whose tag could not be resolved).
--
-- Written at creation for webhook, dispatch, cron and follow-up Runs. An
-- inline Run (`POST /v1/runs`) has no file and leaves all three NULL, as do
-- Runs created before this migration (no backfill). The libraries a Run
-- invoked are not stored here: the stored IR records each inlined Step's
-- origin.
--
-- Expand step (ADR-0003): three nullable columns without a default, which
-- Postgres 11 and later add in the catalog without rewriting a row. The
-- ALTER takes an ACCESS EXCLUSIVE lock on `runs` for that moment and waits
-- at most 5 s for it; on a busy database the start then fails with
-- `canceling statement due to lock timeout` and the Pod's restart runs it
-- again.

SET LOCAL lock_timeout = '5s';

ALTER TABLE runs
    ADD COLUMN pipeline_path       TEXT,
    ADD COLUMN pipeline_source     TEXT,
    ADD COLUMN pipeline_source_sha TEXT;
