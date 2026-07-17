# Production-readiness audit — gaps & facades

- **Date:** 2026-07-16 · **HEAD:** `c9660bd` (main, clean tree)
- **Method:** seven parallel code audits (facades, forge, durability core, k8s
  execution, security/identity, ops/deploy, product surface), every finding
  grounded in `file:line`. Cross-checked against CONTEXT.md, ADRs 0001–0044,
  `docs/positioning.md` red lines, `docs/followups.md`, and the slice-7 handoff.
- **Scope:** what separates today's code from a production system. Not a bug
  list — a map of what is real, what is facade, and what is missing entirely.

**One-sentence verdict:** Scarab today is a well-architected, crash-safe
*orchestrator of image-only containers* with a genuinely strong durable
substrate (outbox, fencing, leases, idempotent Pod launch) — but it is not yet
a CI (no source ever reaches a step), not yet multi-user (auth is off,
fail-open), and several of its own signature invariants (forward-progress-or-
dead-letter, retry taxonomy, retention) exist only in docs and type
definitions.

---

## Part I — The load-bearing facades, ranked

### 1. Production authentication does not exist (and fails *open*)

- `AppState::with_auth` is never called in the binary — only in tests
  (`crates/scarab-server/src/main.rs` has no call; `tests/auth.rs:36`).
- With no session store, `authorize()` **grants every anonymous caller
  `Owner`**: `crates/scarab-server/src/lib.rs:1864-1870`.
- `POST /v1/auth/login` returns 404 in production (`lib.rs:1822-1824`); the
  only `Authenticator`/`SessionStore` impls are `FakeAuthenticator` /
  `InMemorySessions` in scarab-testkit (`testkit/src/lib.rs:1162,1195`). No
  OAuth code exists anywhere (`grep oauth` in `crates/` = 0 hits); no
  `sessions`/`rbac` tables exist in any migration.
- The scoped `Rbac`/`Binding` model (`scarab-identity/src/lib.rs:64-108`) is
  dead code: `authorize()` checks only the *global* role
  (`lib.rs:1880`), never role-in-`{org}/{repo}`.
- Session cookie (were login ever wired) lacks `Secure`
  (`lib.rs:1837-1840`); no CSRF protection beyond `SameSite=Lax`.

**Why it ranks first:** every RBAC-gated route in the API — secrets CRUD,
environment protection rules, privileged-grant admission — is currently open.
The entire ADR-0037/0039 governance edifice rests on an authorizer that
returns `Owner` unconditionally.

### 2. Outbound GitHub is `unimplemented!()` — and never wired anyway

- 8 of 9 `ForgePort` methods panic: `latest_commit` (`scarab-forge-github/src/lib.rs:178`),
  `read_file_at_ref` (:187), `list_dir_at_ref` (:196), `register_webhook`
  (:200), `set_status` (:213), `create_deployment` (:217), `post_comment`
  (:221), `get_permissions` (:225). Only `normalize_event` is real (:203-205).
  The `reqwest::Client` and `token` fields are `#[allow(dead_code)]`
  (:157-161). No App-JWT/installation-token code; no JWT dep in the crate.
- Production never wires a forge at all: `spawn_driver(..., None, ...)`
  (`main.rs:229`), `AppState.forge = None`. Webhooks are accepted, verified,
  and **dropped** (`lib.rs:1758-1763` → `{"ignored":"no forge configured"}`).
  Status posting would panic if a forge were wired (`drain_forge_statuses`
  calls `set_status`, `lib.rs:1668`).
- Webhook ingest itself is real (constant-time HMAC, `forge-github:21-29`,
  handler `lib.rs:1721-1790`) but has **no replay/delivery-id dedup** — the
  `x-github-delivery` id is read (`lib.rs:1741`) and never stored/checked.
  ADR-0010:35 claims "redelivery idempotency lives in the adapter" — false.
- The webhook route is hard-bound to the GitHub adapter (calls
  `scarab_forge_github::normalize` directly, `lib.rs:1735,1746`), not routed
  through the port — a second forge would need route surgery, not just an
  adapter crate.

### 3. No source ever reaches a step — the CI cannot check out code

The deepest structural gap, and the one no ADR covers.

- `build_pod` (`scarab-executor-k8s/src/lib.rs:244-345`) mounts **no
  workspace**: no clone/checkout init container, no CAS materialize, no
  post-step snapshot. The only volume is the results emptyDir (:286-327).
