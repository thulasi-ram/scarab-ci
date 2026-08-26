# 0065. Retention, Cache, and re-derivation: keep less, recompute on request, and let the operator tune it

- **Status:** Accepted
- **Date:** 2026-07-28
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0007](0007-data-passing-model.md) (**implements its `Cache`**, and its 2026-07-26
  amendment stands), [0055](0055-placement-profiles.md) (the operator-owned-profile pattern this
  copies), [0050](0050-retention-and-gc.md), [0061](0061-workspace-data-path.md),
  [0027](0027-restart-semantics.md)

> **Amended 2026-08-24 by [0067](0067-the-pack-is-the-record.md) — the retention grain becomes the
> pack, not the object.** The *decided* shape: retention operates over the bucket plus a derived
> Postgres index (on any disagreement the bucket wins), drain-aligned pack boundaries mean most of
> a pack dies with its Run, and deletes are pointers-before-bytes behind a grace window longer
> than any in-flight read — no WAL. **Built so far (updated 2026-08-25, git-bug ad79c90): the
> packs, the index, and the reclaim of *never-finished* drains — stale staged rows and the
> rowless orphan bytes behind them (`reclaim_stale_staging_once` / `reclaim_orphan_packs_once`
> in `workspaced.rs`, pointers-before-bytes behind the grace cadence). Pack-grain expiry of
> COMMITTED packs — retention proper — is still not written**, gated on the ec294b7 cross-fence
> dedup question; until it is, committed packs are never deleted, which errs in the safe
> direction. The classes, the `RetentionProfile` knobs, and the Cache itself are untouched.
> The ec294b7 gate itself is answered as of 2026-08-26: fence-grain borrow edges
> (`depot_fence_borrows`) are recorded in every success record's transaction, so the expiry pass —
> the ticketed successor — deletes a fence only when no live borrower record pins it, and the pin
> wins across retention classes (a short-TTL pack held by a long-TTL borrower stays; the cost is
> attribution, not waste).

## Context

[0007](0007-data-passing-model.md) makes the Workspace **implicit-by-default**: a Step inherits the
merged workspace of its `needs`, and `inputs:`/`outputs:` are precision tools. Its 2026-07-26
amendment makes that permanent, on [0061](0061-workspace-data-path.md)'s governing principle —
*"minimise the substrate idiosyncrasies an author must know… where the substrate is expensive, the
system pays, not the author"* — and forbids `outputs:` ever becoming something an author declares
**for speed**.

That amendment priced wide edges as a **transfer** problem, which
[0062](0062-workspace-export-lazy-without-node-driver.md) then solved by making a Step move only what
it reads. It never priced them as a **retention** problem, and those are not the same thing.

The drain tars `/workspace`, ingests it, and prunes to declared `outputs:` **only if they were
declared**. So under the implicit default, every Step retains its **entire** workspace — `target/`,
`node_modules`, `.git`, temp files — per Step, per Run, for the retention window. Content addressing
dedupes identical files, but a build output churns by design, so dedup does not save it.

**What the classes cost is ecosystem-dependent, and this is estimated rather than measured.** Recorded
as an estimate on purpose: this ADR family has already paid for a probe read as answering a question
it had not asked, and the mirror-image error is an assumption that later reads as a measurement. A
deliberate decision was taken *not* to measure the dogfood repo, because a pre-release project
measuring one Rust repository would over-fit the design to a sample of one, and because the mix
differs per organisation — which is itself the argument for the knobs below.

| ecosystem | dominant class | biggest single item |
|---|---|---|
| JS/TS | dep tree | `node_modules` |
| JVM | dep tree | vendored `~/.m2` |
| Python | dep tree | `.venv` |
| Rust | **build output** | `target/` |
| any, long history | source | `.git` unless shallow |

Two facts from the code shape the decision more than the estimate does.

**`Cache` does not exist.** [0007](0007-data-passing-model.md) names it as the better-fitting concept
for `~/.cargo` and `node_modules` — no migration, no `cache_key`, nothing in the engine or the
pipeline crate. So today the *only* place a dependency tree or a build output can live is the
Workspace, which is why retaining workspaces is expensive.

**The derivation graph already exists.** `attempts.consumed` records, per Attempt, the map
`upstream step -> attempt id` it consumed. With the Run's DAG and the content identity, that is
enough to know what produced a given snapshot.

## Decision

