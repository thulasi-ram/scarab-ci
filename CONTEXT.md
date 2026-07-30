# Scarab — Context & Ubiquitous Language

> Scarab is a **forge-native CI engine for Kubernetes, built on a durable core** — written in
> Rust. The field splits in two: durable *workflow engines* (Argo, Tekton, Temporal) that
> aren't CI, and forge-native *CIs* (GitHub Actions, Woodpecker) whose orchestrator is a job
> runner. Scarab is designed to be both — a cohesive forge-native CI on a crash-safe state
> machine (Postgres; the DBOS/Temporal pattern).
>
> **Thesis:** *A run is durable state, not a fire-and-forget process.*
>
> **Architectural wedge vs public headline:** durable execution is the *architectural* wedge —
> the hard core every ADR is judged against ([ADR-0001](docs/adr/0001-ci-as-durable-execution.md)).
> It is **not** the public headline: it's table stakes against Argo/Tekton/Temporal, so we sell
> **cohesion** (forge-native product on a durable engine), not the bare word "durable". Taglines,
> the AI-era angle, the Woodpecker lineage, and the honest red lines live in
> [docs/positioning.md](docs/positioning.md). This doc is the internal anchor.

This document is the **anchor** for the codebase: the ubiquitous language every crate,
API, ADR, and UI view must use, plus the system overview and non-goals. If a term here
conflicts with a name in code, the code is wrong. Decisions are recorded as ADRs in
[`docs/adr/`](docs/adr/).

The name: the working directory is historically `scrarab-ci`, but the product, crates,
CLI, and env prefix are all **`scarab`** (`SCARAB_*`, `scarab`, `scarab-*`). The scarab is
the Egyptian symbol of resurrection and of rolling the ball uphill — apt for a CI whose
wedge is *resurrecting crashed runs* and rolling builds durably forward.

---

## 1. The wedge (why Scarab exists)

Durability is the differentiator. Everything else — forge integration, k8s-native
execution — is a *means*, not the point. Because the orchestrator is a **crash-safe,
resumable state machine**, the following are *derived* properties rather than bolted-on
features:

- **Resume** a pipeline after a control-plane crash/failover — from the last durable state.
- **Rerun** a single step — it is a re-creatable, individually-addressable object.
- **Time-travel** — inspect the DAG/logs as they were at any point.
- **Suspend** indefinitely on a `gate` (human approval, timer, external event) — for
  seconds or for weeks — at nearly zero cost, because state lives in Postgres.

## 2. The durability contract (read this twice)

Durable execution here means a **durable orchestrator**, *not* replayable step interiors.

- **Durable & exactly-once:** the DAG state machine — which steps ran, their exit codes,
  their output manifests — and the append-only event log.
- **Not replayable:** the *interior* of a step. A step is an opaque black box
  (`docker push`, `terraform apply`) that is **re-executed wholesale** on restart.
- **Execution guarantee:** **at-least-once** per step. Idempotency of *external effects* is
  a **step-author contract**, for which Scarab provides tooling (content-addressed skip,
  **fencing tokens**), not a magic promise. See ADR-0021.

## 3. Non-goals (what Scarab deliberately is NOT)

- **Not multi-backend.** Kubernetes is the **only** execution substrate. No Docker-socket,
  local, or SSH backends. (This — not "does many things" — is the "Woodpecker baggage" we shed.)
- **Not a hostile-tenant public SaaS (v1).** Soft multi-tenancy (orgs/repos/teams under
  RBAC, namespace-per-run) only. gVisor/Kata hostile isolation is deferred.
- **Not a general durable-execution engine.** We own a *bounded* CI state machine on
  Postgres (the "DBOS pattern, in Rust"), not a Temporal/Restate-style generic engine.
- **Not forge-coupled for identity.** IAM is forge-**agnostic** (OAuth/OIDC + Scarab-native RBAC).
- **Not a replay-of-step-logic system.** No deterministic replay of user shell.

---

## 4. Ubiquitous language

### 4.1 Definition plane (authored, in-repo, declarative)

