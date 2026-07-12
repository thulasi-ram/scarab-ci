# Handoff — Scarab after slices 2–6 (slice 7 + follow-ups)

Slices 1–5 are shipped and green; slice 6 is the scaffold it was scoped to be.
This picks up where the unattended `slices-2-6` loop left off. Read `CONTEXT.md`
and the cited ADRs before starting any item.

- **Predecessor handoff:** [`slices-2-6.md`](slices-2-6.md) — historical (the
  backlog that this work completed). Locked decisions there (+ ADR-0031/0032)
  are still in force.
- **HEAD when written:** `1639da0`. **`cargo check --workspace` green; 119 tests
  pass, 4 `#[ignore]` (kube/registry-gated); clippy clean.**

## Current state (what works end-to-end)

The whole GitHub→GitHub loop runs against real Postgres with fakes for the true
externals:

- **Slice 2** — YAML→IR compile+validate (DAG/matrix), CEL (`when`/interp/matrix
  predicates), dependency-aware admission, per-file merkle CAS, content-addressed
  workspace along DAG edges, restart-a-step.
- **Slice 3** — canonical `ForgePort`; GitHub webhook ingest + HMAC verify;
  in-repo `.scarab/ci.yaml` → run on trigger; Check/commit-status posted back via
  the outbox; OAuth login + Scarab-native RBAC.
- **Slice 4** — concurrency groups + cancellation; auto-cancel superseded runs;
  fairness/backpressure/priority; durable Gate step (suspend/approve across
  restart); first-class Environments + protection rules + deploy history.
- **Slice 5** — envelope-encrypted secrets (Postgres); scoped injection + log
  redaction; Scarab as an OIDC issuer (JWKS + rotation); fork-PR secret lockout +
  restricted subject; rootless BuildKit build step.
- **Slice 6** — `scarab-server --emit-openapi` + committed `openapi.json`;
  `ui/` SolidJS scaffold with a generated, typed client (`npm run gen` /
  `npm run typecheck`).

All 27 slices-2-6 `git-bug` issues are **closed**; each commit body records its
decisions and any deferral.

## Environment (this machine)

- Real Postgres: `export SCARAB_TEST_DATABASE_URL="postgres://thulasiram@localhost:5432/postgres"`
  — PG-backed tests run; they skip cleanly without it.
- **Node 24 is available** (mise) — the `ui/` TS toolchain works here.
- **No Docker daemon / no `kind` / no `just`** → cluster/BuildKit/browser
  live-runs stay `#[ignore]` + env-gated (`SCARAB_TEST_KUBE=1`). Verify with real
  Postgres + fakes.
- ⚠️ The ambient kubeconfig points at **real Acme prod/staging EKS** — never
  launch against it. Dev uses an isolated `dev/.kubeconfig` (kind only).

## Slice 7 (the roadmap's next slice)

CONTEXT.md §9.7: **local exec + CLI polish + provenance/signing fast-follow.**
No `git-bug` issues exist for it yet — break it down with the `to-issues` skill
first. Starting points:

- **`scarab-executor-local`** — currently a stub (`unimplemented!`): implement
  the `Executor` port by spawning a local process per step (kind/local dev, no
  k8s). Idempotent on the fence like the k8s executor (ADR-0019).
- **`scarab-cli`** — a stub crate: the generated-from-OpenAPI CLI (ADR-0012).
  `ui/openapi.json` + a codegen already exist to build from.
- **Provenance/signing** — SLSA/cosign/SBOM export for built images (ADR-0015),
  building on the `ImageArtifact` + push-fence from slice 5.

## Deferred within slices 2–6 (substrate in place, wiring pending)

Each is a *thin* follow-up, not new design; all noted in commit bodies /
`TODO(slice-N)` markers. Highest-leverage first:

> **Progress (follow-up session after `24a570b`):** #1, #2, #3, #4, #6, #7-core,
> and #5-timer are **done**, plus `when:` runtime wiring and the
> `EnvironmentStore`+OIDC production wiring — commits `63f3dcd`, `aba7456`,
> `c535630`, `7bf9ae2`, `5149401`, `5366fe6`, `9483b88`, `fdf4502`, `e5fa736`.
> **Remaining is now all either blocked on live externals or needs a decision:**
> #4-`outputs:` (live CAS path), #5 transitive-skip (a semantic decision — see
> note) and external-gate token-auth (needs a token model), the k8s post-step
> output (cluster), and GitHub-App/OAuth/secrets (stub adapters + live
> externals). Tree green: 141 tests, 4 `#[ignore]`, clippy clean.

1. ✅ **Pipeline authoring of the engine features** — `concurrency:`, `kind:
   gate`, and `environment:` all now author from a committed `.scarab`
   (`63f3dcd`, `5366fe6`).
2. ✅ **`GET /v1/runs` list endpoint** — added (`aba7456`); `Db::list_runs`,
   OpenAPI + typed UI client regenerated. The SolidJS route still needs wiring
   to *call* it (attended UI follow-up).