- `Cas::materialize`/`ingest` (`scarab-storage-s3/src/lib.rs:221-244`) and
  `workspace_inputs` (`scarab-engine/src/lib.rs:379-387`) are **never called**
  outside tests.
- `Executor::output` is not overridden → port default `Ok(None)`
  (`engine/src/ports.rs:383-385`) → `set_step_output` never fires
  (`scheduler.rs:622-624`) → skip-if-unchanged (`scheduler.rs:483-499`) never
  triggers on k8s; `outputs:`/`inputs:` are authored, validated, stored — and
  inert. (Stale doc comment at `scheduler.rs:85-89` still calls the engine-side
  skip a TODO; the admission-side logic exists but is starved of outputs.)
- The `#[ignore]` tests confirm it: `storage-s3/tests/workspace.rs:78-81`
  ("TODO(slice-2): wire init-container/post-step into build_pod"),
  `db-postgres/tests/acceptance.rs:207-210`.
- The local executor is the same: fresh empty temp dir per step
  (`executor-local/src/lib.rs:115-121`).

**Consequence:** every "green pipeline" to date ran steps whose entire world
was their container image. Checkout, build-from-source, artifact-passing along
`needs` edges — the substance of CI — has never executed anywhere except via
`FakeExecutor`. There is no ADR for source provisioning (authenticated clone
vs forge-tarball→CAS, depth, submodules, LFS); ADR-0029's open item is CAS
chunking, not checkout.

### 4. The durability invariants at the center of the wedge are unimplemented

CONTEXT.md §7 invariant 1: "Forward progress or explicit dead-letter. A Run
never loops forever." Current reality:

- **Retry taxonomy is dead code.** `finish_attempt(failure, max_attempts)`
  with the `Infra`-retry branch exists (`engine/src/lib.rs:588-618`) but has
  zero callers; the scheduler hardcodes every failure as `FailureKind::Step`
  and never retries (`scheduler.rs:635-646`). `retry`/`max_attempts` are not
  even expressible in the pipeline IR (grep: no matches). ADR-0020:16
  ("Infra/transient → auto-retry with backoff, by default") is unmet.
- **`DeadLettered` is never produced.** Legal transition defined
  (`engine/src/lib.rs:461`), DB round-trip exists — no code path ever
  transitions to it.
- **No timeouts of any kind.** No step timeout (`activeDeadlineSeconds`
  absent from `build_pod`), no run timeout, no gate deadline-fail — repo-wide
  grep for deadline/timeout on the execution path is empty. A hung container
  wedges its run forever; nothing but explicit cancel (which has no API
  route, see Part II) can end it.
- **Stuck-Pending is only half-fixed.** `c9660bd` maps terminal *waiting*
  reasons to failure (`executor-k8s/src/lib.rs:579,588-607`), but a Pod
  legitimately stuck `Pending` (Unschedulable, quota) is `_ => Pending`
  (:580) and the run waits forever; `Lost` is grouped with
  `Pending|Running` and never escalated (`scheduler.rs:650`).
- **Outbox has no poison handling.** No attempts column, no max-delivery, no
  DLQ (`migrations/0001_initial.sql:67-78`, `0003_outbox_claim_lease.sql`); a
  permanently-failing message (e.g. `MissingSpec`) redelivers every 30s
  forever.
- **Result-ingest fencing isn't attempt-current.** The HMAC covers
  `{run,step,attempt}` (`lib.rs:2100`) but the write is keyed `(run,step)`
  only (`lib.rs:2158-2168`) — a stale prior-attempt sidecar holding a
  legitimately-minted token can overwrite a newer attempt's results, which
  then interpolate into downstream `${{ outputs.* }}`.

What *is* solid: SKIP LOCKED claiming (`db-postgres/src/lib.rs:127-169`),
transactional outbox with idempotency keys (:948-1014), deterministic
fence-named idempotent Pod launch/adopt (`executor-k8s:100-123,219-229`),
optimistic-concurrency transitions (`scheduler.rs:894-926`), leader-gated
admission via Postgres lease (`scheduler.rs:361-367`), durable gate timers
recomputed from the event log (`scheduler.rs:798-842`). The crash/resume
acceptance (`crash_resume.rs`) genuinely passes against real Postgres.

### 5. Crypto that silently self-destructs

