-- `runs.origin_pr_head` is the branch a pull request comes from
-- (`pull_request.head.ref`), so the runs list can show
-- `PR #213 feat/org-page → main` beside `origin_pr_base` (0033). A PR Run's
-- `origin_ref` is `refs/pull/<N>/head`, which does not name the branch.
--
-- Stamped at creation beside the other origin_* columns, from the normalized
-- Event's PullRequest.head_branch, as the forge sends it: for a fork PR it is
-- the fork's branch name. Display only, like origin_pr_base: never read for
-- scheduling, and not in Event::context() (a fork's branch name is chosen by
-- whoever opened the PR).
--
-- NULL on non-PR runs and on runs created before this migration (no
-- backfill); such runs show no head branch.
--
-- Expand step (ADR-0003): one nullable column without a default, added in
-- the catalog without rewriting a row. The ALTER takes an ACCESS EXCLUSIVE
-- lock on `runs` for that moment and waits at most 5 s for it; on a busy
-- database the start then fails with `canceling statement due to lock
-- timeout` and the Pod's restart runs it again.

SET LOCAL lock_timeout = '5s';

ALTER TABLE runs ADD COLUMN origin_pr_head TEXT;
