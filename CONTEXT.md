# Scarab — Context & Ubiquitous Language

> Scarab is a **forge-native CI engine for Kubernetes, built on a durable core** — written in
> Rust. The field splits in two: durable *workflow engines* (Argo, Tekton, Temporal) that
> aren't CI, and forge-native *CIs* (GitHub Actions, Woodpecker) whose orchestrator is a job
> runner. Scarab is designed to be both: a cohesive forge-native CI on a crash-safe state
> machine (Postgres; the DBOS/Temporal pattern).
>
> **Thesis:** *A run is durable state, not a fire-and-forget process.*
>
> **Architectural core vs public headline:** durable execution is the *architectural* core,
> the property every ADR is judged against ([ADR-0001](docs/adr/0001-ci-as-durable-execution.md)).
> It is **not** the public headline: Argo, Tekton and Temporal all have it, so we sell
> **cohesion** (a forge-native product on a durable engine) rather than the word "durable". Taglines,
> the AI-era angle, the Woodpecker lineage, and the known limits live in
> [docs/positioning.md](docs/positioning.md). This doc is the internal reference.

This document is the **reference** for the codebase: the ubiquitous language every crate,
API, ADR, and UI view must use, plus the system overview and non-goals. If a term here
conflicts with a name in code, the code is wrong. Decisions are recorded as ADRs in
[`docs/adr/`](docs/adr/).

The name: the working directory is historically `scrarab-ci`, but the product, crates,
CLI, and env prefix are all **`scarab`** (`SCARAB_*`, `scarab`, `scarab-*`). The scarab is
the Egyptian symbol of resurrection and of rolling the ball uphill, which suits a CI built
around *resurrecting crashed runs* and rolling builds durably forward.

---

## 1. Why Scarab exists

Durability is what sets Scarab apart. Everything else (forge integration, k8s-native
execution) is a means, not the goal. Because the orchestrator is a **crash-safe,
resumable state machine**, the following are *derived* properties rather than bolted-on
features:

- **Resume** a pipeline after a control-plane crash/failover — from the last durable state.
- **Rerun** a single step — it is a re-creatable, individually-addressable object.
- **Time-travel** — inspect the DAG/logs as they were at any point.
- **Suspend** indefinitely on a `gate` (human approval, timer, external event), for
  seconds or for weeks, at nearly zero cost, because state lives in Postgres.

## 2. The durability contract (read this twice)

Durable execution here means a **durable orchestrator**, *not* replayable step interiors.

- **Durable & exactly-once:** the DAG state machine — which steps ran, their exit codes,
  their output manifests — and the append-only event log.
- **Not replayable:** the *interior* of a step. A step is an opaque black box
  (`docker push`, `terraform apply`) that is **re-executed wholesale** on restart.
- **Execution guarantee:** **at-least-once** per step. Idempotency of *external effects* is
  a **step-author contract**, for which Scarab provides tooling (content-addressed skip,
  **fencing tokens**) but no guarantee. See ADR-0021.

## 3. Non-goals (what Scarab is NOT)

- **Not multi-backend.** Kubernetes is the **only** execution backend. No Docker-socket,
  local, or SSH backends. (This, not "does many things", is the "Woodpecker baggage" we dropped.)
- **Not a hostile-tenant public SaaS (v1).** Soft multi-tenancy (orgs/repos/teams under
  RBAC, namespace-per-run) only. gVisor/Kata hostile isolation is deferred.
- **Not a general durable-execution engine.** We own a *bounded* CI state machine on
  Postgres (the "DBOS pattern, in Rust"), not a Temporal/Restate-style generic engine.
- **Not forge-coupled for identity.** IAM is forge-**agnostic** (OAuth/OIDC + Scarab-native RBAC).
- **Not a replay-of-step-logic system.** No deterministic replay of user shell.
- **Not a CD tool.** Scarab governs and runs the Run that deploys, and keeps no state about
  deployments between Runs. What is live where is the CD tool's to answer (ADR-0074).

---

## 4. Ubiquitous language

### 4.1 Definition plane (authored, in-repo, declarative)

