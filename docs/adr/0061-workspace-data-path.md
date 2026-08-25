# 0061. Workspace data path: workspace service + node driver, lazy materialisation

- **Status:** Accepted — **part 2 (the node driver) superseded by
  [0062](0062-workspace-export-lazy-without-node-driver.md)**
- **Date:** 2026-07-26
- **Deciders:** thulasi.ram (architect)

> **Read this first.** A constraint arrived after acceptance — *no DaemonSet from Scarab at all,
> privileged or not* — which forbids **part 2**'s node driver. The goal part 2 exists for (lazy
> reads, so a Step moves what it reads rather than what it inherits, with the author declaring
> nothing) is **not** withdrawn; 0062 reaches it a different way. Parts 1, 3 and 4 stand
> unchanged. Three claims below are amended in place and marked: part 2 itself, the "privileged
> node driver becomes an install prerequisite" consequence, and the "cross-AZ traffic is confined
> to the archive drain" consequence.

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

**One path is a direction-at-a-time claim, and both directions have now arrived.**
s3-feed converged the **read** path: every Step that inherits a workspace materialises it from
the service, in every deployment mode, with no second route. **s3-drain converged the write
path** (2026-08-01): the drain runs *inside the Pod* — the control plane execs `scarab-wsfetch
drain` in the egress helper, the helper ingests `/workspace` to the Depot warm-first
(`/have`-dedup), prunes to the authored `outputs:` and folds the content identity in-process,
and posts a **drain record** to the Depot as the rendezvous. The control plane reads the record
back, awaits the archival `flush`, and patches root/identity/durability onto the Pod — hashes
and a record, never bytes.

**2. A Scarab node driver, shipped and installed as standard.**
It mounts a Workspace Snapshot into a Step Pod as a **read-only lower layer** with a
**pod-local writable upper layer** on top. Materialisation is **lazy**: a Step transfers what
it reads, not what it inherits. This is what makes wide edges affordable without authors
declaring anything, and it is the only mechanism that does — intercepting reads requires a
privileged mount, which an unprivileged Pod cannot arrange for itself.

Service and driver are **one component in two halves** (server and client), not two concepts.

> **⚠️ Part 2 is SUPERSEDED by [0062](0062-workspace-export-lazy-without-node-driver.md).** A
> later deployment constraint — *no DaemonSet from Scarab at all* — forbids shipping a node
> driver, and the paragraph above then argued the goal was unreachable. **That argument is
> wrong, and its own last clause is why.** "An unprivileged Pod cannot arrange a privileged
> mount for itself" is true and irrelevant: in Kubernetes the privileged mounter is **kubelet**,
> which mounts on a Pod's behalf, so an unprivileged Pod can hold an intercepting filesystem at
> `/workspace` without any capability. 0062 keeps the goal — lazy reads, a read-only lower and a
> writable upper, nothing declared by the author — and moves the mechanism to a **Workspace
> Export**: `overlayfs` over a hardlink **Snapshot Farm** on the service's own disk, delivered
> to the Pod as a PersistentVolumeClaim. The lazy read is unchanged; the driver is gone. The
> "one component in two halves" framing survives with the client half now being kubelet's mount
> rather than our code.

**3. The control plane leaves the data path.** The server exchanges **root hashes** — tens of
bytes. Helper containers become Scarab-owned images rather than `busybox` doorstops. The
`kubectl exec` tar tunnel is deleted.

