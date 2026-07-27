# 0062. Workspace export: lazy materialisation without a node driver

- **Status:** Accepted
- **Date:** 2026-07-28
- **Deciders:** thulasi.ram (architect)

## Context

[0061](0061-workspace-data-path.md) decided a four-part workspace data path. Part 2 was **a
Scarab node driver, shipped and installed as standard**, which would mount a Workspace Snapshot
as a read-only lower layer with a pod-local writable upper layer, materialising **lazily** so a
Step transfers what it reads rather than what it inherits. Part 2 is the part 0061's own
measurement section identifies as load-bearing:

> **Consequence for sequencing.** Part 2 (**lazy materialisation**) is the load-bearing part,
> not part 3.

A deployment constraint arrived after 0061 was accepted and it forbids the mechanism:

> **No DaemonSet from Scarab at all — privileged or not.** The only nodes Scarab may touch are
> the node its server runs on and the nodes its Step Pods run on.

The constraint protects against a **cluster-wide footprint that Scarab installs and versions**:
something present on every node, outliving every Run, that a platform team must accept, upgrade
and trust. It is not a blanket prohibition on privilege, and it is not a prohibition on touching
a node a Step Pod is already running on.

0061 did not test part 2 against this, because it asserted the problem away:

> …it is the only mechanism that does — intercepting reads requires a privileged mount, which an
> unprivileged Pod cannot arrange for itself.

Both halves of that sentence are true and the conclusion drawn from them is not, because **the
Pod was never going to be the thing arranging the mount.** In Kubernetes the privileged mounter
is **kubelet**: a Pod *declares* a volume and kubelet performs the mount on its behalf. So an
entirely unprivileged Pod can have an exotic filesystem at `/workspace` without holding any
capability at all. 0061's ten "Alternatives considered" are all about transport and topology and
none of them asks where the mount could come from instead.

### The complete space, which is what makes this decidable

**On a cold Pod, moving less than the whole workspace requires either interception or
foreknowledge. There is no third source of laziness.** Foreknowledge means inferring a Step's
read set, which fails silently when wrong (a missing file makes a test skip, not fail). That
leaves interception, and interception has exactly three possible owners:

| owner | verdict |
|---|---|
| **a privileged mount inside the Step Pod** (FUSE, `fuse-overlayfs`) | **Dead.** Measured: rejected by PodSecurity at **both** `baseline` and `restricted`. |
| **kubelet**, mounting a network filesystem | **This ADR.** kubelet holds the privilege; the Pod holds none. |
| **the container runtime** (lazy snapshotters — stargz, nydus) | Not ours to install or version; a platform team adopts it on its own schedule. |

That table is the decision. Everything below is the consequence of picking row two.

### Measured facts

All on `colima`, k3s `v1.35.0+k3s1`, runtime `docker://29.5.2`, node filesystem **ext4**.
Recorded because several of them contradict what this design was expected to need.

**Mount propagation.** A mount made in one container can reach a sibling:

```
unprivileged sidecar + mountPropagation: Bidirectional
      → Forbidden: Bidirectional mount propagation is available only to privileged containers
privileged sidecar + Bidirectional, alongside an unprivileged `restricted`-baseline step container
      → valid Pod (server dry-run)
```

and the mount **stacks directly on the shared volume's own root**, which is what keeps
`/workspace` at `/workspace`. From the step container's `/proc/self/mountinfo`:

```
1187 1179 0:153 / /wsa rw,relatime master:498 - tmpfs tmpfs rw,inode64     # stacked ON the volume root
1188 1179 253:1 …/empty-dir/wsb /wsb … ext4                                 # plain emptyDir, for contrast
1189 1188 0:154 / /wsb/nested … tmpfs                                       # nested one level under
```

Nesting would have forced the workspace to move to a deeper path and every pipeline with a
hardcoded `/workspace` would have paid for it. It does not.

**Unmount does not propagate back.** When the sidecar unmounted, the step kept the mount and
merely lost its `master:` link — contents still readable. So a daemon that dies mid-Step leaves a
live mountpoint that fails every operation. This is designed for below, not discovered later.

**PodSecurity is what kills the in-Pod mount, and it kills it at `baseline`:**

| Pod shape | `enforce=baseline` | `enforce=restricted` |
|---|---|---|
| plain `emptyDir` | allowed | allowed |
| **privileged sidecar + `Bidirectional`** | **denied** | **denied** |
| inline `nfs:` volume | allowed | **denied** — restricted volume types |
| **`persistentVolumeClaim`** | **allowed** | **allowed** |
| `image:` volume | allowed | allowed |

Two things follow. `baseline` — not `restricted` — is the level orgs actually set as a cluster
default, because `restricted` breaks too much; so the population that cannot run a privileged
sidecar is every enforcing cluster, not a minority of them. And **a PVC is the PSA-legal envelope
for a volume type that is otherwise forbidden**, because `persistentVolumeClaim` is on
`restricted`'s allow-list and the PV behind it is an admin-mediated object. Inline `nfs:` is
denied at `restricted`; the same NFS wearing a PVC is allowed.

