-- Run origin: the trigger facts a Run is born from, stamped at creation from
-- the normalized Event (beside tenancy, ADR-0049, and deploy context, ADR-0037).
--
-- Discrete, independently-nullable columns — NOT a bundle blob — because the
-- facts are naturally sparse: different trigger kinds populate different
-- subsets (a cron run has no actor/ref/sha; a PR has a number, a push does
-- not). This mirrors how tenancy is stamped as two plain columns rather than a
-- struct. The raw Event is deliberately not hoarded; we extract exactly the
-- facts the runs list shows.
--
--   origin_trigger_kind — the TriggerKind token (push/pull_request/tag/…).
--   origin_actor         — the Actor login (CONTEXT §4.5): who caused the event.
--   origin_ref           — the symbolic branch/tag ref (refs/heads/main, a tag).
--   origin_sha           — the resolved commit the run pinned to.
--   origin_pr_number     — the pull-request number, for pull_request events.
--
-- All NULL on runs created before this migration (no backfill), exactly as
-- tenant_org was when it landed.
ALTER TABLE runs ADD COLUMN origin_trigger_kind TEXT;
ALTER TABLE runs ADD COLUMN origin_actor        TEXT;
ALTER TABLE runs ADD COLUMN origin_ref          TEXT;
ALTER TABLE runs ADD COLUMN origin_sha          TEXT;
ALTER TABLE runs ADD COLUMN origin_pr_number    BIGINT;
