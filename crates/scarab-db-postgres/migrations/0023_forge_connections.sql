-- ADR-0046: the ForgeConnection registry — which forge, which base URL, which
-- credential handle serves a repo, and which governed Project owns it.
-- credential_ref is a SecretProvider handle; secret bytes NEVER land here.
CREATE TABLE forge_connections (
    id             TEXT PRIMARY KEY,
    kind           TEXT NOT NULL,
    base_url       TEXT NOT NULL,
    credential_ref TEXT NOT NULL
);

-- The RepoRefs a connection owns, each bound to its governed Project
-- (org, project). A RepoRef (owner, name) is globally unique in v1 so
-- resolution is deterministic; multi-host collisions are deferred.
CREATE TABLE forge_repos (
    connection_id TEXT NOT NULL REFERENCES forge_connections(id) ON DELETE CASCADE,
    owner         TEXT NOT NULL,
    name          TEXT NOT NULL,
    org           TEXT NOT NULL,
    project       TEXT NOT NULL,
    PRIMARY KEY (owner, name)
);

CREATE INDEX forge_repos_connection_idx ON forge_repos (connection_id);