There is a mechanical escape from PSA — it is per-namespace, a namespace label overrides the
cluster default, and Scarab owns its step namespace — and it is rejected. It would make the
default data path depend on being permitted to weaken the posture of our own namespace, which any
org that takes PSA seriously blocks with Kyverno or Gatekeeper. A default path resting on that is
not a path.

**A hardlink farm under `overlayfs` works on ext4, and protects the store.** One privileged pod,
directories on a real ext4 volume (`overlayfs` cannot be stacked on the container rootfs, which is
itself `overlayfs`):

```
fs type            : ext2/ext3                    # ext4 family — no reflink
hardlink farm      : blob link count = 2          # free, zero bytes copied
plain overlay (lowerdir = the farm)   : MOUNTED
  step writes through it →
    merged sees      : step wrote this
    CAS blob now     : original content           # UNCHANGED — the store cannot be corrupted
    upper holds      : main.rs                    # the EXACT change set
overlay + index=on,nfs_export=on      : MOUNTED   # exportable over NFS
```

**A hardlink cannot carry per-snapshot metadata, and that is the real constraint on the Farm.**
This was measured *after* the paragraph above was first written, and it corrects it:

```
set the snapshot's recorded mtime on the FARM entry → CAS blob mtime moved too
chmod 755 the farm entry                            → CAS blob mode changed too
reflink (APFS clonefile) instead of hardlink        → CAS blob metadata independent
```

A hardlink is a second *name* for one inode, and mode and mtime live on the inode. [0061](0061-workspace-data-path.md)
s7 made mode/mtime fidelity a pinned contract (`crates/scarab-storage-s3/tests/fidelity.rs`) after
measuring that dropping it silently degraded cross-Step incremental compilation, and s8 rejected
letting a first-writer's timestamp win for a shared blob. So restoring a snapshot's metadata onto a
hardlinked farm entry would **mutate the CAS blob** — the precise corruption this design claims to
prevent — and two snapshots sharing content but not timestamps would fight over it.

**Therefore the Farm materialises by `reflink` where the filesystem offers it and by plain copy
where it does not; hardlinks are not the mechanism.** The earlier claim that "reflink is a bonus,
not a dependency" is withdrawn. What survives is the part that matters: even the copy rung is a
**local** copy at disk speed, against 4–6 ms per file of network round-trip in the measured status
quo — for 50 000 files that is seconds versus minutes. The ordering argument is untouched; only the
size of the win moves, and only on filesystems without reflink.

The verified `overlayfs` result above still stands on its own terms — **copy-up does not modify the
lower layer**, so a Farm built by any rung is safe against a Step writing through it. That was what
the probe tested; it did not test metadata, and saying so is the point.

This inverts one warning and it is worth keeping the corrected form: the local dogfood disk is
**ext4**, which has no reflink, so the dogfood exercises the **copy** rung and the reflink rung is
the one it cannot reach. Any benchmark must therefore state which rung it took.

## The governing principle, unchanged

0061's principle still decides the arguable calls:

> **Minimise the substrate idiosyncrasies an author must know.**

`/workspace` stays at `/workspace`, holds the same bytes, and a Step cannot tell which of the
configurations below produced it. Where the substrate is expensive, the system pays.

## Decision

**Five parts.**

**1. A Snapshot Farm — the CAS given tree shape, without leaving the disk.**
The workspace service materialises a Workspace Snapshot into a directory tree on **its own disk**
whose entries are **`reflink` clones of CAS blobs where the filesystem supports it, and local copies
where it does not**. Either way **no round-trip is made and nothing crosses a network.** A Farm is
**immutable and read-only**, keyed by snapshot root, built once and **shared by every Step that
inherits that snapshot** — so fan-out costs nothing. The Farm is a warm-tier cache object: bounded
by space, LRU-evicted, and a miss is slower and never wrong.

Not hardlinks: see the measurement above — a hardlink shares its inode, so restoring a snapshot's
mode and mtime onto a farm entry mutates the CAS blob, and 0061 s7 makes that fidelity a pinned
contract.

This is where the per-file cost goes to die, and the reason is the **CAS**, not the transport: the
store already holds exactly one copy of each content **on the same filesystem as the Farm**, so
building a tree never has to ask the network for a byte. 0061 measured the CAS legs at 4–6 ms per
file against *loopback* object storage. A reflink is a local metadata operation in the tens of
microseconds; a local copy is disk bandwidth. For a 50 000-file `node_modules` or `target/` that is
**about a second on reflink, a few seconds on a plain copy, and minutes today.**

