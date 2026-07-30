# 0063. Step logs live on the Data Depot, and the workspace service is renamed

- **Status:** Accepted
- **Date:** 2026-07-28
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0013](0013-history-and-observability.md) (the log pipeline this moves),
  [0061](0061-workspace-data-path.md) (the warm/cold tiers and the service being renamed),
  [0027](0027-restart-semantics.md) ("smart never means mysterious")

## Context

[0013](0013-history-and-observability.md) sends a Step's stdout/stderr to **chunked, gzipped blobs in the
object store**, with a per-Step **byte-offset index in Postgres** (never the bodies), feeding a
live SSE tail and full replay. `LogService::new(store, db)` takes a single `Arc<dyn ObjectStore>`,
and in composition that is the **cold** store.

Two facts about that arrangement, both verified rather than assumed.

**Object storage is a silent prerequisite for logs surviving.** With `SCARAB_S3_BUCKET` unset the
store falls back to `StoreConfig::LocalDir("./.scarab/objects")`, which on the Helm chart resolves
inside the server Pod's `scratch` **`emptyDir`**. So a deployment without object storage loses every
log on every Pod roll, and nothing says so. This is the same failure class
[0061](0061-workspace-data-path.md) records for the CAS in `deploy/local-helm` — *"a design that
relied on it to prevent data loss would have rebuilt the same bug with a bigger disk"* — arriving
through a different door.