| Term | Meaning |
|---|---|
| **Pipeline** | Top-level authored unit: a **flat recursive DAG** of Steps. Lives in-repo (`.scarab/`), read at the triggering ref, compiled to IR. |
| **Step** | The atomic unit and the **Pod boundary**. An OCI image + command + I/O convention. The unit of re-execution. |
| **`needs`** | A dependency edge between Steps. Parallelism is *emergent* from the DAG (anything not transitively dependent runs concurrently). |
| **`invoke`** | A built-in Step kind that references **another Pipeline in the same Run**. **Compile-time inlining, not a runtime object** (ADR-0025): the referenced Pipeline's Steps are flattened into the caller's DAG, id-namespaced by the invoke-step id (`deploy/build`). Composition = recursion; replaces "reusable workflows" and "composite actions". A "job" is a named subgraph — sugar, not structure. |
| **Library pipeline** | A Pipeline authored under **`.scarab/lib/`** that exists to be `invoke`d, not triggered. Referenced by repo-relative path, read at the caller's ref (atomic git versioning; no registry, no semver). Out of trigger discovery by convention (subdir) and by carrying no matching `on:`. |
| **Matrix** | An orthogonal *modifier* on a Step: expands to N instances (cartesian product) at **submit time** (static in v1). Applied to an `invoke` step, it fans out N copies of the whole referenced subgraph — "run this reusable subgraph once per coordinate". |
| **Trigger (`on:`)** | What starts a Pipeline: `push`, `pull_request`, `tag`, `release`, comment-command, `cron`, `manual`, `api`, `upstream`. Matched against the normalized **Event** via CEL. |
| **Gate** | A built-in Step kind: a **durable suspend point** awaiting human approval, a timer, or an external event. |
| **Service** | A Scarab-provisioned backing container (db/redis/etc.) a Pipeline needs at runtime, in one of two forms — **Sidecar service** or **Shared service** (ADR-0058). **Never a `needs`-able DAG node**: a Service is infrastructure *for* Steps, not a Step (it has no id to depend on, no exit code, no Attempt/evidence of its own). |
| **Sidecar service** | The default form: a throwaway container **co-located inside the declaring Step's Pod**, reachable at `localhost:<port>`, alive only for that one Step's execution. **Fenced by inheritance** — shares the Step's Attempt identity and teardown, no cross-Pod networking, no k8s `Service` object. The common case: one test Step + a throwaway DB. Two Steps that each declare Postgres each get their own. |
| **Shared service** | The opt-in form: a **Run-scoped** standalone Pod with a cluster DNS name, reachable by the Steps that opt in, torn down when the Run (that Take) ends — riding namespace-per-run teardown, not a refcount. **A fresh instance per Take**: a Rerun's Take gets a new instance keyed by `{run, take}` and never sees the prior Take's writes (Takes are independent — ADR-0056). **Unfenced external mutable state**: intra-Take idempotency across auto-retries and parallel writers is a **step-author contract** (ADR-0021), exactly as for any other external effect — Scarab does not fence it. |
| **`clone`** | A built-in Step kind: forge-aware checkout (ADR-0008, 0045). Runs `git` in a restricted Pod, pinned to the resolved commit SHA, producing the Run's **workspace root** that downstream Steps inherit via `needs`. Zero-config — repo/ref/SHA/token are implicit from the Run's trigger context; `depth`/`submodules`/`lfs`/`ref` are optional. Authored explicitly (no implicit checkout); a `push`/`pull_request` Pipeline with no `clone` step is a lint warning, not an error. |
| **Environment** | A first-class deployment target (staging/prod) with **protection rules**: approvers, wait timer, allowed refs, concurrency, secret scope, OIDC subject, deployment history, and a **privilege whitelist** (which image digests may run with which governed grants — ADR-0039). |
| **Grant** | A named, closed-vocabulary escalation above the hardened "restricted" step baseline (ADR-0039). `run-as-root` (self-service — does not escape the sandbox); `add-capabilities` and `privileged` (**governed** — require an Environment whitelist entry keyed on the image **digest**, Administer-only). Requested by the pipeline author, granted by the Environment admin, admitted fail-closed. A grant is a ceiling, not a default. |
| **`placement_profiles`** | A Step field (ADR-0055): a list of **`PlacementProfile`** names. Their admin-defined k8s overlays are merged (in listed order) onto the Step's Pod; empty → the `default` profile. The author **names profiles**, never raw k8s → no topology/k8s leak. Sibling of **`resources`** (exact `cpu`/`memory` — no `size` tiers, ADR-0055). |
| **PlacementProfile** | An **operator-owned, cluster-scoped** named bundle (ADR-0055) mapping a name → concrete k8s **Placement** (nodeSelector/tolerations/runtimeClass/annotations — an *opaque overlay*, not a fixed schema). Lives in Scarab operator config (gitops), **not** per-Project. One may be `default`. It is *where a Step lands* — **not** an **Environment** (deploy governance), **not** settings of a **Run** (a Pipeline instance). Composed atop the control-plane **placement baseline** (default tolerations/nodeSelector/resources stamped on *every* step Pod). |
| **RetentionProfile** | An **operator-owned, cluster-scoped** named bundle (ADR-0065) mapping a name → a retention policy: the Data Depot's space budget, per-class TTLs, which directories are Cache-eligible, and the thresholds at which a Snapshot is dropped in favour of re-deriving it. Lives in Scarab operator config (gitops), **not** per-Project. A Pipeline may **name** one; it never defines the values — so no byte cost enters authored YAML. Deliberate sibling of **PlacementProfile**: same pattern, different axis (*how long things are kept* vs *where a Step lands*). Composed with ADR-0061's manual **pin** ("keep this Run's workspaces"), which is the per-Run escape hatch for investigations. |
| **`k8s_overlay`** | A Step's **governed escape hatch** (ADR-0055): a raw pod-spec fragment merged **last** onto the Pod. Carries **no authority** — like a governed **Grant**, it takes effect only if the Run's Environment permits raw overlays, else the Run is rejected **fail-closed**. The `k8s_` prefix marks the backend-coupling (won't run on the local executor). For the rare *dynamic per-job* k8s need; static placement belongs in a `PlacementProfile`. |

### 4.2 Data plane (nine distinct concepts — never conflate)

| Term | Scope | Lifetime | Purpose |
|---|---|---|---|
| **Parameter** | launch-time input, supplied from *outside* the run | resolved once at launch, then persisted for the run's life | a typed value a launcher supplies to start a Pipeline. Declared in the Pipeline's `interface.inputs`; each is `required` (static bool) with an optional `default` and optional `validate:` CEL predicate. Supplied by a human/API on a `manual`/`api` launch **and** by an `invoke:` caller's `with:` — one declaration, one env rail (`SCARAB_PARAM_<NAME>`), one launch-time CEL binding `${{ inputs.<name> }}`. **Not** the per-Step workspace `inputs:` (ADR-0007), which is a different concept sharing the word. |
| **Workspace** | one Step's execution | ephemeral (dies with the Attempt) | the **mutable** filesystem a Step executes in. A Step may write anywhere in it; nothing outside the Step reads it directly. **Where its bytes live is not part of the concept** — they may sit on the Pod's node, on the workspace service, or be split across both (ADR-0061), and a Step cannot tell which. The Workspace is the *view* a Step executes against; it never outlives the Attempt. |
| **Workspace Snapshot** | intra-run, flows along DAG edges | retained per policy, then archived, then gone (ADR-0061) | the **immutable**, content-addressed tree (per-file merkle CAS) that crosses a DAG edge and that an Attempt owns as evidence. Implicit-by-default (inherit `needs`), explicit-on-demand (`inputs:`/`outputs:`). Two coordinates, never conflated: the **snapshot root** is *where the bytes are* — the address a Step materialises, GC marks from, and an Attempt records; the **content identity** is *what the bytes are* — the same merkle fold with mtimes dropped, which is what restart invalidation compares (ADR-0027, 0061 s8). A root moves when a file's mtime moves and an identity does not, so **only the root is an address**: nothing is stored under an identity and nothing can be fetched by one. |
| **Workspace Export** | one Attempt | dies with the Attempt; reaped, never archived | the **delivered** form of a **Workspace**: a per-Attempt, writable, network-mounted view of one **Workspace Snapshot** that a Step Pod receives *as* its Workspace (ADR-0062). Distinct from the Snapshot it is a view of, and the distinction has consequences: losing an Export fails that Attempt, which retries; losing a Snapshot widens a rerun's scope (ADR-0061 retention). Its address is a **capability** — unguessable, TTL'd to the Step deadline, client-pinned — not a name, because the mount protocol cannot carry a per-Step identity. |
| **Change set** | one Attempt | derived; folded into a Snapshot, then discarded | the set of paths an Attempt wrote — what the drain hashes, and the reason it need not re-read an unchanged tree. **Known** where the Attempt had a **Workspace Export**: the `overlayfs` upper layer is the kernel's own record of what was touched (ADR-0062), so it is exact and rests on nothing. **Derived** where there was none (the local executor): each file's `(size, mtime, ctime)` compared against the input manifest. The derived form is sound but *conditionally* — it assumes ctime cannot be forged (no syscall sets it), a capture stamped after materialisation completed, a filesystem whose ctime is not coarse, and a Step that cannot move the clock back. `(size, mtime)` alone is **not** sound: `cp -p`, `touch -r` and `tar -xp` defeat it deterministically, not as a race. |
| **Result** | intra-run, flows along DAG edges | ephemeral | small typed values (a version, a bool) for params/conditionals. **No bytes anywhere** — the value *is* JSONB in Postgres (`attempts.results`); nothing is stored in the CAS or the object store. Interpolated as `${{ results.<id>.<name> }}` (ADR-0041, renamed from `outputs.` — see **`outputs:`** below). |
| **Artifact** | output of record | retained (TTL), downloadable, UI-visible | binaries, reports, coverage, images. Bytes in the object store; Postgres holds only metadata. **Immutable per Attempt** — a retry never overwrites a prior Attempt's version; the name-addressed record resolves to the latest *successful* Attempt's version. |
| **Cache** | cross-run | best-effort, evictable | `~/.cargo`, `node_modules` — **author-declared** directories under a key (e.g. a lockfile hash), restored at Step start and saved at Step end (ADR-0065). Stored as a content-addressed tree, so it rides the same Farm/Export machinery as a Snapshot and materialises lazily. **Not** correctness-critical and **never evidence** — which is what puts it outside the durability rules. A keyed *directory* cache, never a shared mutable *mount*: two concurrent Runs writing one `node_modules` is corruption. |
| **`outputs:`** | a Step field | — | the workspace-relative **paths** a Step publishes downstream (ADR-0007) — bytes on a filesystem, narrowing what flows along an edge. A *precision* tool for cache keys, safe fan-out and remapping; **never** something an author declares for speed (ADR-0007 amendment, ADR-0065). **Not** a **Result**: until 2026-07-28 the Result interpolation namespace was also spelled `outputs.<id>.<name>`, so one word named both a list of file paths and a namespace of typed scalars. The namespace is now **`results.`** and this term means paths only. |
| **Step logs** | one Attempt | own retention class, long TTL | an Attempt's stdout/stderr. Bytes on the **Data Depot**, keyed `{run, step, attempt}` and **not** in the CAS — content addressing buys nothing for an append-only stream (ADR-0063). Postgres holds byte offsets only, never bodies. **The one class that cannot be re-derived**: re-running a Step yields *different* logs, so eviction here is real loss, not a latency event — hence pinned until a durable sink acknowledges them, and when the bytes are **gone for any reason at all**, said so out loud — absence is authoritative and the eviction record only explains it, because a lost volume was evicted by nobody. External log systems (Loki, VictoriaLogs) are **additional sinks, never the system of record**. |

### 4.3 Run-time / instance plane (what the durable engine tracks)

| Term | Meaning |
|---|---|
| **Run** | A durable *instance* of a Pipeline for a specific Event/commit. Stores the compiled IR + `{ir_version, event_schema_version}` (self-describing). |
| **StepRun** | A durable instance of a Step within a Run. |
| **Attempt** | A single execution of a StepRun. **A Rerun or Retry creates a new Attempt.** Each Attempt owns its evidence — logs, Results, Artifacts, workspace snapshot — keyed by `{run, step, attempt}`, and records which upstream Attempts it consumed. |
| **Superseded** | **Intrinsic terminal** outcome of an Attempt **cut short while running** because a human **reran or retried an ancestor** (the cascade re-armed this step): its live Pod is torn down (SIGTERM + grace) and any late verdict is rejected by the fence, so it could not honestly finish and a fresh Attempt takes its place. It is **the Attempt's own fate** — shown as *superseded* in every view, never repainted as the transient "running" it passed through nor as a later Attempt's success (ADR-0056). Distinct from **Cancelled** — a *deliberate* stop with **no** replacement (`cancel_run_request`) — from **Failed** (ran and errored), and from **Not-run** (never started, no Pod). Evidence retained like any other. |
| **Not-run** | Terminal display state of a step that **never started** in a version **superseded before its turn** (an ancestor was rerun while this step was still upstream-blocked). Distinct from **Skipped** (a `when:` was false or a dependency died), **Pending** (implies a future that *will* happen — it will not, in this version), and **Superseded** (had a live Pod and may have partially side-effected — a not-run step did nothing). Per-version, derived from the event log (ADR-0056). |
| **Auto-retry** | The **engine** re-executing a not-yet-succeeded StepRun within its retry budget (ADR-0047). Machine-initiated, never on a succeeded or in-progress step; adds an Attempt but does **not** fork the Run (stays within the current Take). Its human counterpart — same version, no fork, on a Failed step — is a manual **Retry** (one concept, two triggers). |
| **Retry** (manual) | The **human** trigger of a Retry: re-executing a **Failed** step as **another Attempt in the *current* version** (Take) — **no fork**: it reopens the settled Run (`Failed → Running`) and re-arms the target plus its dependent cascade (ADR-0027). Backend `retry_step`; event `StepRetryRequested`. The human counterpart of **Auto-retry** — one concept, two triggers (manual vs auto); the authored `retry:` policy governs the auto side. Contrast **Rerun** (forks a new version). Offered on Failed steps only. |
| **Rerun** | The **human** action of re-executing a step as a **new Run version** — **forks a new Take** (a new history row) and cascades to descendants (ADR-0027). Offered on **any** target, terminal or not; an in-flight member of the invalidation set is **voided** (torn down, marked **Superseded**; ADR-0056). Backend `rerun_step`; event `RunRerunRequested`. Contrast **Retry** (another Attempt in the *same* version, Failed-only). _Avoid_: "restart" (user-facing). |
| **Cascade** | The downstream half of a Rerun: descendants dragged into re-execution because an ancestor was rerun (per smart invalidation, ADR-0027) — not individually chosen by the human. A cascade Attempt's cause points back at the rerun that triggered it ("⟵ b was rerun"). |
| **Shadowed** | A **finished-successful** Attempt whose result is retained and readable but is **no longer the "of-record" latest** — a newer Attempt (from a Rerun/cascade) replaced its role. Distinct from **Superseded** (never finished — cut short while running). ADR-0056's word for non-latest Artifact versions; generalized here to any evidence. |
| **Take** | The span of a Run between two **Reruns**. A Rerun closes the current Take and opens the next (a new history row); auto-retries, crash re-adoptions, and dead-letters happen *within* a Take. The run-level version unit ("Take 2 of 3") — a bookmark over a frontier of Attempts, not a copy of anything. **Internal term** — the end-user run-history language is still being designed (does not surface "Take"/"Attempt" verbatim; the surfaced form is a row per Rerun). |
| **Event log** | Append-only, versioned, immutable record of state transitions. Drives SSE, timeline, audit, time-travel. State tables are the source of truth; the event log is derived-but-durable (via the outbox). |
| **Admission** | *Scarab's* scheduling decision — which Runs/Steps are allowed to run (concurrency groups, fairness, priority, backpressure). Distinct from k8s **Placement** (node fit). |
| **Fence** | A monotonic `{run, step, attempt}` token handed to each Attempt and to cooperating external systems (idempotency keys, digest/generation checks) to neutralize the double-effect hazard. |

### 4.4 Structural / architectural

| Term | Meaning |
|---|---|
| **Port** | A domain-owned trait describing an external capability in domain language (`ForgePort`, `SecretProvider`, `ObjectStore`, `Cas`, `Executor`, `Db`, `Clock`, `OidcIssuer`). |
| **Adapter** | A concrete implementation of a Port, in a **separate vendor crate** (`scarab-forge-github`, `scarab-db-postgres`, …), holding all infra deps. |
| **IR** | The typed, versioned **Pipeline Intermediate Representation** — the *real* DSL. YAML is one frontend; the API schema *is* the IR. |
| **Forge** | The domain *concept* of a source-of-repos/sink-of-status (GitHub, GitLab, Forgejo). Vendors are **adapters**, never their own domain. |
| **Data Depot** | The long-lived Scarab-operated service that holds the data plane **near where Steps run**: the warm CAS, **Snapshot Farms**, live **Workspace Exports**, the **Step log** namespace, and **Cache** (ADR-0061, 0062, 0063, 0065). Same binary as the control plane, different role (`--role depot`) and **no durable core** — it never connects to Postgres. Was "the workspace service" until 2026-07-28; renamed because that named it after *one of its tenants* and broke as soon as a second kind of data arrived. `warm store` was rejected for naming it after one of its *properties*, which encodes a tiering decision and dates the same way. **Not** a **Workspace**, a **Workspace Snapshot** or a **Workspace Export** — those are data it holds, and they keep their names. |

### 4.5 Tenancy & forge binding

| Term | Meaning |
|---|---|
| **Org** | A top-level tenant. Owns Projects. The Scarab tenancy boundary — **not** the forge's `owner` namespace (one Org may span a GitHub org *and* a Forgejo instance). |
| **Project** | Scarab's **governed unit of CI** and the **aggregate root** beneath an Org: it binds a **source** (a `RepoRef` on a forge, via a `ForgeConnection`) to its **governance** (Environments + `ProtectionRules`, privilege whitelist, secret scope, OIDC subject) and owns the **Pipelines and Runs** produced from that source. RBAC is enforced at the Project scope. **1:1 with a repo** in v1 (monorepo per-subdir governance deferred to an optional path scope). There is no separate governed "Repo" entity — a Project *is* the governed repo. |
| **`RepoRef`** | A **forge coordinate** — `{owner, name}` as the forge addresses a repository, plus the forge it lives on. External and mutable (a forge rename/transfer changes it). Carried by `Event`/`Status`. Lives in `scarab-forge`; it is the *only* concept named "Repo". Resolved to a Project via a `ForgeConnection`. |
| **Actor** | The identity that caused an **Event** — the forge principal who pushed, opened the PR, cut the tag, or dispatched a `manual`/`api` run (a vendor login, normalized). Carried on every `Event` variant (not just dispatch), stamped onto the Run at creation as a discrete fact (beside the trigger kind, ref, and commit SHA — not bundled), and surfaced in the UI (labelled "author"). A forge-originated fact, **not** a Scarab RBAC principal — do not conflate with the authenticated caller in an authorization check. |
| **Headline** (trigger title) | The one normalized human line that says *what a Run is about*, disambiguated by trigger kind: a push's commit **subject**, a pull request's **title**, or a `manual`/`api` dispatch **reason** (a `cron` schedule / `upstream` run id for those). A forge-originated, **display/audit-only** provenance fact — a sibling of **Actor**, stamped on the Run at creation, never load-bearing and **never entering trigger-matching or `${{ }}` interpolation**. Surfaced as the secondary line of the Trigger cell (ADR-0057). |
| **`ForgeConnection`** | A configured link between Scarab and a forge account (a GitHub App installation, a Forgejo connection): `{forge_kind, base_url, credential_ref}` owning a set of `RepoRef`s. The **seam** that resolves `RepoRef` → Project and supplies credentials. The *type* is pure (`scarab-forge`, holds a credential **reference**, not secret bytes); persistence is a store **port** + adapter; credentials live in `SecretProvider`. |

---

## 5. System overview

```
 forge (webhook/API) ──▶ [webhook role] ──normalize──▶ Event
                                                         │
 UI (SolidJS) ─┐                                         ▼
 CLI (scarab) ─┼─▶ REST/OpenAPI + SSE ─▶ [api role] ─▶ Run created (durable)
 3rd parties ──┘        (one dogfooded API)              │
                                                         ▼
                                    [scheduler role]  Admission (durable)
                                    concurrency groups · fairness · auto-cancel
                                    · priority · backpressure   │
                                                                ▼ (outbox)
                                    [executor role]  Executor port
                                    ├─ scarab-executor-k8s
                                    └─ scarab-executor-local ─ kind/local
                                             │        │
   Postgres ◀── state tables + event log ────┘        │  creates + watches
                                                      │  ROOT HASHES ONLY (tens of bytes)
                                                      ▼
                                                 [ Step Pod ]  one per Step
                                                      │  ▲
                                 BULK snapshot bytes  │  │  ✅ feed  (Scarab-owned fetcher)
                                                      ▼  │  ⛔ drain (s3-drain, NOT built)
                                          [workspace role]  the workspace service (ADR-0061)
                                          warm merkle CAS on a PersistentVolume, one per
                                          failure domain · bounded by SPACE · no Postgres
                                                      │
   Object store (S3/MinIO) ◀── logs · artifacts · cache ┘
                           ◀── Workspace Snapshots: the COLD archive, bounded by TIME
                           ◀── ⚠ BULK, TODAY: the DRAIN tars every byte out of the Step Pod
                               over `kubectl exec` into the control plane, which hashes it
                               and writes it here. Artifact harvest is a third `exec` tar.
                               So ADR-0061 part 3 is HALF done: the read path left the
                               control plane, the write path has not.
```

Which edge carries bulk is the whole point of ADR-0061, so the four labels are load-bearing.
**control plane ↔ Step Pod** carries root hashes only — that property belongs to *this* edge
and no other. **Step Pod ↔ workspace service** is the bulk edge by design; its feed half is
real, its drain half is s3-drain and is not built. **control plane → object store** is bulk
*today*, for the drain, and a diagram without that edge asserts part 3 is finished.
**control plane ↔ workspace service** is small and off the critical path: Browse reads
snapshots through the service (falling through to the object store when it cannot answer),
and the drain seeds it as a warm tier.

- **One binary, many roles**
  (`scarab-server --role converged|api|scheduler|executor|webhook|workspace`).
  Postgres (outbox) is the coordination bus; no service-to-service RPC required internally.
  `workspace` is the one **data-plane** role: it never connects to Postgres and never runs
  a migration (ADR-0061), so it keeps serving a Step its inputs through a database outage.
- **Control-plane / data-plane split:** the durable *brain* records intent ("Step S should
  run"); the *executor* observes it, creates the Pod, watches it, writes back terminal state.
  The intent is that bulk **data** does not cross the brain — the control plane exchanges root
  hashes and Step Pods talk to the workspace service. **Half true today**: ADR-0061 supersedes
  the `kubectl exec` tar tunnel on the **feed** side, where a Scarab-owned fetcher has replaced
  it. The **drain** still tars every byte out through the Kubernetes API server, and artifact
  harvest is a third `exec` tar; both go with s3-drain. Do not read this bullet as a
  description of the running system's write path.
- **Three stateful components — and the third is deliberate.** Postgres (state + event log),
  an object store (blobs), and the **workspace service**'s persistent volume.
  [ADR-0004](docs/adr/0004-execution-topology.md) called object storage "the second (and
  last)"; [ADR-0061](docs/adr/0061-workspace-data-path.md) knowingly adds a third, because
  one standard path beats a fast path plus a fallback path, and because binding volumes to a
  long-lived service removes every cost PVCs have when bound to short-lived Step Pods.
  The two storage tiers carry **different promises**, and that asymmetry is load-bearing:
  the workspace service's volume is bounded by **space** and promises nothing (a miss is
  slower, never wrong), while the object store is bounded by **time** (a retention TTL) and
  **is** the guarantee users are given. Nothing else is stateful.