- **Secrets KEK:** `SCARAB_MASTER_KEY` unset or malformed →
  `master_from_env().unwrap_or_else(random_bytes)`
  (`scarab-secrets-postgres/src/lib.rs:44,222-228`) — a random ephemeral key,
  no warning, no startup failure. Every secret written becomes permanently
  undecryptable at the next restart. No KEK rotation, no key_version column,
  no KMS path.
- **OIDC issuer:** a fresh RSA key is generated **every boot**
  (`main.rs:267-275`); JWKS changes on each restart/replica, breaking any
  cloud-side trust. And `OidcIssuer::issue` is never called on the run path —
  no per-run token is minted or injected (tests only) — so keyless federation
  (ADR-0015) and the fork-PR restricted-subject downgrade
  (`fork_policy().oidc_env`, `lib.rs:2518`, consumed only by tests) are
  currently decorative.

### 6. Enabling result capture fails every step

A live interaction, not just a missing piece: the ADR-0042 sidecar image does
not exist (nothing builds it; `image.yml:26-27` calls it "future"), the
default points at a phantom (`ghcr.io/scarab/sidecar:latest`, `main.rs:180` —
which also disagrees with the chart's `ghcr.io/thulasi-ram/scarab-sidecar:latest`,
`values.yaml:45`). Set `SCARAB_RESULTS_TOKEN_SECRET` and the sidecar is
injected (`executor-k8s:316-327`) → ImagePullBackOff on the init container →
`c9660bd`'s terminal-waiting detection (which chains
`init_container_statuses`, :604) **fails the step**. Unset it and results are
silently empty (`ports.rs:393-398`). Named results on k8s (ADR-0041/0042) are
therefore unreachable in both configurations.

### 7. The CLI lies about succeeding

4 of 5 subcommands are stubs that print "not yet implemented" and **exit 0**
(`scarab-cli/src/main.rs:126-139`). A script running `scarab validate` in CI
passes while validating nothing. Only `run` (dispatch + `--describe`,
`--param`) is real (:160-166). CONTEXT.md:172 and README.md:99 call this a
"generated-from-OpenAPI CLI"; it is a hand-written clap tree covering 2 of 14
spec'd operations.

---

## Part II — Subsystem gap maps

### Forge / triggers

| Gap | Evidence |
|---|---|
| GitHub App auth (JWT → installation token, refresh) absent | `forge-github/src/lib.rs:156-173`, Cargo.toml has no JWT dep |
| Replay/delivery-id dedup absent | id read `lib.rs:1741`, never stored |
| Repo CRUD / repo registry absent — nothing to attach a webhook or permissions to | router `lib.rs:2588-2628` has no repo routes |
| `on: cron` — type exists (`scarab-forge/src/lib.rs:144`), **no scheduler fires cron events** | no producer anywhere |
| `on: upstream` — type exists (:114), **no dispatcher produces it**; cross-pipeline causation unbuilt | no producer |
| Second forge (GitLab/Forgejo): port is clean but webhook route hard-binds GitHub | `lib.rs:1735,1746` |
| Status context hardcoded `"scarab"`, `target_url: None` (no deep link back to the run UI) | `lib.rs:1659-1661` |

### Durable engine

| Gap | Evidence |
|---|---|
| Retry taxonomy unwired; IR can't express retries | `engine/src/lib.rs:588-618` (no callers), `scheduler.rs:635-646` |
| No step/run/gate timeouts | greps empty; no `activeDeadlineSeconds` |
| `DeadLettered` never produced | `engine/src/lib.rs:461` (edge only) |
| Outbox poison → infinite redelivery | schema `0001_initial.sql:67-78` |
| Run cancellation has **no API route** and scheduler never calls `executor.cancel` | router `lib.rs:2588-2628`; grep `cancel` in scheduler → `cancel_run` transitions state only (`scheduler.rs:772-791`) |
| Event log cannot rebuild state (no fold/replay) — fine per ADR-0002, but "time-travel" headline has no query endpoint and no replay substrate | `0001_initial.sql:3-4,50-52`; no replay code |
| `tick_all` is one serialized loop over all runs at 500ms, N sequential DB round-trips per tick — first scale ceiling | `scheduler.rs:348-358`, `main.rs:233` |
| Multi-replica: admission/timers leader-gated (safe); log tailer is per-process in-memory → **2 replicas double-ingest every log** | `log_tail.rs:80-96` |
| Control-plane restart re-tails running Pods from byte 0 → duplicate chunks across restart (in-process dedup only) | `log_stream` no `since_time` (`executor-k8s:171-175`) |

