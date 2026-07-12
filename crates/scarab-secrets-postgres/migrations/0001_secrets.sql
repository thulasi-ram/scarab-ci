-- Envelope-encrypted secrets at rest (ADR-0014, 0032).
--
-- Each secret is stored as: value ciphertext (AES-256-GCM under a per-secret
-- random data key) + the data key wrapped (AES-256-GCM under the master key) +
-- both nonces. Plaintext never touches disk. Scoped by an encoded org/repo/env
-- string; the (scope, key) pair is unique.
CREATE TABLE secrets (
    scope       TEXT  NOT NULL,
    key         TEXT  NOT NULL,
    ciphertext  BYTEA NOT NULL,
    value_nonce BYTEA NOT NULL,
    wrapped_key BYTEA NOT NULL,
    key_nonce   BYTEA NOT NULL,
    PRIMARY KEY (scope, key)
);
