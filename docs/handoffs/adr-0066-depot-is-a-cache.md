# Handoff — ADR-0066 decided: the Depot is a cache, the lazy feed is cancelled, HA is next

**Branch:** `docs/adr-0061-workspace-data-path` (PR #93). **Head at handoff:** the ADR-0066 commit.
**Session:** 2026-08-01 → 2026-08-03.

---

## Read this first, in this order

1. **[ADR-0066](../adr/0066-the-depot-is-a-cache.md)** — the whole design. Ten decisions, all
   grilled, all with reasoning. Everything below is navigation for it.
2. **git-bug `4ce7f2c`** — the measurement that cancelled the lazy feed. If you read one artifact
   to understand why the plan changed, read this one.
3. **git-bug `e140121`** — we are not resilient today. This is the prerequisite for everything.
4. `CONTEXT.md` §4.3–4.4 and §7 — the glossary gained *Provisioning deadline*, *Warm-only*, and a
   sixth key invariant.

---

## The one thing that will bite you

**Fence affinity is a correctness requirement, not routing convenience — and nothing enforces it
today.** `CONCURRENCY = 16` (`crates/scarab-workspace-client/src/lib.rs:64`) means the drain opens up
to sixteen connections, and kube-proxy load-balances **per connection**. So the moment
`workspace.replicaCount > 1`, one fence's blobs scatter across replicas and `post_drain`'s closure
validation — which runs against **one** replica's ledger and warm tier — fails. That is *every drain*,
not a rare race.

The chart says `replicaCount: 1` and calls `>1` "unproven". It is worse than unproven: it is
currently **broken by construction**. Do not raise it before ADR-0066 point 3 lands.

---

## What was decided, and what is not relitigable

The ten decisions are in the ADR with their reasoning. The four that reshape everything else:

- **The Depot is definitionally a cache.** Anything making it a system of record is a defect to fix,
  not a design to support. This dissolves HA, eviction and spot-preemption at once — and its
  consequence, **HA requires object storage**, is booked as a documented product statement.
- **The lazy feed (NFS/ganesha/Export) is cancelled on measurement**, and so is PV-per-step.
  The eager `scarab-wsfetch` init container stays. ~10k lines become deletable.
- **The drain is now 5× the feed.** All optimisation effort points at the drain, not the feed.
- **Packing is the next ADR** — because laziness was the only reason to address blobs individually,
  and we cancelled it.

Three things were **measured** rather than argued, and the numbers are in the tickets: the feed
(`4ce7f2c`), nfs-ganesha's real failure semantics (`20e8786`), and the overlay change-set tier
finally executing (`0ad393c`, now closed).

---

## State

**Shipped this session, on the branch:**

| commit | what |
|---|---|
| `e58ce1f` | 212bb13 s1 — the drain leaves the Pod as hashes; per-fence write ledger closing a real cross-fence exfiltration hole |
| `97de21d` | the overlay chain measured end to end on a live kernel |
| `18034e7` | `just overlay-tests-colima` — the change-set tier executes for the first time anywhere |
| `1ecfac8` | `reflink-copy` + `xattr` adopted; the feed benchmark that ended the lazy-feed question |
| *(this one)* | ADR-0066 + amendments to 0013/0051/0061/0062/0063/0065 + CONTEXT.md |

**Suite:** 993 passing, 28 env-gated skips. `just openapi-drift` green.

---

## Order of work, and why this order

1. **`e140121` — resilience at Init.** No retry, no timeout, `restartPolicy: Never`, and three
   auto-attempts burned in seconds → `DeadLettered`. A 20–30s Depot outage kills runs *today*.
   **HA is meaningless until this lands.**
2. **`cfb5edb` — split the clocks.** Independently justified: the engine backstop is anchored at
   Attempt *claim*, so queueing delay already bills the step's timeout on any busy cluster.
3. **`6cb4a27` — Depot HA.** Run-hashed affinity by recorded choice, Pod spec as the record,
   headless Service. Gated behind `0ec3b39` (deleting Export/Farm) because the fence-residue sweep
   has to be extracted from `sweep_exports_once` first.
4. **`974440b` — logs stop serving an empty pane.** Small, live in every deployment, and independent
   of everything above.
5. **`c0b6e76` → `a0c28aa`** — measure the cold flush, then the packing ADR.
6. **`42d997c` — warm eviction**, reachability first.

Deferred with explicit triggers, all in ADR-0066 point 5: JuiceFS (the named revival path if
laziness ever returns — *do not rebuild ganesha*), Dragonfly and the node-cache DaemonSet (gated on
`7ff26da`), SeaweedFS (supported cold tier now, untested by us), Kraken (rejected, archived).

---

## The honesty items that matter most

**Four claims I made confidently this session were wrong, and measurement caught every one.** They
are worth knowing because the pattern will repeat:

- "Pinning `Filesystem_id` prevents post-remount ESTALE" — **measured false**; `uuid=on` already
  handles it.
- "Multi-lowerdir is the fan-in answer" — the opposite, for placed mounts.
- "colima can't run an NFS client" — one `apt-get install`.
- "PV handoff works for linear pipelines, breaks at fan-in" — **inverted**; with per-step evidence
  it is *chains* that break, at clone depth ~4.

Each was plausible mechanism-level reasoning, and each was overturned within the hour by an
experiment. **Measure before you design on it.**

**Unresolved and known:** a diamond DAG where two branches write the same path silently resolves by
declared `needs:` order with no diagnostic anywhere (`2e1a458`) — a live correctness bug found in
passing, unrelated to storage.

---

## Traps that cost real time

- **Never run `cargo fmt`** in any form. `main` is not fmt-clean and even `cargo fmt -- <file>`
  reformats the workspace. Hand-format.
- **Commit messages go in a file, not on the command line.** Backticks in a `-m` string get executed
  by zsh — it happened this session and dumped the shell environment, including AWS credentials,
  into the transcript. Use `git commit -F <file>`.
- **`kubectl config current-context` must be exactly `colima`** before any cluster command;
  production EKS contexts sit beside it.
- **A live Helm dogfood stack runs on colima and is not yours.** `just down` runs `compose down -v`.
- **`git bug bug ls` is broken here** — enumerate via `git for-each-ref refs/bugs` and
  `git bug bug show <id>`. Comments need the id *before* the flags.
- **A darwin `cargo check` never compiles `changeset.rs`** (Linux-only). Verify that code with
  `just overlay-tests-colima`, which runs it on a real kernel.

---

## How to work here

Thin-orchestrator with subagents worked well and is how the whole session ran: purpose-built agents
in parallel on disjoint file allowlists, tests run centrally by the orchestrator (never in an agent),
a skeptical reviewer over the combined diff, and a red team on any decision before implementing it.

**Verify agents rather than believing them.** Two examples from this session: an agent concluded "no
production caller" from a grep that could not see trait-object dispatch, and another reported a design
resting on a premise a red team then disproved. The reviewer caught a blocker in shipped code
(`invoke`-namespaced step ids contain `/`, so the drain-record GET would 404 forever) that three
implementation agents had missed.

`/grill-with-docs` produced ADR-0066 and is the right tool for a decision of this size.

---

## First move

Read ADR-0066. Then `e140121`, and make the Init path survive a replica reschedule — timeouts,
bounded retry sized as a fraction of the step timeout, and splitting transient from permanent in
`workspace_fetch_failed`. Everything else in the plan assumes it.
