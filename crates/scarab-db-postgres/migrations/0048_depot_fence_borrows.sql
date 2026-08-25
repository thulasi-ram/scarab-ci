-- git-bug ec294b7: fence-grain borrow edges — who depends on whose packs.
--
-- `/have`'s durable answer and the drain gate's `durable_present_of` accept a
-- member as durable if ANY fence's COMMITTED pack holds it, so fence B's
-- success record can depend, for its only durable copy, on `packs/<A>/...` —
-- and until this table, nothing recorded that dependency. Latent while packs
-- are never deleted; fatal the day committed-pack expiry is written without
-- it (deleting A's packs would silently unback B's committed evidence).
--
-- `depot_fence_borrows`: one row per (borrower, owner) fence pair — written
-- inside the borrower's drain-record transaction (SUCCESS records only),
-- keyed on the record's FULL published closure, atomically with the record
-- itself. Fence grain, not pack grain, because expiry is fence-grain
-- (ADR-0067 part 7: a pack never outlives its drain's fence). `run` is
-- insurance: it keeps the borrower→run join alive even if the borrower's
-- drain-record row is gone. Rows are derived and rebuildable from the pack
-- index plus the drain records (ADR-0067 part 11 discipline) — losing them
-- costs a rebuild before any committed expiry may run, never data loss.
--
-- The deletion contract these edges gate (defined by ec294b7, built by its
-- successor ticket): fence F is deletable only when its run is terminal and
-- past its retention class's TTL AND no borrow edge on F has a borrower whose
-- drain record still lives — borrower-record lifetime IS borrower-fence
-- lifetime (see 0043's sweep exemption). Deletion removes POINTERS only;
-- the bytes become rowless and the shipped orphan reclaimer collects them.
--
-- `depot_borrow_tracking_epoch`: the backfill floor, stamped ONCE at the
-- moment this migration runs (Postgres `now()` — the same single clock
-- authority the reclaimer uses). Success records posted BEFORE this instant
-- may have silently borrowed from anything (their drains predate edge
-- recording), so committed-pack expiry must additionally refuse to run while
-- any live success record with `posted_at < epoch` exists. Time heals: those
-- records keep their TTL sweep, and once the last one is gone the floor
-- costs nothing. No closure re-walk migration.

CREATE TABLE depot_fence_borrows (
    borrower_fence TEXT NOT NULL,
    owner_fence    TEXT NOT NULL,
    -- The borrower's run id — the retention join survives the borrower's
    -- drain-record row (insurance; audit A2).
    run            TEXT NOT NULL,
    created_at     BIGINT NOT NULL,
    PRIMARY KEY (borrower_fence, owner_fence)
);

-- The expiry pass asks "who borrows from this owner?"; without this it is a
-- full-table scan per victim fence.
CREATE INDEX depot_fence_borrows_owner ON depot_fence_borrows (owner_fence);

CREATE TABLE depot_borrow_tracking_epoch (
    -- One row, mechanically: the PK admits only TRUE.
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    epoch     BIGINT NOT NULL
);

INSERT INTO depot_borrow_tracking_epoch (singleton, epoch)
VALUES (TRUE, EXTRACT(EPOCH FROM now())::bigint);