**2. A Workspace Export — the Step's Workspace, mounted by kubelet.**
Per Step, the service mounts `overlayfs` with the Farm as **lowerdir** and a per-Step directory as
**upperdir**, and exports the merged view. The Step Pod receives it as a **PersistentVolumeClaim**
bound to a per-Step PersistentVolume with an NFS source, mounted at `/workspace`.

The Step Pod holds **no privilege, no capability, and no PSA exception**. It does not fetch, and
there is nothing to copy back. Reads fault over the network only for what the Step actually opens
— which is the laziness part 2 of 0061 wanted, obtained from kubelet instead of from a driver.
Delivery **as a PVC rather than an inline `nfs:` volume is load-bearing**, not stylistic: the table
above shows inline `nfs:` is denied at `restricted` and the PVC is not.

**The Export mount carries `redirect_dir=on`, and that is a correctness requirement rather than a
tuning choice.** Without it, `rename(2)` of a directory that exists only in the lower layer returns
**`EXDEV`** — measured: `rename failed: Cross-device link`, with the module default `redirect_dir=N`.
Directory renames are not exotic; git, cargo, npm, pip and maven all do them, and today they work
because `/workspace` is a plain `emptyDir`. An Export that broke them would hand authors "Invalid
cross-device link" as substrate knowledge, which the governing principle forbids. Worse, `mv`
*masks* it by recursively copying the subtree, so the failure mode is not an error but a **silent
full copy of an inherited tree** — which then lands in the upper layer and makes "the change set is
the upper layer, exactly" re-ingest a tree nothing changed. Verified that `redirect_dir=on` coexists
with `nfs_export=on` (unlike `metacopy`, above), so the fix is available.

**This forces part 3's reader to support `trusted.overlay.redirect`, not refuse it.** A directory
rename under `redirect_dir=on` records the old path in that xattr, so a change-set reader that
treats `redirect` as unsupported would refuse every renamed directory. The two halves have to agree,
and the ADR previously implied they could each be decided alone.

**3. The change set is the upper layer, exactly.**
An `overlayfs` upper directory contains precisely the paths the Step touched, put there by the
kernel. The drain reads the upper, hashes only those files, folds them into the CAS and returns a
new snapshot root — **all on the service's own local disk, with no network in the path.**

**Reading a change set must fail closed on missing privilege, and the reason is nastier than it
looks.** An upper layer records deletions as whiteouts (a character device with `rdev == 0`) and
wholesale directory replacements as the `trusted.overlay.opaque` xattr. `trusted.*` xattrs are
readable only with `CAP_SYS_ADMIN` — and the kernel answers an unprivileged read with **`ENODATA`,
which is indistinguishable from "the attribute is not set"** (measured while implementing s2:
kernel 6.8.0, an unprivileged reader saw *nothing* on an upper that demonstrably carried the
markers). So a drain that merely *tried* to read them would report "no opaque directories, no
renames", be believed, and publish a snapshot missing every deletion. The reader therefore checks
its own effective capabilities up front and **refuses** rather than answering. A silent "nothing
changed" is the worst available failure and it is one syscall away from being the default.

Two further measured refinements, because they decide what has to be supported rather than
tolerated. A **file** rename is exact — it appears as a whiteout plus a full copy, with no
`redirect` — so only **directory** renames need refusing; and `redirect_dir=on` *is* honoured when
passed at mount time even though the module default is off, unlike `metacopy` below, so the refusal
path is reachable in production rather than theoretical. Copy-up also stamps `origin` on files and
`impure` on their parent directories, which must be tolerated as bookkeeping — treat them as
unsupported markers and every real drain fails on its first edited file.

