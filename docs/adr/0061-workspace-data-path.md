# 0061. Workspace data path: workspace service + node driver, lazy materialisation

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** thulasi.ram (architect)

## Context

[0004](0004-execution-topology.md) chose **pod-per-step** with the workspace passed as
content-addressed snapshots in object storage, and promised a **control-plane / data-plane
split**: the durable brain records intent, data moves on its own path.

The implementation does not honour that promise. `build_pod` gives each Step Pod an
`emptyDir` plus two `busybox` doorstop containers, and `drive_workspace` moves the workspace
**through the control plane**: `tar -xf` in over `kubectl exec`, `tar -cf` out over
`kubectl exec`, hashed on the server. So every byte of every workspace crosses the
**Kubernetes API server** — a control-plane component never intended for bulk data — twice
per Step.

This ADR originally continued: "[0029](0029-workspace-cas.md)'s dedup and incremental transfer
are real, but they apply only between the server and object storage, i.e. *after* the expensive
part." **Measurement — s0, recorded under Open below — showed that sentence to be backwards.**
The server↔object-storage leg *is* the expensive part, 81–88% of every Step boundary, and the
dedup saves approximately nothing: the cost is one sequential round-trip **per file**, not the
bytes. Every structural complaint below still holds. The money is simply not where this
paragraph guessed it was.

**This is unsound by inspection, independent of any benchmark.** A tar piped through an
`exec` stream is: a single sequential connection (no parallel ranges, no saturating a NIC);
unresumable (a break at 90% starts over); routed through the cluster's most contended
component, competing with the control traffic that keeps the cluster alive; funnelled through
one `scarab-server` process that must also hold and hash every byte; and it moves the *whole*
workspace in both directions regardless of what changed. Object storage clients have the
opposite properties on every line — *in principle*; the client we actually wrote does not (s0
again: `materialize`/`ingest` walk the tree one file at a time and await a round-trip each, so
they have no parallelism either). No measurement is required to reject this path — only to
size the win.

The operating environment sharpens this into a design constraint rather than a performance
bug. Scarab's target deployment is **Karpenter + spot**: nodes are provisioned on demand,
consolidated aggressively, and reclaimed without notice. Co-location of consecutive Steps
cannot be assumed — consolidation actively works against it. **Every Step boundary is a
network boundary**, and no scheduling hint changes that.

That rules out the topologies other CI engines use to make this problem disappear. They all
avoid it by choosing a coarser unit that owns a machine for its lifetime — GitHub Actions
(the job), Woodpecker (the pipeline, one volume, node-pinned under RWO), Tekton (the Task,
plus an affinity assistant that pins a whole run). Sharing is free inside the unit and
painful across it. Argo Workflows makes our choice — step-grain pods, object-storage
passing — and pays what we pay. Pod-per-step is not the mistake; **routing the data through
the brain is**.

## The governing principle

> **Minimise the substrate idiosyncrasies an author must know.**

A Pipeline author should not have to learn that this engine is expensive at Step boundaries,
that caches shouldn't live in the workspace, or that wide edges cost money. Rules of that
shape are unenforceable, they date badly, and they push our operational problems into other
people's YAML. Where the substrate is expensive, **the system pays, not the author.**

This principle decides things that would otherwise look arguable, and it is why this ADR
does *not* make `outputs:` a performance requirement (see Consequences).

## Decision

**Four parts.**

**1. A workspace service — in the standard path, not an optional accelerator.**
A long-lived, Scarab-operated service holding a warm content-addressed store on a persistent
volume, deployed once per failure domain (per AZ, where AZs exist). Generic **object
storage** sits behind it as the cold archive under a retention TTL. *One* path in every
deployment mode — dev, `kind`, colima, production — because two modes is two mental models
and the taxonomy cost is worse than the component cost.

The persistent volume attaches to **the service**, never to a Step Pod. Every problem PVCs
have here — attach/detach latency at each boundary, stuck volumes when a spot node is
reclaimed, per-node attachment quotas, provisioning delay — comes from binding volumes to
short-lived pods. Bind them to the long-lived thing instead and all of it goes away.