3. ✅ **Restart skip-if-unchanged** (ADR-0027 default) — done (`c535630`). Added
   `Executor::output`, `Db::set_step_input`/`step_input` (migration 0013), a pure
   `input_signature`, and the skip decision in `admit` (unchanged descendant →
   `Succeeded` + `StepSkipped` event, output carried forward; explicit target
   always re-runs; side-effecting/no-output steps never skip). **Note:** the k8s
   executor's `output` still returns `None` until the live post-step CAS snapshot
   is wired, so in a *real cluster run* skip won't trigger yet (safe cascade);
   the engine logic is fully exercised via the FakeExecutor. Wire k8s
   post-step output with the live-workspace path.
4. ⚠️ **Explicit workspace `inputs:`/`outputs:`** — `inputs:` **done** (`7bf9ae2`):
   authored as a subset of needs, stored via `set_step_inputs`, and used to
   compute a precise skip-if-unchanged signature. **Remaining:** `outputs:` (which
   paths a step publishes) — a CAS-layer path filter at snapshot time, part of the
   live-workspace path.
5. ⚠️ **Gate auto-release + transitive skip.** **timer auto-release done**
   (`9483b88`): a `timer` gate carries `gate_after` seconds and the scheduler
   releases it once elapsed. Also **`when:` is now applied at run creation**
   (`fdf4502`). **Remaining:** (a) *transitive skip* — a descendant of a pruned
   step currently runs as a root (edges dropped); making it skip instead is a
   **semantic reversal** of the documented `select_steps` behavior (the
   `when_selects_steps_and_prunes_edges` test codifies "kept, runs as root") —
   needs an explicit decision, likely an ADR. (b) *external-gate token auth* — an
   external gate is releasable today via `POST …/gates/:step/approve` (RBAC);
   releasing via an opaque token needs a token model.
6. ✅ **Multi-file `.scarab/*.yaml`** discovery — added (`5149401`);
   `ForgePort::list_dir_at_ref`, `trigger_run_from_event` → `Vec<RunId>`,
   per-pipeline supersede key.
7. ⚠️ **Deploy auto-cancel opt-out via a real Environment target** — **core
   done** (`5366fe6`): an authored `environment:` opts the run out of newest-wins
   supersede. **Remaining:** enforce the environment's protection rules at
   admission for *triggered* deploy pipelines — needs `EnvironmentStore` in
   `AppState`/`main.rs` (see Production wiring). The explicit
   `POST /v1/environments/.../deploy` endpoint already enforces them.

## Production wiring in `scarab-server/src/main.rs`

- ✅ **`EnvironmentStore` + `OidcIssuer`** now constructed in `main.rs`
  (`e5fa736`): the Postgres adapter backs `EnvironmentStore` (when connected);
  an `Rs256Issuer` is generated when `SCARAB_OIDC_ISSUER` is set.
- ⛔ **GitHub App auth** (App JWT → installation token) — **blocked, not mere
  wiring:** `GithubForge`'s API methods (`read_file_at_ref`, `set_status`,
  `latest_commit`, …) are all `unimplemented!()`. Needs a real GitHub HTTP
  adapter + App-auth, verifiable only against live GitHub (`#[ignore]`). Until
  then `AppState.forge = None` and the live webhook read / status-post don't run.
- ⛔ **Real OAuth + PG-backed `SessionStore`** — only `FakeAuthenticator` /
  `InMemorySessions` exist; both need new adapters (OAuth needs live creds).
- ⚠️ **`SecretProvider` (PostgresSecrets)** — the adapter exists, but there is no
  injection wiring in the driver (`converged.rs`/scheduler don't reference it).
  Threading it into the executor step-launch path is the remaining work.

## Live paths gated `#[ignore]` (need a real cluster/registry)

k8s executor round-trip, rootless BuildKit build, cluster workspace round-trip,
live-kind diamond, and any live GitHub/BuildKit/cloud-OIDC calls. Run with
`SCARAB_TEST_KUBE=1` against the dev kind cluster once one exists.

## Attended follow-up (needs a browser env, out of scope for an unattended loop)

The rich SolidJS UI — live DAG, SSE logs, restart/resume controls, time-travel
timeline — plus a real router and a Vite dev server (ADR-0028). The `ui/`
scaffold + typed client are the foundation.

## Housekeeping

- **`git-bug` `59d5d36`** ([slice-1] durable-core epic) is still `open` — its
  ACCEPTANCE (`cb7a394`) closed long ago; it is likely just a tracking issue to
  close. Confirm before closing (not owned by the slices-2-6 loop).

## Ritual (unchanged from the predecessor handoff)

Per issue: read cited ADRs → implement → keep `cargo check --workspace` green →
minimal tests the acceptance implies (real Postgres via `SCARAB_TEST_DATABASE_URL`,
mock only true externals, cluster/BuildKit/GitHub/UI live-runs `#[ignore]`+env-
gated) → commit `<type>(<area>): <subject>` with a body, trailer
`Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` →
`git-bug bug status close <id>`. Honor hexagonal purity (ADR-0016/0031) and
classical testing (ADR-0017).
