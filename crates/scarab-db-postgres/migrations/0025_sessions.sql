-- ADR-0049 C1: server-side login sessions. The id is the opaque bearer
-- credential; the principal snapshot rides as JSONB (flat roles until the
-- scoped RBAC of C2); csrf is the session's double-submit token for browser
-- mutations. Forward-only (expand-contract, ADR-0003).
CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,
    principal  JSONB  NOT NULL,
    csrf       TEXT   NOT NULL DEFAULT '',
    expires_at BIGINT NOT NULL
);

-- The retention sweeper (ADR-0050) reaps by expiry.
CREATE INDEX sessions_expires_at_idx ON sessions (expires_at);