## 6. Crate map (hexagonal, domain-first)

Pure domain crates carry **zero infra deps** (compiler-enforced by crate boundaries):

```
scarab-engine      durable core: DAG state machine, scheduler, ports (Db, Clock, Executor). DST lives here.
scarab-pipeline    IR, YAML→IR compile, CEL binding, validation
scarab-forge       ForgePort + canonical Event/Status/RepoRef + ForgeConnection type
scarab-identity    Authenticator, OidcIssuer, RBAC
scarab-secrets     SecretProvider
scarab-storage     ObjectStore + Cas (merkle content-addressing)
scarab-project     Org/Project/Environment + protection rules (Project = governed repo; ADR-0046)
```

Adapter crates (infra lives here; one per vendor):

```
scarab-db-postgres · scarab-forge-github · scarab-forge-forgejo · scarab-secrets-postgres
scarab-storage-s3 · scarab-executor-k8s · scarab-executor-local
scarab-workspace-client   the workspace service's client (ADR-0061), behind BOTH
                          scarab-storage ports: `Cas` (so Browse and the executor
                          can point at the service with no call-site change) and
                          `ContentSource` (byte ranges, sizes without reads,
                          batched existence, one-call tree manifests — what a lazy
                          mount needs and `Cas` structurally cannot express).
                          Over reqwest, with NO kube dep, so the node driver can
                          use it without linking the kubernetes executor.
```