### K8s execution

| Gap | Evidence |
|---|---|
| **Completed/failed pods are never deleted** — they leak in the namespace; no ownerReferences, no TTL, and `Executor::cancel` (`executor-k8s:138-151`) has no scheduler caller | `build_pod` metadata :330-335; grep |
| Authored `resources:` dropped in IR→StepSpec lowering — step pods run with **no requests/limits** | `StepSpec` `engine/src/lib.rs:183-202` has no resources; `pipeline/src/lib.rs:336,417-419` authored but unlowered (ADR-0026 unmet) |
| No nodeSelector/affinity/tolerations/runtimeClass | `PodSpec` :336-342 |
| `kind: build` never dispatches — `build_pod_for_build`/`BuildSpec`/`push_fence` (:427-555) referenced only by tests; `launch` always calls `build_pod` (:115) | grep |
| BuildKit registry auth absent (no docker config, no pull/push secrets) | `build_pod_for_build` :462-533 |
| Secrets delivered as plaintext env in the Pod **spec** — readable by anyone with pod-read in the shared namespace | `secret_executor.rs:83-85`, `build_pod` :256-264 |
| Shared namespace for all step pods (not namespace-per-run as ADR-0005 states); no NetworkPolicy default-deny (ADR-0030 claims it); default SA token mounted (no `automountServiceAccountToken: false`) | `main.rs:88-89`; greps empty |
| `service` step kind (CONTEXT §4.1) does not exist in the IR | `pipeline/src/lib.rs` step kinds = image or gate (:264-265,1214-1220) |
| Log append re-reads the full chunk index per 8KiB chunk — O(n²) over a long log | `logs.rs:120-123` |
| `waiting_reason` double-`unwrap` on Pod status | `executor-k8s:620` |

### Security & tenancy

| Gap | Evidence |
|---|---|
| Tenancy is not modeled on runs: no org/repo owner columns (only `deploy_org/deploy_repo` for env runs, migration 0016); `list_runs` is unscoped `SELECT ... FROM runs` — any caller lists/reads any run/logs/events | `0001_initial.sql:13-22`, `db-postgres:716-722`, `lib.rs:786,806-831` |
| Gate + results HMAC tokens carry no timestamp → no expiry, no revocation (ADR-0034 defers; fine to defer knowingly) | `lib.rs:2077,2100-2102` |
| Server is plaintext HTTP only (assumes terminating proxy — fine, but undocumented); sidecar posts fence token over in-cluster `http://` by default | `main.rs:281-283,177-178` |
| Interpolated `${{ outputs.* }}` flows unquoted into argv/env; safe as exec-form argv, but poisoned results (see stale-attempt overwrite) reach entrypoints verbatim | `lib.rs:1487-1504`, `cel.rs:113-118` |
| No cap on matrix expansion / total step count (invoke depth capped at 8, `pipeline:637,729-731`) — a pipeline can compile arbitrarily large DAGs | grep `MAX_STEPS` empty |
| CEL: no explicit eval budget (relies on CEL totality); known upstream parser panic contained via `catch_unwind` (`cel.rs:19-35`) | |
| Log redaction is exact-byte substring only — transformed/encoded secrets pass through | `logs.rs:80-87,176-192` |
| API error bodies leak backend detail (`db error: {sqlx}`) | `lib.rs:456-458` |

### Ops & deploy