**Logs are the only class in the data plane that cannot be re-derived.** Everything else can be
recomputed from the derivation graph (`attempts.consumed` plus the Run's DAG): re-run the producing
Step and you get the same content, because content is what the CAS addresses. Re-run a Step and you
get **different logs** — new timestamps, new interleaving, possibly a different outcome. So logs are
simultaneously the *cheapest* class to keep (gzipped chunks, bodies never in Postgres, an index of a
few integers per chunk) and the only one where loss is final.

Those two facts point the same way: the class that can never be recovered is the class currently
depending on an unstated prerequisite.

## Decision

**1. Log bytes live on the Data Depot's warm volume, not in the object store.**
The control plane ships chunks to the Depot; the Depot owns them. Object storage stops being a
prerequisite for logs existing.

**2. Not in the CAS.** Logs are keyed `{run, step, attempt}` plus a chunk sequence — a separate
namespace on the same volume. Content addressing buys nothing for an append-only stream: chunks are
unique so there is no dedup to win, hashing every chunk costs something for nothing, and an ordered
offset index is still required to reassemble the stream. **So the Depot's volume hosts two stores**,
the CAS and the log namespace, which is a fact about the component rather than an accident.

**3. Postgres keeps the byte-offset index and nothing else**, exactly as 0013 has it. Bodies never
enter the database.

**4. Live tail is untouched.** `log_tail.rs` in the control plane tails Pod stdout and calls
`LogService::append`, which broadcasts in-process for SSE. Only the **durable** write redirects to
the Depot; replay reads from it. The seam is clean because the tailing process and the storing
process were already different concerns in one binary.

**5. Logs are space-bounded and evictable, and absence is loud.** They sit on a bounded volume and
are evicted under pressure like anything else there. **An Attempt whose log bytes are gone must say
so** — "logs are no longer available" in the UI and in the API, never an empty pane.
[0027](0027-restart-semantics.md)'s rule that smart never means mysterious applies to absence as
much as to invalidation: a visible gap in the record is acceptable, a blank screen that looks like a
Step produced no output is not.

**The index asserts, the volume decides, and disagreement is the finding.** The read path has three
branches, and the third one is the entire mechanism:

| index | bytes | what the reader is told |
|---|---|---|
| no chunks | — | the Step genuinely printed nothing — an empty pane, truthfully |
| chunks | present | serve them |
| chunks | **absent** | "logs are no longer available" |

**The eviction record explains the third branch; it must never be what triggers it.** Consulting the
sweeper's record first and streaming whatever is on disk when no row is found inverts this, and it
fails precisely where it matters: a volume that was lost or reprovisioned was never evicted by
anybody, so there is no row to find, and the reader gets the blank pane this decision exists to
prevent. Nor can that case be caught by detecting the loss — **a volume's loss is not observable as
an event.** A fresh empty volume is indistinguishable from one that never held data; there is no "I
was lost" bit to read. Absence is therefore the only signal available, which is why it has to be the
authoritative one. The sweeper's record then turns *"gone"* into *"expired under policy on 3 August"*
for the cases it knows about — strictly a better message, never a precondition for sending one.

The inverse has shipped in this repo twice already — `redirect_dir`, and the copy rung's `deleted`
set — both of the form *"the record we consult is empty, therefore nothing happened."* Naming the
shape here is cheaper than finding it a third time.

**6. The Depot says at boot whether its volume is the one the index describes.** A marker written on
first init and recorded alongside the index; on boot, a missing or mismatched marker means the volume
is new. Without it, an operator's only evidence is a trickle of individually unremarkable per-Run
absences — each correctly reported, none of them saying *the storage under this Depot is not the
storage this database describes*. This is one loud line at the moment it becomes true, and
deliberately **not** a claim on any Attempt's record: per-Attempt durability is what tiering carries
([0064](0064-durability-tiering-and-the-write-path.md) part 5) for the classes that can be
re-derived. Logs do not carry it. The operator configures a durable sink or does not, and a log that
is gone is gone for wanted and unwanted reasons alike — a distinction with no available action behind
it, so the record does not spend a column on it.

**7. External log systems are an additional sink, never the system of record.** Loki,
VictoriaLogs or an object store may receive a copy for operators who want logs in their existing
stack. None of them becomes the thing Scarab reads back.
[CONTEXT.md](../../CONTEXT.md) §4.3 states that *"each Attempt owns its evidence — logs, Results,
Artifacts, workspace snapshot"*. Making an external system the system of record would make that
sentence unbackable: a green Attempt from three weeks ago whose logs vanished at day 7 under
someone else's retention policy, with Scarab's durable record pointing at data it neither governs
nor can verify. That is the same defect as declaring success before durability, and it is refused
for the same reason.

**8. An un-shipped log is pinned against eviction — when a sink exists.** Where a durable sink is
configured, a log chunk that has not yet been acknowledged by at least one sink is not evictable.
This is the mechanism first proposed for Workspace Snapshots and **rejected there**, because a
snapshot is re-derivable and a miss is recoverable. For logs it is load-bearing, because there is no
recompute path. The pin is bounded by the sink keeping up; if it cannot, eviction resumes and says
so rather than filling the volume — see Consequences.

**9. Logs get their own retention class with a long TTL**, swept as their own class by
[0050](0050-retention-and-gc.md)'s sweeper, as Artifacts already are. They are the cheapest class per
byte and the one users reach for longest after a Run.

**10. The workspace service is renamed the Data Depot.** It now holds the CAS, Snapshot Farms,
Workspace Exports and — with this ADR — the log namespace, with Cache to come
([0065](0065-retention-cache-and-rederivation.md)). "Workspace service" named it after **one of its
tenants**, and the name broke the moment a second kind of data arrived. `warm store` was considered
and rejected for naming it after **one of its properties**: it encodes a tiering decision, reads
oddly in a deployment with no independent cold tier, and would date the same way. A tier-agnostic
proper noun names the thing rather than a current fact about it.

Renamed: `--role workspace` → `--role depot`, `Role::Workspace` → `Role::Depot`,
`WorkspaceServiceConfig` → `DepotConfig`, `SCARAB_WORKSPACE_URL` → `SCARAB_DEPOT_URL`,
`SCARAB_WORKSPACE_DATA_DIR` → `SCARAB_DEPOT_DATA_DIR`, `workspaced.rs` → `depotd.rs`, the chart's
`*-workspace.yaml` objects and `workspace.*` values → `*-depot.yaml` and `depot.*`, and
`just workspace-logs`/`workspace-status` → `just depot-logs`/`depot-status`.

**Deliberately not renamed**, because they are correctly named: **Workspace**, **Workspace
Snapshot** and **Workspace Export** are domain terms about workspaces;
`SCARAB_WORKSPACE_TOKEN_SECRET` mints the token a Step Pod presents to read *workspace snapshots*
and its claims are literally snapshot `roots`, so it is the workspace token rather than the Depot's
identity; and `scarab-workspace-client` fetches workspace snapshots for the fetcher init container.
If the Depot later needs authenticated log access, that is a second credential or a scoped widening
of the existing one — a decision for then, not a rename now.

## Alternatives considered

- **Keep logs in the object store (status quo).** Simplest, and it makes object storage a
  prerequisite that is currently *unstated* — the worst combination, because the failure is silent
  and total. Rejected on that, not on cost.
- **An external log system as the system of record** (Loki, VictoriaLogs). Cheapest to operate and
  it is what most CI systems do. Rejected: it breaks attempt-owned evidence, the in-process SSE
  broadcast, and per-attempt replay in the Runs UI, and it subordinates Scarab's durable record to a
  retention policy Scarab does not own. It also costs the product's stated wedge — forge-native CI on
  a durable core is a cohesion argument, and "go look in Loki" is the seam every other CI leaks at.
- **Logs in the CAS**, reusing the whole warm/cold machinery. Tempting because it is already built.
  Rejected on measurement of the idea rather than of code: log chunks are unique, so
  content-addressing them yields no dedup, adds a hash per chunk, and still requires the ordered
  offset index — all cost, no benefit.
- **Refuse to start without a durable log sink.** Honest, and it re-imposes the prerequisite this ADR
  removes, for a class of data many operators do not need durable in a dev cluster. Rejected in
  favour of loud degradation.
- **Pin un-shipped logs forever, and backpressure Runs when the volume fills.** Never loses a log.
  Rejected: it converts a storage-configuration mistake into an outage, and trading "CI stops" for
  "logs kept" is the wrong priority. Eviction resumes and says so instead.
- **Stamp per-Attempt log durability — "were this Attempt's logs ever copied to a sink?"** — the way
  [0064](0064-durability-tiering-and-the-write-path.md) part 5 stamps snapshot durability, and on the
  same argument: an operator who adds a sink next month leaves earlier and later Attempts with
  different guarantees and identical records. **Rejected**, and the asymmetry with 0064 is
  deliberate. 0064 stamps a class that can be *re-derived*, where knowing "this one was never
  archived" tells a reader which Runs are worth re-running; for logs there is no recompute path, so
  the stamp distinguishes two states with the same available action — none. The engine would also be
  re-litigating a choice it delegated: a warm-only deployment is supported precisely because
  durability is the operator's cost decision. Part 5's disclosure duty is met by absence being loud
  at read time and by part 6 being loud at boot. Note the ingredient is nonetheless *present* — part
  8's pin already requires per-chunk sink-acknowledgement state — so this is a decision about what to
  put in the record, not about what is affordable.

## Consequences

- **The Data Depot becomes a dependency for serving logs.** Previously the control plane read them
  straight from the object store. A Depot outage now means logs are unavailable for replay, though
  live tail — which is an in-process broadcast — keeps working. That is a real availability coupling
  and it is accepted because the Depot is already in the standard path in every deployment mode.
- **The rename is an operator-facing breaking change** — env vars, chart values and the role flag all
  move. Cheap now, because nothing is released, and expensive at any later point.
- **Logs are the one class where eviction is genuine loss.** Everywhere else in the data plane
  eviction is a latency event. This is the asymmetry that earns them the pin and the long TTL.
- **A deployment with no durable sink can lose an Attempt's logs**, and must say so in the UI and the
  API rather than showing an empty pane. It says so **without knowing why** — the read path reports
  what it finds, and only the sweeper's own record can add a reason.
- **The log read path now depends on a fact it cannot cache.** "Are the bytes there?" has to be
  answered against the volume on the read, not inferred from the index or from a sweep record. That is
  a cost per replay — one stat, on a path that is already about to open the file — and it is the price
  of the third branch being correct for losses nobody recorded.
- **Two stores on one volume** means the Depot's space accounting covers both. The warm-size gauge
  already walks the volume, so it counts logs the moment they arrive there — its help text needs to
  stop implying it measures only the CAS (git-bug `1a9df08`, which already covers the Farm half of
  the same drift).

## References

- [0013](0013-history-and-observability.md) — the log pipeline: chunked gzipped blobs + Postgres offset index
- [0027](0027-restart-semantics.md) — smart never means mysterious; applied here to absence
- [0050](0050-retention-and-gc.md) — retention classes and the GC sweeper
- [0053](0053-observability-and-lifecycle.md) — `/metrics`, `/readyz`, structured logs
- [0061](0061-workspace-data-path.md) — the warm/cold tiers; the service renamed here
- [0062](0062-workspace-export-lazy-without-node-driver.md) — Snapshot Farms and Workspace Exports,
  the Depot's other tenants
- [0064](0064-durability-tiering-and-the-write-path.md) — what gates `Succeeded`, and how bytes reach
  the tiers
- [0065](0065-retention-cache-and-rederivation.md) — Cache, `RetentionProfile`, and human-triggered
  re-derivation
