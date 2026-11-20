-- A library fetch keeps the branch, tag or commit the admin asked for and
-- the commit it resolved to, and an installation keeps the ones of the fetch
-- it installed.
--
--   * library_versions.requested_ref, .resolved_commit: of the most recent
--     fetch of the version's content from the version's file. A fetch reads
--     a branch or tag at the commit it points to then; `source` keeps the
--     first fetch's commit. The install request names only a content hash,
--     so the version row is where the install reads what was reviewed.
--   * library_installations.requested_ref, .resolved_commit: copied from the
--     version at install. A later fetch of the same content under another
--     ref changes the version row, never an installation.
--   * library_installations.ref_for_hash: the content hash the two belong
--     to. The store reads them only while it equals content_hash: an
--     upgrade by a binary from before this migration changes content_hash
--     and leaves the previous version's ref and commit, which then read as
--     no ref and the commit in the version's source.
--
-- requested_ref is NULL when the fetch named the commit in the source
-- (`…@<commit>`). Every row from before this migration was fetched with no
-- ref kept: requested_ref stays NULL, and resolved_commit is the commit in
-- the version's source.
--
-- Expand step (ADR-0003): nullable columns only. A binary from before this
-- migration writes none of them: its new rows hold NULL, which reads as no
-- ref and the commit in the version's source, and its upgrades leave
-- ref_for_hash naming the hash they replaced.

ALTER TABLE library_versions
    ADD COLUMN requested_ref TEXT,
    ADD COLUMN resolved_commit TEXT;

ALTER TABLE library_installations
    ADD COLUMN requested_ref TEXT,
    ADD COLUMN resolved_commit TEXT,
    ADD COLUMN ref_for_hash TEXT;

UPDATE library_versions
   SET resolved_commit = substring(source FROM '@([^@/]+)$')
 WHERE resolved_commit IS NULL;

UPDATE library_installations i
   SET resolved_commit = v.resolved_commit,
       ref_for_hash = i.content_hash
  FROM library_versions v
 WHERE v.org = i.org
   AND v.content_hash = i.content_hash
   AND i.resolved_commit IS NULL;
