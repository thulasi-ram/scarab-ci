-- `org_fork_run_approval` holds each org's Fork Run approval setting
-- (ADR-0086 §9): whether a Run of fork code waits, before any of its Steps
-- starts, at a `manual` gate someone with write on the repository releases.
-- An org admin changes it in Org settings, Security. An org with no row has
-- the setting off.
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
    enabled    BOOLEAN NOT NULL,
    updated_by TEXT    NOT NULL,
    updated_at BIGINT  NOT NULL
);
