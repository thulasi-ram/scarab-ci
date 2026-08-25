-- ADR-0067 parts 4, 8 and 9 (slice 3): the pack index.
--
-- A drain's DURABLE bytes now stream Depot -> object store as size-capped
-- packs (`packs/<fence_key>/<session>-<seq:06>.pack` — the session component
-- is unique per in-memory pack session, so two sessions of one fence never
-- complete a multipart upload at the same key), closed by a commit pack
-- written last (`packs/<fence_key>/commit.pack`). These two tables are the fast query
-- surface over what the bucket already says about itself: every pack carries
-- its own footer index, so the bucket ALONE can rebuild both tables
-- (ADR-0067 part 11 — on any disagreement the bucket wins; losing these rows
-- is a rebuild job, never data loss). Only the control plane runs this file;
-- the Depot connects and never migrates (ADR-0067 part 2).
--
-- Write ordering is the whole safety story (ADR-0067 part 10): bytes before
-- pointers. A pack's multipart upload completes (atomic), the commit pack
-- lands, and only THEN does one transaction insert these rows beside the
-- drain record. A crash in between leaves unreachable pack bytes — safe,
-- reclaimable — never an index row naming an object that is not there.
--
-- `depot_pack_members` is the presence index, the size index and the read
-- index in one (ADR-0067 part 9): `byte_len` answers what used to cost a full
-- blob download, and (`pack_key`, `byte_offset`, `byte_len`) is a ranged read
-- into the pack. `address` is TAGGED (`sha256:<hex>`, ADR-0067 part 12) —
-- index rows and pack footers are born tagged; bare hex stays confined to
-- storage keys and the fence ledger (see 0043). One address may appear in
-- several packs (two drains can both publish a shared blob); readers take any
-- row.

CREATE TABLE depot_packs (
    pack_key   TEXT PRIMARY KEY,
    fence_key  TEXT NOT NULL,
    -- 'body' (members below) | 'commit' (the receipt written last, no members)
    kind       TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    bytes      BIGINT NOT NULL
);

-- Retention expires Runs, and a pack never outlives its drain's fence
-- (ADR-0067 part 7): pack-grain expiry looks packs up by fence.
--
-- WARNING to whoever writes committed-pack expiry (git-bug ec294b7): a
-- fence's committed pack may be the ONLY durable copy of members a LATER
-- fence's success record depends on — `/have` and the drain gate accept any
-- committed pack's row as durable, whoever owns it. The dependency IS
-- recorded (0048 `depot_fence_borrows`, written in the borrower's record
-- transaction), so expiry of fence F MUST, inside one transaction per victim
-- fence: FOR UPDATE F's `depot_packs` rows FIRST (the drain re-check takes
-- FOR SHARE on them), re-check that no borrow edge on F has a borrower whose
-- drain record still lives, honour the `depot_borrow_tracking_epoch` floor
-- (no committed expiry at all while any live success record predates the
-- epoch), and only then delete F's POINTERS — the bytes go rowless and the
-- orphan reclaimer collects them a cadence later. Deleting a committed pack
-- without that check silently unbacks committed evidence.
CREATE INDEX depot_packs_fence_key ON depot_packs (fence_key);

CREATE TABLE depot_pack_members (
    address     TEXT NOT NULL,
    -- 'blob' | 'tree'
    kind        TEXT NOT NULL,
    pack_key    TEXT NOT NULL REFERENCES depot_packs (pack_key),
    byte_offset BIGINT NOT NULL,
    byte_len    BIGINT NOT NULL,
    PRIMARY KEY (address, pack_key)
);

-- Deleting a pack's rows (pointers before bytes, part 10) and rebuilding a
-- pack's index both address members by pack.
CREATE INDEX depot_pack_members_pack_key ON depot_pack_members (pack_key);
