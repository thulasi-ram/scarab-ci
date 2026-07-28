# 0064. Durability tiering and the write path: warm-first, flush before `Succeeded`, and the promise is the backing

- **Status:** Accepted
- **Date:** 2026-07-28
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0061](0061-workspace-data-path.md) (**amends its part 4**),
  [0062](0062-workspace-export-lazy-without-node-driver.md) (the drain this write path serves),
  [0063](0063-step-logs-on-the-data-depot.md) (the Data Depot),
  [0027](0027-restart-semantics.md), [0029](0029-workspace-cas.md)

## Context

[0061](0061-workspace-data-path.md) part 4 states the rule:

> **4. An Attempt is not `Succeeded` until its Workspace Snapshot is durable.** On spot, a node can
> vanish between "the Step exited 0" and "its evidence is safe". Declaring success before durability
> puts a claim in the durable record that the record cannot back — the one thing this product may not
> do.

That rule is implemented, and it is implemented as a **write ordering**. `TieredCas::put_blob` and
`put_tree` `await` cold **first** and propagate its error; a warm failure is counted and swallowed.
The comment at `tiered.rs:250` says so in as many words: *"Cold first: this is the leg that licenses
`Succeeded`."*

Two problems with that mechanism came out of building [0062](0062-workspace-export-lazy-without-node-driver.md)'s
drain, and one came out of checking what actually backs each tier.

**The per-blob cold round-trip is the cost this whole line of ADRs exists to remove.**
`TieredCas::ingest` is:

```rust
let snapshot = self.cold.ingest(path).await?;   // full walk, one cold round-trip per blob
match self.warm.ingest(path).await { ... }      // a SECOND, independent full walk
```

Two complete walks of the tree, and the cold walk pays a network round-trip **per file** — the 4–6 ms
per file that 0061 measured as **81–88%** of the cost it set out to fix. There is even a tripwire for
the case where the two independent walks disagree on the root. The change-set fold is better, since
only changed files are written, but each changed file still carries an interleaved cold round-trip on
the critical path.

**"Durable" is a property of the backing, not of the tier's name.** `StoreConfig` is `S3 | LocalDir`,
and `LocalDir` may point anywhere — including a directory on the warm volume itself, or the Helm
chart's `scratch` **`emptyDir`**. In those configurations "written to cold" licenses nothing: cold is
the same disk as warm, or worse, a volume that dies with the Pod. The word "cold" implies a promise
that in those deployments nothing is making.

**And the deployments that matter do have a real cold tier**, which is worth recording because an
earlier reading of this said otherwise. `deploy/local-helm/deploy.sh` defaults `S3_BUCKET` and
stands up in-cluster MinIO on a PVC; `just up` runs MinIO under compose; a real chart install has the
operator set `scarab.s3.bucket`. The degenerate case is the chart's **unset default**, labelled
"dev only", which no shipped deployment path uses. So this is a misconfiguration hazard rather than
the normal case — but it is silent, and silence is what this ADR family refuses.

## Decision

**1. The write path is warm-first with a single batched flush.**
A drain writes to the Data Depot's warm tier locally — one walk, no network — and then performs
**one batched archival flush** to cold. This replaces per-blob interleaved cold round-trips and the
second independent walk. It is strictly faster than the status quo and it is also simpler: one
writer, one walk, one flush.

**2. The flush gates `Succeeded` wherever a real cold tier exists.** Part 4's guarantee is kept in
full: the Attempt is not `Succeeded` until the flush completes. What changes is *how* the bytes get
there, not *what must be true* before success. So "success only when written to cold" now means **the
flush completed**, not that every blob individually round-tripped during the fold.

**3. A cold tier is one whose backing is independent — measured, not assumed.**
The test is `st_dev` of the cold directory against the warm directory, which is one `stat`:

| cold | verdict |
|---|---|
| S3 / MinIO | genuine second tier |
| `LocalDir` on a **separate volume** (a second PVC) | `st_dev` differs → **genuine second tier** |
| `LocalDir` on the warm volume | `st_dev` matches → **not a tier**; no independent durability |
| `LocalDir` on the chart's `emptyDir` | same volume, and it dies with the Pod → worse than warm |

"Is it object storage?" was the obvious test and it is the wrong one: it would reject a second PVC,
which is a perfectly good cold tier, and accept nothing else. **The backing is the promise.**

**4. Where there is no independent cold tier, the Depot degrades gracefully rather than refusing.**
Warm gates `Succeeded`, and the deployment is *loudly* a warm-only-durability deployment: stated at
startup, and stamped on the record (below). Refusing to boot was considered and rejected — it
re-imposes object storage as a prerequisite, which [0061](0061-workspace-data-path.md) declines to do
even for the CAS ("an operator's cost decision, not an engine decision").

