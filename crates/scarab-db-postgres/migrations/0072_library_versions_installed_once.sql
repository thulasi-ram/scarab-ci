-- Whether a version was ever installed in its org (ADR-0076 §4). A fetch
-- stores a version for review; one never installed was never compiled into
-- a Run, so the oldest of those can be deleted to keep an org's fetches
-- bounded. Versions stored before this column existed count as installed,
-- so none of them is deleted.
ALTER TABLE library_versions ADD COLUMN installed_once BOOLEAN NOT NULL DEFAULT true;
ALTER TABLE library_versions ALTER COLUMN installed_once SET DEFAULT false;
--
-- Expand step (ADR-0003): an added column with a default, so an older
-- binary's inserts still work (they get `false`); the prune also skips any
-- version an installation points at, which covers an older binary's install.
