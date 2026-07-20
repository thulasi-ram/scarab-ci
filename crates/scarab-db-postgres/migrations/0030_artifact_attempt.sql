-- ADR-0056 (amends 0052): artifacts become immutable per attempt. The old
-- PRIMARY KEY (run_id, name) with ON CONFLICT DO UPDATE silently destroyed a
-- failed attempt's version — precisely the evidence (a failure report) you
-- want when asking what a retry recovered from — contradicting 0027_artifacts'
-- own "immutable once written" comment. Now each (name, step, attempt) is its
-- own version; only a re-drive of the SAME attempt overwrites (same fence,
-- same bytes, deterministic).
--
-- `succeeded` records the publishing attempt's verdict: the name-addressed
-- "of record" download resolves to the latest SUCCESSFUL version, so a
-- consumer never silently receives a failed attempt's partial file.
--
-- Pre-ADR-0056 rows carry '' provenance (unknown step/attempt) and stay
-- of-record candidates.
ALTER TABLE artifacts ADD COLUMN step_id TEXT NOT NULL DEFAULT '';
ALTER TABLE artifacts ADD COLUMN attempt_id TEXT NOT NULL DEFAULT '';
ALTER TABLE artifacts ADD COLUMN succeeded BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE artifacts DROP CONSTRAINT artifacts_pkey;
ALTER TABLE artifacts ADD PRIMARY KEY (run_id, name, step_id, attempt_id);