| Gap | Evidence |
|---|---|
| **No CI runs the Rust suite** — workflows are image-build + docs only; tests/clippy never run on push/PR | `.github/workflows/{image,docs}.yml`; README.md:45 admits it |
| **Zero tags ever cut** → GHCR `latest`/semver never published; docs site (tag-gated Pages) never deployed | `git tag` empty; `docs.yml:9-12` |
| No metrics (no metrics/prometheus dep at all), no structured logs, no request IDs, bare `fmt::init()` | `main.rs:104`; Cargo.tomls |
| `/healthz` is a static "ok" — no readiness distinct from liveness, no DB/store checks; Helm probes both point at it | `lib.rs:2528-2530`, `values.yaml:104-115` |
| No graceful shutdown — no signal handler, driver JoinHandle discarded | `main.rs:281-283,225` |
| **No retention/GC anywhere**: runs/events/log chunks/objects grow forever; ADR-0030's sweeper + lifecycle rules unbuilt | greps; `0001_initial.sql:52` |
| Pool defaults (2×10 conns), no statement/lock timeouts; claim-sort and outbox-kind filters not fully index-covered | `db-postgres:52-53`, `secrets-postgres:50-53` |
| S3 creds `unwrap_or_default()` → empty-string creds silently | `main.rs:145-148` |
| No startup config validation (missing KEK, empty S3 creds, phantom sidecar image all boot "successfully") | `main.rs` |
| Helm: no HPA/PDB/NetworkPolicy/ServiceMonitor; no bundled or documented Postgres/S3 story beyond "bring your own"; no migration Job (in-process migrate at boot) | `deploy/helm/scarab/*`, `main.rs:131` |
| ADR-0022's "CI tests old-binary × new-schema" does not exist; work-claiming carries no engine-version tag | `claim_ready_steps` filters status only |
| No backup runbook (ADR-0030 promises PITR + restore runbook) | absent |
| `Notifier` port (ADR-0030) does not exist | grep empty |

### Product surface

| Gap | Evidence |
|---|---|
| UI has **no production serving path**: server serves no static files, no CORS layer, Dockerfile excludes `ui/` — Vite-dev-proxy only | grep `ServeDir` empty; `Dockerfile:25-28`; `vite.config.ts:14-17` |
| Dashboard inbox/activity/repos **hardcoded** (`catalog.ts:96-108,135-178,193-202`); RepoView environments/settings mocked; run provenance (sha/branch/message/duration) **fabricated client-side** by `enrichProvenance` (`catalog.ts:74-94`) even on real runs | |
| Real UI paths: run detail (DAG from real steps, SSE logs, restart), Run-Pipeline param form, secrets CRUD/matrix | `RunDetail.tsx:72-124`, `RunPipeline.tsx:42-125`, `RepoView.tsx:286-402` |
| No UI login route at all; user hardcoded `"t.ram"` | `App.tsx:13-16`, `Layout.tsx:32` |
| No gate approve/resume control in UI (inbox rows carry `real:false`); Cancel disabled ("lands with scheduler support") | `catalog.ts:137-177`, `RunDetail.tsx:168-170` |
| Missing product APIs: run cancel, whole-run re-run, artifact download (no artifact store exists at all), repo CRUD, user/org management, run deletion | router `lib.rs:2588-2628` |
| `Cache` (CONTEXT §4.2) has no implementation anywhere | grep empty |
| OpenAPI covers 14 of ~23 routes (hand-maintained `#[openapi(paths)]`, `lib.rs:2541-2556`); environments/matrix/results/login absent → UI plain-fetches untyped (`client.ts:265-282`); no CI drift gate; client gen manual | |
| Docs site: solid skeleton (ADR/OpenAPI auto-synced) but deploy-helm page is a stub that predates the actual chart, authoring/reference pages WIP, hero screenshot is a placeholder, and the site has never been published | `astro.config.mjs:66`, content files |
| Onboarding zero→green exists **only** for the canned local demo (`just up && just demo`); onboarding a real repo is impossible (no forge, no repo registry, no webhook registration) | README:109-149 |

---

## Part III — Concepts in the ubiquitous language with no implementation

From CONTEXT.md §4 — worth knowing which words are currently fiction:

| Concept | Status |
|---|---|
| `service` step kind | Absent from IR |
| `Cache` (cross-run) | Absent entirely |
| `Artifact` (retained, downloadable) | Only `ImageArtifact` struct in executor tests; no store, no API |
| Triggers `cron`, `upstream`, comment-command dispatch | Types normalize; nothing produces or schedules them |
| Time-travel | Event log exists (append-only, versioned); no query/replay endpoint, no state-rebuild capability |
| Notifications (`Notifier` port) | Absent |
| Resource/placement (ADR-0026) | Authored in YAML, dropped before the Pod |
| Multi-cluster remote agent | Designed-for only (as documented — consistent) |

---

## Part IV — Claims-vs-code ledger

Statements in the repo's own docs that the code contradicts today:

1. CONTEXT §7.1 "forward progress or explicit dead-letter" — no dead-letter,
   no timeouts (Part I.4).