> **Status of part 3.** There were **three** `exec` tar tunnels on a Step boundary, not one.
> **s3-feed** deleted the first: a Scarab-owned fetcher image pulls the Step's inputs from the
> workspace service, and the feed-side tunnel and its `busybox` doorstop are gone. **s3-drain**
> (2026-08-01) deleted the second, and reshaped it rather than merely relocating it:
>
> - The egress barrier is the **wsfetch image** now — one knob with the fetcher
>   (`SCARAB_WSFETCH_IMAGE` / chart `workspace.fetcherImage`) — held open by `scarab-wsfetch
>   hold` and pinned `runAsUser: 0` (root-read is the privilege the drain always had: the
>   busybox barrier ran as root, and a `0600` file left by a `run_as_root` step must not
>   silently vanish from the snapshot).
> - The control plane execs `scarab-wsfetch drain --workspace /workspace --outputs <glob>…` in
>   that container — argv in, exit code out; the globs are transport, the Pod annotation is the
>   truth. The helper ingests warm-first with `/have` dedup, prunes and folds the content
>   identity in-process (no HTTP tree read-backs on the hot path), and POSTs its **DrainRecord**
>   to the Depot LAST — so a record's existence implies the ingest completed. The Depot
>   validates the record against the fence's write ledger and the closure's warm presence
>   before accepting it.
> - Classification is **record-first**: the record is the truth, the exec's exit code only a
>   hint when no record exists (a success record publishes; an `OutputContract` record fails
>   Config with the helper's detail; exit 11 with no record fails Config; 126/127/"executable
>   file not found" is a named image/control-plane **skew** failure, and so is **exit 0 with no
>   record** — a stale wsfetch has no subcommands, ignores the `drain` argv, runs fetch and
>   exits 0 having published nothing, while a current binary always POSTs its record before
>   exiting 0 — prompt and legible, never a wedge to the step budget; everything else re-drives
>   under the 5-minute escalation clock).
> - `ws-timing` moved to **v2**: the control plane keeps `exec_drain_ms` / `cold_flush_ms` /
>   `total_ms`; `files`, `tree_bytes`, `blobs_uploaded`, `bytes_uploaded`, `have_hits`,
>   `ingest_ms`, `prune_ms` come off the record, measured where the work now happens.
>   `tar_bytes` / `tar_unpack_ms` / `walk_ms` died with the tunnel.
>
> Still live, on purpose: the **artifact-harvest** tunnel (`harvest_artifacts` — the last
> `exec` tar, framed and fail-closed). Artifacts ride an independent store with an independent
> lifecycle; moving them onto the Depot is a filed follow-up ("artifacts ride the Depot"), not
> part of this slice.

**4. An Attempt is not `Succeeded` until its Workspace Snapshot is durable.** On spot, a node
can vanish between "the Step exited 0" and "its evidence is safe". Declaring success before
durability puts a claim in the durable record that the record cannot back — the one thing
this product may not do.

> **Amended by [0064](0064-durability-tiering-and-the-write-path.md) (2026-07-28) — conditional on
> the tiers a deployment actually has.** The rule above is unconditional, and the retention table
> below says warm "promises none", so an Attempt succeeding on warm alone contradicts it. 0064 allows
> exactly that **where no independent cold tier is configured** — and the test is `st_dev` of the cold
> directory against the warm one, because `StoreConfig::LocalDir` may point at the warm volume itself,
> in which case "written to cold" licenses nothing. A second PVC is a real cold tier; a `LocalDir`
> beside the CAS is not.
>
> **The invariant survives; the mechanism weakens.** This paragraph's own reasoning is that the
> product may not put a claim in the record that the record cannot back. Cold-before-`Succeeded` was a
> mechanism for that, not the point of it. A deployment that *declares* "durability here is warm's"
> and then succeeds on warm makes a smaller, true claim — so 0064 requires the degraded guarantee to be
> stated at startup **and stamped on the Attempt**, since a deployment's tiers change over time while
> its old records do not. What this paragraph was really protecting against is the **silent** version,
> and disclosure removes it.
>
> The related prohibition three paragraphs into "the price of seeding the warm tier" — that writing
> warm-first and letting the service tier onward makes warm load-bearing for durability — **still
> stands**. 0064 writes warm-first for *one batched flush* that is synchronous with respect to
> `Succeeded`; it does not tier onward asynchronously. Warm is the write path, cold remains the promise
> wherever it exists.

### Vocabulary

[CONTEXT.md](../../CONTEXT.md) previously used **Workspace** for two different things. They
are now distinct:

- **Workspace** — the *mutable* filesystem a Step executes in. Pod-local, dies with the Pod.
- **Workspace Snapshot** — the *immutable*, content-addressed tree that flows along a DAG
  edge and that an Attempt owns as evidence. Retained per policy, then archived, then gone.
  It has two coordinates (s8, below): a **snapshot root** — *where the bytes are*, the address
  a Step materialises — and a **content identity** — *what the bytes are*, the digest restart
  invalidation compares. Only the root is an address.

### Retention

Two tiers, two policies, so eviction can never break a promise:

| tier | bounded by | promise |
|---|---|---|
| workspace service (warm) | **space** — evict least-recently-used | none; a miss is slower, never wrong |
| object storage (cold) | **time** — retention TTL, as Artifacts already are | this is the guarantee users are given |