| Term | Meaning |
|---|---|
| **Pipeline** | Top-level authored unit: a **flat recursive DAG** of Steps. Lives in-repo (`.scarab/`), read at the triggering ref, compiled to IR. |
| **Step** | The atomic unit and the **Pod boundary**. An OCI image + command + I/O convention. The unit of re-execution. |
| **`needs`** | A dependency edge between Steps. Parallelism is *emergent* from the DAG (anything not transitively dependent runs concurrently). |
| **`invoke`** | A built-in Step kind and the **one way to reuse**: it names a target by prefix, `file:<path>` (a **Library** in the repository), `lib:<publisher>/<name>` (a Library installed in the Org) or `plugin:<installation>/<gate>` (a **plugin gate**). **Compile-time inlining, not a runtime object** (ADR-0025, 0076): the target's Steps are flattened into the caller's DAG, id-namespaced by the invoke-step id (`deploy/build`), and its `with:` binds the Library's inputs. Composition = recursion; replaces "reusable workflows" and "composite actions". A "job" is a named subgraph — sugar, not structure. Remote `invoke` by URL is rejected. |
| **Library** | A YAML file marked `kind: library` that exists to be `invoke`d, never triggered: an `interface` and Steps. Reached by `file:` when it lives in the repository (written there or copied in, read at the caller's ref) or by `lib:` when an Org admin installed it. An installed Library may `invoke` only installed Libraries and plugin gates, so a repository cannot change its Steps. Not an OCI artifact: in Scarab an image reference means "this runs". _Avoid_: library pipeline, composite action. |
| **Publisher** | The first half of an installed Library's name (`acme` in `lib:acme/deploy-helm`): by default the owner of the git source it was installed from. Scarab's own Libraries follow the same rule (`lib:thulasi-ram/clone` while the repository lives there). A name proves nothing about origin: trust comes from the **source** and the content hash. No publisher name is reserved. |
| **LibraryVersion** / **LibraryInstallation** | A LibraryVersion is one Library's content, stored by content hash, with the git source it came from. A LibraryInstallation is an Org's choice of which version a name resolves to; it is usable by every Project in the Org until a Policy narrows it. An Org admin may **yank** a version in that Org: no Run, Step or Gate whose `origin` includes it runs again, Reruns included, and no Policy can allow it. |
| **`origin`** | The chain of `invoke` targets an inlined Step or Gate came from: each one's kind, name, source and content hash. Recorded by the compiler, never authored, and stored with the Run, so a Policy or a yank can act on a Step through any depth of nesting and a Rerun never recompiles. |
| **Matrix** | An orthogonal *modifier* on a Step: expands to N instances (cartesian product) at **submit time** (static in v1). Applied to an `invoke` step, it fans out N copies of the whole referenced subgraph — "run this reusable subgraph once per coordinate". |
| **Trigger (`on:`)** | What starts a Pipeline: `push`, `pull_request`, `tag`, `release`, comment-command, `cron`, `manual`, `api`, `upstream`. Matched against the normalized **Event** via CEL. |
| **Gate** | A built-in Step kind: a **durable wait point** awaiting human approval, a timer, or an external event. Resolves **passed** or **failed with a reason**: a failed Gate skips its dependents and fails the Run (failure class `gate`, never auto-retried). Any kind can fail: by expiry, by a human **reject** with a reason (`POST /v1/runs/{id}/gates/{step}/reject`), or by an external gate's token `/release` with `passed: false`. A waiting Gate holds **only the Steps that depend on it**; the Run suspends only when nothing else can run (so waiting bills no Run budget). **Each wait is an Attempt**: once every need has succeeded, core opens a `running` Attempt while the Gate Step itself stays `Pending`. **Each Gate has its own wait clock**: the latest finish of its `needs` (or its own latest re-arm, if later), frozen on that Attempt's start and evaluated whether the Run is running or suspended. It is never a Run-level clock; timer and expiry both measure from it. A Gate is approved, released or rejected only once reached, and approvals count **per wait**. A verdict is final **per Attempt**: it is journaled once, as `GateResolved`, when first reached. A Retry of a failed Gate is a new Attempt that waits afresh (a rejected approval asks again). A Gate cannot be marked `deploy: true` (§4.6). Kinds: `manual` (people approve; the only kind approvals and approvers apply to), `timer`, `external` (an HMAC token releases it) and `plugin` (the installed Plugin that defines it releases it, `invoke: plugin:…`; it must set an expiry). Policies never add a Gate: every Gate is in the YAML. |
| **Service** | A Scarab-provisioned backing container (db/redis/etc.) a Pipeline needs at runtime, in one of two forms — **Sidecar service** or **Shared service** (ADR-0058). **Never a `needs`-able DAG node**: a Service is infrastructure *for* Steps, not a Step (it has no id to depend on, no exit code, no Attempt/evidence of its own). |
| **Sidecar service** | The default form: a throwaway container **co-located inside the declaring Step's Pod**, reachable at `localhost:<port>`, alive only for that one Step's execution. **Fenced by inheritance** — shares the Step's Attempt identity and teardown, no cross-Pod networking, no k8s `Service` object. The common case: one test Step + a throwaway DB. Two Steps that each declare Postgres each get their own. |
| **Shared service** | The opt-in form: a **Run-scoped** standalone Pod with a cluster DNS name, reachable by the Steps that opt in, torn down when the Run (that Take) ends, as part of namespace-per-run teardown rather than by refcount. **A fresh instance per Take**: a Rerun's Take gets a new instance keyed by `{run, take}` and never sees the prior Take's writes (Takes are independent — ADR-0056). **Unfenced external mutable state**: intra-Take idempotency across auto-retries and parallel writers is a **step-author contract** (ADR-0021), as for any other external effect — Scarab does not fence it. |
| **`clone`** | **Being replaced (ADR-0076):** the release that ships the first-party Library `lib:thulasi-ram/clone` removes this kind, with no deprecation window; the Library gets its credential through `forge_token` and its commit through `${{ event.sha }}`. Until then: a built-in Step kind, forge-aware checkout (ADR-0008, 0045). Runs `git` in a restricted Pod, pinned to the resolved commit SHA, producing the Run's **workspace root** that downstream Steps inherit via `needs`. Zero-config: repo/ref/SHA/token are implicit from the Run's trigger context; `depth`/`submodules`/`lfs`/`ref` are optional. Authored explicitly (no implicit checkout); a `push`/`pull_request` Pipeline with no `clone` step is a lint warning, not an error. |
| **Environment** | An **org-level place that governs the Runs which name it** (staging/prod), with **protection rules** — its **baseline**: approvers, wait timer (carried and merged but **not enforced** — git-bug `2b94989`), allowed refs and `require_reason`. A Run whose Pipeline names it is an **Environment-governed Run** (§4.6): admitted against these rules, given the Environment's scoped secrets and privilege grants, and never cancelled by newest-wins. Naming an Environment does **not** make a Run a **Deploy Run**: a smoke test or a backup can need prod credentials too. The Environment keeps no record of what is deployed in it (ADR-0074). The OIDC subject is **not** the Environment's: it is still minted per repo from the Run's coordinates. Which Projects may run in it is its **EnvironmentProjectEligibility**. Administered by **org admins** today; RBAC has no scope finer than Project. An unknown name is **refused** (fail-closed): a pipeline's `environment:` must name an existing org Environment that admits the Project. *Administer on that Environment*, an RBAC scope of its own so prod can be delegated (e.g. to a platform team) without org admin, is **MLP, not built**. (Was per-Project under ADR-0037; the migration keeps what every visible Run does: each old per-repo rule set, `require_reason` included, becomes that Project's EnvironmentProjectOverlay over an initially permissive baseline; a name only ever referenced, never defined, gets eligibility *all projects*. The migration sees only the database (legacy rows and the Runs that happened, never pipeline YAML), so two **never-seen** cases are now **refused** where they were admitted: a name no Project defined and no Run used (*unknown Environment* — an org admin must create it), and a Project that names an Environment another Project defined but never ran a Run in it (*not eligible* — an org admin must add it). ADR-0071 §10.) |
| **EnvironmentProjectEligibility** | The set of **Projects** an Environment admits Runs from: *all projects*, or an explicit list (project-name globs allowed). **Owned by the Environment, never the Project**: the YAML `environment:` is the *request*, eligibility is the *authority*. A new Environment starts with **no** eligible Projects (fail-closed). Checked at Run creation, before any Pod or secret — and **again** at the point of use: a manual or external gate does not release (409) for a Project no longer eligible, and **every** launch of a step in an Environment-governed Run is re-admitted; if refused, the step fails `Config` with the reason, before any secret, OIDC token or privilege grant is used (timer gates are covered by this launch check). A refused webhook Run is named in the delivery body. _Avoid_: bare "eligibility", opt-in. |
| **EnvironmentProjectPrivilegeGrant** | The privilege whitelist (which image **digests** may use which governed **Grants**, ADR-0039) and the raw `k8s_overlay` permission (ADR-0055), granted **to one Project within one Environment** by the Environment's admin — never to the Environment as a whole, so one team's privileged image never becomes another's. Today that admin is an **org admin**: a Project's own admins can no longer grant privileges (they could under ADR-0037). |
| **EnvironmentSecretScope** / **EnvironmentProjectSecretScope** | The two Environment secret scopes: org-wide for an Environment (shared by every eligible Project's Runs in it), and one Project within an Environment. Resolution, most specific first: **EnvironmentProjectSecretScope → EnvironmentSecretScope → Project → Org**. An Environment-specific value always beats a generic one. |
| **EnvironmentProjectOverlay** | A Project's **tighten-only** layer over an org Environment's protection rules: approvers accumulate; a ref must **match the baseline and match the overlay** (empty list = any ref; never a list intersection, which fails open); `require_reason` holds if either sets it; the stricter timer wins. A Project can make *its* Runs in that Environment stricter than the org's baseline, never looser. Absent = the baseline applies as-is. Written by that **Project's admins** (or an org admin), only for an Environment the Project is eligible for or already holds an overlay in; it pins the Environment against deletion. |
| **Grant** | A named, closed-vocabulary escalation above the hardened "restricted" step baseline (ADR-0039). `run-as-root` (self-service; does not escape the sandbox); `add-capabilities` and `privileged` (**governed**: require an **EnvironmentProjectPrivilegeGrant** keyed on the image **digest**, Administer-only). Requested by the pipeline author, granted by the Environment admin, admitted fail-closed. A grant is a ceiling, not a default. |
| **`placement_profiles`** | A Step field (ADR-0055): a list of **`PlacementProfile`** names. Their admin-defined k8s overlays are merged (in listed order) onto the Step's Pod; empty → the `default` profile. The author **names profiles**, never raw k8s → no topology/k8s leak. Sibling of **`resources`** (exact `cpu`/`memory` — no `size` tiers, ADR-0055). |
| **PlacementProfile** | An **operator-owned, cluster-scoped** named bundle (ADR-0055) mapping a name → concrete k8s **Placement** (nodeSelector/tolerations/runtimeClass/annotations — an *opaque overlay*, not a fixed schema). Lives in Scarab operator config (gitops), **not** per-Project. One may be `default`. It decides *where a Step lands*. It is **not** an **Environment** (Run governance) and **not** settings of a **Run** (a Pipeline instance). Applied on top of the control-plane **placement baseline** (default tolerations/nodeSelector/resources stamped on *every* step Pod). |
| **RetentionProfile** | An **operator-owned, cluster-scoped** named bundle (ADR-0065) mapping a name → a retention policy: the Data Depot's space budget, per-class TTLs, which directories are Cache-eligible, and the thresholds at which a Snapshot is dropped in favour of re-deriving it. Lives in Scarab operator config (gitops), **not** per-Project. A Pipeline may **name** one; it never defines the values, so no byte cost enters authored YAML. Deliberate sibling of **PlacementProfile**: same pattern, different axis (*how long things are kept* vs *where a Step lands*). Composed with ADR-0061's manual **pin** ("keep this Run's workspaces"), which is the per-Run escape hatch for investigations. |
| **`k8s_overlay`** | A Step's **governed escape hatch** (ADR-0055): a raw pod-spec fragment merged **last** onto the Pod. Carries **no authority** — like a governed **Grant**, it takes effect only if the Run's Environment permits raw overlays, else the Run is rejected **fail-closed**. The `k8s_` prefix marks the backend-coupling (won't run on the local executor). For the rare *dynamic per-job* k8s need; static placement belongs in a `PlacementProfile`. |

### 4.2 Data plane (nine distinct concepts — never conflate)

| Term | Scope | Lifetime | Purpose |
|---|---|---|---|
| **Parameter** | launch-time input, supplied from *outside* the run | resolved once at launch, then persisted for the run's life | a typed value a launcher supplies to start a Pipeline. Declared in the Pipeline's `interface.inputs`; each is `required` (static bool) with an optional `default` and optional `validate:` CEL predicate. Supplied by a human/API on a `manual`/`api` launch **and** by an `invoke:` caller's `with:`: one declaration, one env var (`SCARAB_PARAM_<NAME>`), one launch-time CEL binding `${{ inputs.<name> }}`. **Not** the per-Step workspace `inputs:` (ADR-0007), which is a different concept sharing the word. |
| **Workspace** | one Step's execution | ephemeral (dies with the Attempt) | the **mutable** filesystem a Step executes in. A Step may write anywhere in it; nothing outside the Step reads it directly. **Where its bytes live is not part of the concept** — they may sit on the Pod's node, on the workspace service, or be split across both (ADR-0061), and a Step cannot tell which. The Workspace is the *view* a Step executes against; it never outlives the Attempt. |
| **Workspace Snapshot** | intra-run, flows along DAG edges | retained per policy, then archived, then gone (ADR-0061) | the **immutable**, content-addressed tree (per-file merkle CAS) that crosses a DAG edge and that an Attempt owns as evidence. Implicit-by-default (inherit `needs`), explicit-on-demand (`inputs:`/`outputs:`). Two coordinates, never conflated: the **snapshot root** is *where the bytes are*: the address a Step materialises, GC marks from, and an Attempt records. The **content identity** is *what the bytes are*: the same merkle fold with mtimes dropped, which is what restart invalidation compares (ADR-0027, 0061 s8). A root moves when a file's mtime moves and an identity does not, so **only the root is an address**: nothing is stored under an identity and nothing can be fetched by one. |
| **Workspace Export** | one Attempt | dies with the Attempt; reaped, never archived | the **delivered** form of a **Workspace**: a per-Attempt, writable, network-mounted view of one **Workspace Snapshot** that a Step Pod receives *as* its Workspace (ADR-0062). Distinct from the Snapshot it is a view of, and the distinction has consequences: losing an Export fails that Attempt, which retries; losing a Snapshot widens a rerun's scope (ADR-0061 retention). Its address is a **capability** (unguessable, TTL'd to the Step deadline, client-pinned), not a name, because the mount protocol cannot carry a per-Step identity. |
| **Change set** | one Attempt | derived; folded into a Snapshot, then discarded | the set of paths an Attempt wrote — what the drain hashes, and the reason it need not re-read an unchanged tree. **Known** where the Attempt had a **Workspace Export**: the `overlayfs` upper layer is the kernel's own record of what was touched (ADR-0062), so it is exact and assumes nothing. **Derived** where there was none (the local executor): each file's `(size, mtime, ctime)` compared against the input manifest. The derived form is sound only *conditionally*: it assumes ctime cannot be forged (no syscall sets it), a capture stamped after materialisation completed, a filesystem whose ctime is not coarse, and a Step that cannot move the clock back. `(size, mtime)` alone is **not** sound: `cp -p`, `touch -r` and `tar -xp` defeat it deterministically, not as a race. |
| **Result** | intra-run, flows along DAG edges | ephemeral | small typed values (a version, a bool) for params/conditionals. **No bytes anywhere**: the value *is* JSONB in Postgres (`attempts.results`); nothing is stored in the CAS or the object store. Interpolated as `${{ results.<id>.<name> }}` (ADR-0041, renamed from `outputs.` — see **`outputs:`** below). |
| **Artifact** | output of record | retained (TTL), downloadable, UI-visible | binaries, reports, coverage, images. Bytes in the object store; Postgres holds only metadata. **Immutable per Attempt**: a retry never overwrites a prior Attempt's version; the name-addressed record resolves to the latest *successful* Attempt's version. |
| **Cache** | cross-run | best-effort, evictable | `~/.cargo`, `node_modules` — **author-declared** directories under a key (e.g. a lockfile hash), restored at Step start and saved at Step end (ADR-0065). Stored as a content-addressed tree, so it uses the same Farm/Export machinery as a Snapshot and materialises lazily. Correctness does **not** depend on it and it is **never evidence**, which is what puts it outside the durability rules. A keyed *directory* cache, never a shared mutable *mount*: two concurrent Runs writing one `node_modules` is corruption. |
| **`outputs:`** | a Step field | — | the workspace-relative **paths** a Step publishes downstream (ADR-0007) — bytes on a filesystem, narrowing what flows along an edge. A *precision* tool for cache keys, safe fan-out and remapping; **never** something an author declares for speed (ADR-0007 amendment, ADR-0065). **Not** a **Result**: until 2026-07-28 the Result interpolation namespace was also spelled `outputs.<id>.<name>`, so one word named both a list of file paths and a namespace of typed scalars. The namespace is now **`results.`** and this term means paths only. |
| **Step logs** | one Attempt | own retention class, long TTL | an Attempt's stdout/stderr. Bytes in the **object store**, keyed `{run, step, attempt}` and **not** in the CAS, because content addressing gains nothing for an append-only stream (ADR-0063, **amended 2026-08-23**). Postgres holds byte offsets only, never bodies. The **Data Depot** may *cache* log chunks for a fast live tail, but its copy is **never authoritative**: logs are the one class that cannot be re-derived, and the Depot is by definition a cache, so it may not be their home. Re-running a Step yields *different* logs, so eviction here is real loss, not just a slowdown. Logs are therefore pinned until a durable sink acknowledges them, and when the bytes are **gone for any reason**, Scarab says so: the absence is authoritative and the eviction record only explains it, because a lost volume was evicted by nobody. External log systems (Loki, VictoriaLogs) are **additional sinks, never the system of record**. |

### 4.3 Run-time / instance plane (what the durable engine tracks)

| Term | Meaning |
|---|---|
| **Run** | A durable *instance* of a Pipeline for a specific Event/commit. Stores the compiled IR + `{ir_version, event_schema_version}` (self-describing). |
| **StepRun** | A durable instance of a Step within a Run. |
| **Attempt** | A single execution of a StepRun. **A Rerun or Retry creates a new Attempt.** Each Attempt owns its evidence — logs, Results, Artifacts, workspace snapshot — keyed by `{run, step, attempt}`, and records which upstream Attempts it consumed. |
| **Superseded** | **Intrinsic terminal** outcome of an Attempt **cut short while running** because a human **reran or retried an ancestor** (the cascade re-armed this step): its live Pod is torn down (SIGTERM + grace) and any late verdict is rejected by the fence, so it can never finish with a result that counts, and a fresh Attempt takes its place. It is **the Attempt's own outcome**: shown as *superseded* in every view, never repainted as the transient "running" it passed through nor as a later Attempt's success (ADR-0056). Distinct from **Cancelled** (a *deliberate* stop with **no** replacement, `cancel_run_request`), from **Failed** (ran and errored), and from **Not-run** (never started, no Pod). Evidence retained like any other. |
| **Not-run** | Terminal display state of a step that **never started** in a version **superseded before its turn** (an ancestor was rerun while this step was still upstream-blocked). Distinct from **Skipped** (a `when:` was false or a dependency died), **Pending** (implies a future that *will* happen — it will not, in this version), and **Superseded** (had a live Pod and may have partially side-effected — a not-run step did nothing). Per-version, derived from the event log (ADR-0056). |
| **Auto-retry** | The **engine** re-executing a not-yet-succeeded StepRun within its retry budget (ADR-0047). Machine-initiated, never on a succeeded or in-progress step; adds an Attempt but does **not** fork the Run (stays within the current Take). Its human counterpart (same version, no fork, on a Failed step) is a manual **Retry** (one concept, two triggers). |
| **Retry** (manual) | The **human** trigger of a Retry: re-executing a **Failed** step as **another Attempt in the *current* version** (Take) — **no fork**: it reopens the settled Run (`Failed → Running`) and re-arms the target plus its dependent cascade (ADR-0027). Backend `retry_step`; event `StepRetryRequested`. The human counterpart of **Auto-retry** — one concept, two triggers (manual vs auto); the authored `retry:` policy governs the auto side. Contrast **Rerun** (forks a new version). Offered on Failed steps only. |
| **Rerun** | The **human** action of re-executing a step as a **new Run version** — **forks a new Take** (a new history row) and cascades to descendants (ADR-0027). Offered on **any** target, terminal or not; an in-flight member of the invalidation set is **voided** (torn down, marked **Superseded**; ADR-0056). Backend `rerun_step`; event `RunRerunRequested`. Contrast **Retry** (another Attempt in the *same* version, Failed-only). _Avoid_: "restart" (user-facing). |
| **Cascade** | The downstream half of a Rerun: descendants dragged into re-execution because an ancestor was rerun (per smart invalidation, ADR-0027) — not individually chosen by the human. A cascade Attempt's cause points back at the rerun that triggered it ("⟵ b was rerun"). |
| **Shadowed** | A **finished-successful** Attempt whose result is retained and readable but is **no longer the "of-record" latest** — a newer Attempt (from a Rerun/cascade) replaced its role. Distinct from **Superseded** (never finished — cut short while running). ADR-0056's word for non-latest Artifact versions; generalized here to any evidence. |
| **Take** | The span of a Run between two **Reruns**. A Rerun closes the current Take and opens the next (a new history row); auto-retries, crash re-adoptions, and dead-letters happen *within* a Take. The run-level version unit ("Take 2 of 3") — a bookmark over a frontier of Attempts, not a copy of anything. **Internal term**: the end-user run-history language is still being designed (it does not show "Take"/"Attempt" verbatim; users see a row per Rerun). |
| **Event log** | Append-only, versioned, immutable record of state transitions. Drives SSE, timeline, audit, time-travel. State tables are the source of truth; the event log is derived-but-durable (via the outbox). |
| **Admission** | *Scarab's* scheduling decision — which Runs/Steps are allowed to run (concurrency groups, fairness, priority, backpressure). Distinct from k8s **Placement** (node fit). |
| **Fence** | A monotonic `{run, step, attempt}` token handed to each Attempt and to cooperating external systems (idempotency keys, digest/generation checks) to neutralize the double-effect hazard. |
| **Provisioning deadline** | The bound on how long an Attempt may spend **getting ready to run** — scheduling, image pull, workspace feed — as opposed to **running** (ADR-0066). A **separate clock** from the Step's authored `timeout:`, which measures execution only, from the moment the step container starts. The principle: **waiting for infrastructure must not bill the Step's execution budget**. Before the split, queueing delay ate the author's timeout and a Step that never ran could fail as `Timeout`. Overrunning it is its own **failure class**, never `Timeout`, so the two causes are distinguishable in the record. Distinct again from the **Run budget**, which is a wall-clock spend limit over a whole Run and is not split. |

### 4.4 Structural / architectural

| Term | Meaning |
|---|---|
| **Port** | A domain-owned trait describing an external capability in domain language (`ForgePort`, `SecretProvider`, `ObjectStore`, `Cas`, `Executor`, `Db`, `Clock`, `OidcIssuer`). |
| **Adapter** | A concrete implementation of a Port, in a **separate vendor crate** (`scarab-forge-github`, `scarab-db-postgres`, …), holding all infra deps. |
| **IR** | The typed, versioned **Pipeline Intermediate Representation** — the *real* DSL. YAML is one frontend; the API schema *is* the IR. |
| **Forge** | The domain *concept* of a source-of-repos/sink-of-status (GitHub, GitLab, Forgejo). Vendors are **adapters**, never their own domain. |
| **Data Depot** | The long-lived Scarab-operated service that holds the data plane **near where Steps run**: the warm CAS, **Snapshot Farms**, live **Workspace Exports**, the **Step log** namespace, and **Cache** (ADR-0061, 0062, 0063, 0065). Same binary as the control plane, different role (`--role workspace`) and **no durable core**. It **connects to the control plane's Postgres** but **owns no schema and never migrates** (2026-08-23): the rule being protected is that it is *not a system of record*, not that it *never uses a database*: a Depot rolled ahead of the control plane must not be able to half-migrate anything, and everything it reads there is **derived and rebuildable from the object store**. Was "the workspace service" until 2026-07-28; renamed because that named it after *one of its tenants* and broke as soon as a second kind of data arrived. `warm store` was rejected for naming it after one of its *properties*, which encodes a tiering decision and dates the same way. **Not** a **Workspace**, a **Workspace Snapshot** or a **Workspace Export** — those are data it holds, and they keep their names. **By definition a cache** (ADR-0066): it holds nothing that cannot be recreated. That is why running more replicas is enough for HA, eviction is safe by construction, and losing a replica only costs latency. Anything that makes it a system of record is a defect. |
| **Warm-only** | **Retired as a deployment mode (2026-08-23).** **Postgres and an object store are both hard requirements**; Scarab is not expected to function without either. Warm-only survives only as a *runtime degradation* (the cold tier is configured but briefly unreachable), never as a supported way to deploy. What follows is the superseded definition, kept because the disclosure vocabulary it introduced still fits that degradation. ~~The named deployment mode in which **no independent cold tier is configured** (ADR-0064's `st_dev` test) — object storage is a **soft, recommended** requirement, not a hard one (ADR-0066).~~ Supported, with a **smaller but accurate** guarantee rather than a silent downgrade: the **Data Depot** offers a bounded *recent window*, every Attempt is stamped `warm-only` in `attempts.output_durability`, and warm eviction is real data loss, not just added latency. It does **not** mean the Depot becomes the system of record — it means there **is** no system of record for **Workspace Snapshots** in that deployment, which is disclosable because they are reproducible from the forge. Depot **HA requires object storage** — a documented consequence, not a gap. |

### 4.5 Tenancy & forge binding

| Term | Meaning |
|---|---|
| **Org** | A top-level tenant. Owns Projects. The Scarab tenancy boundary — **not** the forge's `owner` namespace (one Org may span a GitHub org *and* a Forgejo instance). |
| **Project** | Scarab's **governed unit of CI** and the **aggregate root** beneath an Org: it binds a **source** (a `RepoRef` on a forge, via a `ForgeConnection`) to its **governance** (Environments + `ProtectionRules`, privilege whitelist, secret scope, OIDC subject) and owns the **Pipelines and Runs** produced from that source. RBAC is enforced at the Project scope. **1:1 with a repo** in v1 (monorepo per-subdir governance deferred to an optional path scope). There is no separate governed "Repo" entity — a Project *is* the governed repo. |
| **`RepoRef`** | A **forge coordinate** — `{owner, name}` as the forge addresses a repository, plus the forge it lives on. External and mutable (a forge rename/transfer changes it). Carried by `Event`/`Status`. Lives in `scarab-forge`; it is the *only* concept named "Repo". Resolved to a Project via a `ForgeConnection`. |
| **Actor** | The identity that caused an **Event** — the forge principal who pushed, opened the PR, cut the tag, or dispatched a `manual`/`api` run (a vendor login, normalized). Carried on every `Event` variant (not just dispatch), stamped onto the Run at creation as a discrete fact (beside the trigger kind, ref, and commit SHA — not bundled), and shown in the UI (labelled "author"). A forge-originated fact, **not** a Scarab RBAC principal — do not conflate with the authenticated caller in an authorization check. |
| **Headline** (trigger title) | The one normalized human line that says *what a Run is about*, disambiguated by trigger kind: a push's commit **subject**, a pull request's **title**, or a `manual`/`api` dispatch **reason** (a `cron` schedule / `upstream` run id for those). A forge-originated, **display/audit-only** provenance fact — a sibling of **Actor**, stamped on the Run at creation, never used in logic and **never entering trigger-matching or `${{ }}` interpolation**. Shown as the secondary line of the Trigger cell (ADR-0057). |
| **`ForgeConnection`** | A configured link between Scarab and a forge account (a GitHub App installation, a Forgejo connection): `{forge_kind, base_url, credential_ref}` owning a set of `RepoRef`s. It resolves `RepoRef` → Project and supplies credentials. The *type* is pure (`scarab-forge`, holds a credential **reference**, not secret bytes); persistence is a store **port** + adapter; credentials live in `SecretProvider`. |

### 4.6 Deploys

Scarab runs and governs the Run that deploys. Once that Run ends, Scarab keeps nothing
about the deploy: what is live where, a rollout that fails an hour later, a hotfix applied
by hand. The CD tool (Argo CD, Flux, Kargo, Spinnaker) answers those (§3, ADR-0074).

Two settings that are easy to conflate stay separate. `environment:` **governs** a Run.
`deploy: true` **marks** one. A Run can have either, both or neither.

| Term | Meaning |
|---|---|
| **Environment-governed Run** | A Run whose Pipeline names an **Environment**. It is admitted against the Environment (EnvironmentProjectEligibility, protection rules, approvers, `allowed_refs`), gets the Environment's scoped secrets and the Project's privilege grants in it, and is never cancelled by newest-wins: it carries no supersede key, and its concurrency group always queues. One Environment per Run (pipeline-level `environment:`). Says nothing about whether the Run deploys. |
| **`deploy: true`** | The author's mark that a Pipeline, or one Step in it, deploys. Set on the Pipeline (every Run of it is a Deploy Run) or on a Step (that Step is a Deploy Step). A Gate cannot be marked. Needs no Environment. |
| **Deploy Run** | A Run whose Pipeline is marked `deploy: true`, or which has a Deploy Step. Decided at Run creation from the Pipeline and stored on the Run. The Runs list API and the Runs tab filter by `deploy` and by `environment`, in the database query, and the two combine; the Run list marks Deploy Runs. A Deploy Run is exempt from newest-wins cancel in the same way as an Environment-governed Run (no supersede key; its concurrency group queues even under `cancel-in-progress`), so a newer push never cancels a deploy half-way. The mark changes nothing else: no admission, no secrets, no approval. Its success means only that its Steps succeeded. With GitOps that is "the change merged" unless a **wait Step** checked the rollout. A rollback is a Deploy Run like any other. |
| **Deploy Step** | A Step marked `deploy: true`. Makes its Run a Deploy Run; the Run page marks it. |
| **Environment authority** | What a Step receives only because an Environment governs its Run: declared `secrets:`, a governed Grant or a `k8s_overlay`, `forge_token: write`, or a `deploy: true` mark (on the Step or its Pipeline). In an Environment with approvers, every Step that uses Environment authority needs an approved `manual` Gate upstream, or the Run is refused; at launch the approval is checked again (allowed refs, approver set, Policies, a maximum age of 24 hours by default), so a Rerun on a stale approval is refused. Steps with no approved Gate upstream get the `env=none` OIDC subject that fork pull requests get. _Avoid_: bare "privileged step", "protected step". |
| **wait Step** | An ordinary Step, after the Deploy Step, that blocks until the CD tool reports the change live or failed (`argocd app wait`, `kubectl rollout status`). Its exit code is the outcome, its log holds the CD tool's messages, and its `timeout` bounds the wait. Scarab adds nothing for it: a Step is any OCI image (ADR-0008). Writing it is the pipeline author's job, and so is its correctness: a wait that trusts Argo's Synced and Healthy alone can pass at an earlier commit. The docs site's "Deploying with Argo CD" page has a recipe. Not a domain object; the name is for prose. |

_Avoid_: **Deployment**, **DeploymentTarget**, **DeploymentVersion**, **DeploymentStatus**,
**DeploymentStatusReport**, **DeploymentRequest** (`deployment_requests`),
**DeploymentGate** (`gate: deployment`), **DeploymentReporterContract**,
**EnvironmentDeploymentDeadline**, **EnvironmentDeploymentSources**,
**DeploymentTargetProjectOwnership**, per-DeploymentTarget concurrency, **Unexplained** /
**Unconfirmed** / **Moved**, the `/deployments` board and the `report_deployments`
capability. ADR-0070 to ADR-0072 defined them and ADR-0074 deleted them; the code is kept
at the tag `archive/deployment-record` (`ebafe41`). Before ADR-0074, "Deploy Run" meant
any Run that named an Environment; it now means a Run marked `deploy: true`. What stays
from ADR-0070 is §9 (every Gate can fail with a reason, has its own clock and holds only
its dependents; see **Gate**) and the step-level `results:` declaration.


### 4.7 Plugins

A **Plugin** extends Scarab with concepts it is not aware of or should not own: vendor
knowledge (how to read and act on Argo CD, Flux, Slack…). It is a **second extension
point beside Steps**, not a replacement: a Step is still any container (ADR-0008). Core
owns the vendor-neutral nouns; a Plugin owns only vendor knowledge, so CD knowledge lives
in CD Plugins and core keeps no CD concept. **First-party Plugins use the same contract
third parties do** — if one needs more, core changes.

**Status (ADR-0074 §7).** The plugin runtime, the CredentialBroker and PluginConnections
are built, but no built PluginCapability acts: a Plugin can be installed and run, and
nothing it does reaches a Run. ADR-0076 makes a Plugin a non-human member of the Org
that acts as a Principal **outside Runs**: it uses the public API under its own RBAC,
reads state, and resolves the `plugin` Gates it defines. Core never calls a Plugin. Work
done inside a Run stays a Step, reused through `invoke`. Until that is built, the rows
below describe the contract. _Avoid_: **PluginAction**, **`plugin_action`**,
`offer_actions`, `subscribe_events`, `approve_gates`: designed in ADR-0072 and dropped by
ADR-0074 and ADR-0076.

| Term | Meaning |
|---|---|
| **Plugin** | A **digest-pinned OCI image + PluginManifest**, run as a long-lived workload Scarab manages, outside any Run. Depends only on core, **never on another Plugin**. Resolves only the `plugin` Gates it defines and **never approves a `manual` Gate**, never holds a long-lived credential, never reads secrets/workspaces/artifacts/logs unless a future capability says so. Brings its own storage if it needs any (see **PluginInstallation**). |
| **PluginInstallation** | An org's approval to run **one pinned digest** of a Plugin with its PluginManifest. Nothing changes without an admin action: **no auto-apply**, ever (a new digest is new code). Same-or-narrower manifest → one-click upgrade by one org admin; wider → review of only the new capabilities; elevated tiers → dual approval on every digest. Subject to the operator's ceiling (which publishers may be trusted, whether elevated tiers exist at all) and to a kill switch. Runs as one instance per org. May name a **PersistentVolumeClaim** its operator created in the plugins namespace, which the plugin runtime mounts into the Plugin's Pod; a Plugin that needs a database reaches it through a `delivered` PluginConnection. Scarab does not provision, migrate, back up or read that storage, and it is outside Scarab's durability claims. A CD Plugin that wants a record of what is live keeps it there. |
| **PluginPublisher** | A signing identity (Sigstore identity or key) an org trusts to publish Plugins. **Unsigned or untrusted-publisher images cannot be installed.** Scarab's own release identity is trusted by default (first-party); "which plugins do we trust" is answered as "which publishers". |
| **PluginPrincipal** | A Plugin installation's own identity — **not tied to any human** — under which it calls the broker and, to resolve its `plugin` Gates, the public API. Proven by **projected ServiceAccount tokens** (audiences `scarab-broker` and `scarab-api`, short TTL, automount off), verified by TokenReview or JWKS and mapped to the installation; the kill switch revokes at that mapping. Appears in audit and the UI as `plugin:<name>` ("released by plugin:argocd-prod"). It has no human, so it can never step up, and it is never an approver. |
| **PluginManifest** | The Plugin's declaration of every **PluginCapability** it wants, with scopes. What an org admin approves is *(image digest, manifest)*. |
| **PluginCapability** | One entry of a **closed vocabulary** (grows only with a Scarab release) of what a Plugin may do, each scoped: `use_connection` (reach a named external system without holding its credential), `resolve_gates` (release or fail the `plugin` Gates it defines, effective only where an admin lists the Plugin in "plugins whose gates may run here"; two keys, two roles), and, later, `contribute_ui` (a **PluginUiContribution**). |
| **PluginConnection** | An org-admin-configured binding of **a credential to the one destination it may be used against** (base URL + credential reference + auth scheme): the destination is part of the authority, so the two are never separated. The secret bytes live in the SecretProvider. May be **scoped to an Environment** (e.g. `argocd-prod` with a token limited to Argo's prod project), so the vendor's own RBAC tells prod from staging. _Avoid_: bare "credential", PluginCredential. |
| **PluginConnectionCredentialMode** | How a Plugin uses a PluginConnection. `brokered` (default) — the Plugin **never holds the credential**: the **CredentialBroker** adds it to requests the Plugin sends to a stable in-cluster address, and the Plugin keeps its own client (native SDK, retries, streaming). `delivered` (escape hatch, for protocols the broker cannot proxy) — the credential is mounted into the Plugin, egress tunnelled through the broker and pinned to the destination's address and TLS name (SNI), **dual org-admin approval**, re-approved on every digest; everything the Plugin emits (labels, links, action results) is scrubbed by the secret redactor. Limit: behind a **shared ingress** it may reach other sites on that ingress, so it is meant for destinations with their own address (databases, SSH, dedicated endpoints). |
| **CredentialBroker** | Core's authenticating reverse proxy between Plugins and PluginConnections: checks each request against the manifest's **PluginConnectionRequestRules**, adds the credential, forwards, journals. Supports streaming. A Plugin's network reaches **only** the broker. |
| **PluginConnectionRequestRule** | A manifest-declared allowed request shape on a PluginConnection — method + path pattern + the **query keys** it may carry (any other key, or one twice, is refused, not dropped) (+ body schema). **Read** rules apply any time. **Write** rules wait for write actions, which are not designed: first a Policy-based answer to which Project may act on which vendor object is needed (ADR-0074 §7, ADR-0076 §8). A second line of defence; vendor-side least privilege (e.g. an Argo role that can sync but not override) is the first. |
| **PluginUiContribution** | UI a Plugin adds, at a declared **PluginUiTrustTier**. |
| **PluginUiTrustTier** | `framed` (default) — the Plugin's UI runs in a **sandboxed, opaque-origin iframe** at a **UiExtensionPoint**: it owns its own DOM inside the frame (any UI library), gets core's design tokens to look native, and talks to core only through a narrow bridge (context + **proposals** of actions, which wait for write actions — the bridge only proposes; core renders the confirmation **outside the frame**, so frame code cannot fire an action without a real click); it **cannot act as the viewer or read Scarab**. One org admin approves. `trusted_dom` — same-origin code that can change any page and **can do anything any viewer of that page can do**: equivalent to adding code to Scarab itself. Allowed only for publishers the **operator** names, dual org-admin approval, re-approved on every digest, served pinned (SRI). Its guardrails ensure *you chose to trust it*; they do **not** contain it — no exfiltration or page-boundary guarantee is claimed. |
| **UiExtensionPoint** | A named place in core's UI where a `framed` contribution is mounted (a Run panel, an Environment page, an org tab, a Step panel). |
| **StepUpConfirmation** | A fresh, script-unforgeable confirmation by a **human in an interactive session** — a Scarab-registered **passkey** (WebAuthn), or (not built yet) a forced re-login **only where the identity provider proves the authentication is fresh** (a login through a provider that cannot prove it, such as GitHub or Forgejo OAuth, never counts: those users confirm with a passkey, enrolling one the first time they attempt a crown-jewel operation) — required for **crown-jewel operations**: Plugin install/upgrade, PluginConnection create/edit (above all its destination), the operator ceiling, ForgeConnection edits, identity links, secret writes, role changes, API token creation, anything that **loosens** an Environment (protection rules, including removing an approver or widening an allowed ref; EnvironmentProjectEligibility; privilege whitelist), approvals on Gates of Environments with approvers, adding or removing a passkey after the first, and **irreversible destruction** (deleting an Org, Project, Environment or secret; shortening retention). Every mutating route carries one classification in a single route table, so a new route cannot skip the guard; an operation with no route yet (Plugin upgrade, the operator ceiling, shortening retention, deleting an Org) is covered the day its route appears. Environment loosening and Gate approval are **conditional**: the handler judges the change against the stored state (fail-closed; a concurrent change answers 409), and an approval needs a step-up when the Environment has approvers or its governance cannot be resolved. **An API token never satisfies a step-up**; it reaches a crown-jewel operation only through a StepUpConfirmationRequest. A confirmation covers the **cookie** session that made it for 5 minutes **from the moment of confirming** (activity never extends it, and the same session presented as `Bearer` never uses it), except the operations that mint a new credential or outbound channel (API token creation, Plugin install/upgrade, every PluginConnection write, adding/removing/resetting passkeys), which confirm **every time** with a challenge committed to that exact action. Tightening needs no step-up. Defends against stolen tokens, CSRF and unattended sessions, **not** against `trusted_dom` code, which can swap what is being confirmed. |
| **StepUpConfirmationRequest** | A crown-jewel operation a **non-interactive caller** (an API token, or a CLI presenting a human's session as `Bearer`) asked for and cannot perform itself: it is held as a pending request naming **exactly** the operation, its target and what changes, and runs **once** only when a human confirms that exact request with a StepUpConfirmation. It is held as the **sealed** request itself and, once confirmed, **replayed** through the real route, pinned to the confirmed operation and request digest and to the loosenings the human saw. It expires after 5 minutes, cannot be reused or widened, and is recorded as requested-by the caller and confirmed-by the human. At most 10 wait per requester. The confirm page shows the method, path, query and every value in full (a URL's parsed host included). The caller never satisfies step-up; the human does, per request. **Only the human the caller acts for** (the token's owner, the CLI session's human, or a Principal that human was merged into) may confirm it (never another admin, who gets a 404), and it runs with the **narrower** of the caller's scope and that human's live role; a PluginPrincipal has no human and cannot raise one. |
| **OrgSecurityEventLog** | The Org's append-only record of step-up activity: every StepUpConfirmation (who, when, which passkey, which session), every crown-jewel operation and the confirmation or StepUpConfirmationRequest that authorised it, every **refusal** (a caller that attempted a crown-jewel operation without step-up), and every passkey enrollment, removal and reset (the server break-glass included). Postgres refuses every update and delete, and retention never touches it. Confirmations, enrollments, authorised operations and resets are **fail-closed**: an action whose record cannot be written does not happen. Org admins see the Org's log: its own events plus the Org-less confirmations and passkey events of its members, with no detail of another Org's operation. Each user sees their own; global admins see the whole install. Forwarding it elsewhere is the job of an API client, not core's. |

### 4.8 Policies

One engine decides every governance rule (ADR-0075). Typed settings (forms) and CEL rules
both compile into it. RBAC decides whether a caller may use an API at all; a Policy then
decides what that call may do, and never grants what the role does not.

| Term | Meaning |
|---|---|
| **Policy** | A governance rule an org, an Environment or a Project attaches: one **PolicyCheckpoint**, a typed `match` (projects, `except_projects`, environments, trigger kinds, actor kinds), and rules, each a CEL `when`, a **PolicyEffect** and a static message. Mode `enforce` or `audit` (records what it would do, enforces nothing). Project admins write typed settings only; org and Environment admins write CEL too. Any deny wins and requirements add up, so a lower layer can only tighten. A Policy never changes a Pipeline or a Step, and never adds a Gate. Stored in Scarab's database, not in a repository. _Avoid_: rule set, guard. |
| **PolicyCheckpoint** | One of the three fixed places Scarab consults the engine, each with its own typed input: **`run.create`** (a Run about to be created: trigger, Pipeline, every Step and Gate with its `origin`), **`step.admit`** (each launch of an Attempt) and **`gate.resolve`** (an approve, reject or release, with who is acting). The set grows only with a Scarab release. Settings changes are **not** a checkpoint: the step-up route table governs them. Library and plugin rules live at `run.create`; there is no compile-time checkpoint. |
| **PolicyEffect** | What a matching rule does: **`deny`** (refuse, with the message), **`require_approval`** (named Principals approve a `manual` Gate the author wrote; valid only at `gate.resolve`) or **`require_step_up`** (the acting human confirms with a passkey; valid only at `gate.resolve`). There is no allow: a decision is allowed when nothing denies it. A missing reason is a `deny`. Requirements hold releases only, never a reject. A requirement the actor cannot meet (a plugin or an external token cannot step up) refuses that actor. |
| **PolicyDecision** | The record of one consultation: the checkpoint, its input and schema version, the **PolicySetDigest**, and each matching Policy's outcome and messages. Stored with the Run and shown on the Run page; a refused Run's decision goes to the org's decision log. Each checkpoint decides once (a `step.admit` decision once per Attempt) and later reads reuse it. |
| **PolicySetDigest** | A content hash of every **PolicyVersion** a decision evaluated, across org, Environment and Project, so a decision names exactly the rules it ran. |
| **PolicyVersion** | One saved state of a Policy, with its author, time, reason and the step-up that approved it when one was needed. Every save makes one; any two can be compared. |
| **PolicyInvariant** | A rule in code that no Policy can turn off, checked before the engine runs: the hardened Step baseline, the fork pull request limits (no secrets, `env=none` OIDC subject, no build push, read-only clone token), step-up for settings changes that loosen governance (editing or deleting a Policy included), a yanked LibraryVersion, which Steps may receive a forge credential, and the `env=none` OIDC subject for Steps with no approved Gate upstream in an Environment with approvers. |
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
                                                      ▼  │  ✅ drain (`scarab-wsfetch drain`, in the Pod)
                                          [workspace role]  the Data Depot (ADR-0061, 0067)
                                          warm merkle CAS on a PersistentVolume, one per
                                          failure domain · bounded by SPACE (LRU sweep) ·
                                          derived rows in Postgres, never migrates
                                                      │
   Object store (S3/MinIO) ◀── logs · artifacts · cache ┘
                           ◀── Workspace Snapshots as packs: the COLD archive, bounded by TIME
                           ◀── ⚠ BULK, TODAY: artifact harvest still tars `/scarab/artifacts`
                               out of the Step Pod over `kubectl exec` into the control
                               plane, which writes it here. It is the last `exec` tar.
```

Which edge carries bulk data is the central question of ADR-0061, so the four labels matter.
**control plane ↔ Step Pod** carries root hashes only — that property belongs to *this* edge
and no other. **Step Pod ↔ workspace service** is the bulk edge by design, and both halves now
use it: the fetcher feeds a Step's inputs, and `scarab-wsfetch drain` in the Pod's egress
container sends its outputs to the Depot, which writes them to the object store as packs.
**control plane → object store** is still bulk *today* for artifacts only, because artifact
harvest is an `exec` tar through the control plane.
**control plane ↔ workspace service** is small and off the critical path: Browse reads
snapshots through the service (falling through to the object store when it cannot answer),
and the control plane reads the drain's record from the Depot to decide the Attempt.

- **One binary, many roles**
  (`scarab-server --role converged|api|scheduler|executor|webhook|workspace`).
  Postgres (outbox) is the coordination bus; no service-to-service RPC required internally.
  `workspace` is the one **data-plane** role: it connects to Postgres only for derived,
  rebuildable rows (the pack index, drain records, write ledgers) and never runs a migration
  (ADR-0067). Its pool connects lazily, and reads served from the warm tier never touch
  Postgres, so through a database outage it keeps serving inputs that are on warm. A warm miss
  needs the pack index and fails until Postgres is back (the fetcher retries).
- **Control-plane / data-plane split:** the durable *brain* records intent ("Step S should
  run"); the *executor* observes it, creates the Pod, watches it, writes back terminal state.
  The intent is that bulk **data** does not cross the brain — the control plane exchanges root
  hashes and Step Pods talk to the workspace service. **True today for workspaces**: a
  Scarab-owned fetcher feeds inputs and `scarab-wsfetch drain` sends outputs to the Depot, so
  neither crosses the Kubernetes API server. The exception is **artifact harvest**, which is
  still an `exec` tar into the control plane until artifacts move onto the Depot.
- **Three stateful components, and the third is deliberate.** Postgres (state + event log),
  an object store (blobs), and the **workspace service**'s persistent volume.
  [ADR-0004](docs/adr/0004-execution-topology.md) called object storage "the second (and
  last)"; [ADR-0061](docs/adr/0061-workspace-data-path.md) knowingly adds a third, because
  one standard path beats a fast path plus a fallback path, and because binding volumes to a
  long-lived service removes every cost PVCs have when bound to short-lived Step Pods.
  The two storage tiers carry **different promises**, and the difference matters:
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

Testing support + composition:

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
5. **The UI uses the same API as everyone else.** No private UI backchannel.
6. **The Data Depot is a cache.** Nothing on its volume may be irreplaceable; anything that
   makes it a system of record is a defect (ADR-0066). Guarantee what cannot be recreated,
   degrade what can.

---

## 8. Testing philosophy (see ADR-0017)

Classical (Detroit-school): real collaborators in-process, **mock only true externals** at
the adapter boundary. **Minimal in v1** — enough integration tests to catch glaring bugs;
**grow the suite from real bugs** (each fix leaves a regression test). E2e per layer, a few
real cross-layer e2e. Exception: 2–3 targeted crash/resume tests guard durability.
Don't fight test infra; keep moving.

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