**5. The durability that backed a snapshot is stamped on the Attempt**, beside `output_snapshot` and
`output_identity`. A startup log tells whoever read logs that day; it cannot explain a Run a month
later. Deployments change — an operator adds S3 next month, and then Attempts from before and after
have **different guarantees with identical records**. Stamping it means the UI can say *"this Run's
evidence was never archived; this deployment had no independent durable tier"* instead of showing a
mystery gap. This is [0027](0027-restart-semantics.md)'s rule applied to durability rather than to
invalidation.

### The amendment to 0061 part 4, stated plainly

Part 4 as written is unconditional, and allowing `Succeeded` on warm alone contradicts it — 0061's own
retention table says warm "promises none". So this **amends part 4**, and the amendment is marked in
0061 in place.

**What the amendment preserves is the invariant; what it changes is the mechanism.** Read part 4's own
reasoning: *"Declaring success before durability puts a claim in the durable record that the record
cannot back — the one thing this product may not do."* The invariant is **never make a claim the
record cannot back.** Cold-before-`Succeeded` was one mechanism for it, and a good one where a cold
tier exists.

A deployment that *declares* "durability here is warm's" and then succeeds on warm is not making an
unbacked claim. It is making a smaller, true one. What 0061 was protecting against is the **silent**
version — success implying an archive that was never written. Disclosure plus the per-Attempt stamp
preserves the invariant under a weaker mechanism, which is why this is an amendment rather than a
retreat.

0061 also explicitly forbade a related thing, and that prohibition **stands**:

> *"writing warm-first and letting the service tier onward would make the warm tier load-bearing for
> durability, which part 4 forbids."*

This ADR writes warm-first and **does not let the service tier onward asynchronously**. The flush is
synchronous with respect to `Succeeded`. Warm is the write *path*; cold remains the *promise* wherever
it exists. Asynchronous archival was considered and rejected — see below.

## Alternatives considered

- **Keep cold-first per blob (status quo).** The invariant comes free and needs no disclosure
  mechanism. Rejected on cost: a network round-trip per file on the critical path, plus a second full
  walk, is precisely the per-file cost 0061 and 0062 exist to eliminate.
- **Warm-first with asynchronous archival, `Succeeded` on warm.** The fastest option, and the one
  first proposed in the discussion that produced this ADR. Rejected: it makes warm load-bearing for
  durability, which is what part 4 forbids and what 0061 named explicitly. It also requires the
  machinery that falls out of that choice — an eviction pin for un-archived snapshots, and a state
  for an Attempt whose evidence was lost after it went green, which would mean a `Succeeded` Attempt
  becoming un-green *after downstream Steps consumed it and the forge already showed a checkmark*.
  That is a large amount of vocabulary and cascade semantics bought for a latency win that the
  batched flush delivers anyway.
- **Refuse to boot without an independent cold tier.** Honest and simple, and it makes object storage
  a hard prerequisite. Rejected per (4).
- **Test for cold-ness by asking "is it S3?"**. One line, and wrong in both directions: it rejects a
  second PVC and cannot detect a `LocalDir` sitting on the warm volume. Rejected in favour of
  `st_dev`.
- **Record the degraded guarantee only at startup.** Free. Rejected: it cannot explain an old Run, and
  a deployment's guarantee changes over time while its old records do not.

## Consequences

- **`TieredCas`'s ordering is superseded**, including the "Cold first" comment at `tiered.rs:250`,
  which is currently the clearest statement of the old mechanism and must be rewritten rather than
  deleted — a reader who finds it will otherwise trust it.
- **The two-walk `ingest` goes**, and with it the root-disagreement tripwire between two independent
  walks, which existed only because there were two walks. The canonicalisation-skew tripwire on
  `put_tree` is a different hazard and stays.
- **A flush that fails fails the Attempt** where a cold tier exists. The Step exited 0 and its
  evidence did not reach the archive, so it is a retryable failure that must name the cause rather
  than surfacing a mystery I/O error.
- **One new column on `attempts`** for the durability tier that backed the snapshot.
- **A warm-only deployment is a supported configuration** with a stated, weaker guarantee — not a
  misconfiguration to be rejected, and not a silent downgrade either.
- **The batched flush is a new failure surface**: a partial flush must not report success, and the
  batch must be idempotent, because the CAS is content-addressed and a retried flush will re-offer
  the same keys.

## References

- [0027](0027-restart-semantics.md) — content-addressed invalidation; smart never means mysterious
- [0029](0029-workspace-cas.md) — per-file merkle CAS; why a retried flush is idempotent
- [0050](0050-retention-and-gc.md) — retention classes; cold is the tier with the TTL
- [0061](0061-workspace-data-path.md) — **amends part 4**; the warm/cold tiers and the retention table
- [0062](0062-workspace-export-lazy-without-node-driver.md) — the change-set drain this serves
- [0063](0063-step-logs-on-the-data-depot.md) — the Data Depot; logs take the opposite decision, and
  the asymmetry is that logs cannot be re-derived
- [0065](0065-retention-cache-and-rederivation.md) — what is kept, and what is recomputed instead
