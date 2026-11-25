-- `org_leak_check_limits` holds each org's limits on leak checks
-- (`POST /v1/orgs/{org}/secrets/match`): how many checks one person may run
-- in the org per rolling hour and per rolling day. An org Owner sets them in
-- Org settings, Security. An org with no row has the defaults (20 an hour,
-- 40 a day); 0 in either column turns the check off for the org.
--
-- The checks themselves are counted from the `secret.match_checked` rows of
-- `org_security_events`; this table holds only the limits.
--
-- Locks: a new table takes no lock on another.

CREATE TABLE org_leak_check_limits (
    org        TEXT    PRIMARY KEY,
    per_hour   INTEGER NOT NULL CHECK (per_hour BETWEEN 0 AND 100),
    per_day    INTEGER NOT NULL CHECK (per_day BETWEEN 0 AND 1000),
    updated_by TEXT    NOT NULL,
    updated_at BIGINT  NOT NULL
);