> **Amended 2026-07-28 by [0063](0063-step-logs-on-the-data-depot.md) and
> [0064](0064-durability-tiering-and-the-write-path.md).** Three corrections, and the first is the one
> that matters:
>
> - **"A miss is slower, never wrong" is true only of re-derivable content.** It holds for Workspace
>   Snapshots, because a miss refetches from cold or widens a rerun. It is **false for Step logs**,
>   which 0063 puts on this tier and which cannot be re-derived at all — re-running a Step produces
>   *different* logs. So logs carry their own promise on this tier: pinned against eviction until a
>   durable sink acknowledges them where one exists, and where none exists, evicted **loudly** so the
>   absence is visible in the record rather than looking like a Step that printed nothing.
> - **Warm is not always promise-free.** Where no independent cold tier is configured (0064's `st_dev`
>   test), warm is the only tier there is, and it carries whatever the operator's volume provides. The
>   guarantee is then stated and stamped rather than absent.
> - **"Workspace service" is now the Data Depot** (0063). It hosts the CAS, Snapshot Farms, Workspace
>   Exports, the log namespace, and Cache — so a tier table row named after one tenant no longer
>   describes it.

> **Amended 2026-08-03 by [0066](0066-the-depot-is-a-cache.md) — the LRU deferral is superseded, and
> the warm row's eviction policy changes shape.** This table's *"evict least-recently-used"* was
> written as an eventual nicety and deferred in implementation (the s5 space bound this ADR's own
> "GC deletes from cold and leaves warm" bullet calls *"the only thing that will ever reclaim it"*).
> **It is now the gating gap**, for two reasons this ADR could not see:
>
> - **Warm-only makes it load-bearing.** With no cold tier there is no second copy, so what warm
>   evicts is what a rerun loses — a bounded *recent window* rather than a latency event. An
>   unbounded, unswept warm tier in that mode is not a cache with a soft edge; it is a volume that
>   fills and then fails writes.
> - **Nothing sweeps warm today, at all.** 0066 traced the chain: warm-only skips the cold flush →
>   cold stays empty → [0050](0050-retention-and-gc.md)'s sweeper only deletes **unmarked cold
>   objects** → it deletes nothing, ever. The GC reports success and reclaims zero bytes.
>
> **And plain LRU is not the mechanism.** 0066 replaces it with a layered policy: evict **unreachable**
> content first (point the existing mark walk at warm — no new machinery), then, *only where a cold
> tier exists*, evict reachable-but-cold-backed content by recency; and in warm-only over the mark,
> **refuse writes loudly** rather than evict. Recency is the part deferred, and deliberately: for
> immutable content `list_objects` gives least-recently-**written**, which is meaningless, so true LRU
> needs an access index that is only worth building if reachability-first proves insufficient. The
> budget stays a **size watermark**, never a TTL — space is the bound an operator controls and the one
> already instrumented. The warm space budget itself lands as a
> [0065](0065-retention-cache-and-rederivation.md) `RetentionProfile` knob.

Plus a manual **pin** ("keep this Run's workspaces") for investigations, and **graceful
degradation**: expired inputs widen a rerun's scope and say so, rather than failing. Rerun
affordances must state which they are — "Rerun this step" vs "Inputs expired — this re-runs
from *clone*" — per [0027](0027-restart-semantics.md)'s rule that smart never means mysterious.

## Consequences

- **A third stateful component.** [0004](0004-execution-topology.md) called object storage
  "the second (and last)". That is now wrong, deliberately. Accepted because one standard
  path beats a fast path plus a fallback path.
- ~~**A privileged node driver becomes an install prerequisite**~~ — **withdrawn with part 2
  ([0062](0062-workspace-export-lazy-without-node-driver.md)).** It was booked here as an
  honest cost and it is now a forbidden one. 0062 replaces it with a privileged **workspace
  service** Pod — one operator-installed StatefulSet on the node Scarab's server already runs
  on, not a component on every node — and makes even that *preferred rather than required*, since
  the Farm degrades to a `reflink` or plain local copy without the capability. The comparison to
  Woodpecker's "only a default StorageClass" no longer applies in our favour or against us: what
  an operator must accept is one stateful component, which is what they were installing anyway.