**2. A Scarab node driver, shipped and installed as standard.**
It mounts a Workspace Snapshot into a Step Pod as a **read-only lower layer** with a
**pod-local writable upper layer** on top. Materialisation is **lazy**: a Step transfers what
it reads, not what it inherits. This is what makes wide edges affordable without authors
declaring anything, and it is the only mechanism that does — intercepting reads requires a
privileged mount, which an unprivileged Pod cannot arrange for itself.

Service and driver are **one component in two halves** (server and client), not two concepts.

**3. The control plane leaves the data path.** The server exchanges **root hashes** — tens of
bytes. Helper containers become Scarab-owned images rather than `busybox` doorstops. The
`kubectl exec` tar tunnel is deleted.

**4. An Attempt is not `Succeeded` until its Workspace Snapshot is durable.** On spot, a node
can vanish between "the Step exited 0" and "its evidence is safe". Declaring success before
durability puts a claim in the durable record that the record cannot back — the one thing
this product may not do.

### Vocabulary

[CONTEXT.md](../../CONTEXT.md) previously used **Workspace** for two different things. They
are now distinct:

- **Workspace** — the *mutable* filesystem a Step executes in. Pod-local, dies with the Pod.
- **Workspace Snapshot** — the *immutable*, content-addressed tree that flows along a DAG
  edge and that an Attempt owns as evidence. Retained per policy, then archived, then gone.

### Retention

Two tiers, two policies, so eviction can never break a promise:

| tier | bounded by | promise |
|---|---|---|
| workspace service (warm) | **space** — evict least-recently-used | none; a miss is slower, never wrong |
| object storage (cold) | **time** — retention TTL, as Artifacts already are | this is the guarantee users are given |

Plus a manual **pin** ("keep this Run's workspaces") for investigations, and **graceful
degradation**: expired inputs widen a rerun's scope and say so, rather than failing. Rerun
affordances must state which they are — "Rerun this step" vs "Inputs expired — this re-runs
from *clone*" — per [0027](0027-restart-semantics.md)'s rule that smart never means mysterious.

## Consequences

- **A third stateful component.** [0004](0004-execution-topology.md) called object storage
  "the second (and last)". That is now wrong, deliberately. Accepted because one standard
  path beats a fast path plus a fallback path.
- **A privileged node driver becomes an install prerequisite** — including on `kind` and
  colima, so the dogfood exercises the real thing. Note this is a *larger* ask than
  Woodpecker's (which needs only a default StorageClass) and should be honest in the docs.
- **The implicit inherit-everything default stays**, and `outputs:` ([0007](0007-data-passing-model.md))
  stays a *precision* tool — for cache keys, safe fan-out and restricting what flows — never
  a performance tax. Laziness plus content addressing is the automatic version of a narrow
  edge: a Step that reads 5% of a tree moves 5% of it, having declared nothing. Directly
  downstream of the governing principle.
- **"Don't put your cache in the workspace" is not a rule we will teach.** It is
  unenforceable and it is substrate knowledge. [0007](0007-data-passing-model.md)'s Cache
  concept remains the *better* tool; using it stays an optimisation, not a requirement.
- **The Helm-dogfood failure class disappears.** `deploy/local-helm` puts the CAS on an
  `emptyDir`, so a server restart wipes every workspace and reruns of older Runs hang at
  `Init:1/3` and dead-letter. A standard-path service with a real volume removes that
  entirely.
- **Cross-AZ traffic is confined to the archive drain**, which can be throttled or scheduled.
  Step Pods talk only to their own zone's service. (In-region object storage is typically
  free per byte; EC2-to-EC2 across AZs is not. The cost was never "object storage" — it was
  the zone boundary.)
- **Object storage stays generic.** Scarab names no product and makes no single-zone /
  multi-zone / storage-class call. That is the operator's cost model, not ours.
- **Version skew** between server, service and driver becomes a supported concern.
- **Browse** ([0056](0056-run-takes-and-attempt-grain-evidence.md)) reads snapshots from the
  service, which keeps working after the Run's nodes are gone — on spot, that is within
  minutes, and a live-volume design would show a blank pane exactly when people look.
- **OCI artifacts are an available internal representation** for the service, not a separate
  concept. Kubernetes already distributes immutable content to ephemeral nodes with per-node
  caching and dedup; if the service chooses to ride that, nothing above it changes.

## Alternatives considered