**1. `Cache` becomes real, as an explicit, author-declared, keyed directory cache.**
An author names directories to cache and a key (a lockfile hash, typically). The cache is restored at
Step start and saved at Step end. Best-effort, evictable, **never evidence** — a miss is slower and
never wrong, which is what lets it sit outside the durability rules entirely.

This **implements [0007](0007-data-passing-model.md) as originally written** and reverses nothing: the
amendment forbids making Cache *mandatory for speed*, not making it available. 0007 already specified
it as keyed and author-declared.

> **Built 2026-08-26 (git-bug `dbe05e5`), and the durability question is DECIDED: warm-only**
> (owner decision, 2026-08-26). A Cache tree is drained **cache-only** — warm, evictable, never
> packed, never consuming pack-index rows or object storage for evictable content — and the
> `cache_entries` mapping row is **a hint, never a promise**. The mechanism: `cache: { dirs, key }`
> on a step; the control plane folds the key from the key files' **blob hashes** (already in the
> tree entries — no content reads) and mints the restore roots into the workspace token's existing
> exact-roots claim (zero new auth surface; keys never cross the trust boundary); the drain excludes
> cache dirs from the published root and reports each dir's subtree root on the `DrainRecord`, which
> the Depot verifies against the fence's **own write ledger** before accepting (a forged root 422s
> the whole drain). Everything else is best-effort: an unresolvable key, a 404 restore, a failed
> upsert are all a slower attempt, never a wrong one — asserted by the miss-never-wrong test.
>
> **The evidence bar that would reopen durable-drained caches:** measured miss-after-eviction cost.
> If `cache_entries` provenance shows the median warm lifetime of a saved tree is **shorter than the
> median key lifetime** (the lockfile-change interval), caches are dying before reuse, and the
> re-derive spend (miss-rate × cold-build minutes) can be priced against pack-row + object-storage
> cost. The `saved_at` refresh and the fetcher's hit/miss log lines built here are exactly that
> instrumentation.
>
> **Honesty at replicaCount > 1: cache hits assume `replicaCount = 1`.** Warm is replica-local and a
> restore's blob GETs scatter independently, so at N replicas the effective hit rate for a real
> directory is **~zero** (every GET must land on the holder: ~(1/N)^files) — each such restore
> degrades to the tolerated miss. Follow-up filed as git-bug `12e2f6b` (warmth-affinity restore
> routing OR durable cache copies, priced with this arithmetic).

**2. A Cache is another content-addressed tree**, so it rides
[0062](0062-workspace-export-lazy-without-node-driver.md)'s Snapshot Farm and Workspace Export
machinery. A Step that touches 5% of `node_modules` materialises 5% of it. Cache differs from a
Workspace Snapshot only in its **retention semantics**, not in its storage or its transport.

> **Amended 2026-08-03 by [0066](0066-the-depot-is-a-cache.md) — the statement STANDS; the rationale
> and the dependency do not.** A Cache remains a **content-addressed tree**, and that split is the
> right one for the reason git demonstrates: **git's object model is content-addressed and packfiles
> are a storage detail.** How bytes are laid out in a bucket says nothing about the logical model, so
> 0066's packing work (its point 7, deferred to its own ADR) applies **underneath** this decision
> without touching it.
>
> Two things in the paragraph above are now wrong:
>
> - **The justification.** *"A Step that touches 5% of `node_modules` materialises 5% of it"* is
>   **laziness**, which 0066 **cancels** on measurement (git-bug `4ce7f2c`: the feed is 0.54 ms/file
>   and 55% of it is local filesystem writes a lazy mount pays anyway). **Restate the rationale as
>   blob-granular dedup** — two Runs with an unchanged lockfile share every blob of the cached tree,
>   at file grain, with no key negotiation at all. That is a *better* justification than the one it
>   replaces, and unlike it, it is still true.
> - **The dependency.** The **Snapshot Farm and Workspace Export machinery** is being **deleted**
>   (0066 point 10, with three carve-outs). A Cache rides the **CAS and the eager feed**, like
>   everything else. Point 3's argument against shared mutable cache mounts is unaffected — it turns
>   on **concurrency**, not on materialisation cost — but its closing sentence, *"laziness gives
>   mount-like performance with none of the concurrency hazard"*, must be reread as the eager feed's
>   0.54 ms/file, which is the number that actually answers the "copying a 1 GB tree is brutal"
>   objection.
>
> **And point 8 gains a specific job.** The `RetentionProfile`'s **warm space budget** is exactly the
> knob 0066 point 4's layered warm eviction needs: a **size watermark**, never a TTL, because space is
> the bound an operator controls and the one already instrumented. 0066 also records that **nothing
> sweeps warm today at all**, so that knob has no effect until its arm of the sweeper exists.

