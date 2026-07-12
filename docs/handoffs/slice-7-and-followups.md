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

> **Progress (follow-up session after `24a570b`):** #1 (concurrency + gate +
> environment authoring), #2 (`GET /v1/runs`), #6 (multi-file discovery), and
> #7 (deploy supersede opt-out) are **done** — commits `63f3dcd`, `aba7456`,
> `5149401`, `5366fe6`. #3 was investigated and found **blocked** — see its note.
> Remaining: #4, #5, and the admission-enforcement tail of #7.

1. ✅ **Pipeline authoring of the engine features** — `concurrency:`, `kind:
   gate`, and `environment:` all now author from a committed `.scarab`
   (`63f3dcd`, `5366fe6`).
2. ✅ **`GET /v1/runs` list endpoint** — added (`aba7456`); `Db::list_runs`,
   OpenAPI + typed UI client regenerated. The SolidJS route still needs wiring
   to *call* it (attended UI follow-up).
3. ⛔ **Restart skip-if-unchanged** (ADR-0027 default) — **blocked, not thin.**
   The decision needs a step's output hash *before vs after* re-run, but the
   production path never records outputs: `Scheduler::finalize_step` doesn't call
   `set_step_output`, and `ExecState::Succeeded` carries no output hash. The
   `set_step_output`/`step_output` *storage* substrate exists but is only ever
   written from tests. Implementing just the decision logic now would land
   inert, unexercised code. **Do this together with output-snapshot production
   wiring** (the executor reporting an output hash + `finalize_step` recording
   it) — i.e. after/with #4 and the live-workspace path.
4. **Explicit workspace `inputs:`/`outputs:`** — only implicit-by-default is done;
   needs IR fields. `engine/lib.rs` TODO(slice-2). (Consumer — workspace
   materialization — is still test-only; see #3's note.)
5. **Gate `timer`/`external` auto-release** + transitive skip of a gated step's
   descendants. `pipeline/lib.rs` TODO(slice-4).
6. ✅ **Multi-file `.scarab/*.yaml`** discovery — added (`5149401`);
   `ForgePort::list_dir_at_ref`, `trigger_run_from_event` → `Vec<RunId>`,
   per-pipeline supersede key.
7. ⚠️ **Deploy auto-cancel opt-out via a real Environment target** — **core
   done** (`5366fe6`): an authored `environment:` opts the run out of newest-wins
   supersede. **Remaining:** enforce the environment's protection rules at
   admission for *triggered* deploy pipelines — needs `EnvironmentStore` in
   `AppState`/`main.rs` (see Production wiring). The explicit
   `POST /v1/environments/.../deploy` endpoint already enforces them.

## Production wiring not yet connected in `scarab-server/src/main.rs`

Built + tested, but the converged binary doesn't construct them (so these paths
are inert in a running server):

- **GitHub App auth** (App JWT → installation token) — until wired,
  `AppState.forge = None` and the driver's forge is `None`, so the live webhook
  `read_file_at_ref` and status-post paths don't run.
- **Real OAuth + PG-backed `SessionStore`** (currently in-memory; API authz is
  default-open when no store is configured).
- **`SecretProvider` (PostgresSecrets) / `OidcIssuer` (Rs256Issuer) /
  `EnvironmentStore`** — not yet placed in `AppState`/`main.rs`.

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
