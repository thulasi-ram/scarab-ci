-- ADR-0079 §2: an identity link that would fold in a Principal holding
-- passkeys needs an assertion from one of that Principal's passkeys. The
-- challenge for it has its own purpose, `merge`.
--
-- Expand step (ADR-0003): the CHECK only widens. A server built before this
-- migration refuses to boot against a database that has it; to roll back,
-- delete row 84 from `_sqlx_migrations` (and any `merge` challenges, which
-- expire within minutes anyway).
ALTER TABLE webauthn_challenges DROP CONSTRAINT webauthn_challenges_purpose_check;
ALTER TABLE webauthn_challenges ADD CONSTRAINT webauthn_challenges_purpose_check
    CHECK (purpose IN ('register', 'window', 'action', 'request', 'merge'));