- **The implicit inherit-everything default stays**, and `outputs:` ([0007](0007-data-passing-model.md))
  stays a *precision* tool — for cache keys, safe fan-out and restricting what flows — never
  a performance tax. Laziness plus content addressing is the automatic version of a narrow
  edge: a Step that reads 5% of a tree moves 5% of it, having declared nothing. Directly
  downstream of the governing principle.
- **"Don't put your cache in the workspace" is not a rule we will teach.** It is
  unenforceable and it is substrate knowledge. [0007](0007-data-passing-model.md)'s Cache
  concept remains the *better* tool; using it stays an optimisation, not a requirement.
- **The Helm-dogfood failure class goes — and the warm tier is not what removes it.** This
  bullet used to credit the wrong component, which matters because the mis-attribution
  contradicts this ADR's own retention table. The failure was real: `deploy/local-helm` had no
  object store, so the CAS fell back to a local directory on the server Pod's `scratch`
  **`emptyDir`**, every deploy rolled the Pod, and every rerun of an older Run hung at
  `Init:1/3` and dead-lettered. What fixes it is `deploy.sh` **deploying MinIO** and pointing
  the server at it: the durable copy lives in **cold**, and cold is the only tier that promises
  anything. The workspace service's PV cannot be the fix — the retention table above says it is
  bounded by space, evicts, and "promises none" — so a design that relied on it to prevent data
  loss would have rebuilt the same bug with a bigger disk. The service's volume is a cache, and
  a cache that survives a Pod roll is a latency win, not a durability one.

  > **Amended 2026-08-03 by [0066](0066-the-depot-is-a-cache.md) — this sentence was right, and it
  > was violated twice before anyone noticed it was an invariant.** *"The service's volume is a
  > cache"* is stated here as an observation in a bullet about a different bug. Two later changes
  > contradicted it: **`e58ce1f`** put **drain records** and **write ledgers** on the Depot — state
  > that exists on exactly one replica's disk and that a later request needs in order to succeed —
  > and **warm-only** ([0064](0064-durability-tiering-and-the-write-path.md) part 4) made the warm
  > tier the only copy of a Workspace Snapshot. Both made the Depot a **system of record** for
  > something.
  >
  > 0066 **promotes the sentence to the governing invariant** — *anything that makes the Depot a
  > system of record is a defect* — and repairs both violations rather than blessing them. The drain
  > record stays replica-local but its **absence becomes transient rather than `FatalConfig`**, so a
  > lost replica costs a re-drive instead of a permanently red build; the re-drive is idempotent by
  > construction (the drain reads a frozen `/workspace`, re-`PUT`s every tree unconditionally, and
  > `/have` reports everything missing on a fresh replica). Warm-only is not a violation once it is
  > read correctly: it does not make the Depot the system of record, it means there **is** no system
  > of record for workspaces in that deployment — disclosed per Attempt via
  > `attempts.output_durability`.
  >
  > The payoff for holding the invariant is why it is worth the repair: **HA becomes "run more
  > replicas"** (replicas hold nothing unique), **eviction is safe by construction**, and **spot
  > preemption costs a cold cache**. The accepted cost, documented rather than apologised for: **HA
  > requires object storage.**
- **Cross-AZ traffic is confined to the archive drain**, which can be throttled or scheduled.
  Step Pods talk only to their own zone's service. (In-region object storage is typically
  free per byte; EC2-to-EC2 across AZs is not. The cost was never "object storage" — it was
  the zone boundary.)

  **Amended by [0062](0062-workspace-export-lazy-without-node-driver.md) part 4: "confined" is
  too strong.** A Workspace Export lives on *one* replica's disk and the volume must be named in
  the Pod spec before the scheduler places anything, so "Step Pods talk only to their own zone's
  service" cannot be an invariant without hard-pinning every Step to a zone — which would fight
  the spot capacity this ADR names as the operating environment. 0062 makes zone affinity a
  **preference**: in-zone when capacity allows, cross-AZ when it does not, priced and never
  wrong. The sentence should be read as the intent, not the guarantee.
- **Object storage stays generic.** Scarab names no product and makes no single-zone /
  multi-zone / storage-class call. That is the operator's cost model, not ours.
