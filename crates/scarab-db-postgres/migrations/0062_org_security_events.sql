-- ADR-0073 §7: the OrgSecurityEventLog — every confirmation, refusal,
-- crown-jewel operation, request, and passkey change. APPEND-ONLY: the
-- trigger below refuses every UPDATE and DELETE, so not even a bug in this
-- binary can rewrite what happened. (TRUNCATE is a DBA act, outside it.)
--
-- org NULL = not bound to one Org (passkeys, confirmations, install-wide
-- ForgeConnection operations); an Org's admins still see such an event about
-- any of their members, through the visibility query in the store.
CREATE TABLE org_security_events (
    seq             BIGSERIAL PRIMARY KEY,
    id              TEXT   NOT NULL UNIQUE,
    org             TEXT,
    at              BIGINT NOT NULL,
    kind            TEXT   NOT NULL,
    -- The human/principal the event is about.
    subject         TEXT   NOT NULL,
    -- 'session:cookie' | 'session:bearer' | 'api_token:<id>' | 'operator:break-glass'
    caller          TEXT   NOT NULL,
    op              TEXT,
    target          TEXT,
    confirmation_id TEXT,
    request_id      TEXT,
    passkey_id      TEXT,
    session_hash    TEXT,
    detail          JSONB  NOT NULL DEFAULT '{}'
);
CREATE INDEX org_security_events_org_idx ON org_security_events (org, seq DESC);
CREATE INDEX org_security_events_subject_idx ON org_security_events (subject, seq DESC);

CREATE FUNCTION org_security_events_append_only() RETURNS trigger LANGUAGE plpgsql AS
    $$ BEGIN RAISE EXCEPTION 'org_security_events is append-only'; END $$;
CREATE TRIGGER org_security_events_no_update BEFORE UPDATE OR DELETE ON org_security_events
    FOR EACH ROW EXECUTE FUNCTION org_security_events_append_only();