2. CONTEXT §6/README "generated-from-OpenAPI CLI" — hand-written, mostly
   stubs.
3. CONTEXT §7.5 "UI eats the same API" — the dashboard's inbox/activity/
   repos/environments render invented data.
4. ADR-0010 "webhook redelivery idempotency in the adapter" — absent.
5. ADR-0020 "infra failures auto-retry with backoff by default" — never.
6. ADR-0022 "CI tests old-binary×new-schema overlap" — no CI at all.
7. ADR-0026 resource label mapping — dropped in lowering.
8. ADR-0030 NetworkPolicy default-deny, no-SA-token-unless-requested,
   retention TTLs, PITR runbook, rate limits, Notifier — all unbuilt.
9. ADR-0032 "PG-backed session, httpOnly **secure** cookie" — no PG store, no
   `Secure` flag, no OAuth.
10. `docs/positioning.md` red lines — all still accurate (verified line by
    line); positioning is the one document that does not overclaim.

---

## Part IV.5 — Empirical validation (live run, same day)

The static findings above were then exercised against a running server (local
Postgres) and a live **Colima k3s** cluster (verified single-node, API at
`127.0.0.1`, never an EKS context). Results:

**Server-only (Postgres, no cluster):**
- Auth fail-open: **anonymous `POST /v1/runs` → `201` and created a run**;
  anonymous `GET /v1/runs` → `200`. No credentials, Write/Read-gated routes.
- `POST /v1/auth/login` → **`404 not found`** (auth unconfigured; login
  impossible).
- Boot with no `SCARAB_MASTER_KEY`: server started, secrets store initialized,
  **zero warnings** — the silent-random-KEK path is real.
- Well-formed **HMAC-signed** webhook → `200 {"ignored":"no forge
  configured"}` (accepted, verified, dropped). Bad signature → `401`. HMAC gate
  is genuinely enforced; the forge is genuinely unwired.
- `scarab validate` / `lint` / `restart` → "not yet implemented", **exit 0**.

**Live k3s (`SCARAB_EXECUTOR=k8s`):**
- **Source-checkout gap — proven.** A `run_as_root` busybox step printed its
  world: `cwd=/`, workdir is the bare image rootfs, **`NO_WORKSPACE_DIR`**,
  **`NO_GIT_NO_SOURCE`**, env = only `SCARAB_RUN/STEP/ATTEMPT`. A step Pod
  receives no repo, no workspace, nothing but its image + fence vars.
- **Restricted baseline rejects stock root images.** A plain `busybox` step
  (no grant) → `CreateContainerConfigError: container has runAsNonRoot and
  image will run as root`. The run **failed fast** (c9660bd's terminal-waiting
  handling verified live), but the failure is cryptic and unguided.
- **Pod leak — proven.** After both a failed and a succeeded run reached
  terminal, **both Pods remained** in the namespace (`Pending` and
  `Succeeded`). Nothing deletes them.
- **SA token automounted — proven.** The step Pod had
  `kube-api-access-*` projected — every user workload gets in-cluster API
  credentials, contradicting ADR-0030 ("no service-account token unless
  requested").
- **No NetworkPolicy, single shared namespace — proven.** `kubectl get
  networkpolicy` → none; only the one `scarab` namespace (not per-run).
- **Finalization latency (new, observed).** A Pod that reached `Succeeded`
  at ~t+6s did not flip its run to `succeeded` until ~t+25-30s; during that
  window the log tailer logged `log tail ended with error … BadRequest` every
  ~3s (the `RETRY_BACKOFF` path). Not a permanent wedge — it did complete —
  but the pod-terminal→run-terminal lag and the error-spam during it are worth
  a focused look (likely the poll/log-tail interaction, `log_tail.rs` +
  `scheduler.rs` reconcile cadence).

Net: the audit's most consequential claims (auth off, forge unwired, **no
source reaches a step**, pods leak, SA token exposed) are not inferences —
they were reproduced on a real cluster.

## Part V — What to build, in what order (recommendation)

Premise: no MVP corner-cutting; the goal is the *cohesion* thesis actually
being true. The forcing function that orders everything: **Scarab must build
Scarab.** Dogfooding is unreachable until the system can check out source,
and per ADR-0017 the test suite is supposed to grow from real bugs — which
requires real usage. Everything below serves getting to, then exploiting,
that loop.

