-- ADR-0076 §4-§6: org-installed libraries, their versions, yanks, and where
-- each org's first-party defaults stand. Numbered 0071; 0070 is held for a
-- pending renumber.
--
-- Expand step (ADR-0003): new tables only, so a binary from before this
-- migration keeps working against it. Everything is per org; an org is the
-- `org` of a repo binding (`forge_repos`).

-- One library file as fetched into an org, keyed by the SHA-256 of its
-- content: the hash the compiler records in a Step's `origin`.
CREATE TABLE library_versions (
    org          TEXT    NOT NULL,
    content_hash TEXT    NOT NULL CHECK (content_hash ~ '^[0-9a-f]{64}$'),
    content      TEXT    NOT NULL,
    -- The git source, pinned to a commit.
    source       TEXT    NOT NULL,
    publisher    TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    -- The hash is in a Scarab release manifest the server trusts.
    first_party  BOOLEAN NOT NULL DEFAULT false,
    fetched_by   TEXT    NOT NULL,
    fetched_at   BIGINT  NOT NULL,
    -- Whether the version was ever installed in its org (ADR-0076 §4). A
    -- fetch stores a version for review; one never installed was never
    -- compiled into a Run, so the oldest of those can be deleted to keep an
    -- org's fetches bounded. An install sets it in the same transaction.
    installed_once BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (org, content_hash)
);

-- The version an org uses for one `<publisher>/<name>`.
CREATE TABLE library_installations (
    org          TEXT   NOT NULL,
    publisher    TEXT   NOT NULL,
    name         TEXT   NOT NULL,
    content_hash TEXT   NOT NULL,
    installed_by TEXT   NOT NULL,
    installed_at BIGINT NOT NULL,
    step_up_ref  TEXT,
    PRIMARY KEY (org, publisher, name),
    FOREIGN KEY (org, content_hash) REFERENCES library_versions (org, content_hash)
);

-- A version refused in one org (ADR-0076 §6), with the admin's reason.
CREATE TABLE library_yanks (
    org          TEXT   NOT NULL,
    content_hash TEXT   NOT NULL,
    reason       TEXT   NOT NULL,
    yanked_by    TEXT   NOT NULL,
    yanked_at    BIGINT NOT NULL,
    PRIMARY KEY (org, content_hash),
    FOREIGN KEY (org, content_hash) REFERENCES library_versions (org, content_hash)
);

-- Where an org's first-party defaults stand (ADR-0076 §5). A row is made
-- once per org: by the first bind of a repo into it ('pending', then
-- 'installed' or 'missing'), or below.
CREATE TABLE library_org_defaults (
    org        TEXT   PRIMARY KEY,
    state      TEXT   NOT NULL CHECK (state IN ('offered', 'pending', 'installed', 'missing')),
    -- [{publisher, name, error}] for 'missing'.
    missing    JSONB  NOT NULL DEFAULT '[]',
    mirror     TEXT,
    updated_at BIGINT NOT NULL
);

-- Orgs that exist already are offered the first-party libraries; nothing is
-- installed into them without an admin (ADR-0076 §5).
INSERT INTO library_org_defaults (org, state, updated_at)
SELECT DISTINCT org, 'offered', 0 FROM forge_repos
ON CONFLICT (org) DO NOTHING;
