-- Scarab's own libraries are installed like any other library: the server no
-- longer reads a release manifest, marks no version first-party, installs
-- nothing into a new org, and trusts no library with forge credentials
-- unless an org admin trusted its source.
--
-- Until now an org trusted the clone library (`thulasi-ram/clone`) with
-- long-lived forge credentials without a row in `library_trusted_sources`:
-- the trust was computed at Run creation and at each launch from
-- `library_versions.first_party`. That column goes, so the trust becomes a
-- row here first, for every org that has a first-party clone version stored
-- (installed now, or used by a Run that may still be rerun). The row is
-- keyed as the server keys trust: the version's source without its
-- `@<commit>` (`trust_key`), one row per distinct source, so a copy read
-- from a mirror is trusted at the mirror's source. An admin can remove it
-- like any other trusted source. Nothing else changes for those orgs: their
-- installed libraries stay installed, and so do their versions and yanks.
--
--   * library_versions.first_party (0071): dropped.
--   * library_org_defaults (0071): where each org's first-party defaults
--     stood. Dropped with the auto-install and the retry endpoint.
--
-- Contract step (ADR-0003): a binary from before this migration reads and
-- writes `first_party` and `library_org_defaults`, so it cannot run against
-- a database migrated past here.

INSERT INTO library_trusted_sources (org, source, trusted_by, trusted_at, step_up_ref)
SELECT DISTINCT
    org,
    regexp_replace(source, '@[^/@]*$', ''),
    'scarab:migration-0090',
    (extract(epoch FROM now()) * 1000)::BIGINT,
    NULL::TEXT
FROM library_versions
WHERE first_party
  AND publisher = 'thulasi-ram'
  AND name = 'clone'
ON CONFLICT (org, source) DO NOTHING;

ALTER TABLE library_versions DROP COLUMN IF EXISTS first_party;

DROP TABLE IF EXISTS library_org_defaults;
