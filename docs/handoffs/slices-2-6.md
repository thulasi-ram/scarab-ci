# Handoff — Scarab slices 2–6 (unattended loop)

This document sets up an autonomous `/loop` run across **slices 2–6**. Slice 1 (the durable-core
walking skeleton) is complete and green. Read `CONTEXT.md` and the cited ADRs before each issue.

## Current state (slice 1, done)
- `cargo check --workspace` green; **42 test suites pass, 0 failures, 0 warnings**.
- Working: `POST /v1/runs` (inline 1 step) → durable run/step → scheduler admits → executor
  launches a Pod → logs (SSE + compressed object-store blobs + PG offset index) → terminal →
  **crash-safe exactly-once resume** (`crates/scarab-db-postgres/tests/crash_resume.rs`).
- Converged binary boots and drives runs end-to-end in-process (`server::converged`).
- Dev harness: `just up && just demo` (compose + kind + MinIO) — needs Docker/kind.

## Locked decisions (do not re-litigate)
1. **CEL lives in `scarab-pipeline`** as a pure-computation dep — see **ADR-0031** (purity = no
   I/O/infra, not no deps). No port/adapter for CEL.
2. **Compiled IR is stored on the run** (`runs.ir JSONB`, self-describing per ADR-0022); `needs`
   edges persisted per step. Slice-2 admission reads these.
3. **Workspace on k8s (v1): whole-file CAS.** Init-container fetches the input tree from CAS
   before the step; a post-step wrapper uploads outputs. Chunking internals stay deferred
   (ADR-0029 leaves them open). Cluster round-trip tests are `#[ignore]`-gated.
4. **Slice 6 = scaffold only**: emit `openapi.json`, generate a TS client, and a minimal SolidJS
   project skeleton that type-checks. The rich live-DAG/SSE-logs/restart/time-travel UI is an
   **attended follow-up** (needs a Node/browser env this loop lacks). Put UI under `ui/` (a
   TS project, not a cargo member).
5. **All slice 3–5 implementation choices are fixed in ADR-0032** — cite it, don't re-decide.
   Highlights: **GitHub App** auth (App id + PEM private key from env → installation tokens);
   webhook HMAC verify; `.scarab` read via contents API at the SHA; Check Runs posted via the
   outbox; forge-agnostic identity (GitHub OAuth login → PG-backed session, native RBAC);
   concurrency `queue|cancel-in-progress`; auto-cancel on for non-deploy; gate kinds
   manual/timer/external; Environments with protection rules; AES-256-GCM envelope secrets
   (`SCARAB_MASTER_KEY`); RS256 OIDC with **`sub = scarab:org/<org>/repo/<repo>/env/<env>/ref/
   <ref>`**; fork-PR = head≠base → no secrets + restricted subject; `kind: build` rootless
   BuildKit. Env-var names in ADR-0032 are the config contract. Live GitHub/BuildKit/cloud-OIDC
   paths are `#[ignore]`/env-gated (untestable in this env).

## Unattended-loop policy (while the maintainer is away)
- **Decide, document, continue.** On a genuinely blocking ambiguity: make the best architect-level
  decision consistent with the ADRs, **write the assumption in the commit body**, and if it is
  load-bearing, add a new ADR (0032+) and link it. Leave a regression test or `TODO(slice-N)`.
- **Park, don't halt.** If an issue truly can't proceed, leave it **open**, add a `git-bug bug
  comment <id>` explaining why, and move to the **next independent issue**. Do not block the whole
  slice on one item.
- **Only hard-stop** if `cargo check --workspace` is red and you cannot get it green — never leave
  the tree uncompilable between iterations.
- Respect purity (ADR-0016/0031), classical testing (ADR-0017), and the per-issue commit +
  `git-bug bug status close <id>` ritual from slice 1.

## Dependency-ordered backlog (git-bug ids)
Work top-to-bottom; finish a slice's ACCEPTANCE before the next slice. Skip closed.

**Slice 2 — real pipelines (IR + YAML + CEL, DAG, workspace CAS, restart):**
1. `6f38114` pipeline: YAML → IR compile + validate (DAG cycle/needs/matrix)
2. `bcb2e8f` pipeline: CEL binding (when/interpolation/matrix) — ADR-0031
3. `174041b` engine+db: persist compiled IR + dependency-aware admission
4. `237466b` storage-s3: per-file merkle CAS (Cas port)
5. `3f8f596` engine+executor: content-addressed workspace along DAG edges
6. `22035e7` api+engine: restart-a-step (smart invalidation)
7. `383e946` ACCEPTANCE: diamond DAG + workspace passing + restart

