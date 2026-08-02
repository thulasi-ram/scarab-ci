# 0066. The Depot is a cache: HA, warm-only, and what each costs

- **Status:** Accepted
- **Date:** 2026-08-03
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0061](0061-workspace-data-path.md) (**amends its part 4 LRU deferral**, and restores
  its "the service's volume is a cache" invariant),
  [0062](0062-workspace-export-lazy-without-node-driver.md) (**cancels the lazy feed** and deletes the
  Export/Farm machinery), [0063](0063-step-logs-on-the-data-depot.md) (**amends where log bytes
  land**), [0064](0064-durability-tiering-and-the-write-path.md) (the tier stamp this reuses
  unchanged), [0065](0065-retention-cache-and-rederivation.md) (**amends its point 2's rationale**),
  [0051](0051-multi-replica-operation.md) (multi-replica — for the *control plane*; the Depot's story
  is new here), [0013](0013-history-and-observability.md) (**amends "bodies never"**),
  [0047](0047-retry-classification-and-attempt-model.md) (the failure classes the clocks split into)

## Context

Four things arrived between [0064](0064-durability-tiering-and-the-write-path.md) and this ADR, and
together they change what the Data Depot **is** rather than how one of its legs performs.

### The measurement inverted the remaining premise (git-bug `4ce7f2c`)

[0061](0061-workspace-data-path.md) s0 priced the feed as the expensive leg and part 2 — lazy
materialisation — as the load-bearing fix. [0062](0062-workspace-export-lazy-without-node-driver.md)
then spent an enormous amount of design on delivering laziness without a node driver. A benchmark of
the real `workspaced::router` over the real `WorkspaceClient` says the premise no longer holds:

| leg | per file | at 50k files |
|---|---|---|
| feed (`Cas::materialize` — what `scarab-wsfetch` calls) | **0.54 ms** | **~27 s** |
| cold drain | **2.68 ms** | **~134 s** |

The feed is roughly **10× cheaper** than 0061 s0 measured, and **the drain is now 5× the feed.** The
cheap leg got cheap enough that the expensive leg changed identity.

The composition of the feed finishes the argument. Of its 1076 ms (2000 files), only **434 ms is blob
GETs**; **55% is local filesystem writes** — and a lazy mount pays those writes anyway, on first read.
So laziness could at best defer a minority share of an already-cheap leg, in exchange for a
privileged mount, a per-node prerequisite, an NFS server and the machinery in
[0062](0062-workspace-export-lazy-without-node-driver.md).

**The lazy feed is therefore cancelled and the eager `scarab-wsfetch` init container stays.** That is
not a retreat to the status quo; it is the status quo winning on measurement, which is the precedent
0061 set when its own central premise inverted.

### Object storage is a soft, recommended requirement (git-bug `42d997c`)

With object storage Scarab is fully featured. Without it the Depot is a warm-only cache offering a
bounded **recent window**, with every degradation disclosed rather than silent — the mode
[0064](0064-durability-tiering-and-the-write-path.md) part 4 already supports and part 5 already
stamps (`attempts.output_durability` = `warm-only`).

Logs are the exception, and the exception has a principled shape. They are the only class in the data
plane with **no recompute path** ([0063](0063-step-logs-on-the-data-depot.md)); workspaces are
reproducible from the forge. So logs fall back to **compressed blobs in Postgres**, which is already a
mandatory dependency, rather than to a volume that dies with a Pod.

> **Guarantee what cannot be recreated; degrade what can.**

That single sentence is why logs and workspaces take opposite decisions in the same deployment.

### We are not resilient today (git-bug `e140121`)

Verified 2026-08-03: a **20–30 second Depot outage during Init dead-letters Runs.** The chain is
short and every link is real:

1. `scarab-wsfetch` uses a bare `reqwest::Client::new()` — **no retry, no timeout**
   (`crates/scarab-workspace-client/src/lib.rs:151`).
2. The fetcher runs as an init container with `restart_policy: Never`
   (`crates/scarab-executor-k8s/src/lib.rs:3782`).
3. Connection refused → exit 1 → `Infra { never_started: true }` → three auto attempts, back to back
   → **DeadLettered in seconds**.
4. Exit 1 (transient: the Depot is rolling) and exit 2 (a genuine 404: the snapshot is gone) map to
   the **same class**, so a permanent error and a two-second blip are indistinguishable downstream.

**This is the prerequisite for HA, not a consequence of it.** Running more replicas is meaningless
while a replica reschedule kills every Run in flight.

### The failure principle: block at Step boundaries, never mid-execution

**Every boundary operation is idempotent, and a Pod blocks at a boundary rather than failing.** What
makes this safe is a *structural* property rather than a policy: with an eager feed a Pod touches the
Depot exactly twice — at **Init** (fetch) and at the **end** (drain). A Depot outage in the middle of
a 40-minute build is **invisible to that build**.

That property was **bought by cancelling the lazy feed.** A lazily-mounted workspace makes every
running Pod continuously dependent on the Depot for the whole Step, and the failure mode is not a
clean error: `20e8786` measured nfs-ganesha's actual semantics, and a vanished server wedged a test
client **unkillably** in `nfs4_proc_destroy_session`. A design where a Depot restart can leave
uninterruptible processes on Step nodes cannot have a "block and retry" story at all.

Retry windows are sized as a **fraction of the Step timeout**, so the cost of waiting is proportional
to the work at risk rather than a fixed number someone has to defend.

**Waiting is not free, and this ADR does not pretend otherwise.** The argument for blocking is that
the alternative is worse at both boundaries, for different reasons: at **Init** nothing has been done
yet, so waiting costs only latency; at **drain** the work is *already done*, and discarding a
completed build's evidence because the storage service was rescheduled is the expensive mistake.

### The invariant was already written down, and violated twice

[0061](0061-workspace-data-path.md):261-262 says it plainly:

> The service's volume is a cache, and a cache that survives a Pod roll is a latency win, not a
> durability one.

Two changes since then made the Depot a **system of record** for something:

- **`e58ce1f`** added **drain records** and **write ledgers** — state that exists only on one
  replica's disk and that a later request needs in order to succeed.
- **warm-only** made the warm tier the only copy of a Workspace Snapshot.

Neither was wrong at the time and neither is being reverted. But an invariant that is stated in one
ADR and quietly contradicted by two changes is not an invariant — it is a comment. This ADR makes it
load-bearing again.

## Decision

### 1. The Depot is definitionally a cache. Anything that makes it a system of record is a defect.

This is not a preference or a design goal. It is **the invariant every other decision here is judged
against**, and the reason to state it that strongly is that it makes three separately hard problems
trivially correct at the same time:

| hard problem | why the cache invariant dissolves it |
|---|---|
| **HA** | "run more replicas". Replicas hold nothing unique, so there is no consensus, no replication protocol, no split-brain — losing one loses cached bytes |
| **Eviction** | safe **by construction**. There is nothing on the volume whose deletion can lose information |
| **Spot preemption** | costs a **cold cache**, which is a latency event, not a data-loss event |

Every one of those is a problem other systems solve with machinery. We solve them by refusing to put
anything irreplaceable on the Depot.

**The consequence is accepted and documented, not apologised for: HA requires object storage.** If
the only copy of a snapshot is one replica's disk, no amount of replica count buys availability for
it. That is a coherent position rather than a gap, because of the next paragraph.

**Warm-only does not mean "the Depot is the system of record".** It means there **is** no system of
record for workspaces in that deployment — which is a smaller, true claim, disclosed per Attempt
through [0064](0064-durability-tiering-and-the-write-path.md)'s existing
`attempts.output_durability` / `DurabilityTier`. No new vocabulary is needed; 0064 already built the
disclosure and this ADR simply declines to build a second one.

**So the two violations get repaired rather than blessed**: point 2 makes the drain record
self-healing, and point 4 makes eviction reachability-driven. After both, a replica's disk again holds
only things that can be recreated.

### 2. The drain record stays replica-local; its **absence** becomes transient, not `FatalConfig`.

**The bug today.** A replica dying between the Pod's `POST /v1/drains` and the control plane's `GET`
turns a **successful build** into a **permanent failure**. `classify_drain`
(`crates/scarab-executor-k8s/src/lib.rs:1913-1919`) maps a missing record to
`FailureClass::Config`, which is `allowed = 1` and **ignores the author's `retry:` policy**
([0047](0047-retry-classification-and-attempt-model.md)). The Step exited 0 and the Run is red because
a storage Pod was rescheduled.

The tempting fix is to make the record durable somewhere. Both durable options were rejected:

- **Postgres.** Breaks the Depot's **no-database** invariant — the Depot is the same binary in a
  different role precisely because it never connects to Postgres
  ([CONTEXT.md](../../CONTEXT.md) §4.4).
- **Object storage.** Absent in warm-only, which would couple a **core path** to the **optional**
  tier — exactly inverting `42d997c`'s decision.
- **[0063](0063-step-logs-on-the-data-depot.md) part 6's volume-identity marker**, extended to say
  *"this replica is not the one that took your drain"*. **Strictly better** than absence-as-transient,
  and **not worth it yet** — it can be added later without changing anything decided here, because it
  only ever converts a retry into a faster, more legible retry.

So the record stays replica-local and its absence is treated as **transient**. That is only sound if
re-driving the drain is genuinely idempotent, which was verified in the strong sense rather than
assumed:

| link | why a re-drive is safe |
|---|---|
| the drain reads `/workspace` **after** the step container exited | content is frozen; a second read sees the same bytes |
| a re-drive re-`PUT`s **every tree unconditionally** | no tree is assumed present from a prior run |
| `/have` reports **every blob missing** on a fresh replica | so the re-drive re-uploads exactly what that replica lacks |
| the **409** fires only for an existing **SUCCESS** record | a partial or absent record does not block the retry |
| once `ANNOTATION_WS_ROOT` is patched, the **`already` guard** skips the leg | so a *succeeded* drain is never redone |

**Accepted cost, booked explicitly:** a genuinely stale helper image — one whose drain will never
succeed — now takes **five minutes** to fail instead of failing instantly. That is the price of not
misclassifying a rescheduled replica as a configuration error, and it is the right way round: a slow
true negative beats a fast false one.

**Consequence: `WS_DRAIN_ESCALATION_MS` is promoted from an internal constant to a documented HA
parameter.** It is no longer an implementation detail; it is **the ceiling on how long a Depot outage
may stall a completed build before its evidence is abandoned**. Operators must size it against
realistic replica-reschedule time in their cluster, and it must be documented where they will look for
it — not left as a `const` someone discovers while debugging.

### 3. Fence affinity is a **correctness requirement**, not an optimisation, the moment N > 1.

This is the finding that decides the shape of Depot HA, and it is easy to mistake for a performance
note.

`CONCURRENCY = 16` (`crates/scarab-workspace-client/src/lib.rs:64`) means a drain opens up to **16
connections**, and kube-proxy load-balances **per connection**. So with two replicas behind one
ClusterIP, **one fence's blobs scatter across replicas.** `post_drain`'s closure validation then runs
against **one** replica's ledger and **one** replica's warm tier, finds most of the snapshot missing,
and fails.

**This is not a rare interleaving. It is every drain, as soon as you scale past one replica.**

**And the validation cannot be relaxed to make the problem go away** — the ledger check *is* the
cross-fence exfiltration fix from `e58ce1f`. Weakening it to tolerate scatter would restore the
security hole in order to fix an availability one.

**Mechanism: affinity by recorded choice, hashed by Run — not by fence.**

- The **control plane picks a replica at launch** and stamps its address into the Pod env, where
  `WorkspaceFetch.url` already goes.
- The control plane **reads that address back off the Pod spec** for the drain `GET` and for the
  flush. The **Pod spec is already the durable record that survives a control-plane restart** — this
  is the same pattern as `ANNOTATION_WS_OUTPUTS`, not a new one.
- The hash is over the **Run**, not the fence, so **every Step of a Run lands on one replica** and
  Step 2 finds Step 1's content **warm**. This matters more than it looks: `2e1a458` establishes that
  fan-in is a sequential replay of each parent's snapshot over one directory, so a Step with several
  `needs:` reads several parents' content — Run-grain affinity keeps all of it local, fence-grain
  affinity would not.

**No new column, no hash ring, no schema migration.** The choice is recorded where the Pod already
records everything else about its data path.

**Costs, both real:**

- **The Service must become headless** for per-pod DNS. And
  `deploy/helm/scarab/templates/service-workspace.yaml` currently carries the comment *"Deliberately
  NOT headless… nothing addresses an individual workspace replica"* — **that comment must change with
  the code.** A stale rationale left beside a reversed decision is worse than no comment, because the
  next reader trusts it.
- **`WorkspaceFetch.url` stops being one field stamped identically into every Pod.** It becomes a
  per-Run value, which is a small but genuine widening of what the launch path computes.

**Inert at N = 1**, which is every deployment today — so this is buildable and testable before anyone
scales.

### 4. Warm eviction: layered, reachability first, access index **deferred**.

**A correction to the record first, because deleting the wrong sentence would be worse than leaving
it.** Two comments look false in warm-only and are not:

- `WarmTier::evict`'s *"Safe by construction: cold still holds them"*
  (`crates/scarab-storage/src/tiered.rs:567-569`)
- `note_warm_write_failure`'s *"the snapshot IS durable"* (`crates/scarab-storage/src/tiered.rs:242`)

They are **incomplete, not wrong**: exactly true with object storage, wrong **only** in warm-only.
**Qualify them; do not delete them.** They are the clearest existing statement of why eviction is safe
in the normal case, and a reader who finds them deleted learns nothing.

With object storage the **pin set is empty** — the Depot is a pure cache and every byte is
recreatable. In warm-only, eviction is genuine **loss**, but loss that is **within contract** (the
Attempt is stamped `warm-only`), so pins protect only **in-flight** work rather than history.

**The policy is layered:**

1. **Evict unreachable content first.** Point `crates/scarab-server/src/retention.rs`'s **existing
   mark walk** at the warm tier. No new machinery, and it also fixes a finding that would otherwise
   sit undetected: **nothing sweeps warm today.** The chain is warm-only skips the cold flush → cold
   stays empty → the sweeper only deletes **unmarked cold objects** → it deletes **nothing, ever**.
   The GC is running, reporting success, and reclaiming zero bytes.
2. **If still over the high-water mark *and* a cold tier exists**, evict reachable-but-cold-backed
   content by **recency**. This is the step that needs an **access index**, because `list_objects`
   gives least-recently-**written**, which is **meaningless for immutable content** — a blob written
   once and read a thousand times looks identical to a blob written once and never touched. **This is
   the step we defer**, gated on measuring whether (1) suffices.
3. **In warm-only, over the mark: refuse writes, loudly.** There is no (2) available, and warm
   `ENOSPC` there means **the snapshot is nowhere**. A loud refusal at the boundary is the honest
   outcome; a silent partial write is not.

**The budget is a size watermark, not a TTL.** Space is what the operator actually controls, it is
what is already instrumented (the warm-size gauge walks the volume), and it is the only bound that
degrades predictably. A TTL bounds nothing when the fill rate changes.

**One product surface must change with this.** `ui/scarab-web-ui/src/snapshot-retention.ts:129-131`
promises that eviction *"only ever makes a rerun slower, never wrong"*. That is true with object
storage and false in warm-only — the same asymmetry [0064](0064-durability-tiering-and-the-write-path.md)
part 5 handles for durability. **It must become tier-aware, exactly like the durability stamp**, and
for the same reason: the same UI string cannot describe two deployments with different guarantees.

### 5. Deferred storage candidates, recorded so nobody re-evaluates them from scratch.

Four systems were considered. Recording the verdicts *and their triggers* is the point — an
undocumented "we looked at it" costs the next person the whole evaluation again.

| candidate | verdict | trigger to revisit |
|---|---|---|
| **SeaweedFS** | **Supported cold-tier option now** | none — it works today |
| **JuiceFS** | **Deferred**; the named replacement if laziness revives | read-fraction measurement, or workspaces large enough that 27 s matters |
| **Dragonfly** | **Deferred** | the N-concurrent-fetch measurement |
| **Kraken** | **Rejected** | — |

**SeaweedFS is supported now, with zero code change** — it is S3-compatible, so it is a `StoreConfig`
value. Its Haystack-style needle layout suits **millions of small blobs**, which is precisely our
shape, and it has real replication and erasure coding. **Document it beside MinIO and Ceph RGW, and
label it explicitly untested-by-us** — "compatible" is a protocol claim, not an endorsement we have
earned.

**JuiceFS is deferred and named**, and naming it is the whole value: **if laziness ever revives, the
answer is JuiceFS plus its CSI driver, not a rebuild of the ganesha path.** It needs the privileged
CSI DaemonSet that [0062](0062-workspace-export-lazy-without-node-driver.md) already priced out, so
the cost is known rather than hypothetical.

**Dragonfly is deferred because it answers a question we have not asked.** It solves **origin
saturation under concurrent fan-out** — many Pods pulling the same content at once. Our benchmark
(`4ce7f2c`) was **single-client**, so saturation is **unmeasured**. A P2P mesh bought on an unmeasured
premise is exactly the mistake 0061 s0 made in the other direction.

**Kraken is rejected**: archived upstream, so it fails the maintained-library rule. *(Moderate
confidence — verify the archival before citing this at anyone.)*

**The framing that makes all four one decision rather than four:** every one of them optimises **the
feed**, which the measurement just made the **smaller** leg, along the **concurrency** axis rather
than the **per-file** axis we actually measured. So **the gate is one experiment, not four
evaluations**: measure N concurrent fetches of the same snapshot. And if saturation *is* found, reach
first for an **unprivileged per-node Depot cache DaemonSet** (`wsfetch` hits `$NODE_IP`) before a P2P
mesh — same benefit, an order of magnitude less machinery.

### 6. Per-object **request cost** is unmeasured and may favour a self-hosted cold tier independently of latency.

One request per file has a bill as well as a latency:

| operation | requests | approximate S3 cost |
|---|---|---|
| 50k-file cold drain, all new | ~50k PUTs | **~$0.25** |
| 50k-file cold drain, fully deduped | ~50k HEADs | **~$0.02** |

*(Pricing from memory — flag as needing verification before it is quoted at anyone.)*

The reason this belongs in the same ADR as the latency argument: **it scales on the same axis** — file
count. Anything that reduces the number of objects reduces both. That is the bridge to point 7.

### 7. Packing: promoted from deferred to **the next ADR**, gated on measuring the cold flush.

**There is no batch PUT in S3.** `DeleteObjects` batches deletes only; S3 Batch Operations is a
managed job over a pre-existing inventory, not a way to write 50k new objects in one call. So **the
only lever is packing** many blobs into one object with an index.

**The key insight — and the reason this is now tractable when it was not before:**

> **Laziness was the only reason to address blobs individually.**

With eager materialisation, every read is *"give me this whole snapshot"*. `/flat` already returns the
**entire manifest in one call**, and we then make 50k requests to fetch what that one response
describes. So packing's main downside — losing random access to an individual blob — is a cost we **no
longer pay for anything**. And it helps **both** legs, feed and drain, which nothing else on this list
does.

**Critical gap, and the reason this is gated rather than decided: the cold flush is unmeasured.**
`flush_to_cold` (Depot → object storage) was **not** covered by `4ce7f2c`, which measured
client → Depot **over loopback**. The flush **gates `Succeeded`**
([0064](0064-durability-tiering-and-the-write-path.md) part 2) and runs against **real S3**, not
loopback. **Measure it first, as a sizing input** — pack size, index granularity and whether packing
is worth its costs all depend on numbers nobody has.

**Packing's real costs, to be carried explicitly by that ADR rather than discovered during it:**

- **(a) A durable global `hash → (pack, offset, len)` index**, realistically **Postgres**. This loses
  the property that **the bucket is self-describing** — that a fresh Depot pointed at an existing
  bucket just works. That property is currently free and is worth naming before it is spent.
- **(b) GC becomes repacking.** A 5000-blob pack with 4000 dead blobs **cannot be partially deleted**;
  reclaiming space means writing a new pack and **atomically swapping the index** against concurrent
  readers.
- **(c) Append-only immutability is lost.** Repacking rewrites, and a bug in the rewrite **loses
  blobs** — a failure class the current store structurally cannot have.
- **(d) Read efficiency depends on locality.** Without **reachability-order repacking** (what
  `git gc` does), a snapshot sharing 90% of its content with another needs blobs scattered across
  thousands of packs, and the win evaporates.
- **(e) The bucket becomes opaque to `aws s3 ls`.** Operators lose the ability to look.

**What it gives back, beyond the request-count win: the presence index becomes free.** A local index
lookup replaces the network `HEAD`, which **subsumes** the near-term cheap win and also addresses
`1d4b3ce`'s serial per-blob `warm_has` walk on the drain's critical path.

**Why it needs its own ADR with a migration story, rather than being decided here:** it is a change to
the **frozen canonical on-disk form**, and it carries a **cross-binary skew tripwire** — a Depot and a
control plane at different versions must not disagree about how bytes are addressed.

### 8. [0065](0065-retention-cache-and-rederivation.md)'s Cache stays a content-addressed **tree**; packing applies underneath it.

The logical/physical split is the right one, and git is the precedent: **git's object model is
content-addressed and packfiles are a storage detail.** A Cache being a content-addressed tree says
nothing about how the bytes are laid out in a bucket.

So **0065 point 2's statement survives; its justification does not.** It was justified as *"a Step
that touches 5% of `node_modules` materialises 5% of it"* — that is **laziness**, now cancelled — and
it lists the **Snapshot Farm / Workspace Export machinery** as its dependency, which point 10 deletes.

**Restate the rationale as blob-granular dedup**, which is both a **better** justification and still
true: two Runs with an unchanged lockfile share every blob of the cached tree, at file grain, with no
key negotiation at all.

### 9. Split the clocks — the **provisioning deadline** is separate from the **execution timeout**.

**This is a live bug, independent of everything else in this ADR.** The engine backstop is anchored at
**Attempt claim** time (`crates/scarab-engine/src/scheduler.rs:1645-1652`), which is **before
dispatch**. Therefore:

> **Queueing delay already bills the Step's timeout.**

On a busy cluster, a Step that waits ten minutes for a slot starts with **ten minutes less** than its
author asked for. When it overruns, it surfaces as `Timeout` with **no hint that waiting caused it** —
the author reads it as their code being slow and raises a limit that was never the problem.

**Principle: waiting for infrastructure must not bill the Step's execution budget.**

**Design, verified cheap:**

- A new **nullable** `attempts.execution_started_at`, stamped by the scheduler **the first time
  `poll` reports the step container running and the column is `NULL`**.
- **No executor API change** — the executor **already** tracks the step container specifically, in
  order to detect its termination for the drain. The signal exists; it is simply not recorded.
- The **engine backstop re-anchors** to `execution_started_at`, so the author's `timeout:` measures
  execution.
- A new **provisioning deadline** (generous default) covers **scheduling, image pull and the feed**,
  enforced as *"`execution_started_at IS NULL` and `now > started_at + provisioning_deadline`"*, and
  failing with **its own legible class** rather than as `Timeout`
  ([0047](0047-retry-classification-and-attempt-model.md)).
- kubelet's `activeDeadlineSeconds` becomes **`provisioning_deadline + timeout`** — the **outer**
  backstop — because a **pod-level** deadline cannot be split into two phases.

**This fixes three things at once**, which is why it is worth doing as one change: the Depot-outage
case (a blocked Init no longer eats the build's budget), the queueing case, and the *"failed as
`Timeout` having never run"* misclassification.

**The Run budget is left alone**, deliberately — it is a wall-clock spend limit on a whole Run, and
splitting *that* clock is a different question with a different owner.

### 10. Delete the Export / Farm / change-set machinery — with three carve-outs.

Roughly **10k lines** across `crates/scarab-server/src/export.rs`, `farm.rs`, `changeset.rs` and the
Export half of `settle.rs`. **No control-plane or executor caller exists** — established by an
HTTP-route grep, which is the form of search that **cannot** miss trait-object dispatch (the routes
are the entry points; a `&dyn` call cannot conjure one).

**Dormant code is not free**, and the three costs are concrete rather than aesthetic:

- **Live HTTP routes are attack surface.** We Browse-gated `/v1/exports*` last week precisely because
  **any fence token could `DELETE` another fence's Export**. Unused code with a live route is not
  unused.
- **Every refactor pays to keep it compiling** — a tax levied on work that has nothing to do with it.
- **It invites someone to wire it up** without re-reading why it was shelved. Deleted code makes them
  read the ADR; dormant code lets them skip it.

**It is in git, and this ADR is the record of what it was.** That is the whole argument for deleting
rather than keeping.

**Three carve-outs, in order:**

- **(a) Extract the fence-residue sweep from `sweep_exports_once` *first*.** The **ledger** and
  **drain-record** TTLs live inside it and are **load-bearing** — point 2 depends on drain records
  ageing out. Deleting the sweep with its host file would silently remove a cleanup nobody is
  watching.
- **(b) Keep `changeset.rs` and its test tier.** It is the **only code anywhere** that reads an
  `overlayfs` upper layer, it was **finally executed on 2026-08-02**, and it is **exactly** what a
  revived laziness effort would want. Deleting a just-proven capability to save a file is a bad trade.
- **(c) Keep the reflink primitive** (now `reflink-copy`). A future **local** cache — the per-node
  Depot cache of point 5, or a warm-tier snapshot fast path — wants it, and it is small.

**Name the revival path so nobody rebuilds ganesha: JuiceFS plus its CSI driver** (point 5). What
survives from [0062](0062-workspace-export-lazy-without-node-driver.md) is its **substrate facts**
(measured, still true, and the reason the revival path is a known quantity) and the **change-set
tier**.

**Not affected:** the **artifact-harvest** exec-tar leg (`0d3666a`) is a separate path with a separate
reason to exist — the artifact `ObjectStore` is control-plane-side with its own credentials, so the
framing machinery survives *for artifacts only*. Deleting Export does not touch it.

## Alternatives considered

- **Make the Depot a replicated system of record** (Raft over the drain records and write ledgers, or
  a shared Postgres). The obvious HA answer, and it is what a storage service usually does. **Rejected
  on the invariant**: the moment a replica holds something unique, HA needs consensus, eviction needs
  a durability check, and spot preemption becomes a data-loss event. Every one of those problems is
  bought back by the very thing that was supposed to solve them. Recorded because it was yesterday's
  answer — shard-by-fence with a drain protocol (`6cb4a27`) — and cancelling the lazy feed is what
  made the simpler shape available.
- **Put the drain record in Postgres.** One table, and the problem disappears. **Rejected**: it breaks
  the Depot's no-database property, which is the thing that lets the Depot be deployed, scaled and
  lost independently of the durable core.
- **Put the drain record in object storage.** No new dependency where object storage exists.
  **Rejected**: it is absent in warm-only, so a **core** path would depend on the **optional** tier,
  inverting `42d997c`.
- **Relax `post_drain`'s closure validation so scattered blobs stop failing it.** The one-line fix for
  point 3. **Rejected outright**: the ledger check *is* `e58ce1f`'s cross-fence exfiltration fix.
  Trading a security property for an availability one is never the trade it looks like.
- **A hash ring or a new `attempts` column for replica choice.** More principled-looking than reading
  the Pod spec. **Rejected as unnecessary**: the Pod spec is *already* the durable record that
  survives a control-plane restart, and `ANNOTATION_WS_OUTPUTS` is the precedent. A schema migration
  to store something already stored is cost without benefit.
- **Keep pursuing laziness with a different mount technology.** `4ce7f2c` says the feed is 0.54
  ms/file and 55% of it is local writes a lazy mount pays anyway. **Rejected on measurement** — the
  same way 0061's own premise was overturned, which is the precedent and not the exception.
- **Keep the Export/Farm code dormant "in case".** Free-looking. **Rejected** on live routes as attack
  surface, refactor tax, and the invitation to re-enable it without reading why it was shelved.
- **Evict warm by TTL rather than by a size watermark.** Simpler to reason about per object.
  **Rejected**: the operator's actual constraint is **space**, a TTL bounds nothing when fill rate
  changes, and space is the bound already instrumented.
- **Build the access index now** rather than deferring it. It is the honest LRU. **Deferred** rather
  than rejected: reachability-first eviction may well suffice, and building an access-tracking write
  path for immutable content before knowing that is machinery bought on a guess.
- **Fix the queueing-eats-the-timeout bug by simply raising default timeouts.** Zero code.
  **Rejected**: it hides a misattribution rather than correcting it, and the failure still reports
  `Timeout` for a Step that never ran.

## Consequences

- **"Is this a cache?" becomes the review question for any change to the Depot.** A change that puts
  something on the Depot's volume that cannot be recreated is a defect by this ADR, regardless of how
  convenient it is.
- **HA requires object storage, and that is now a documented product statement**, not an inference an
  operator has to make. Warm-only remains supported, with a bounded recent window and a per-Attempt
  stamp (`42d997c`, [0064](0064-durability-tiering-and-the-write-path.md) part 5).
- **`WS_DRAIN_ESCALATION_MS` becomes operator-facing documentation** with a sizing rule, and a stale
  helper image now fails slowly. Both are new things to explain in the operations docs.
- **The workspace Service becomes headless**, and
  `deploy/helm/scarab/templates/service-workspace.yaml`'s "Deliberately NOT headless" comment must be
  rewritten **in the same change** as the code.
- **`WorkspaceFetch.url` becomes per-Run.** Anything that assumed one Depot URL for the whole
  deployment — tooling, docs, tests — has to stop.
- **The GC gains a warm arm**, and the discovery that **nothing has been sweeping warm at all** means
  existing deployments have unbounded warm growth today. That is a live operational finding, not just
  a design gap.
- **`snapshot-retention.ts`'s "slower, never wrong" copy becomes tier-aware**, joining the durability
  stamp as UI that must know which deployment it is describing.
- **A new nullable column `attempts.execution_started_at`**, a new provisioning-deadline failure class,
  and a changed meaning for the author's `timeout:` — which is a *user-visible semantic change*, in
  the user's favour, and must be released as one.
- **~10k lines are deleted**, and the fence-residue sweep must be extracted **before** the deletion,
  not during it.
- **The next ADR is packing**, and it is blocked on a measurement (`flush_to_cold` against real S3)
  rather than on a decision. Filing it without that number would repeat 0061 s0's error.
- **Three measurements are now owed**, and each one gates a specific deferred decision — which is the
  point of recording them together: the **cold flush** (gates packing), **N concurrent fetches**
  (gates Dragonfly / a per-node cache), and the **read fraction** (gates any laziness revival).

## References

- [0013](0013-history-and-observability.md) — the log pipeline; **amended** here for the Postgres
  fallback
- [0047](0047-retry-classification-and-attempt-model.md) — failure classes; why `FailureClass::Config`
  ignoring `retry:` is the sharp edge in point 2, and where the provisioning-deadline class lands
- [0050](0050-retention-and-gc.md) — the sweeper whose mark walk point 4 points at warm
- [0051](0051-multi-replica-operation.md) — multi-replica for the **control plane**; the Depot's story
  is this ADR
- [0061](0061-workspace-data-path.md) — the cache invariant this restores; **amends** its part 4 LRU
  deferral
- [0062](0062-workspace-export-lazy-without-node-driver.md) — the lazy Export; **cancelled** here on
  measurement, with three carve-outs
- [0063](0063-step-logs-on-the-data-depot.md) — the Data Depot; its part 5 (absence is authoritative)
  and part 6 (volume-identity marker) are **reused verbatim** here
- [0064](0064-durability-tiering-and-the-write-path.md) — `DurabilityTier` / `output_durability`, the
  disclosure mechanism this ADR reuses instead of inventing a second one
- [0065](0065-retention-cache-and-rederivation.md) — Cache and `RetentionProfile`; **amends** point 2's
  rationale and supplies the warm space budget point 4 needs
- git-bug `4ce7f2c` — the measurement that cancelled laziness
- git-bug `e140121` — we are not resilient today; the prerequisite for HA
- git-bug `20e8786` — measured ganesha semantics; why a continuously-dependent Pod has no block-and-retry story
- git-bug `42d997c` — the warm-only matrix; object storage as a soft, recommended requirement
- git-bug `6cb4a27` — Depot HA; yesterday's shard-by-fence answer and why it simplified
- git-bug `974440b` — logs swallow errors in every deployment, not just warm-only
- git-bug `2e1a458` — fan-in is a sequential per-path union; why affinity hashes by Run
- git-bug `1d4b3ce` — the drain's serial closure validation; what packing's free presence index subsumes
- git-bug `0d3666a` — artifact harvest stays on its exec-tar leg; unaffected by the deletion
- git-bug `16a7768` — the in-Pod drain's resource story
