-- ADR-0067 part 2 (slice 2): the Depot's two replica-local files become rows.
--
-- Until now a fence's drain record lived at `warm_dir/drains/<fence>/record.json`
-- and its write ledger at `warm_dir/ledgers/<fence>` — one replica's disk, which
-- is exactly the "system of record on a cache" defect ADR-0067's context table
-- names: at replicaCount > 1 the control plane GETs the record through the
-- ClusterIP (an arbitrary replica) and closure validation on another replica
-- sees an empty ledger. Moving both to Postgres makes any replica able to
-- answer; ADR-0067 part 2 licenses the connection (the Depot connects, reads
-- and writes DERIVED rows — it NEVER migrates; only the control plane runs
-- this file).
--
-- `depot_drain_records`: one row per fence — what `POST /v1/drains` deposits
-- and `GET /v1/drains/{fence_key}` serves. `record` is the wire `DrainRecord`
-- verbatim (JSONB); `version` is the stored-record format version so a future
-- reader refuses what it would mis-parse. A success row is write-once at the
-- handler (409), an error row may be overwritten by a later POST.
--
-- `depot_fence_writes`: the write ledger — one row per tree PUT a
-- fence-claimed token made (git-bug 212bb13: a drain may only publish a root
-- its own fence wrote; a content address is not a secret). `tree_address` is
-- the NORMALIZED BARE HEX (64 lowercase hex chars): tagged spellings
-- (`sha256:<hex>`, ADR-0067 part 12) are normalized at the handler edge and
-- everything below — this table included — sees bare hex only. Tags belong in
-- pack footers and `depot_pack_members` (slice 3), not here.
--
-- FENCE RESIDUE, refined by git-bug ec294b7 (`written_at` / `posted_at` are
-- unix seconds): `depot_fence_writes` rows and ERROR drain records are
-- residue, TTL-swept by the Depot — no workspace token outlives the sweep
-- bound, and deleting a stale ledger row only re-restricts reads, the safe
-- direction. SUCCESS drain records posted at/after `depot_borrow_tracking_epoch`
-- (0048) are NOT residue: each is the anchor of its fence's borrow edges
-- ("borrower still has a record" is committed expiry's gate), so it lives
-- with its fence and fence expiry is its only deleter. Pre-epoch success
-- records keep the TTL sweep — their borrows were never recorded, and
-- sweeping them is what drains the epoch floor holding committed expiry
-- shut. Losing either table entirely is a re-restriction plus a re-drain,
-- never data loss (ADR-0067 part 11: Postgres holds a derived index; the
-- bucket wins) — but a lost success record now also demands the borrow-edge
-- rebuild before any committed expiry may run.

CREATE TABLE depot_drain_records (
    fence_key TEXT PRIMARY KEY,
    run       TEXT NOT NULL,
    step      TEXT NOT NULL,
    attempt   TEXT NOT NULL,
    version   INT NOT NULL,
    posted_at BIGINT NOT NULL,
    record    JSONB NOT NULL
);

CREATE TABLE depot_fence_writes (
    fence_key    TEXT NOT NULL,
    tree_address TEXT NOT NULL,
    written_at   BIGINT NOT NULL,
    PRIMARY KEY (fence_key, tree_address)
);

-- The residue sweep deletes by age across ALL fences; without this it is a
-- full-table scan on every pass.
CREATE INDEX depot_fence_writes_written_at ON depot_fence_writes (written_at);
CREATE INDEX depot_drain_records_posted_at ON depot_drain_records (posted_at);
