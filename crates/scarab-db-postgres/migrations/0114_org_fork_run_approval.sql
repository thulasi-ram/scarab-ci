-- `org_fork_run_approval` holds each org's Fork Run approval setting
-- (ADR-0086 §9): which Runs of fork code wait, before any of their Steps
-- starts, at a `manual` gate someone with write on the repository releases.
-- `off` (none), `agent_started` (those started through an `oauth` token) or
-- `all`. An org admin changes it in Org settings, Security. An org with no
-- row has `agent_started`. `org` is the org name in ASCII lower case
-- (`fork_run_approval_org_key`): the API and Run creation spell an org as
-- its URL and its forge do, and org names are case-insensitive there.
--
-- Numbered 0114, not 0112: 0112 and 0113 are taken on other branches, and
-- this file keeps clear of both. The migrator applies by version, so the gap
-- is harmless.
--
-- Locks: a new table takes no lock on another table. The timeout bounds the
-- wait anyway, as for every migration since 0110: on a busy database the
-- start fails with `canceling statement due to lock timeout` and the Pod's
-- restart runs it again.

SET LOCAL lock_timeout = '5s';

CREATE TABLE org_fork_run_approval (
    org        TEXT    PRIMARY KEY,
    setting    TEXT    NOT NULL CHECK (setting IN ('off', 'agent_started', 'all')),
    updated_by TEXT    NOT NULL,
    updated_at BIGINT  NOT NULL
);