**Slice 3 — forge integration + identity:**
8. `b832a52` forge: canonical Event/Status/Repo + ForgePort
9. `d1e1e93` forge-github: webhook ingest + signature verify
10. `281b370` engine+pipeline: in-repo `.scarab` config → run on trigger
11. `03f8fd2` forge-github: post checks/status back
12. `3930b15` identity: OAuth login + RBAC
13. `a5bee19` ACCEPTANCE: webhook → in-repo run → checks back; login

**Slice 4 — scheduler richness + gates + environments:**
14. `e8fe6f7` engine+db: concurrency groups + cancellation
15. `edf20b8` engine: auto-cancel superseded runs
16. `082cc92` engine+db: fairness, backpressure, priority
17. `af7344b` engine: Gate step kind (durable suspend)
18. `c851ffe` projects+api: first-class Environments + protection rules
19. `320265e` ACCEPTANCE: serialize prod, auto-cancel, gate resume

**Slice 5 — secrets + OIDC issuer + BuildKit:**
20. `c1373bf` secrets: envelope-encrypted secrets in Postgres
21. `027f3f2` engine+executor: inject scoped secrets into steps (never logged)
22. `6436943` identity: Scarab as OIDC issuer (keyless federation)
23. `3c19b70` executor: fork-PR secret lockout + restricted subject
24. `71cf0f5` executor: rootless BuildKit image-build step
25. `c233a6c` ACCEPTANCE: secret used (not logged), OIDC verifies, build runs

**Slice 6 — UI scaffold + generated client (scaffold only):**
26. `a698215` api: export OpenAPI spec artifact
27. `3a0048b` ui: generated TS client + minimal SolidJS scaffold

## Environment (this machine)
- Real Postgres: `export SCARAB_TEST_DATABASE_URL="postgres://thulasiram@localhost:5432/postgres"`
  — PG-backed tests run; they skip cleanly without it.
- **No Docker daemon / no `kind` / no `just`** → keep cluster/BuildKit/UI-browser live-runs
  `#[ignore]`-gated + env-gated. Verify with real Postgres + fakes.
- ⚠️ The ambient kubeconfig points at **real ACME prod/staging EKS** — never launch against it.
  The dev harness uses an isolated `dev/.kubeconfig` (kind only).

## The loop prompt

Paste this into a new session to resume the unattended run (ids are the dependency order above;
verify against `git-bug bug` in case any are already closed):

```
/loop You are implementing "Scarab", a durable-execution k8s-native CI system, in this repo. Slice 1 is done and green. Now build slices 2–6.

FIRST, every iteration, re-ground: read CONTEXT.md, docs/handoffs/slices-2-6.md (state + LOCKED DECISIONS + env notes), and docs/adr/README.md. Open the specific ADRs an issue cites before implementing it. Run `git-bug bug` for the backlog; `git-bug bug show <id>` for detail.

WORK ONE ISSUE PER ITERATION in this dependency order (skip any already closed). Finish a slice's ACCEPTANCE before starting the next slice:
  Slice 2: 6f38114, bcb2e8f, 174041b, 237466b, 3f8f596, 22035e7, 383e946
  Slice 3: b832a52, d1e1e93, 281b370, 03f8fd2, 3930b15, a5bee19
  Slice 4: e8fe6f7, edf20b8, 082cc92, af7344b, c851ffe, 320265e
  Slice 5: c1373bf, 027f3f2, 6436943, 3c19b70, 71cf0f5, c233a6c
  Slice 6: a698215, 3a0048b

NON-NEGOTIABLE: hexagonal purity per ADR-0016/0031 (no I/O or infra crates in the pure domain crates; pure-computation deps like CEL are OK). Classical testing per ADR-0017 — real Postgres via SCARAB_TEST_DATABASE_URL=postgres://thulasiram@localhost:5432/postgres, mock only true externals, keep cluster/BuildKit/GitHub/UI-browser live-runs #[ignore]-gated + env-gated. Honor ALL locked decisions in docs/handoffs/slices-2-6.md and ADR-0031/0032 (cite them; don't re-decide). Never touch the ambient kubeconfig (it points at production); kind only via dev/.kubeconfig.

EACH ITERATION: read cited ADRs → implement → keep `cargo check --workspace` green → add the minimal tests the acceptance implies → commit `<type>(<area>): <subject>` with a body, ending with the trailer `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` → `git-bug bug status close <id>` → STOP (the loop re-invokes you for the next issue).

WHEN BLOCKED (decide/document/continue): make the best ADR-consistent decision, write the assumption in the commit body, add a new ADR (0033+) if it is load-bearing, and leave a regression test or TODO(slice-N). If an issue genuinely cannot proceed, leave it OPEN with a `git-bug bug comment <id>` explaining why, and move to the next INDEPENDENT issue. Only hard-stop if `cargo check --workspace` is red and you cannot make it green — never leave the tree uncompilable between iterations.

END THE LOOP when 3a0048b is closed and `cargo check --workspace` is green, or when the backlog is empty. Report a summary.
```
