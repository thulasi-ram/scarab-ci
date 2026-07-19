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
- **Restart** a single step — it is a re-creatable, individually-addressable object.
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
| **Service** | A built-in Step kind: a sidecar (db/redis) alive for the duration of dependent Steps. |
| **`clone`** | A built-in Step kind: forge-aware checkout (ADR-0008, 0045). Runs `git` in a restricted Pod, pinned to the resolved commit SHA, producing the Run's **workspace root** that downstream Steps inherit via `needs`. Zero-config — repo/ref/SHA/token are implicit from the Run's trigger context; `depth`/`submodules`/`lfs`/`ref` are optional. Authored explicitly (no implicit checkout); a `push`/`pull_request` Pipeline with no `clone` step is a lint warning, not an error. |
| **Environment** | A first-class deployment target (staging/prod) with **protection rules**: approvers, wait timer, allowed refs, concurrency, secret scope, OIDC subject, deployment history, and a **privilege whitelist** (which image digests may run with which governed grants — ADR-0039). |
| **Grant** | A named, closed-vocabulary escalation above the hardened "restricted" step baseline (ADR-0039). `run-as-root` (self-service — does not escape the sandbox); `add-capabilities` and `privileged` (**governed** — require an Environment whitelist entry keyed on the image **digest**, Administer-only). Requested by the pipeline author, granted by the Environment admin, admitted fail-closed. A grant is a ceiling, not a default. |
| **`placement_profiles`** | A Step field (ADR-0055): a list of **`PlacementProfile`** names. Their admin-defined k8s overlays are merged (in listed order) onto the Step's Pod; empty → the `default` profile. The author **names profiles**, never raw k8s → no topology/k8s leak. Sibling of **`resources`** (exact `cpu`/`memory` — no `size` tiers, ADR-0055). |
| **PlacementProfile** | An **operator-owned, cluster-scoped** named bundle (ADR-0055) mapping a name → concrete k8s **Placement** (nodeSelector/tolerations/runtimeClass/annotations — an *opaque overlay*, not a fixed schema). Lives in Scarab operator config (gitops), **not** per-Project. One may be `default`. It is *where a Step lands* — **not** an **Environment** (deploy governance), **not** settings of a **Run** (a Pipeline instance). Composed atop the control-plane **placement baseline** (default tolerations/nodeSelector/resources stamped on *every* step Pod). |
| **`k8s_overlay`** | A Step's **governed escape hatch** (ADR-0055): a raw pod-spec fragment merged **last** onto the Pod. Carries **no authority** — like a governed **Grant**, it takes effect only if the Run's Environment permits raw overlays, else the Run is rejected **fail-closed**. The `k8s_` prefix marks the backend-coupling (won't run on the local executor). For the rare *dynamic per-job* k8s need; static placement belongs in a `PlacementProfile`. |

### 4.2 Data plane (five distinct concepts — never conflate)

| Term | Scope | Lifetime | Purpose |
|---|---|---|---|
| **Parameter** | launch-time input, supplied from *outside* the run | resolved once at launch, then persisted for the run's life | a typed value a launcher supplies to start a Pipeline. Declared in the Pipeline's `interface.inputs`; each is `required` (static bool) with an optional `default` and optional `validate:` CEL predicate. Supplied by a human/API on a `manual`/`api` launch **and** by an `invoke:` caller's `with:` — one declaration, one env rail (`SCARAB_PARAM_<NAME>`), one launch-time CEL binding `${{ inputs.<name> }}`. **Not** the per-Step workspace `inputs:` (ADR-0007), which is a different concept sharing the word. |
| **Workspace** | intra-run, flows along DAG edges | ephemeral (per run) | the filesystem/checkout Steps build on. **Content-addressed** (per-file merkle CAS). Implicit-by-default (inherit `needs`), explicit-on-demand (`inputs:`/`outputs:`). |
| **Result** | intra-run, flows along DAG edges | ephemeral | small typed values (a version, a bool) for params/conditionals. |
| **Artifact** | output of record | retained (TTL), downloadable, UI-visible | binaries, reports, coverage, images. |
| **Cache** | cross-run | best-effort, evictable | `~/.cargo`, `node_modules` — keyed (e.g. lockfile hash). **Not** correctness-critical. |

### 4.3 Run-time / instance plane (what the durable engine tracks)

| Term | Meaning |
|---|---|
| **Run** | A durable *instance* of a Pipeline for a specific Event/commit. Stores the compiled IR + `{ir_version, event_schema_version}` (self-describing). |
| **StepRun** | A durable instance of a Step within a Run. |
| **Attempt** | A single execution of a StepRun. **Restart-step creates a new Attempt.** |
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

### 4.5 Tenancy & forge binding

| Term | Meaning |
|---|---|
| **Org** | A top-level tenant. Owns Projects. The Scarab tenancy boundary — **not** the forge's `owner` namespace (one Org may span a GitHub org *and* a Forgejo instance). |
| **Project** | Scarab's **governed unit of CI** and the **aggregate root** beneath an Org: it binds a **source** (a `RepoRef` on a forge, via a `ForgeConnection`) to its **governance** (Environments + `ProtectionRules`, privilege whitelist, secret scope, OIDC subject) and owns the **Pipelines and Runs** produced from that source. RBAC is enforced at the Project scope. **1:1 with a repo** in v1 (monorepo per-subdir governance deferred to an optional path scope). There is no separate governed "Repo" entity — a Project *is* the governed repo. |
| **`RepoRef`** | A **forge coordinate** — `{owner, name}` as the forge addresses a repository, plus the forge it lives on. External and mutable (a forge rename/transfer changes it). Carried by `Event`/`Status`. Lives in `scarab-forge`; it is the *only* concept named "Repo". Resolved to a Project via a `ForgeConnection`. |
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
                                    ├─ scarab-executor-k8s  ── Pod per Step
                                    └─ scarab-executor-local ─ kind/local
                                                                │
   Postgres  ◀── state tables + append-only event log ──────────┤
   Object store (S3/MinIO) ◀── workspace (merkle CAS) · logs · artifacts · cache
```

- **One binary, many roles** (`scarab-server --role converged|api|scheduler|executor|webhook`).
  Postgres (outbox) is the coordination bus; no service-to-service RPC required internally.
- **Control-plane / data-plane split:** the durable *brain* records intent ("Step S should
  run"); the *executor* observes it, creates the Pod, watches it, writes back terminal state.
- **Two stateful dependencies only:** Postgres (state + event log) and an object store
  (blobs). Nothing else.

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
scarab-db-postgres · scarab-forge-github · scarab-secrets-postgres
scarab-storage-s3 · scarab-executor-k8s · scarab-executor-local
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

## 9. Roadmap (tracer-bullet vertical slices)

1. **Durable core skeleton** — `POST /runs` (inline 1-step) → admit → k8s Pod → logs
   (SSE + object store) → terminal → **kill control-plane mid-run, resume exactly-once.**
2. IR + YAML + CEL; multi-step DAG; content-addressed workspace; restart-a-step.
3. GitHub adapter + identity: webhook → in-repo `.scarab` → run → checks back; OAuth login.
4. Scheduler richness + gates: concurrency groups, auto-cancel, fairness, priority; Environments/approvals.
5. Secrets + OIDC issuer + BuildKit: encrypted secrets, keyless federation, fork-PR lockout, image builds.
6. UI (SolidJS): live DAG, logs, restart/resume, time-travel timeline.
7. Local exec + CLI polish + provenance/signing fast-follow.
