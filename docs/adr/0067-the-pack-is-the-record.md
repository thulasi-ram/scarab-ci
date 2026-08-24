# 0067. The pack is the record: mandatory dependencies, a direct write path, and a self-describing bucket

- **Status:** Accepted
- **Date:** 2026-08-24
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0066](0066-the-depot-is-a-cache.md) (**reverses its "soft, recommended" object
  storage**; keeps and finally *implements* its cache invariant),
  [0064](0064-durability-tiering-and-the-write-path.md) (**retires its flush RPC and warm-first
  write**), [0063](0063-step-logs-on-the-data-depot.md) (**amends where log bytes land — again**),
  [0061](0061-workspace-data-path.md) (**amends part 4's tiering**),
  [0065](0065-retention-cache-and-rederivation.md) (**amends its retention grain**),
  [0029](0029-workspace-cas.md) (the per-file merkle CAS, unchanged),
  [0007](0007-step-outputs.md) (`outputs:` as the trim boundary)

## Context

[0066](0066-the-depot-is-a-cache.md) defined the Data Depot as **definitionally a cache** — *"it
holds nothing that cannot be recreated, which is what makes running more replicas a sufficient HA
story, eviction safe by construction, and a lost replica a latency event. Anything that makes it a
system of record is a defect."*

The code does not honour that sentence, and the gap is not incidental. Four things live on a
Depot replica's local disk and exist nowhere else:

| what | where | consequence at `replicaCount > 1` |
|---|---|---|
| the **drain record** | `warm_dir/drains/<fence>/record.json` | the control plane `GET`s it through the ClusterIP — an arbitrary replica. Absent → `FatalConfig` → **permanent**, blaming the operator's image |
| the **write ledger** | `warm_dir/ledgers/<fence>`, and the code says *"the disk is the only copy"* | closure validation on another replica sees an empty ledger → 422 on every drain |
| the drain's **blobs**, until the flush | warm only, by [0064](0064-durability-tiering-and-the-write-path.md) part 1 | 16 client connections scatter blobs across replicas while the serial tree loop pins to one |
| `/have`'s answer | a `stat` of the local warm dir | a fresh replica reports **every blob missing**, so every drain becomes a cold drain — the measured 5360 ms instead of 642 ms |

So the Depot is a system of record today, in four places, and each one independently breaks
horizontal scaling. [0066](0066-the-depot-is-a-cache.md) points 2 and 3 propose fixes for two of
them (make record-absence transient; add Run-grain affinity behind a headless Service) and have
**zero code** twenty days on.

This ADR takes the other road. Rather than teaching the system to route a Step back to the replica
holding its state, it **removes the state**. That turns out to be cheaper, because the mechanism it
needs — grouping many small objects into one — is a thing the cold tier wanted anyway.

### The measurement that makes a direct write affordable

[0066](0066-the-depot-is-a-cache.md)'s own numbers: the drain is **2.68 ms/file cold** and the cost
is **per-request, not per-byte** — 8.19 MB in 5360 ms is absurd as bandwidth and exactly right as
~2000 round trips. Object storage has **no batch PUT**, which is why
[0066](0066-the-depot-is-a-cache.md) point 7 named packing *"the only lever"* on the per-object bill
and deferred it to a later ADR.

Packing is that ADR, and it arrives for a second reason: **a pack is atomic.** An object either
lands whole or does not appear. That single property is what makes a direct write to the cold tier
safe, and it is why [0064](0064-durability-tiering-and-the-write-path.md)'s carefully ordered
two-phase flush — deepest tree level first, so a failure never leaves cold holding a parent naming
an absent child — has nothing left to protect.

### Warm-only was load-bearing for the wrong thing

[0066](0066-the-depot-is-a-cache.md) made object storage *soft* and paid for it: a
`DurabilityTier::WarmOnly` mode, a boot banner, an `st_dev` probe, `attempts.output_durability`, a
GC tier suppression, and — for **Step logs**, the one class with no recompute path — a
compressed-bodies-in-Postgres fallback.

Every one of those exists to describe a deployment in which the Depot **is** the system of record.
That deployment is the direct contradiction of the cache invariant, and supporting it is what
forced the warm tier to hold things it may not lose.

## Decision

**Eleven parts. Part 1 licenses the rest.**

### 1. Postgres and an object store are both hard requirements

Scarab is not expected to function without either. The chart may optionally ship MinIO; an operator
may point it at any S3-compatible endpoint. Boot **fails closed** when the object store is not
reachable *and writable* — replacing the current unprobed assumption at `workspaced.rs:396`, which
returns `DurabilityTier::Object` for any configured bucket, existent or not.

**Warm-only is retired as a deployment mode.** It survives only as a *runtime degradation* — the
cold tier is configured and momentarily unreachable — and in that state a drain **fails** rather
than succeeding with a smaller promise. `StoreConfig::LocalDir` stops being the default.

> This reverses [0066](0066-the-depot-is-a-cache.md)'s *"object storage is a soft, recommended
> requirement"*. The reversal is the owner's call and it is not a discovery: 0066 already recorded
> *"Depot **HA requires object storage** — a documented consequence, not a gap."* Making HA a
> requirement makes its prerequisite one.

### 2. The Depot connects to the control plane's Postgres — and owns no schema

`main.rs` guards this today: *"it never connects to Postgres and never runs a migration … none of
which the workspace service has, needs, or may be allowed to do **from N per-failure-domain
replicas**. Do not move this down."*

**The boundary being protected is "not a system of record", not "no database."** Half the guard
stays and half goes:

- **connects:** yes, to the same database, for **derived, rebuildable** reads and writes only;
- **migrates:** never. The control plane owns every table's DDL, so a Depot rolled ahead of the
  control plane cannot half-migrate anything — which is the deployment-ordering property the guard
  was actually defending.

The DB credential already arrives in the Depot's environment via the shared Secret and is currently
ignored (`statefulset-workspace.yaml`: *"the role simply ignores it"*), so this adds no new
distribution of credentials.

### 3. Step log bytes are authoritative in the object store

The Depot may **cache** log chunks for a fast live tail; its copy is **never authoritative**.
Postgres continues to hold byte offsets only, never bodies, and the
**compressed-bodies-in-Postgres fallback is deleted** — it existed only for warm-only, which part 1
retires.

This ratifies what the code already does (`logs.rs` writes chunks to the cold store under `logs/`)
and resolves a contradiction CONTEXT.md carried in two adjacent entries: logs described as *"bytes
on the Data Depot"* and *"the one class that cannot be re-derived"*, on a service defined as holding
nothing that cannot be recreated.

> **Guarantee what cannot be recreated; degrade what can** — [0066](0066-the-depot-is-a-cache.md)'s
> sentence, applied to its own storage decision. Logs are guaranteed by living in the mandatory
> durable tier, not by being pinned in a cache.

### 4. The durable write happens in **one pass**, not a deferred second one

The flush RPC, its two-phase ordering, and warm-first writes all **retire**. As a drain's durable
bytes arrive, the Depot streams them into a **pack** and writes that pack to the object store — so
there is no second pass, and no window in which a replica holds the only copy of anything durable.

**"Directly" here means *in one pass*, not *by the pod*.** The pod never holds an object-store
credential and never writes the bucket, before or after this change (part 5). What retires is the
second hop in *time* — content that was durable-later is now durable-now — and the control-plane hop,
since the Depot writes the bucket itself rather than being asked to later.

[0064](0064-durability-tiering-and-the-write-path.md)'s pinned invariant —
`a_flush_that_fails_leaves_no_cold_tree_naming_an_absent_child` — is not weakened but **made
vacuous**: a pack is one object, so there is no half-written state for an ordering rule to protect.
Reachability still begins at the commit pack (part 8), which is written last.

### 5. The Depot streams the pack through; the pod never holds a storage credential

The pod uploads to the Depot exactly as it does today. The Depot:

1. **verifies each member against its address** — the existing integrity boundary, unmoved. This is
   the one check no reader-side verification can replace, because only the Depot sees a
   *client-supplied* address on a *write*;
2. **streams bytes into a multi-part upload** as they arrive, so memory is bounded by one part
   rather than by the workspace;
3. writes the **index as the final part** and completes the upload, which is atomic;
4. **records what it accepted** — which is the write ledger, now part of the pack (part 8).

The pod therefore needs no object-store credential, and the Depot stays in the write path for the
two reasons that justify it: verification and the ledger. It is *not* in the write path for
durability, which is the change.

### 6. The pod labels each upload `durable` or `cache-only`

A Step's declared `outputs:` are usually a fraction of its workspace, and build scratch
(`target/`, `node_modules/`) has no business in the durable tier — today it lands warm-only and is
*"deliberately never flushed"*.

The Depot cannot infer the trim from arriving trees, because trees arrive children-first and the
root arrives last. So the **pod labels each upload**: it already scans and hashes the whole
workspace before uploading anything, so it computes the trimmed closure locally with no extra reads.

- `durable` → streamed into the pack;
- `cache-only` → kept on the Depot's local disk, **unpromised and evictable**, which preserves the
  post-hoc "what did this build actually produce" view without paying storage for scratch.

This keeps [0007](0007-step-outputs.md)'s amendment intact: `outputs:` remains a **precision** tool,
never something an author declares for speed. The system pays the wire cost of the untrimmed
remainder, exactly as it does today.

### 7. Pack boundaries: size-capped, always closed at the drain

- **small members** are packed; **large members stay loose**, because packing an already-efficient
  object buys nothing;
- a pack rolls over at a size cap and is **always closed at the drain boundary**, so a drain
  produces one or more packs and **never shares one with another drain**.

The drain-aligned boundary is what keeps compaction mild. The classic pack problem — deleting one
member means rewriting the pack — is severe when packs mix unrelated lifetimes. Here a pack holds
one Attempt's new content, retention expires Runs, and **most of a pack therefore dies together.**

### 8. The commit pack is written last, and it is both the receipt and the ledger

```
drain → pack A1 (bodies) → pack A2 (bodies) → pack A0 (commit)   ← last
```

`A0` carries the **fence**, the **published root**, and the **sibling list**; every pack's index
carries every hash it holds and its length. Therefore:

- *"did this Attempt's drain finish?"* is **does its commit pack exist** — no `record.json`;
- *"may this fence read this tree?"* is **is it in this fence's own pack index** — no ledger file.

Both answers are properties of the object store, so any replica can give them, and the
cross-fence exfiltration protection from `e58ce1f` survives unweakened: a fence still cannot
publish a snapshot naming trees it did not write, because the pack it wrote is the record of what it
wrote.

### 9. The pack index triples as presence index and size index

One lookup answers three questions that today need three mechanisms — a local `stat`, a HEAD, and
(for size) **a full blob download**, because `FlatEntry.size` is not recorded in a `TreeEntry` and
`blob_size`'s cold arm reads the whole object to learn its length.

**No `TreeEntry` format change.** The index's length field answers the size question, so the frozen
canonical tree form and its cross-binary skew tripwire are untouched — and with them every recorded
snapshot identity.

### 10. No write-ahead log. Two ordering rules and a grace window

The dangerous state is an index that claims a member exists when it does not, because `/have` then
reports it present, the client **skips uploading it**, and the bytes are lost with no second chance.
So:

- **on write: bytes before pointers.** The pack lands, then the index rows. A crash leaves
  unreachable leftovers, which are safe and reclaimable.
- **on delete: pointers before bytes.** Index rows go, then the pack. A crash leaves unreferenced
  bytes, not a lying index.
- **deletes wait out a grace window** longer than any in-flight read, which is how concurrent
  read-versus-delete is handled — not with a log.

A pack upload is atomic and an index write is one transaction, so **the index write is the commit
point and it is already atomic.** The pack is its own log record. Postgres has a write-ahead log;
this design does not rebuild one on top of it.

Compaction is the one multi-object operation that wants an **intent record** ("compacting [A,B] → C,
phase: writing"). That is a resumable job row, not a WAL, and it obeys the same two rules.

### 11. Postgres holds a derived index; the bucket wins

Every pack carries its own index, so **the bucket alone is sufficient to rebuild** the Postgres
index. Postgres is a fast query surface, not a second source of truth: on any disagreement the
bucket wins, and losing the index is a rebuild job rather than data loss. This is what licenses
part 2 — the Depot reads a database, and nothing there is a record of anything.

### 12. Addresses grow an algorithm tag; the hash stays SHA-256 for now

*(Added 2026-08-24, owner decision.)* This ADR ships layer 1 of
[0066](0066-the-depot-is-a-cache.md) point 11: every address the system writes from now on is
**algorithm-tagged** (`sha256:<hex>`), and every reader accepts both tagged and legacy bare
addresses. Nothing is rewritten; no historical root changes meaning. The tag is what makes the
digest choice reversible — a later `blake3:` address coexists with every `sha256:` one, and mixed
trees stay unambiguous.

BLAKE3 itself is the intended follow-up, **not** part of this ADR. Two facts sized that call
(verified 2026-08-24): it is a registered REAPI digest function, so the swap stays inside the
standard's vocabulary; and its verified-streaming story — ranged reads a client can verify without
the whole blob — is real but not free, since mainline `bao` is beta cryptography with a ~6.25%
outboard overhead at 1 KiB chunks (the chunk-group variants that cut this to ~0.1% live in forks,
`abao` / n0's `bao-tree`). Adopt the tag now, decide the hash when that story is load-bearing.

## Consequences

**What this deletes.**

- The flush RPC, `flush_to_cold`'s two phases, `FlushOutcome`, and the tier-pair guard.
- `DurabilityTier::WarmOnly`, the boot banner, the `st_dev` probe, the GC tier suppression, and —
  since every successful drain is now durable by construction — `attempts.output_durability`,
  which becomes a constant. **Delete the column rather than keeping it as ceremony.**
- The drain record file and the ledger file, and with them the `warm_dir/drains` and
  `warm_dir/ledgers` residue sweeps (extract the fence-residue TTL first — it is load-bearing, per
  [0066](0066-the-depot-is-a-cache.md) point 10 carve-out (a)).
- **Eviction pins.** Nothing on local disk is ever the only copy of anything durable, so eviction
  needs no lease, no reachability walk, and no in-flight exception. This is
  [0066](0066-the-depot-is-a-cache.md) point 4's problem *dissolved* rather than solved.
- **Fence affinity and cordon, as correctness mechanisms.** Both remain available as *cache-warmth*
  optimisations, freely abandonable.

**What this does not fix, and must not be confused with.** The remaining availability defect is
independent of every part above: `scarab-wsfetch` uses a bare `reqwest::Client::new()` with no
timeout and **no backoff**, so a 20–30 s outage burns an Attempt's whole budget in seconds and the
Run dead-letters. That is [0066](0066-the-depot-is-a-cache.md) point 2's other half and it is still
the single cheapest availability fix in the tree. **`replicaCount > 1` is worth nothing without it.**

**What gets harder.**

- The drain's tail is now one upload rather than zero. Pre-shipping during the Step — the `hold`
  sidecar is already resident and idle for the Step's whole life — is the mitigation, and it must be
  measured with **Step duration** as the metric, not drain duration, because it spends the Step's
  own I/O.
- Packing needs a **dual-read migration**: look up the index, fall back to the loose object. Existing
  content stays loose; nothing is rewritten.
- **Compaction policy is deliberately not decided here.** Part 7's drain-aligned boundary is the bet
  that retention will reclaim most packs wholesale and compaction can be deferred until measured.

**Scope this ADR does not carry.** Making the object store mandatory is a wide, mechanical change:
config defaults, `values.yaml`'s empty `bucket`, ~28 test sites that boot a local directory,
`crash_resume.rs` (which explicitly clears every `SCARAB_S3_*` variable), and CI — `ci.yml` has no
MinIO and `kind.yml` runs the live tier on a local directory. Note also that **no workflow runs
`helm lint` or `helm template`**, so chart changes have no CI coverage at all today.

## Alternatives considered

- **Build [0066](0066-the-depot-is-a-cache.md) points 2 and 3 as written** — make record-absence
  transient, add a headless Service and Run-grain affinity. Strictly less work than this ADR and it
  does make `replicaCount > 1` correct. Rejected as the *endpoint* because it keeps four kinds of
  replica-local state alive, and every later feature has to keep respecting them: eviction needs
  pins, scale-down needs draining, and a hot replica needs balancing. Affinity is retained here as
  an optimisation precisely because it is cheap; it is refused as the correctness mechanism.
- **Move the drain record and ledger to Postgres, leave the write path alone.** Fixes two of the four
  states and leaves the other two — the pre-flush blob window and `/have`'s per-replica answer. The
  second is the expensive one: an 8× drain regression on the leg already measured as the bottleneck.
- **Adopt the Bazel remote-execution CAS protocol.** Its shape is an eerily exact match —
  `FindMissingBlobs` is `/have`, `GetTree` is `/flat`, batch read/write is the wire work the
  transfer handoff was going to invent, and its digest carries a size, which is part 9's problem
  solved at the format level. One rejection ground originally claimed here was **wrong** and is
  corrected (2026-08-24, verified against the proto): REAPI is *not* limited to an executable
  bit — since v2.2, typed `NodeProperties` carry `mtime` and `unix_mode` on `FileNode`,
  `SymlinkNode` and `Directory`. The caveats are real but smaller: property support is
  server-dependent (a server may `INVALID_ARGUMENT` names it does not accept), and populated
  properties enter the `Directory` digest — the same coupling our `TreeEntry` already has. What
  actually rejects the protocol: the tree hash **is** the recorded snapshot identity, so changing
  the tree format changes every historical Run's root; and REAPI has no equivalent of the fence
  ledger, the `durable`/`cache-only` label, or the commit-pack atomic boundary — the parts of this
  ADR that carry its guarantees. **The design lessons are adopted (batch shape, size available
  without a read); the protocol is not.**
- **Encrypt content so authorisation leaves the data path.** Explored and rejected: with a
  content-derived key, rotation changes every address; with a per-file key table, the address is
  stable but rotation, erasure and the fork-PR case each need their own answer; and either way the
  durability accounting that licenses `Succeeded` has nothing left to report. It also solved a
  problem that turned out not to exist — see the next bullet.
- **Give the pod a scoped storage credential and remove the Depot from the data path.** The premise
  that motivated it is **false**: the isolation grain is already the *container*, not the pod
  (`executor-k8s/src/lib.rs`: *"they are mounted into different containers … so neither credential's
  presence implies the other's"*), so the untrusted step container never sees the Depot token and a
  storage credential could ride the same tmpfs-Secret mechanism today. The real obstacle is
  request count: `/flat` is documented *"Not optional"* because without it a 50 000-file checkout is
  50 000 sequential round trips, and `/have` batches 5 000 hashes into one call. Direct reads
  reinstate exactly the cost [0061](0061-workspace-data-path.md) and
  [0066](0066-the-depot-is-a-cache.md) were spent removing.
- **One pack per drain, any size** — best possible retention grouping, but a large Step yields a
  multi-gigabyte object and reading three files means ranging into it. Part 7's cap is the
  compromise.
- **Fixed-size packs ignoring drain boundaries** — most even object sizes, and the worst compaction
  story: expiring one Run punches holes across many packs, which is the classic pack-garbage
  problem at full force.

## Implementation notes (2026-08-24)

Recorded when the branch landed, so the ADR stays honest about where the build diverged and what
is still unbuilt. None of these reverse a decision; two refine one.

- **Part 7, oversized members:** the text says "large members stay loose"; the build gives an
  oversized member its **own single-member pack** instead — no loose-durable side channel, so pack
  footers alone describe every durable byte. The refinement is deliberate (one code path, one
  self-description story) and this note is its record.
- **Parts 8 and 11, where the operational reads happen:** the *authority* is the bucket exactly as
  written — the commit pack is written last and carries the receipt, the ledger and the index. But
  the **operational read path is the Postgres rows** (`depot_drain_records`, `depot_fence_writes`,
  `depot_pack_members`): `GET /v1/drains` and closure validation read rows, not the commit pack,
  and part 11's rebuild-from-footers job is **not yet written**. Until it exists, "Postgres is a
  derived copy" is a design property with no exerciser — treat a lost row as a re-drive, not a
  rebuild.
- **Replica safety now covers the drain write path too** (git-bug afb13c2). A body pack's index
  rows are **staged** (`depot_packs.committed = FALSE`) the moment its multipart upload completes
  — per-pack bytes-before-pointers — and an idle tail pack is sealed by a 2 s linger, so the one
  replica that receives `POST /v1/drains` builds the commit pack from the fence's rows across all
  replicas and flips them committed inside the record transaction. Every durable-presence read
  carries `committed OR fence_key = caller`: staged rows are visible only to the fence that owns
  them, so no other fence can ever dedup against a drain that may never finish. Closure
  validation reads trees warm-or-index for the same reason. Affinity stays refused as a
  correctness mechanism (a possible warmth optimisation only); the chart's `replicaCount` note
  says the same.
- **Retention's deletion machinery is unbuilt** (see the 0065 pointer): packs are currently never
  deleted, which errs safe but means part 7's retention-grouping payoff is not yet collectable, and
  cross-fence dedup (a later fence's record depending on an earlier fence's pack) must be
  refcounted or re-packed **before** pack-grain expiry is ever written — ticketed.

## References

- [0007](0007-step-outputs.md) — `outputs:` as a precision tool, never a speed knob; the trim
  boundary part 6 uses
- [0029](0029-workspace-cas.md) — the per-file merkle CAS; why every write is idempotent and a
  retried pack is a no-op
- [0050](0050-retention-and-gc.md) — retention classes; the TTL that reclaims packs wholesale
- [0061](0061-workspace-data-path.md) — **amends part 4's tiering**; the Data Depot, and the
  measurement that made request count the currency
- [0063](0063-step-logs-on-the-data-depot.md) — **amended by part 3**: log bytes are authoritative in
  the object store, not on the Depot
- [0064](0064-durability-tiering-and-the-write-path.md) — **retired by part 4**: warm-first and the
  flush RPC; its ordering invariant is made vacuous, not weakened
- [0065](0065-retention-cache-and-rederivation.md) — **amends its retention grain**: the pack, not
  the object, is what expires
- [0066](0066-the-depot-is-a-cache.md) — the cache invariant this ADR finally implements; its point 7
  named packing "the only lever", and its "soft, recommended" object storage is **reversed** by
  part 1
- git-bug `4ce7f2c` — the measurement: 2.68 ms/file cold drain, cost per request not per byte