- **Version skew** between server, service and driver becomes a supported concern.
- **Browse** ([0056](0056-run-takes-and-attempt-grain-evidence.md)) reads snapshots from the
  service **and falls through to object storage when the service cannot answer**, which keeps
  working after the Run's nodes are gone — on spot, that is within minutes, and a live-volume
  design would show a blank pane exactly when people look. The fall-through is the second half
  of the sentence and it is not a caveat: the control plane already holds object-store
  credentials, so going direct crosses no trust boundary and creates no second data path for
  Steps. It is the literal reading of "a warm miss is slower, never wrong", and it is the
  **opposite** of a Step Pod, which must fail closed because it deliberately has no credentials
  ([0042](0042-trusted-egress-sidecar.md)).

  For a while this bullet was false rather than merely incomplete: the composition root handed
  Browse the object store directly and never constructed a client of the service at all, so
  "reads snapshots from the service" described nothing that ran. It is true now — the control
  plane's workspace store is warm-then-cold with the service as the warm tier — which is also
  what makes `POST /v1/cas/have`, both `PUT` verbs and the `browse` token scope live code
  rather than test-only.
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

  > **Amended 2026-08-01 (s3-drain).** Every figure in this bullet and s2's — the 4–15% tunnel
  > share, the 81–88% CAS share — is **pre-0064 and pre-s3-drain**: measured on the shape where
  > the control plane unpacked the drain tar into a tempdir and ingested it itself, with cold in
  > the write path. Neither shape exists any more. The drain slice was accordingly **not
  > motivated by these latencies** but by the architecture (part 3: the control plane leaves the
  > data path — the prerequisite for 0062's Export) and by **control-plane RAM**: the old drain
  > buffered the entire workspace tar in one CP `Vec` and unpacked it into a CP tempdir per Step
  > boundary, a per-drain memory bill proportional to the largest workspace times drain
  > concurrency. ws-timing v2 keeps the leg observable, but no fresh latency claim is made here.
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

  **That last clause was half an answer, and s8 below is the other half.** "A tree hash moves
  with an mtime, which is correct" is true of an *address* and false of the question ADR-0027
  asks. s7 went further and argued the consequence was harmless — *"Cost **not** paid: nothing
  decision-relevant — restart skip-if-unchanged compares the recorded snapshot roots of
  upstream steps that did **not** re-run, which are byte-identical strings regardless."* That
  sentence excluded from consideration **exactly the case skip-if-unchanged exists for**: an
  ancestor that *did* re-run and produced the same output. The live kube tier then measured it
  (git-bug `945b1f4`): two Attempts writing byte-identical content produced roots differing in
  `mtime_ms` alone. Recorded here because the reasoning error is reusable — a cost argument
  that names the cases it does not pay for is not yet an argument.
- **Two digests, one address — s8. DONE, and it closes `945b1f4`.** A Workspace Snapshot now
  has two coordinates, and only one of them is an address:

  | | covers | is an address? | answers |
  |---|---|---|---|
  | **snapshot root** | names, targets, modes, **mtimes** | **yes** — `trees/<hash>` | where are these exact bytes? |
  | **content identity** | names, targets, modes | **no** | is this the same content? |

  The identity is the same merkle fold with every mtime dropped and each sub-tree named by
  *its* identity. It is folded up for free during `ingest` (one extra SHA-256 per directory,
  **zero** round-trips — nothing is stored under it), recorded beside the root as Attempt
  evidence, and it is what [0027](0027-restart-semantics.md)'s input signature compares.

  **Why not the three options the ticket sketched.** Each is a real position and each loses
  to a specific argument:

  - **Normalise mtimes on ingest** (to the epoch, as reproducible-build tooling does) —
    rejected as **unsafe**, not merely as a loss of s7's gain. mtime-based build tools are
    wrong in exactly one direction: a timestamp that is too *old* makes them skip a rebuild
    they needed. Constant mtimes make every inherited source look ancient, so a **Cache**
    ([0007](0007-data-passing-model.md)) restored beside the workspace with real timestamps
    looks newer than the sources it was built from, and `make`/`cargo` skip. Silently wrong
    output is a worse failure than a slow one.
  - **Drop mtime from the hash preimage and carry it as unhashed entry metadata** — the
    cheapest fix, and the tempting one, because the counter-argument to s7's objection nearly
    works: two trees sharing a content identity hold *identical content*, so serving either
    one's timestamps cannot mislead a tool comparing files *within* the tree. It fails because
    CI tools do not compare only within the tree. Under `put_if_absent` the **first**-stored
    hint wins, so a path whose content went X → Y → X (a revert, a flaky generator, two
    branches sharing a store) is served the *week-old* X timestamp, and a Cache built from Y
    yesterday then looks newer than the sources — the same wrong-output class as above. It
    also breaks the CAS's defining property: the key would no longer determine the content,
    so a tree could not be verified against its address at all (blobs are, today), and
    `materialize` would stop being a pure function of the root. **s7's objection to this
    option holds — but for a sharper reason than s7 gave.** Non-determinism of *timestamps*
    is not the problem; being served a *stale* one from a different lineage is.
  - **Keep mtimes and make ADR-0027's comparison structural (compare blob sets)** — this is
    what s8 does, made cheap. Comparing blob *sets* means walking two trees at admission
    time; a single derived digest answers the same question in a string compare, and is
    computed where the tree is already in hand.

  Also considered and rejected: **carrying mtimes forward from a baseline** — on ingest, reuse
  the previous snapshot's mtime for any path whose content is unchanged. This is the only
  scheme that keeps *one* digest and stays safe, and it is genuinely elegant: it makes a root
  reproducible *and* keeps every timestamp truthful about when content last changed. It loses
  on this ADR's own retention model. The baseline is a snapshot, snapshots expire (warm by
  space, cold by TTL), and when the baseline is gone the mtimes revert to wall clock and the
  root moves again. A determinism guarantee that decays with retention, silently, is the
  failure we are already fixing.

  **Consequences, stated plainly:**

  - **The identity is never an address.** Nothing is stored under it, `materialize` never sees
    it, GC's mark walk still starts from roots, `prune_tree` is unchanged. It is a label on
    evidence.
  - **Cross-run *tree* dedup stays lost** — two runs producing identical content still store
    two root objects, because their mtimes differ. Trees are small JSON, and s0/s2 measured
    that dedup buys storage rather than wall-clock, so this is priced and accepted.
  - **The tree preimage is untouched**, so no stored snapshot is orphaned. `tests/hashing.rs`
    still pins the same literals; the identity literals beside them were derived the same
    independent way.
  - **A snapshot recording no mtimes has an identity equal to its root** (dropping an absent
    field changes no bytes), which is why the fallback for a pre-s8 row — compare by root — is
    exact for those rows and merely conservative for the rest.
  - **One canonical form, one digest function.** Both were hand-copied in `scarab-storage-s3`
    *and* `scarab-workspace-client`, with a runtime tripwire in `tiered` to notice them
    drifting. They now live in `scarab-storage`; the tripwire is kept for version skew between
    deployed binaries, which is a different hazard.

    > **Amended 2026-08-01.** The `TieredCas` tripwire is **removed as unreachable**: both
    > hashes it compared came from one statically-linked `canonical_tree_bytes` executing in
    > one process — `WorkspaceClient::put_tree` canonicalises client-side — so no pair of real
    > tiers could ever disagree, and the "version skew between deployed binaries" it claimed to
    > guard was never in its comparison. The check now lives where the two binaries genuinely
    > meet: the Depot's `PUT /v1/cas/trees` re-serialises the parsed body through **its own**
    > linked `canonical_tree_bytes` and answers a distinct `400` ("canonicalisation skew") on
    > a byte difference — the client's serialiser versus the Depot's, which is the comparison
    > the old tripwire pretended to be. This makes the format's evolution rule load-bearing:
    > the tree format may only evolve by **additive `Option` fields** with
    > `#[serde(default, skip_serializing_if)]`, so parse → re-serialise stays byte-identical
    > across one version of skew. A non-additive change breaks mid-rollout PUTs **by design**
    > (fail-closed at the door, rather than one tree silently filed under two addresses).
