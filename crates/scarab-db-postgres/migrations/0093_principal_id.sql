-- ADR-0080: a Principal has a random, immutable id; the readable id ADR-0069
-- §2 minted (`github:alice`, `okta:s.patel@acme.example`) becomes its handle.
--
-- Before this migration the readable string was the key every binding, token
-- owner, approver list and passkey stored. It is built from what a provider
-- asserts (a login, a verified email), so personal data sat in keys, in URLs
-- and in append-only history. From here on the key is `p_` + 32 hex
-- characters (a UUID v4 without its dashes), and the handle is only what
-- people type and read.
--
-- This is a CONTRACT step (ADR-0003 asks for expand-only; ADR-0080 §6 says
-- why this one is not): current-state columns are rewritten from handle to
-- id. A server from before 0093 cannot run against this database. Upgrade
-- with the Recreate strategy after a backup; restoring the backup is the
-- rollback. `principals` itself is the mapping back: every row keeps its
-- handle beside its id, so any rewritten value maps back with one join.
--
-- History is NOT rewritten: events.payload, org_security_events (a trigger
-- refuses it), policy_decisions, policy_versions and every attribution column
-- (`updated_by`, `created_by`, …) keep the handle they were written with.
-- Reads accept either form.

-- 1. The readable id becomes `handle`; the foreign keys on it go first.
ALTER TABLE principal_identities DROP CONSTRAINT principal_identities_principal_id_fkey;
ALTER TABLE principals DROP CONSTRAINT principals_merged_into_fkey;
ALTER TABLE principals DROP CONSTRAINT principals_pkey;
ALTER TABLE principals RENAME COLUMN id TO handle;
ALTER TABLE principals ADD CONSTRAINT principals_handle_key UNIQUE (handle);

-- `legacy_placeholder`: created here for a stored reference that named no
-- Principal (a binding from before 0052). Only an `unqualified_ids` sign-in
-- or import whose handle is exactly this one adopts it (ADR-0080 §6).
-- `handle_case_duplicate`: this handle equals an older one ignoring case. Both
-- are kept and reported at boot; neither is merged automatically.
ALTER TABLE principals
    ADD COLUMN legacy_placeholder    BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN handle_case_duplicate BOOLEAN NOT NULL DEFAULT FALSE;

-- 2. A placeholder for every current-state reference that names nobody, so
-- every rewritten value below has an id to become.
INSERT INTO principals (handle, created_at, legacy_placeholder)
SELECT DISTINCT refs.s, (extract(epoch FROM now()) * 1000)::BIGINT, TRUE
FROM (
    SELECT subject AS s FROM rbac_bindings
    UNION SELECT owner_subject FROM api_tokens
    UNION SELECT unnest(approvers) FROM org_environments
    UNION SELECT unnest(approvers) FROM environment_project_overlays
    UNION SELECT principal_id FROM passkeys
    UNION SELECT principal_id FROM passkey_users
    UNION SELECT for_human FROM step_up_requests
) refs
WHERE refs.s <> ''
  AND NOT EXISTS (SELECT 1 FROM principals p WHERE p.handle = refs.s);

-- 3. The id. Postgres's gen_random_uuid() draws from a CSPRNG.
ALTER TABLE principals
    ADD COLUMN id TEXT NOT NULL DEFAULT ('p_' || replace(gen_random_uuid()::TEXT, '-', ''));
ALTER TABLE principals ADD PRIMARY KEY (id);

UPDATE principals p SET merged_into = t.id
FROM principals t
WHERE p.merged_into = t.handle;
ALTER TABLE principals
    ADD CONSTRAINT principals_merged_into_fkey FOREIGN KEY (merged_into) REFERENCES principals (id);

UPDATE principal_identities i SET principal_id = p.id
FROM principals p
WHERE i.principal_id = p.handle;
ALTER TABLE principal_identities
    ADD CONSTRAINT principal_identities_principal_id_fkey
    FOREIGN KEY (principal_id) REFERENCES principals (id);

-- 4. A new handle may not equal an existing one ignoring case. Existing
-- case-only duplicates are kept: every one but the oldest of its group is
-- marked and left out of the index, so the group's handle still blocks new
-- look-alikes.
UPDATE principals p SET handle_case_duplicate = TRUE
WHERE EXISTS (
    SELECT 1 FROM principals o
    WHERE lower(o.handle) = lower(p.handle)
      AND (o.created_at, o.handle) < (p.created_at, p.handle)
);
CREATE UNIQUE INDEX principals_handle_ci_idx ON principals (lower(handle))
    WHERE NOT handle_case_duplicate;

-- 5. Current state: handle → id. Each mapping is one-to-one, so no primary
-- key below can collide.
UPDATE rbac_bindings b SET subject = p.id
FROM principals p
WHERE b.subject = p.handle;

UPDATE api_tokens t SET owner_subject = p.id
FROM principals p
WHERE t.owner_subject = p.handle;

-- Approver lists keep their order (and any duplicate an admin typed).
UPDATE org_environments e SET approvers = ARRAY(
    SELECT COALESCE(p.id, u.a)
    FROM unnest(e.approvers) WITH ORDINALITY u(a, i)
    LEFT JOIN principals p ON p.handle = u.a
    ORDER BY u.i)
WHERE cardinality(e.approvers) > 0;

UPDATE environment_project_overlays o SET approvers = ARRAY(
    SELECT COALESCE(p.id, u.a)
    FROM unnest(o.approvers) WITH ORDINALITY u(a, i)
    LEFT JOIN principals p ON p.handle = u.a
    ORDER BY u.i)
WHERE cardinality(o.approvers) > 0;

UPDATE passkeys k SET principal_id = p.id
FROM principals p
WHERE k.principal_id = p.handle;

UPDATE passkey_users u SET principal_id = p.id
FROM principals p
WHERE u.principal_id = p.handle;

UPDATE step_up_requests r SET for_human = p.id
FROM principals p
WHERE r.for_human = p.handle;

-- 6. Short-lived rows that hold a Principal snapshot or are bound to a
-- session: gone. Everyone signs in again.
DELETE FROM step_up_confirmations;
DELETE FROM webauthn_challenges;
DELETE FROM sessions;