**3. A keyed directory cache, not a shared mutable cache mount.** BuildKit-style
`--mount=type=cache` was considered and rejected on **concurrency**, not cost: a shared mutable
directory means two concurrent Runs both writing `~/.cargo/registry` or `node_modules`, which is
contention at best and corruption at worst. BuildKit serialises cache mounts for exactly this reason,
and serialising CI steps on a shared cache is an availability problem that grows with usage. The usual
argument for a mount — that copying a 1 GB tree per Step is brutal — is answered by (2): laziness
gives mount-like performance with none of the concurrency hazard.

**4. No automatic Cache detection.** Recognising `node_modules`, `.venv` or `~/.m2` from lockfiles and
routing them to Cache without the author asking was filed (git-bug `165c2dc`) and is **decided
against**. Its own acceptance criteria named the hazard that kills it: *"`target/` is Cache-shaped when
the next Step rebuilds and an **output** when the next Step consumes a built binary"* — the split is
behavioural, not nameable, and guessing "cache" on the second case breaks a pipeline silently.

The honest cost of this choice, booked rather than discovered: **an opt-in knob saves nothing for
authors who do not turn it on**, and the amendment says we will not teach "don't put your cache in the
workspace" as a rule. Naive pipelines keep paying for retention. That is accepted.

**5. Re-derivation is human-triggered, and it is the mechanism 0061 already specifies.**
[0061](0061-workspace-data-path.md) already says *"expired inputs widen a rerun's scope and say so,
rather than failing"*, with affordances that must state which they are — *"Rerun this step"* versus
*"Inputs expired — this re-runs from `clone`"*. Losing a snapshot to eviction or expiry is the same
event with a different cause, so it needs **no new Attempt outcome**. A `Corrupted` state was proposed
and is not needed: the Attempt genuinely succeeded, and what is unavailable is its *evidence*.

**Because a human is in the loop, the engine needs no notion of a Step being pure.** An earlier
draft of this reasoning required a purity property — safe-to-re-run versus effectful — to stop the
system re-running a `push` while recovering bytes. That requirement disappears once nothing re-runs
autonomously.

**6. The widened-rerun affordance must name the whole widened set, not its start point.**
The hazard survives the purity property's removal, in a different place: widening to `build` cascades
to descendants under [0027](0027-restart-semantics.md), so on `build → deploy-staging → push`,
retrying `push` with expired inputs drags `deploy-staging` with it and re-deploys staging as a side
effect of a *retry*. The answer is disclosure, not prevention: the affordance says "this re-runs from
`build`, which also re-runs `deploy-staging`", and the human decides. Their call to make, and they can
only make it if told.

**7. Checkout is not a special case.** It was assumed to be the one thing needing re-derivation, and
[0029](0029-workspace-cas.md)'s **per-file** merkle CAS says otherwise: consecutive commits share
every unchanged blob, so retaining source across a hundred Runs costs roughly one copy of the
repository plus the deltas, and Runs on the same SHA share it entirely.

| class | cheap to keep? | cheap to re-derive? |
|---|---|---|
| checkout / source | **yes** — file-level dedup across commits and Runs | yes — re-clone, SHA-pinned |
| build output | **no** — churns per build, dedupes poorly | **no** — minutes of compute |

So checkout is cheap on both axes and needs no special handling; build output is expensive on both and
is the only genuinely hard class. Declining to archive clone output stays available as a knob, with a
stated failure mode — a force-push or a deleted repository can remove the SHA, and then the Run can
never be re-run — but it is not the default.

**8. Knobs are operator-defined and author-selectable: a `RetentionProfile`.**
Because the right mix differs per organisation, retention is tunable — and tunable by the **operator**,
copying [0055](0055-placement-profiles.md)'s `PlacementProfile` exactly. An operator-owned,
cluster-scoped named bundle in gitops carries the warm space budget, the per-class TTLs, which
directories are Cache-eligible, and the drop-and-re-derive thresholds. A Pipeline may **name** a
profile; it never defines the values. No byte cost enters authored YAML, so the governing principle
holds. [0061](0061-workspace-data-path.md)'s manual **pin** ("keep this Run's workspaces") remains the
per-Run escape hatch for investigations.