- **The price of seeding the warm tier on the write path — booked, not measured.** Making the
  control plane's snapshot store warm-then-cold (D1.6) is what stops every freshly-drained
  snapshot being a guaranteed cold miss, and it is what makes half the wire protocol live code.
  It is not free, and the costs are recorded here rather than discovered later:

  > **Amended 2026-08-01 by [0064](0064-durability-tiering-and-the-write-path.md) (the
  > control-plane section).** This bullet's prohibition — point 1's "writing warm-first and
  > letting the service tier onward would make the warm tier load-bearing for durability,
  > which part 4 forbids" — is **superseded for the control plane**, whose write leg is now
  > warm-first too: the Depot's PUT handlers are **warm-only**, and durability is one explicit
  > `POST /v1/cas/flush` the control plane **awaits before `Succeeded`**. That is not the
  > "tier onward" this bullet forbade — the flush is synchronous with respect to success and
  > it is commanded by the caller, not left to the service — so the invariant the prohibition
  > protected (warm never silently licenses `Succeeded`) survives in the flush contract. The
  > original reasoning below is kept: it correctly priced the cold-first shape, and points 1–3
  > describe the shape this amendment retires (point 2's double cold write disappears with it;
  > the flush's existence probe is the `exists` primitive point 2 filed). The Depot being on
  > the durability path means a Depot outage **may fail Attempts — promptly and with a named
  > cause, never as a timeout**; 0064's control-plane section records that as the architect's
  > accepted trade.

  1. **The drain walks its directory twice.** `TieredCas::ingest` hashes to cold (the leg that
     licenses `Succeeded`) and then asks the warm tier to ingest the same directory. s2 measured
     the drain leg at **88% local filesystem** — reading every file in order to hash it — so
     this roughly doubles that leg. Both alternatives are worse: a merkle-level copy cannot be
     made concurrent inside a pure domain crate (no `futures` dependency), so it would be one
     sequential round-trip per file, which is precisely the pattern s0 identified as the
     dominant cost and this ADR forbids the new path from reproducing; and writing warm-first
     and letting the service tier onward would make the warm tier load-bearing for durability,
     which part 4 forbids. Removing the second walk needs a port change — a `Cas` that writes
     one walk to two backends, or an `ingest` that returns the tree it built so a tiered
     implementation could seed warm at merkle grain *with the adapter's concurrency*.
  2. **Genuinely new content is written to cold twice** — once by the drain's own cold leg, once
     by the service's cold-first `PUT`. Dedup bounds it: `POST /v1/cas/have` answers from warm,
     so a re-drain of unchanged content uploads nothing at all, and only new blobs pay. Neither
     `ObjectStore` nor the wire protocol has an **existence** primitive, so neither side can
     skip; adding `exists` to the port is the fix and is filed.
  3. **GC deletes from cold and leaves warm.** ADR-0050's sweep runs against the control plane's
     own object store, and the service exposes no delete verb, so a collected blob can survive
     in the warm tier as a phantom hit. Bounded and not a correctness bug — a phantom hit
     returns content that *was* real and is now unreferenced — but it does mean the warm tier's
     space bound (s5, deferred) is the only thing that will ever reclaim it.

  **None of the three is measured.** No `ws-timing` numbers were taken after this wiring; the
  ordering argument above is from s2's existing shares, not a new run.
- **Overlay diff for the drain** — hashing only the writable upper layer (so a Step never
  re-walks an unchanged tree) is the natural partner to lazy reads and needs the same
  privileged mount. Specified as a follow-up slice, not part of the first cut.

  **Answered by [0062](0062-workspace-export-lazy-without-node-driver.md) part 3, and the "same
  privileged mount" premise was wrong.** The upper layer does not have to be on the *node*. With
  the `overlayfs` mount on the workspace service — over a hardlink Snapshot Farm, verified on
  ext4 — the upper directory holds precisely the paths the Step touched, put there by the kernel,
  and the drain reads it locally with no network in the path. So the drain becomes **exact rather
  than approximate**, which also retires the hazard in the unprivileged approximation that was
  going to be built instead (a `(size, mtime)` stat cache, whose failure mode is silently
  publishing a stale hash on an mtime race). The stat cache remains the right drain for
  configurations with no Export — the local executor, and the lower rungs of 0062's privilege
  ladder — where it is a fallback and not the mechanism.

## References

- [0004](0004-execution-topology.md) — pod-per-step + content-addressed workspace; this ADR
  supersedes its **data path**, not its topology
- [0007](0007-data-passing-model.md) — Workspace / Result / Artifact / Cache
- [0027](0027-restart-semantics.md) — content-addressed invalidation; "smart never means mysterious"
- [0029](0029-workspace-cas.md) — per-file merkle CAS
- [0050](0050-retention-and-gc.md) — retention and GC
- [0055](0055-placement-profiles.md) — placement profiles
- [0056](0056-run-takes-and-attempt-grain-evidence.md) — attempt-grain evidence
