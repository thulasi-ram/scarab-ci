-- ADR-0067, replica-safe drain write path (git-bug afb13c2): body packs are
-- now indexed when they SEAL — before any commit pack exists — so another
-- replica's staged bytes can reach the durable index mid-drain. That makes
-- `depot_packs` rows visible for drains that may never finish, and a
-- fence-blind durable-presence read (`/have`, the drain gate, closure
-- validation) would let a DIFFERENT fence dedup against never-committed
-- staging that the future reclaimer deletes — the ec294b7 exfiltration class
-- widened into data loss.
--
-- `committed` is the marker: FALSE from seal time (staged — visible only to
-- the fence that owns it), flipped TRUE inside the drain-record / settle
-- transaction, atomically with the rows that make the drain real. Every
-- durable-presence read carries the predicate
-- `committed OR fence_key = $caller_fence`: a fence may trust its own
-- staging (its retried drain must not re-upload what it already sealed),
-- nobody else may.
--
-- Existing rows were all written by the old protocol — one transaction, at
-- record time, strictly after the commit pack — so they are committed by
-- definition and are backfilled TRUE.

ALTER TABLE depot_packs ADD COLUMN committed BOOL NOT NULL DEFAULT FALSE;

UPDATE depot_packs SET committed = TRUE;