### Arc A — Make the core truthful (short, high-leverage, unblocks trust)

1. Fail-closed boot: required `SCARAB_MASTER_KEY` when secrets are enabled;
   reject empty S3 creds; validate sidecar image config; startup config
   report. (Kills facade #5 and the silent-misconfig class.)
2. Auth default-deny: `authorize()` without a session store returns 401, with
   an explicit `SCARAB_DEV_INSECURE=1` escape hatch for the dev harness.
3. Durability invariants: wire `finish_attempt` + `FailureKind::Infra`
   (classify OOM/eviction/image-pull), add `retry:`/`timeout:` to the IR,
   `activeDeadlineSeconds` on Pods, run-level deadline, outbox
   `delivery_attempts` + dead-letter, escalate `Lost`/stuck-Pending after a
   deadline, produce `DeadLettered` for real.
4. Attempt-current results guard (compare ingest attempt against the live
   attempt before writing).
5. Rust CI: `cargo test` + clippy + a PG service container on every push —
   plus a kind-in-Actions job that runs the entire `#[ignore]`d live fleet.
   Cheap, and it converts ~5 currently-unverified production claims into
   tested ones.
6. Pod GC: ownerReferences or a delete-on-finalize call; cancel API route
   that actually calls `executor.cancel`.

### Arc B — Become an actual CI (the long pole; everything composes with it)

7. **Source provisioning ADR + implementation** — the missing design. Decide:
   authenticated shallow clone in an init container vs forge-tarball→CAS
   materialization (the CAS route composes with ADR-0029 and makes checkout
   itself content-addressed/skippable). Covers private repos, submodules,
   LFS, depth.
8. **GitHub App adapter for real** — App JWT, installation tokens with
   refresh, the 8 methods, pagination, rate-limit backoff; webhook
   delivery-id dedup table; repo registry (which installations/repos are
   connected) + repo CRUD API.
9. **Workspace in/out of Pods** — init-container materialize + post-step
   ingest, wiring `Executor::output`; this simultaneously activates
   skip-if-unchanged, `inputs:`/`outputs:`, and live workspace passing.
10. **Results egress sidecar image** — the small drain-and-POST binary, built
    in `image.yml` alongside the server; reconcile the two conflicting image
    defaults.
11. Wire `kind: build` dispatch + registry auth; mint + inject per-run OIDC
    tokens (the issuer exists; persistence for its key is part of this).
12. **Dogfood**: Scarab's own repo builds/tests/releases via Scarab on the
    dev cluster. Every gap this trips over becomes the backlog, per the
    testing philosophy.

### Arc C — Operable, multi-user product

13. Identity: real OAuth (GitHub first), PG sessions table, `Secure` cookies,
    CSRF, scoped-RBAC enforcement per `{org}/{repo}`.
14. Tenancy columns on `runs` now (expand-contract is cheap today, painful
    later); scope `list_runs`/`get_run` by principal.
15. Observability: metrics endpoint, structured JSON logs + request IDs, real
    `/readyz` (DB/store checks), graceful shutdown.
16. Retention/GC sweeper + object lifecycle (ADR-0030), backup runbook.
17. Multi-replica: DB-lease the log tailer (or pin tailing to the leader),
    persist OIDC keys, then let the chart say `replicaCount: 2` honestly.
18. Product surface: serve the UI from the server (embed dist) or a proper
    static deploy + CORS; wire dashboard to real data and delete
    `catalog.ts` mocks; run-cancel + artifact story; OpenAPI covers all
    routes with a CI drift gate; finish CLI subcommands (or make stubs exit
    non-zero); cut `v0.1.0` so images + docs site actually publish.

### Missing ADRs to write

- ~~Source provisioning / checkout (Arc B.7) — genuinely undesigned.~~
  **Now designed: ADR-0045 (Proposed)** — `clone` step kind, SHA-pinned git in a
  `scarab-clone` Pod, `.git` into CAS, read-only short-TTL fork token.
- Run cancellation & Pod teardown ownership (scheduler↔executor contract).
- Production identity wiring (OAuth provider, session store, fail-closed).
- Tenancy modeling on runs + scoped-RBAC enforcement.
- Retention/GC implementation (ADR-0030 committed the *what*, not the *how*).
- Multi-replica operation (log-tail lease, role-split guidance, OIDC key
  persistence).
- Artifacts (store, retention, download API) — currently a word.
