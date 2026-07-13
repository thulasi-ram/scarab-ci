# Scarab — Context & Ubiquitous Language

> Scarab is a **durable-execution, Kubernetes-native CI system** written in Rust —
> a forge-integrated alternative to GitHub Actions.
>
> **Thesis:** *Your pipeline is a workflow that survives crashes, not a fire-and-forget batch job.*

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
| **Environment** | A first-class deployment target (staging/prod) with **protection rules**: approvers, wait timer, allowed refs, concurrency, secret scope, OIDC subject, deployment history, and a **privilege whitelist** (which image digests may run with which governed grants — ADR-0039). |
| **Grant** | A named, closed-vocabulary escalation above the hardened "restricted" step baseline (ADR-0039). `run-as-root` (self-service — does not escape the sandbox); `add-capabilities` and `privileged` (**governed** — require an Environment whitelist entry keyed on the image **digest**, Administer-only). Requested by the pipeline author, granted by the Environment admin, admitted fail-closed. A grant is a ceiling, not a default. |

### 4.2 Data plane (four distinct concepts — never conflate)

| Term | Scope | Lifetime | Purpose |
|---|---|---|---|
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
scarab-forge       ForgePort + canonical Event/Status/Repo
scarab-identity    Authenticator, OidcIssuer, RBAC
scarab-secrets     SecretProvider
scarab-storage     ObjectStore + Cas (merkle content-addressing)
scarab-projects    Org/Repo/Project/Environment + protection rules
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