> **Implementation note (2026-08-26, git-bug 6499fb1 + 82c5775):** `RetentionProfile` shipped
> **TTL-only** — name, `default`, and the four per-class TTLs (each falling back to the flat
> `SCARAB_RETENTION_*` env knobs); the warm space budget, Cache-eligible directories and
> drop-and-re-derive thresholds are deliberately **not parsed** until something consumes them (an
> inert knob is a silent lie), so the bundle grows with its consumers rather than ahead of them. The
> committed-pack expiry pass this ADR's retention grain feeds now **exists**
> (`crates/scarab-server/src/depot_expiry.rs`): it resolves the pipeline-named profile out of
> `runs.ir` against the *current* operator registry at sweep time, and the pin wins across classes.

## Alternatives considered

- **Flip the default: a fresh Workspace per Step, explicit outputs only, with a finite exclusion list
  for things like checkout.** The most legible model, saves the most storage, and it is what Concourse
  and Tekton do. **Rejected**: it makes every author learn what the substrate finds expensive, which is
  the exact tax [0007](0007-data-passing-model.md)'s amendment forbids and
  [0061](0061-workspace-data-path.md)'s governing principle exists to prevent; and it breaks every
  existing pipeline, including the live `.scarab/dogfood.yaml`. Recorded because it was the architect's
  first preference and because the case for it is real — the counter-argument is the principle, not the
  merits.
- **Automatic Cache detection.** See (4). Rejected on a silent failure mode.
- **Shared mutable cache mounts.** See (3). Rejected on concurrency.
- **Author-tunable retention values in pipeline YAML.** Maximum flexibility and the performance tax the
  amendment forbids. Rejected.
- **Operator-only retention with no author involvement at all.** Cleanest against the principle, and it
  leaves an author who genuinely needs a snapshot kept with no recourse. Rejected in favour of
  name-a-profile plus the existing manual pin.
- **A system that decides per edge whether to retain or re-derive, autonomously.** Attractive, and it
  is what "the system pays" would suggest. Rejected for now because it requires the purity property
  (5) discards, and because a wrong autonomous decision re-runs an effectful Step. A human asking for
  a widened rerun is the same capability with the risk placed where someone can consent to it.

## Consequences

- **`Cache` moves from a documented concept to a built one**: a `cache_key`, a store, a restore-and-save
  step in the Pod lifecycle, and eviction. It is the first data class that is explicitly *not*
  evidence, which is what keeps it outside [0064](0064-durability-tiering-and-the-write-path.md).
- **`RetentionProfile` is a second operator-owned profile type**, and two of them is the point at which
  the pattern should be shared rather than duplicated from [0055](0055-placement-profiles.md).
- **Retention becomes a thing an operator can get wrong** in a way that costs reruns rather than
  correctness. Graceful degradation is what makes that survivable, so the widened-rerun affordance is
  load-bearing product surface, not a nicety.
- **The implicit inherit-everything default survives**, so no authored pipeline changes and
  [0062](0062-workspace-export-lazy-without-node-driver.md) keeps its purpose. Had the default flipped,
  most of 0062 would have lost its reason to exist.
- **The estimate in Context is unmeasured and must be labelled as such wherever it is repeated.** The
  first number anyone measures should be allowed to overturn it — 0061's own central premise inverted
  under measurement, and that is the precedent, not the exception.

## References

- [0007](0007-data-passing-model.md) — Workspace / Result / Artifact / **Cache**; the amendment that stands
- [0027](0027-restart-semantics.md) — smart invalidation and the cascade this discloses
- [0029](0029-workspace-cas.md) — per-file merkle CAS; why source dedupes and `target/` does not
- [0050](0050-retention-and-gc.md) — retention classes and the sweeper
- [0055](0055-placement-profiles.md) — the operator-owned named-profile pattern `RetentionProfile` copies
- [0061](0061-workspace-data-path.md) — the governing principle, the retention table, the widened rerun
- [0062](0062-workspace-export-lazy-without-node-driver.md) — the Farm/Export machinery a Cache reuses
- [0064](0064-durability-tiering-and-the-write-path.md) — what must be durable before `Succeeded`