- **PVC per Step** — every cost of PVCs, none of the sharing: provisioning and attach latency
  per Step, per-node attachment quotas, one volume to reap per Step, and no reuse between
  Steps, because isolation is the opposite of caching.
- **One RWO PVC per Run** — a detach-then-attach cycle at every boundary (tens of seconds
  each way on EBS-class storage), and on spot the reclaimed-node-holding-the-volume wedge is
  routine rather than exceptional. Only pays off under co-location, which is excluded.
- **RWX PVC per Run** — avoids the attach dance, but NFS-class storage is slowest at exactly
  our file shape (many small files: checkouts, `node_modules`), and it is a much heavier
  prerequisite than the driver.
- **PVC clone / VolumeSnapshot per Step** — genuinely good on copy-on-write storage (Ceph,
  local LVM/btrfs); unusable at Step grain on EBS, where a clone means snapshot-then-restore
  with lazy block hydration. Also dedups only down a lineage, where content addressing dedups
  across unrelated Runs.
- **git as the store, on a shared volume** — the right *idea* (git is a merkle store with a
  working tree) and the wrong tool: `git add -A` walks the whole tree, which is the cost the
  overlay diff exists to avoid; checkout is a full copy with no linking; git cannot represent
  an empty directory and does not preserve mtimes, which build tools use to decide what to
  rebuild (today's `tar` legs do preserve both); and concurrent pods contend on the index
  lock. Worth revisiting only for delta compression of large binaries.
- **Coarsening the unit (pod-per-pipeline or pod-per-stage)** — the GHA / Woodpecker / Tekton
  answer, and it genuinely makes intra-unit sharing free. Rejected because it trades away
  per-Step restart, per-Step placement ([0055](0055-placement-profiles.md)) and per-Step
  right-sizing — which on spot is a direct cost lever — to solve a problem that a lazy mount
  solves without giving anything up.
- **Node-local blob cache only (no service)** — attractive until Karpenter: consolidation and
  spot reclaim keep node lifetimes short and pods land on fresh nodes, so the within-Run hit
  rate approaches zero. As a second-level cache *behind* the service it was parked pending
  measurement; **s0 answers it: not yet, and not for this reason.** The measured cost is
  per-file round-trip count, and a cache is a second-order fix for that — it lowers the
  latency of a round-trip that lazy materialisation would not have made at all, and it only
  ever helps on a hit, which is precisely what Karpenter denies. Do the first-order work
  (lazy reads, then concurrency/batching in the CAS legs) and re-measure; a cache tier
  evaluated before that would be credited with wins that belong to the cheaper fix.
- **Fat helper with eager parallel fetch, no driver** — removes the `exec` tunnel and needs no
  privileges, so it is a legitimate stepping stone. Rejected as an endpoint: it still moves
  the whole workspace to every fresh node, i.e. it rebuilds the same complaint over a shorter
  wire.
- **Teaching authors to declare narrow `outputs:`** — the cheapest fix on paper and rejected
  on principle: it is substrate knowledge in the authoring model, and it assumes an awareness
  of our idiosyncrasies that we have decided to minimise.
- **Naming a specific object-storage product or zone topology** — an operator's cost decision,
  not an engine decision.

## Open — deliberately not decided here

- **Measurement — of the win, not of the premise. DONE (s0); it changed the slice order.**
  Two claims were kept separate on purpose: *"the `exec` tar path is the wrong design"* is
  structural and settled above on its properties, and *"it is the dominant cost today"* was
  empirical and unestablished. This ADR rests only on the first — which is just as well,
  **because the second turned out to be false.**

  `drive_workspace` now emits one `ws-timing` line per leg per Step boundary
  (`just logs | grep ws-timing`). Proc-mode stack, kind on colima, MinIO on loopback, two
  synthetic 3-step chains (`produce → consume → verify`) at a constant 8.19 MB workspace and
  two file counts, so per-file cost separates from per-byte cost:

  | leg | files | `exec` tunnel | server tar | **CAS (object storage)** | total | CAS share |
  |---|---|---|---|---|---|---|
  | feed  | 2000 | 1244 ms | 1102 ms | **11581 ms** | 13940 ms | **83%** |
  | drain | 2000 | 385 ms | 939 ms | **8867 ms** | 10213 ms | **87%** |
  | feed  | 250 | 421 ms | 94 ms | **2300 ms** | 2823 ms | **81%** |
  | drain | 250 | 105 ms | 156 ms | **1960 ms** | 2242 ms | **88%** |

  Three findings, in descending order of how much they matter:

  1. **The `exec` tar tunnel is 4–15% of a Step boundary. The CAS legs are 81–88%.** The
     component this ADR was written to delete is not the bottleneck. Deleting it (part 3) is
     still right — it is unresumable, it abuses the API server, and it blocks lazy reads — but
     on its own it buys roughly a tenth of the boundary.
  2. **The CAS cost tracks file count, not bytes.** At identical total bytes, an 8× file-count
     reduction cut `cas_ingest` 4.4× and `cas_materialize` 5.0×. Cause: `materialize` awaits a
     `get_tree` + `get_blob` per file and `ingest_dir` awaits a `head` + maybe-`put` per file,
     strictly one at a time. ~4–6 ms/file against **loopback** MinIO — so real object storage
     over a real network makes this *worse*, and the CAS share above is a floor, not a ceiling.
  3. **Content-addressed dedup currently saves nothing in time.** `consume` and `verify`
     re-drain byte-identical content the CAS already holds; their fully-deduped `cas_ingest`
     (9704 ms, 10096 ms) is *no faster than* the cold ingest that uploaded everything
     (8867 ms). `put_if_absent` pays a `head` round-trip per file either way, and the avoided
     `put` costs about the same as the `head`. Dedup saves storage, not wall-clock.

  **Consequence for sequencing.** Part 2 (**lazy materialisation**) is the load-bearing part,
  not part 3: it attacks file count, which finding 2 identifies as the actual cost driver.
  Whatever else the workspace service does, it must not reproduce a per-file sequential
  round-trip walk — concurrency and batching in the CAS legs are worth more today than the
  tunnel deletion, and are cheap enough to be worth doing even before the service exists.

  Caveats, stated so the numbers are not over-read: synthetic uniform-size files, not a real
  checkout; one machine, loopback object storage, no cross-AZ latency; and the measured binary
  also carried the in-progress s7 metadata-fidelity work, which adds per-file syscalls (not
  round-trips) to both CAS legs. None of that touches the ordering — the gap is 6–20×.
- **Concurrency in the CAS legs — s2, the slice s0's numbers created. DONE; it moved the
  bottleneck out of object storage entirely.** s0's own consequence paragraph names this:
  *"concurrency and batching in the CAS legs are worth more today than the tunnel deletion,
  and are cheap enough to be worth doing even before the service exists."* Both legs now run
  with bounded parallelism (`SCARAB_CAS_CONCURRENCY`, default 32), and both emit the byte/dedup
  counters s0 explicitly could not obtain from outside — `Cas::ingest` hashes and does its own
  `head`/`put` per blob *internally*, so a decorator could separate neither hashing from
  storage nor bytes-uploaded from bytes-deduped-away. Harvest everything with:

  ```
  just logs | grep ws-timing            # the whole Step boundary
  just logs | grep ws-timing | grep cas=   # just the two CAS legs
  ```

  **Re-measured on the stack, same shape as s0** — proc mode, kind on colima, MinIO on loopback,
  the same two `produce → consume → verify` chains at a constant 8.19 MB and the same two file
  counts. Each row is the mean over that run's legs of that kind (2–4 legs per row), with
  `SCARAB_CAS_CONCURRENCY` at its default 32:

  | leg | files | `exec` tunnel | server tar | **CAS** | total | CAS share | s0's CAS | CAS speed-up | boundary speed-up |
  |---|---|---|---|---|---|---|---|---|---|
  | feed  | 2000 | 539 ms | 4680 ms | **1866 ms** | 7095 ms | **26%** (was 83%) | 11581 ms | **6.2×** | 2.0× |
  | drain | 2000 | 362 ms | 878 ms | **5563 ms** | 6824 ms | **81%** (was 87%) | 8867 ms | **1.6×** | 1.5× |
  | feed  | 250 | 481 ms | 364 ms | **780 ms** | 1628 ms | **47%** (was 81%) | 2300 ms | **2.9×** | 1.7× |
  | drain | 250 | 155 ms | 130 ms | **971 ms** | 1278 ms | **75%** (was 88%) | 1960 ms | **2.0×** | 1.8× |

  Read the *shares*, not only the multipliers. **`materialize` is no longer the dominant cost of a
  feed leg** — 83% → 26% at 2000 files. `ingest` improved far less and is still 75–81% of a drain;
  the counters say exactly why, and it is not the object store (below).

  One number moved the *wrong* way and is reported because it changes an argument: **the feed
  leg's `tar_pack` rose from 1102 ms to ~4700 ms.** Nothing in that code changed. The server
  writes the whole workspace into a tmpdir and immediately tars it back out; when `materialize`
  took 11.6 s, page-cache writeback completed underneath it, and now that it takes 1.9 s,
  `pack_dir` pays that cost instead. So **the feed-leg boundary improved 2.0×, not 6.2×** — and
  that is a case for part 3 which s0's numbers did not contain: the tmpdir round-trip is a
  write-then-read of the entire workspace through the page cache, and its cost does not disappear
  when you make one half of it faster. It has to be *removed*, not accelerated.

  **Controlled A/B, in-process** — the two CAS legs alone, *not* a Step boundary, and labelled as
  such because a shared dev machine moves stack numbers by up to 6× (the first attempt at the
  table above, taken at load average 22, reported `cas_materialize` 12471 ms and `tar_pack`
  21838 ms; it was discarded). Same harness
  (`crates/scarab-storage-s3/tests/throughput.rs`, `SCARAB_BENCH_CAS=1`), same fixture shape,
  release build, real MinIO on loopback. **"Before" is the actual pre-s2 commit**, run from a
  detached worktree — not an inference from a knob:

  | leg | before (pre-s2 commit, serial) | after, c=1 | after, c=32 | speed-up |
  |---|---|---|---|---|
  | cold `ingest` | 12277 ms | 12094 ms | **4540 ms** | **2.7×** |
  | `materialize` | 10145 ms | 2958 ms | **1096 ms** | **9.3×** |

  The `c=1` column matching the pre-s2 commit to within 1% (`ingest`) is the check that this is
  one variable: at one in-flight request the new code *is* the old behaviour. `materialize` at
  `c=1` is already 3.4× faster because the syscall fix below is independent of concurrency.

  | scaling with the in-flight limit | c=1 | c=8 | c=32 | c=128 |
  |---|---|---|---|---|
  | cold `ingest` | 12094 ms | 4668 ms | 4540 ms | 3710 ms |
  | `materialize` | 2958 ms | 1587 ms | 1096 ms | 1129 ms |

  Both flatten by c=32, which is why that is the default: past it, the round-trips are already
  hidden behind the serial local-filesystem work described next, and the only thing a higher
  limit still buys is peak memory. A *remote* object store, with a real network in the path,
  would move that knee to the right — hence a knob and not a constant.

  **The counters relocated the bottleneck, and this is the finding that matters most.** With the
  round-trips overlapped, `store_ms` — a sum over concurrent requests, so it legitimately exceeds
  the leg's wall clock; the *ratio* is the reading — runs 15–90× the leg's duration. **The object
  store is now essentially hidden.** What is left is `fs_ms`: the local filesystem on the control
  plane. On the stack, 2000 files:

  ```
  cas="ingest"      files=2000 fs_ms=4965 store_ms=97388 hash_ms=378 total_ms=5660   # 88% local FS
  cas="materialize" files=2000 fs_ms=973  store_ms=30490 hash_ms=360 total_ms=1655   # 59% local FS
  ```

  **The CAS legs are no longer round-trip-bound.** Three consequences, and none of them is "add
  more concurrency":

  1. `materialize` was doing five syscalls and three path lookups per file — `fs::write`, then a
     *reopen* for `futimens`, then a path `chmod`. One open handle does all three, in the same
     write→mtime→mode order s7 requires. Fixed here: `fs_ms` 7261 → 870 ms at c=1 — and that is
     the whole of `materialize`'s 3.4× gain *before any concurrency*, which is why the write leg
     ends up 4× cheaper than the read leg despite moving the same bytes.
  2. What remains is `std::fs` called from inside the futures, and `buffer_unordered` polls every
     in-flight future from **one** task — so local I/O is serialised even though the network is
     not. Moving the read/write halves onto a blocking pool is the next win. Deliberately left
     out: it makes this adapter require a runtime to be present, a real coupling to decide on its
     own merits rather than as a side effect of a perf change.
  3. At 250 files, `hash_ms` is 340–348 ms of a ~500–970 ms leg — 36–67%. That is a *debug* build
     (`just up` builds debug; the release harness hashes the same 8.19 MB in ~30 ms), so it is an
     artefact, not a finding. It is recorded because it is the shape of the third bottleneck: once
     storage and the filesystem are dealt with, SHA-256 over the whole workspace is what is left,
     and that is a per-byte cost no concurrency removes — only not re-hashing unchanged content
     does, i.e. the overlay-diff drain already listed below.

  **s0's finding 3 survives, and the counters replace its explanation.** A fully-deduped drain
  still costs what a cold one does — 5660 ms vs 5637 ms at 2000 files, 937 ms vs 968 ms at 250 —
  but *not* for the reason s0 gave. It is no longer the `head`-per-file: with the round-trips
  overlapped those are nearly free (`objects_present=2040, bytes_put=5431`, and `store_ms` is
  hidden either way). It is that 88% of the leg is reading every file off local disk in order to
  hash it, and dedup cannot avoid a read whose redundancy it has not yet discovered. The only
  thing that fixes it is not walking unchanged content at all — the **overlay diff for the drain**
  below. Two independent measurements have now landed on that slice. (An in-process run *did*
  show a deduped ingest 4× faster; that harness re-ingests the same directory immediately, so it
  measures a hot page cache as much as dedup. The stack numbers are the ones to believe.)

  A measurement bug the new counters caught in the harness itself, recorded because it is the
  kind that silently produces a flattering number: the fixture salt did not include the pid, so
  a second invocation against the same bucket reported `objects_put=41 objects_present=2000` on
  what it called a *cold* ingest. Nothing outside the CAS could have seen that.

  **Cost of the concurrency, stated plainly.** A blob is still buffered whole, so peak memory is
  now roughly `concurrency × largest blob` where it used to be `1 × largest blob`. At the default
  32 that is nothing for the KB–MB files a checkout is made of and fatal for a workspace holding
  a 500 MB artefact. Mitigated today only by documentation and by `SCARAB_CAS_CONCURRENCY` being
  lowerable; the real fix is a byte budget beside the count budget, or streaming blob bodies
  instead of buffering them — the same primitive ADR-0029's deferred sub-file chunking needs.
- ~~**mtime fidelity across the CAS.**~~ **Answered (s7) — it did not, and now it does.**
  Measured, not reasoned about: the CAS tree entry carried only `name` and `target`, so a
  round-trip silently dropped every mode (an executable came back `0644`) and every mtime
  (reset to the moment of checkout), and `ingest` *failed outright* on a symlink to a
  directory. So cross-Step incremental compilation was already degraded today, exactly as
  suspected, independently of this ADR. Fixed rather than characterised: a tree entry now
  carries `mode` + `mtime_ms`, and a symlink is a blob holding the link target marked
  `MODE_SYMLINK` — git's layout. Blobs stay addressed by their bytes alone, so metadata costs
  no dedup; a *tree* hash does move with an mtime, which is correct, since two checkouts with
  different timestamps are different workspaces to every tool that compares them.
  `crates/scarab-storage-s3/tests/fidelity.rs` is the standing proof, and **s3 must keep it
  passing** — whatever replaces the `tar` legs inherits this contract.
- **Overlay diff for the drain** — hashing only the writable upper layer (so a Step never
  re-walks an unchanged tree) is the natural partner to lazy reads and needs the same
  privileged mount. Specified as a follow-up slice, not part of the first cut.

## References

- [0004](0004-execution-topology.md) — pod-per-step + content-addressed workspace; this ADR
  supersedes its **data path**, not its topology
- [0007](0007-data-passing-model.md) — Workspace / Result / Artifact / Cache
- [0027](0027-restart-semantics.md) — content-addressed invalidation; "smart never means mysterious"
- [0029](0029-workspace-cas.md) — per-file merkle CAS
- [0050](0050-retention-and-gc.md) — retention and GC
- [0055](0055-placement-profiles.md) — placement profiles
- [0056](0056-run-takes-and-attempt-grain-evidence.md) — attempt-grain evidence