Testing substrate + composition:

```
scarab-testkit     FakeClock / InMemoryDb / FakeExecutor  (DST + classical tests)
scarab-server      composition root: mounts each domain's api → one axum app + OpenAPI + SSE
scarab-cli         generated-from-OpenAPI CLI
```

## 7. Key invariants (must hold from commit one)

1. **Forward progress or explicit dead-letter.** A Run never loops forever; poison steps
   hit max-attempts and dead-letter with diagnostics.
2. **Exactly-once admission, at-least-once step.** Enforced by outbox + idempotency keys;
   proven by crash-interleaving tests on the pure `scarab-engine`.
3. **Version tolerance from day one.** Event log + IR carry version stamps; migrations are
   expand-contract; Runs are self-describing. Gates make Runs outlive deploys, so
   resume-across-upgrades is the normal case, not an edge case.
4. **Pure domain crates import no infra.** If `scarab-engine`'s `Cargo.toml` ever lists
   `sqlx`/`kube`/`reqwest`, that is a bug.
5. **The UI eats the same API as everyone else.** No private UI backchannel.

---

## 8. Testing philosophy (see ADR-0017)

Classical (Detroit-school): real collaborators in-process, **mock only true externals** at
the adapter boundary. **Minimal in v1** — enough integration tests to catch glaring bugs;
**grow the suite from real bugs** (each fix leaves a regression test). E2e per layer, a few
genuine cross-layer e2e. Exception: 2–3 targeted crash/resume tests guard the durability
wedge. Don't fight test infra; keep velocity.

Canonical tier names (rules in ADR-0017's addendum): **unit**, **functional** (real
router + engine in-process, fakes at ports), **kind** (executor layer against a real
apiserver), **stack e2e** (cross-layer, nightly); UI pyramid: **no-DOM ≫ DOM ≫ browser**.

## 9. Roadmap (tracer-bullet vertical slices)

1. **Durable core skeleton** — `POST /runs` (inline 1-step) → admit → k8s Pod → logs
   (SSE + object store) → terminal → **kill control-plane mid-run, resume exactly-once.**
2. IR + YAML + CEL; multi-step DAG; content-addressed workspace; restart-a-step.
3. GitHub adapter + identity: webhook → in-repo `.scarab` → run → checks back; OAuth login.
4. Scheduler richness + gates: concurrency groups, auto-cancel, fairness, priority; Environments/approvals.
5. Secrets + OIDC issuer + BuildKit: encrypted secrets, keyless federation, fork-PR lockout, image builds.
6. UI (SolidJS): live DAG, logs, rerun/resume, time-travel timeline.
7. Local exec + CLI polish + provenance/signing fast-follow.
