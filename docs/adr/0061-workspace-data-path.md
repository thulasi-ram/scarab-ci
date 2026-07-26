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
per Step. [0029](0029-workspace-cas.md)'s dedup and incremental transfer are real, but they
apply only between the server and object storage, i.e. *after* the expensive part.

**This is unsound by inspection, independent of any benchmark.** A tar piped through an
`exec` stream is: a single sequential connection (no parallel ranges, no saturating a NIC);
unresumable (a break at 90% starts over); routed through the cluster's most contended
component, competing with the control traffic that keeps the cluster alive; funnelled through
one `scarab-server` process that must also hold and hash every byte; and it moves the *whole*
workspace in both directions regardless of what changed. Object storage clients have the
opposite properties on every line. No measurement is required to reject this path — only to
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
  rate approaches zero. Still viable *behind* the service as a second-level cache; parked
  pending measurement.
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

- **Measurement — of the win, not of the premise.** No numbers were taken. Two claims should
  not be confused: *"the `exec` tar path is the wrong design"* is structural and settled above
  on its properties; *"it is the dominant cost today"* is empirical and is **not** established.
  This ADR rests only on the first. Instrumenting one dogfood run — tunnel time vs hashing
  time vs object-storage time vs bytes per edge — is still worth doing before build, but to
  size the payoff, sequence the slices, and settle the parked question of whether a
  second-level node cache earns its place. It cannot rescue the current path.
- **mtime fidelity across the CAS.** The `tar` legs preserve mtimes and permissions; whether
  the CAS does is unverified. If it does not, incremental compilation across Steps is already
  degraded today, independently of this ADR.
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
