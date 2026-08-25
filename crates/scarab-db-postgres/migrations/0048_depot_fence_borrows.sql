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
--
-- 0043 and 0044 are APPLIED migrations (sqlx checksums freeze their bytes),
-- so the two contract amendments that belong beside them live HERE instead:
--
-- 0043's residue contract, refined: `depot_fence_writes` rows and ERROR
-- drain records are fence residue, TTL-swept by the Depot — no workspace
-- token outlives the sweep bound, and deleting a stale ledger row only
-- re-restricts reads, the safe direction. SUCCESS drain records posted
-- at/after the epoch below are NOT residue: each is the anchor of its
-- fence's borrow edges ("borrower still has a record" is committed expiry's
-- whole gate), so it lives with its fence and fence expiry is its only
-- deleter. Pre-epoch success records keep the TTL sweep — their borrows
-- were never recorded, and sweeping them is what drains the epoch floor
-- holding committed expiry shut. (The behavior and the same words live on
-- `sweep_fence_residue` in workspaced.rs.)
--
-- 0044's warning to whoever writes committed-pack expiry: a fence's
-- committed pack may be the ONLY durable copy of members a LATER fence's
-- success record depends on — `/have` and the drain gate accept any
-- committed pack's row as durable, whoever owns it. Expiry of fence F MUST,
-- inside one transaction per victim fence: FOR UPDATE F's `depot_packs`
-- rows FIRST (the drain re-check takes FOR SHARE on them), re-check that no
-- borrow edge on F has a borrower whose drain record still lives, honour
-- the epoch floor above, and only then delete F's POINTERS — the bytes go
-- rowless and the orphan reclaimer collects them a cadence later. Deleting
-- a committed pack without that check silently unbacks committed evidence.

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