This is not an optimisation of the drain, it is a different kind of answer. 0061's deferred
overlay-diff drain (git-bug `66fc0e3`) wanted this and believed it needed a privileged mount on
the node. A **stat cache** — comparing each file's `(size, mtime)` against the input manifest,
git's index-cache trick — was the approved unprivileged approximation, and its failure mode is
*silently publishing a stale hash* on an mtime race. The upper layer makes the change set exact,
so that failure mode does not exist rather than being defended against. The stat cache survives
as the drain for configurations without an Export (part 5's ladder, and the local executor), where
it is the correct fallback and not the mechanism.

**4. Zone preference is soft, and a miss is a cost line rather than an outage.**
A Workspace Export lives on **one** service replica's disk, and the volume must be named in the
Pod spec before the scheduler places anything. Scarab picks the replica at launch, prepares the
Export there, and expresses a **preferred** (never required) zone affinity on the Pod. If the
scheduler honours it the mount is in-zone; if capacity says otherwise the Pod mounts cross-AZ —
slower, metered, **never wrong**.

Hard-pinning is rejected: it hands the scheduler a zone constraint on every Step, which fights the
spot capacity 0061 names as the operating environment, and it silently amends the placement
overlays [0055](0055-placement-profiles.md) makes the operator's to own. When a snapshot is warm
nowhere, every zone is equally good and the preference has nothing to say.

**5. An optional node-side accelerator, for write-heavy Steps on fast local disk.**
Writes in part 2 cross the network. Where a cluster permits privilege and its nodes have fast
local storage, a Step may instead run with the **same primitive in a different place**: a
privileged Scarab-owned native sidecar mounts `overlayfs` with the **Export as lowerdir
(read-only)** and a **node-local `emptyDir` as upperdir**, and propagates the merged view to
`/workspace` via `Bidirectional` propagation. Reads still fault over the network; **writes never
touch it during the Step**; at exit the sidecar ships the upper layer — again the exact change set
— to the service.

This is opt-in and it is an accelerator, not a second architecture. Both configurations use the
same Farm, the same CAS, the same snapshot roots, the same `/workspace`, and in both the upper
layer is the change set. The only difference is **where writes land in the interim**.

### The privilege ladder

The service needs `CAP_SYS_ADMIN` to mount `overlayfs`. That is one **operator-installed
StatefulSet** — the exception storage operators are routinely granted, on the node the constraint
explicitly permits — and it is *preferred*, not required. The interface is "give me a tree-shaped
writable view of a snapshot on the service's disk", with three backends:

The two axes are independent — how the Farm is built, and whether an `overlayfs` sits on top of it —
so they are two ladders, not one:

| Farm build | needs | cost, 50k files | metadata fidelity |
|---|---|---|---|
| `reflink` clone per blob | XFS (reflink=1), btrfs, APFS | **~1 s**, no bytes | exact |
| plain local copy per blob | nothing | seconds, at disk bandwidth | exact |
| ~~hardlink per blob~~ | — | free | **unsafe — mutates the CAS blob** |

| Export build | needs | change set |
|---|---|---|
| `overlayfs`, Farm as lower | service pod `CAP_SYS_ADMIN` | **exact** (the upper layer) |
| writable copy of the Farm | nothing | stat cache — `(size, mtime)` approximation |

Every live rung produces an identical tree, and the slowest is a **local** copy at disk speed
instead of 50 000 network round-trips. **A build must report which rung it took**; a benchmark that
silently drops a rung reports a number the real deployment would never produce.

**What refusing the capability actually costs.** The first version of this sentence said "never
correctness, and never a second code path", which was wrong on both halves and was corrected once to
say the rungs differ in *correctness*. That was true of the stat cache **as first built** and is no
longer true of it, so here is the settled position.

`(size, mtime)` alone has a hole that is not a race but a **determinism**: any writer that preserves
both length and timestamp — `cp -p`, `touch -r`, `rsync -a`, `tar -xp` — defeats it every time,
reproducibly, and those are ordinary CI operations rather than pathological ones. A racily-clean
check does not help, because nothing about it is racy.

**`ctime` closes it, and this is why git's index records more than size and mtime.** No syscall sets
ctime; `utimensat` *bumps* it as a side effect. So comparing the observed ctime against the moment
materialisation completed catches every one of those writers. Verified on APFS, ext4, tmpfs and an
overlayfs upper: `rewrite+utimensat`, `cp -p`, `touch -r` and `tar -xpf` all moved ctime past the
capture; a pure read did not.

Two things that only measurement would have produced, both now contractual. The mtime cutoff needs a
one-second slack for coarse filesystems and **the ctime cutoff must not have one** — materialisation
finishes in the milliseconds *before* the capture, so a slack there puts every file inside the
untrusted window and the cache degrades to hashing everything (measured: every file re-read, nothing
ever reused). The asymmetry is principled: the mtime cutoff compares a *recorded producer timestamp*
against a different clock, while the ctime cutoff compares two readings of *the same* clock
milliseconds apart. And the capture must be stamped **at least a millisecond after** the last file is
written, or millisecond truncation leaves materialisation's final files inside the capture's own tick.

So the rungs do not differ in correctness. They differ in **what their correctness rests on**: the
upper layer is the kernel's own record of what was touched, while the stat cache is sound only given
an unforgeable ctime, a capture stamped after materialisation, a filesystem whose ctime is not coarse,
and a Step that cannot move the clock backwards (ADR-0039 drops **ALL** capabilities, so no
`CAP_SYS_TIME`). Those assumptions hold in every configuration Scarab ships, and they are assumptions
where the exact path has none. It remains **two drains**, which 0061 part 1 would still count against
this design; the defence is that only one of them is ever used in a cluster, and the other exists
where there is no Export to read — the local executor.

The threat model is worth stating plainly, because it bounds all of this: the stat cache guards
against **accident, not malice**. A Step that deliberately forges timestamps to poison its own change
set corrupts only its own fenced evidence.

**The Farm rung is per-file, not per-build.** A clone can fail on an individual file while
succeeding on its neighbours, so `Mixed` is a real outcome and the *counters* — how many were
cloned, how many copied — are the reportable truth rather than a single label. Discovered while
implementing s1; the two-rung table above reads as if a build picks one rung and keeps it, and that
is a simplification.

### Vocabulary

**Snapshot Farm** — the immutable, shared, tree-shaped hardlink view of one Workspace Snapshot on
the workspace service's disk. Defined here and **deliberately kept out of
[CONTEXT.md](../../CONTEXT.md)**: it is a cache implementation, not domain language, and the
glossary is a glossary.

Two terms do reach the glossary, because they are distinctions a reader must not conflate:

- **Workspace Export** — the *delivered* form of a Workspace: the per-Attempt, writable,
  network-mounted view a Step Pod receives. Losing an Export fails that Attempt, which retries;
  losing a **Snapshot** widens a rerun's scope. Different words, different consequences.
- **Change set** — the exact set of paths an Attempt wrote, *known* from the upper layer rather
  than inferred. Where there is no Export it is *approximated* from `(size, mtime)`, and that is
  the one place in the data path where a wrong answer is possible rather than merely slow.

`Workspace` also loses its "Pod-local, dies with the Pod" claim, which was an implementation leak
in a glossary: a Step cannot tell whether its bytes sit on its node, on the service, or across
both, and the lifetime unit is the **Attempt**.

### The fence

An Export's identity is a **capability, not an assertion**. NFS authenticates with `AUTH_SYS` —
the client asserts a uid and the server believes it — so per-Step isolation cannot come from the
protocol. It comes from the export path being an unguessable 256-bit secret, TTL'd to the Step's
deadline, **pinned to the first client that mounts it**, and revoked when the Attempt settles.
That is structurally the same capability the existing per-Pod HMAC workspace token is, delivered
through a different channel; Scarab already creates a Pod and a Secret per Step, so a PV and a PVC
per Step is the same order of object churn.

## Consequences

- **The service becomes a network filesystem server.** `nfs-ganesha` in userspace is the trodden
  path. It is a real new dependency implementing a protocol with decades of edge cases, and it is
  the largest single risk in this ADR.
- **A privileged workspace-service Pod is the preferred configuration.** Per the ladder this is
  degradable, so it is not an install prerequisite — which is the material difference from 0061's
  node driver, and the reason this design satisfies the constraint at all.
- **`/workspace` does not move**, measured, so no authored pipeline changes.
- **Writes cross the network in the default configuration.** This is the last structural cost and
  it is smaller than it sounds: today's drain ships the **entire** workspace over the `exec` tar
  tunnel whether it changed or not, where an Export ships only the bytes the Step actually wrote,
  once, at write time. **The risk is operation count, not byte volume** — NFS charges a round-trip
  per file operation, and a build that creates, writes and `fsync`s many small files pays latency
  per file that no bandwidth fixes. That is what part 5 exists for and what the spike must measure.
- **Aggressive client attribute caching is safe here and is not in general.** An Export has exactly
  **one client and one writer**, and its lower layer is immutable by construction, so a high
  `actimeo` cannot serve a stale answer. This is the specific lever against the many-small-files
  metadata cost that makes EFS bad at `node_modules`.
- **A dead FUSE-or-NFS mount fails the Attempt, and must say so.** Measured: unmount does not
  propagate back to the Step, so a failed daemon leaves a live mountpoint returning errors on every
  operation. Hard mounts plus the existing Step deadline plus durable retry is the right handler —
  a stall becomes a deadline-exceeded Attempt that retries — but the Attempt must name the cause
  rather than surface a mystery I/O error ([0027](0027-restart-semantics.md): smart never means
  mysterious).
- **Rolling the service is now a drain-then-roll operation.** With per-AZ replicas, a Step mounts
  one replica, so an upgrade must stop accepting new Exports and wait for in-flight Steps (minutes)
  before rolling. Blast radius of a replica loss is the Steps mounted on it, not all of them.
- **Warm-tier eviction stops being optional.** Farms and Exports are directories on a bounded disk,
  and today the warm tier has no eviction at all (git-bug `24476bc`). That ticket becomes blocking.
- **Per-Step PV/PVC objects and per-Step exports are a reaping obligation.** A leaked Export is a
  leaked directory and a leaked capability.
- **`0061`'s cross-AZ claim is amended.** "Cross-AZ traffic is confined to the archive drain" is
  false once a Step can mount a replica in another zone; part 4 makes that a priced, preferred-
  affinity miss instead of an invariant.
- **The eager fetcher becomes the no-Export fallback, not the standard route.** `scarab-wsfetch`
  and the HTTP CAS path shipped on PR #93 remain the path for the local executor and for any mode
  without an Export. They stop being the way a Step gets its inputs in a cluster.

## Alternatives considered

- **FUSE (or `fuse-overlayfs`) inside the Step Pod, via a privileged Scarab sidecar.** The closest
  drop-in, and the union would not even need `overlayfs` — a daemon we write *is* the union (lower
  = fetch-on-open, upper = pod-local, copy-up on write). Rejected on the measured PSA table: denied
  at `baseline`, which is the common cluster default. Survives only as part 5, opt-in, where the
  privilege buys local writes rather than laziness.
- **FUSE inside the Step's *own* container** via [0039](0039-privileged-images.md)'s governed
  `add_capabilities`. Rejected: it hands `SYS_ADMIN` to the *user's* container and guts 0039's
  restricted baseline. Named because it is the obvious move and it is worse than a DaemonSet, not
  better. Mount propagation makes it unnecessary in any case — a sibling container can own the
  mount.
- **Inline `nfs:` volume instead of a PVC.** Simpler and denied at `restricted` (measured). The PVC
  is not ceremony.
- **A warm, content-indexed pool of block PVCs handed to Step Pods** — better than laziness on its
  best axis, since a pre-populated volume moves **0%** of a tree where laziness moves 5%, and it is
  PSA-clean. Rejected because it re-binds volumes to short-lived Pods, which is precisely what
  0061 part 1 designed away: *"Every problem PVCs have here… comes from binding volumes to
  short-lived pods. Bind them to the long-lived thing instead and all of it goes away."* Attach is
  5–20 s on EBS-class storage and detach wedges when a spot node goes; volumes are AZ-pinned, so
  they constrain scheduling and fight 0055; per-node attachment quotas cap concurrent Steps in the
  mid-20s; RWO means a fan-out needs a clone per consumer; and the write-back needs a
  detach-and-reattach before the volume can return to the pool. A pool genuinely fixes provisioning
  and population — the part 0061's "PVC per Step" rejection did not consider — and fixes none of
  the rest.
- **EFS (or any managed RWX filesystem) mounted directly.** 0061 rejected "RWX PVC per Run" partly
  as "a much heavier prerequisite than the driver". **That reasoning is now backwards and is
  corrected here**: the EFS CSI driver is a DaemonSet and therefore forbidden, while EFS reached
  through an NFS-sourced PV needs no driver at all. It still loses, for two reasons that are not
  the availability argument. It makes a managed filesystem product a **prerequisite**, which 0061
  declines to do even for object storage ("an operator's cost decision, not an engine decision").
  And it puts the per-file cost back on the wire: EFS charges roughly a millisecond per metadata
  operation, where a Snapshot Farm charges tens of microseconds locally. Running our own server is
  what buys the Farm; a managed one cannot be given hardlinks into our CAS.
- **A hardlink Farm, so the Farm is free on every filesystem.** This is what the ADR originally
  decided and it is **withdrawn on measurement**: mode and mtime live on the inode, a hardlink is a
  second name for one inode, so writing a snapshot's recorded metadata onto a farm entry mutates the
  CAS blob. 0061 s7 pinned that fidelity after measuring real damage from losing it, and s8 rejected
  first-writer-wins timestamps for shared content. Kept in the record because the mistake is
  reusable: the `overlayfs` probe verified that copy-up does not modify the lower layer and was read
  as verifying the Farm was safe, which it never tested. **A probe proves what it exercised.**
- **A metadata-variant hardlink pool** — one materialised copy per distinct `(content, mode, mtime)`
  and hardlinks from there, so repeats are free. Rejected for the first cut: mtimes vary per file in
  a real build, so the sharing rate collapses toward one copy per file and the pool buys complexity
  rather than speed. Worth revisiting only if measurement shows real trees cluster into few distinct
  metadata variants.
- **A custom NFS server implementing copy-on-write in userspace**, so a hardlink farm could be
  exported directly with no `overlayfs` and no `CAP_SYS_ADMIN`. Technically sound — we would break
  the link on first write ourselves — and rejected because it means writing the CoW semantics the
  kernel already has, in the write path of every Step, in exchange for a capability an
  operator-installed StatefulSet can reasonably hold.
- **OCI `image` volumes as the transport.** PSA-clean at `restricted` (measured), node-cached,
  parallel and runtime-managed, and it becomes lazy for free the day a platform team adopts a lazy
  snapshotter — a node change Scarab neither installs nor versions, which is the cleanest possible
  fit to the constraint. Not chosen because the laziness is contingent on someone else's adoption
  and the read-only union problem returns (a copy into `emptyDir` reads every file; working
  directly in a read-only tree changes the authoring model). Also currently untestable locally:
  the runtime here is `docker://29.5.2`, and cri-dockerd does not implement image volumes. Retained
  as a possible internal representation, which is where 0061 already had it.
- **Fusing the boundary instead of making it lazy** — automatically co-locating a linear chain of
  Steps into one Pod when placement profiles match and no gate or approval intervenes, so no bytes
  move at all. Distinct from 0061's rejected "coarsening the unit", which is an *authoring* change;
  this is invisible. Not rejected on merit — it attacks boundary **count** where this ADR attacks
  boundary **cost** — but deferred: it trades away per-Step restart granularity inside a fused
  group, which collides with [0027](0027-restart-semantics.md) and
  [0056](0056-run-takes-and-attempt-grain-evidence.md) and would have to be governed and visible.
  Filed separately.
- **Inferring a Step's read set (profile-guided prefetch).** Foreknowledge instead of interception.
  Rejected as unsafe in this direction: a wrong guess does not fail, it succeeds with a file
  missing. Write-side inference is a different and safe proposition and is what the stat cache is.

## Red-team findings this ADR does not yet answer

A red team was run against this ADR before code rested on it, with a live cluster. It confirmed the
design's load-bearing claims — an overlay **does** survive being exported and re-read through a
second fresh client with modes and mtimes intact, `/workspace` **does** stay put, per-Step PV/PVC
churn **is** cheap (80 objects in 2.3 s, 40 PVs `Bound` in under 5 s, no quota trouble), and PSA
`restricted` **does** admit an unprivileged Pod with an NFS-backed PVC *including the bind*. The
PVC-as-envelope argument is sound.

It also found the following, which are recorded here rather than quietly fixed because several change
what the design must do. Each is filed.

1. **kubelet needs an NFS client on every Step node — CONFIRMED, and it dents the headline claim.**
   The delivery path had never actually been mounted. When it was: PV bound, PSA admitted the Pod,
   the scheduler placed it, then kubelet failed with *"you might need a /sbin/mount.nfs helper
   program"* and the Pod hung in `ContainerCreating` **forever**. The colima node has no `nfs-utils`.
   So "the Pod needs nothing and kubelet does the mount" is true of *privilege* and false of
   *packages*: an NFS-sourced PV requires an NFS client on each node that runs a Step. That is not a
   DaemonSet and not something Scarab installs, but it **is** a per-node prerequisite, and it must be
   stated as one — it is present on most distributions and its absence on minimal images
   (Bottlerocket, hardened AMIs) is a real risk. It also means the s9 spike cannot run on `just up`
   or `local-helm` until the colima VM gets `nfs-common`.
2. **Evicting a Farm under a live Export is silent corruption — CONFIRMED, and worse than "must not
   happen".** With lower entries deleted while the overlay was mounted, `ls` of the merged directory
   returned **empty** while `cat` of already-cached paths still returned content, and a write into the
   vanished directory returned **rc=0**. A Step would see an empty tree, build nothing, and exit 0 —
   the fail-silently class this ADR rejects read-set inference for. Nothing refcounts Farms against
   live Exports today, and the `actimeo` argument ("the lower layer is immutable by construction")
   inherits the same assumption. Reaping is therefore a correctness mechanism, not housekeeping.
3. **Part 3's "no network in the path" collides with 0061 part 4 — CONFIRMED (documents).** Folding a
   change set into the CAS on the service's own disk lands it in the **warm** tier, and 0061 part 4
   requires durability before `Succeeded` while 0061 explicitly forbids making warm load-bearing for
   durability. So either part 4 weakens or a cold upload stays on the critical path — and this ADR
   books neither. Unbooked cost, exactly what 0061's own "price of seeding the warm tier" section
   exists to avoid repeating.
4. **`fsGroup` does not survive an NFS volume — PLAUSIBLE, strong, and possibly a blocker for part 2.**
   The executor sets `fs_group: WORKSPACE_GID` and comments that the restored `/workspace` is "owned
   by `WORKSPACE_GID` and only group-writable". The in-tree NFS plugin reports `Managed: false`, so
   kubelet applies **no** fsGroup ownership. Write access then rests on AUTH_SYS uid/gid against the
   modes 0061 s7 preserves exactly — and `0644` is not group-writable. **The Step may be unable to
   write to its own Workspace.** This ADR discusses AUTH_SYS only in the context of the fence and
   never as an access-control problem for the Step itself.
5. **The fence is weaker than the word "capability" implies — PLAUSIBLE.** A userspace NFSv4 client
   needs no privilege, no PV and no kubelet, and can assert any uid; a probe mounted with
   `noresvport`, so the privileged-port defence never engaged. First-mount pinning pins the **node**,
   because kubelet does the mounting — so co-tenant Steps on one node share an identity. And path
   secrecy is the only remaining barrier while the path sits in a **cluster-scoped PV object** and in
   the node's `/proc/mounts`. "Structurally the same capability as the HMAC token" overstates it: the
   token is per-Pod and unreadable by other Pods; this is not. Needs a real answer — `resvport`, a
   NetworkPolicy, per-Step uid squashing — or an honest downgrade of the claim.
6. **Revocation and restart are unhandled — CONFIRMED.** Revoking an export under a live client gives
   `ESTALE` then `EACCES`, and `nfsd` start logs a **90-second grace period** plus *"Unable to
   initialize client recovery tracking … Is nfsdcld running?"*. This ADR handles a dead daemon but not
   revocation racing a Pod already in SIGTERM grace, nor the post-restart grace window, nor `nfsdcld`
   as a dependency.
7. **Part 4's zone preference has no merge story with 0055 — PLAUSIBLE.** `affinity` appears nowhere
   in the Rust sources, and a placement overlay is applied as a block, so whichever writer goes last
   replaces the other's affinity wholesale. A preference Scarab adds and an overlay the operator owns
   cannot both survive without a defined merge.
8. **The s9 spike as specified cannot falsify the design — PLAUSIBLE.** Operations-per-second under
   latency exercises none of: `EXDEV`/redirect correctness, whiteout and opaque handling in the drain,
   eviction under a live mount, restart with live Exports, or cross-AZ behaviour. Its rung guard
   covers reflink and `overlayfs` but not `redirect_dir`, not which drain ran, and not whether the
   mount was actually NFS. A spike shaped only to produce a number will produce one.

## Open — deliberately not decided here

- **Operations-per-second under latency, measured, for a `cargo`-shaped write workload against a
  real in-cluster Export.** This is the one unpriced number in the design and it is the number that
  decides whether part 5 is an accelerator or a requirement. Byte throughput is *not* the thing to
  measure. The harness must fail loudly if it silently falls back a rung on the ladder — a spike
  that reports a flattering number because reflink or `overlayfs` was unavailable is the failure
  mode this repo has already paid for.
- **Farm-and-Export reaping and the warm-tier space bound.** Blocked on git-bug `24476bc`, which is
  now on the critical path rather than beside it.
- **`overlayfs` with an NFS mount as `lowerdir`**, which part 5 requires. Documented as working
  with caveats; unverified here, and load-bearing for the accelerator only.
- ~~**Whether `overlayfs` `metacopy=on` can restore a hardlink Farm.**~~ **Answered: not for a
  Workspace Export — but the first answer recorded here was wrong, and the correction matters.**

  **What was wrong.** This ADR briefly claimed `metacopy=on` "mounts and then does a full data
  copy-up anyway", citing `upper entry size 17, blocks 8` on a 17-byte file. That is not evidence of
  a data copy: `metacopy` **preserves `size` by design**, and 8 blocks is ext4's 4 KiB minimum
  allocation. Re-run on an 8 MiB lower file the answer inverts — `size=8388608` with **8 blocks
  allocated** and `trusted.overlay.metacopy=""` on the upper entry. **`metacopy` works, from the
  mount option alone, with the module parameter still `N`.** A hardlink Farm under `metacopy` was
  also verified safe: link count 2, CAS blob still `644`, merged view reading `755`.

  The claim that the module parameter made it "accepted and ineffective" was also a **category
  error**: the service-side overlay is mounted on the *workspace service*, so a module parameter
  would be one node — the one the constraint already permits — not every Step node.

  **The real reason it cannot be used, which is a different fact entirely:**

  ```
  mount -o index=on,nfs_export=on,metacopy=on  →  refused: conflicting options
  ```

  An Export must be **exportable**, and `nfs_export` and `metacopy` are mutually exclusive. So on the
  service side the Farm cannot use hardlinks, and part 1's reflink-or-copy stands — for this reason,
  not the one first written down.

  **And it opens something.** Part 5's node-side accelerator mounts an overlay with **no
  `nfs_export`**, so `metacopy` *is* available there. That is worth following up rather than burying:
  it would let the accelerator's upper carry metadata with no data copy.

  Kept in full because the mistake is the reusable part. The first probe measured a real thing and the
  conclusion drawn from it was unsound, for the third time in this ADR's short life — and each time
  the tell was the same, a probe whose result was read as answering a question it had not asked.
- **Automatic Cache detection for lockfile-derived trees** — recognising `node_modules`, `.venv`,
  vendored `~/.m2` and the pip/uv caches from their lockfiles and routing them through
  [0007](0007-data-passing-model.md)'s **Cache** instead of the Workspace, so that less is subject
  to the write cost at all. Deliberately **not** decided in this ADR: it is a heuristic whose
  failure mode is wrong output (`target/` is Cache-shaped when the next Step rebuilds and an
  *output* when the next Step consumes a binary), and it deserves its own trade-off record rather
  than riding along on a transport decision.

## References

- [0004](0004-execution-topology.md) — pod-per-step; this ADR changes how the workspace reaches a Pod
- [0007](0007-data-passing-model.md) — Workspace / Result / Artifact / Cache
- [0027](0027-restart-semantics.md) — content-addressed invalidation; "smart never means mysterious"
- [0029](0029-workspace-cas.md) — per-file merkle CAS; the Farm is its tree-shaped view
- [0039](0039-privileged-images.md) — the restricted step baseline this ADR does not weaken
- [0050](0050-retention-and-gc.md) — retention and GC
- [0055](0055-placement-profiles.md) — placement profiles; part 4 must not silently amend them
- [0061](0061-workspace-data-path.md) — **supersedes its part 2**; amends its line on privileged
  mounts, its node-driver install prerequisite, and its cross-AZ confinement claim
