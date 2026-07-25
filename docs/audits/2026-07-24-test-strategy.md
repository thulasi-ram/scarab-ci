# Test strategy & coverage plan — 2026-07-24

Audit of the existing suite + a phased plan. Companion to ADR-0017 (classical,
lean, grow-from-bugs) — this does not change that decision, it operationalises
it. Status: **draft, to be grilled**.

## Verdict on "most tests are useless"

**The tests are good. The test *system* is useless.** Pushback with evidence:

- Canonical suite is 18.7k LOC / 75 files (the "110k LOC" figure counts seven
  duplicate worktrees under `.workspaces/`). Sampled assertion quality: ~75%
  genuinely behavioral (drive the real axum router + real engine, assert
  durable observable state), ~15% contract-shape, ~5% tautological, ~5% smoke.
- 4 of 5 known escaped bugs (log dup, hot-loop, cross-env concurrency cancel,
  budget-billing-queued-time) got real regression tests after the fact. The
  grow-from-bugs loop is working. Exception: `run_as_root` dropped in
  create_run — still no server-layer guard (only YAML-compile coverage).
- `crash_resume.rs` + `durability_guard.rs` (real Postgres, kill engine
  mid-DAG, exactly-once assertions) are exactly the wedge coverage ADR-0017
  demands, and they are strong.

What makes it all *effectively* useless today:

1. **CI runs zero tests.** `.github/workflows/ci.yml` is only an
   openapi-drift gate; `image.yml` builds images; nothing runs `cargo test`.
   Every test in the repo is advisory.
2. **The crown jewels silently skip.** PG-backed tests `return` early (not
   `#[ignore]`) when `SCARAB_TEST_DATABASE_URL` is unset — a bare
   `cargo test --workspace` prints green having exercised none of the
   durability suite.
3. **The real k8s wiring is never exercised in automation.**
   `scarab-executor-k8s/tests/cluster.rs` is `#[ignore]` + env-gated; every
   other test uses `FakeExecutor`. Pod-spec *construction* has 45 solid unit
   tests in `lib.rs`; launch/poll/log_stream/cancel against a real apiserver
   has none that run.
4. **UI has literally zero tests.** No vitest/playwright/test script anywhere
   under `ui/`. CI does `tsc --noEmit` + generated-client drift only.
5. **Fake/real drift risk**: `InMemoryDb` hand-reimplements PG semantics
   (optimistic concurrency, non-downgrade, poison bounds). Only the
   env-gated PG tests confirm fake and real agree — and those don't run.

## Retro: the last 40 commits (why it *feels* useless)

14 of the last 40 non-merge commits are `fix:`. Classified:

- **5 web-ui fixes** (DAG zoom/attempt dropdown/activity order, take-scoped
  duration, take-scoped live attempts, per-try outcome colour, top-level
  await boot failure) — none shipped with a test; there is no UI test infra.
  4 of 5 live in the take/attempt derivation seam Phase 3 targets; the boot
  failure would be caught by a single Playwright mock-mode smoke.
- **6 engine/server fixes** (retry-budget per Take, gate-approval guard +
  cancel attribution, concurrency interpolation, supersede/teardown outcome
  cluster) — shipped *with* regression tests, but all were found by
  dogfooding first and the tests gate nothing (CI runs none).
- **3 "unimplemented feature" bugs**: matrix legs never ran (`343555f`), git
  refused the provisioned workspace (`c785d40`), shared-service k8s names
  collided across takes (`4e6915b`). **Regression tests cannot catch these
  by definition** — the feature never worked, so there was nothing to
  regress. Two of the three are only observable on a real cluster.

Conclusion: grow-from-bugs is working *after the fact*, but nothing catches
bugs *before* dogfooding does. Two structural answers:

1. **Feature-acceptance rule** (now an addendum to ADR-0017): every
   engine/server/executor feature lands with at least one functional test
   that *executes* the feature through the real router+engine. A kind-tier
   case is required at land time only when the failure mode is
   k8s-observable-only (invisible to `FakeExecutor`); otherwise it may
   follow within the same milestone. UI features are exempt (grow-from-bugs
   + the no-DOM tier). "Compiles + YAML parses" is not acceptance. The
   matrix bug is the canonical example: a single test running a 2-leg
   matrix and asserting two launches with resolved commands would have
   caught it at merge.
2. **CI must run the suite** (Phase 0) — otherwise even the regression
   tests that do exist protect nothing.

## Strategy

Priorities: backend first; k8s-only focus (proc-mode harness is still the E2E
vehicle since it runs steps on kind — what gets *minimal* testing is
`scarab-executor-local` and helm plumbing); UI limited to run-detail.

### Phase 0 — make the existing 18.7k LOC count (highest ROI, ~1 day)

No new tests until this lands; anything else is building on sand.

- **CI test job** in `ci.yml`: `cargo nextest run --workspace` with a
  `postgres:16` service container, `SCARAB_TEST_DATABASE_URL` set. The
  durability suite finally gates merges.
- **Kill silent skips**: `fresh_db()` keeps the `SCARAB_TEST_DATABASE_URL`
  path (CI service container, compose); the no-URL path becomes a *loud*
  skip locally (eprintln, visible in nextest output) and a panic in CI via
  `SCARAB_TEST_REQUIRE_PG=1`. `just test` ensures the compose Postgres is up
  and exports the URL; for IDE test runners (rust-analyzer invokes bare
  `cargo test`), set the URL in `.env.local`/direnv against the same
  compose instance.
- **Adopt cargo-nextest**: parallelism, per-test process isolation, JUnit
  output, flake-retry reporting. `just test` becomes
  `cargo nextest run --workspace` and auto-starts the compose Postgres from
  `deploy/local-proc/` to provide the URL.
- **Coverage**: `cargo llvm-cov nextest` in CI (PG service present, so the
  durability suite counts); per-crate line report in the job summary + lcov
  artifact. Exclude `.workspaces/`, generated code, `scarab-testkit`. The
  kind-tier and e2e jobs stay outside coverage measurement. Metric
  discipline: **ratchet, not target** — hard gate from day one, no
  arbitrary "80%" goals. Baseline is a committed file
  (`docs/audits/coverage-baseline.toml`) that only a human bumps; a
  `just coverage` recipe regenerates report + baseline, so every baseline
  move is a reviewable diff — raising it rides the PR that earned it,
  lowering it is a visible deliberate act.
- Delete the empty placeholder `rootless_buildkit_builds_an_image`
  (cluster.rs:258) — a test that asserts nothing.

### Phase 1 — backend functional gaps (per-PR tier)

Classical: real router + engine + real Postgres; fakes only at ports
(executor, forge HTTP, object store) per ADR-0017.

New test cases, in priority order:

1. **API→StepSpec field-preservation** (the escaped-bug class):
   `POST /v1/runs` with `run_as_root`, capabilities, placement, resources,
   env, services → assert the `StepSpec` handed to the executor carries them.
   One table-driven test; guards the whole field-drop class.
2. **Open QA-sweep bugs pinned as failing regressions** (2026-07-23 sweep):
   approve-skipped-gate returns 202 without effect; unattributed cancel
   event. Write the test first, fix behind it.
3. **Command matrix through the real router** — every user-facing command
   gets at least one behavioral test: cancel (queued / mid-run / terminal
   idempotent), rerun, retry, restart, gate approve/reject, dispatch with
   typed params, artifact fetch, workspace browse, debug-pod issuance.
   Several already exist; the matrix makes the holes visible (see
   "Visibility" below).
4. **Fake/real parity — two layers.** (a) **`db_contract.rs` in
   scarab-db-postgres/tests** (placed there, not in testkit, so testkit
   never dev-depends on the PG crate; precedent: `forge_contract.rs`):
   trait-level contract
   tests for the load-bearing `Db` port semantics — transition OCC,
   `record_attempt` idempotency + non-downgrade, lease grant/expiry/steal,
   outbox claim/poison bounds, attempt-ordering determinism — run against
   `InMemoryDb` unconditionally and real PG whenever the URL is present
   (always in CI). Any fix that touches `testkit`'s fake to mirror an
   engine change must add its semantic here. The contract suite is the
   arbiter when fake and real disagree. (b) Engine scenarios stay written
   against `&dyn Db`; the `*_inmemory.rs` files get PG-backed twins per-PR
   (service container is hot), and new engine tests run on both by
   default.

Deliberately **not** doing: new unit tests for things the compiler or the
above functional tier already pins. `scarab-executor-local` gets nothing
beyond what exists.

### Phase 2 — k8s module-E2E tier (the real gap)

The product is k8s-only; this is where confidence is missing.

- **Per-PR: kind-in-CI job** running `scarab-executor-k8s/tests/cluster.rs`
  un-ignored (`helm/kind-action`). The test binary is the control plane, so
  no server image is needed; clone/sidecar images pull `edge` and are built
  in-job only when `docker/**` changed. **Path-filtered** to
  `crates/{scarab-executor-k8s,scarab-engine,scarab-server,scarab-storage*}`
  + `docker/**` — UI/docs-only PRs never pay it. **Advisory for its first
  ~2 weeks, promoted to a required check once demonstrably non-flaky** (the
  nextest+PG job, by contrast, is required from day one — no cluster, no
  flake excuse). Cases,
  extending what's there: launch→succeed, exit-code mapping, cancel kills
  Pod, image-pull failure fails fast (terminal waiting reason), log stream →
  LogService with dedup, sidecar result capture through the fence, clone step
  with token, artifact globs upload, **run_as_root actually applied on the
  Pod**, **orphan-Pod teardown** (open git-bug fd6e6d4 — write as failing
  regression).
- **Nightly / on-demand: full-stack E2E** reusing the `local-proc` harness
  (compose PG+MinIO, kind, server as host process — this *is* k8s execution;
  "proc mode" here is only where the server lives). New `crates/scarab-e2e`
  (env-gated `SCARAB_E2E=1`, excluded from default nextest run) driving the
  stack over HTTP with typed DTOs, replacing ad-hoc curl in `demo.sh`.
  Lifecycle: a **`just e2e` recipe** owns the stack (`up.sh` → nextest →
  `down.sh`) — the crate assumes a running stack and stays a pure HTTP
  driver, keeping Justfile recipes canonical. Exception: the crash/resume
  test spawns its **own** server instance (binary path via env, throwaway
  DB) so its `SIGKILL`s can't poison the shared stack. **Zero auto-retries**
  — a red nightly is triaged as a real timing bug (grow-from-bugs applies
  to the harness too). The six scenarios below are a **cap, not a floor**
  (CONTEXT.md §8: "a few genuine cross-layer e2e"); new coverage pressure
  goes to the functional or kind tiers first:
  1. happy path: create run → Pod → logs → succeeded — driven through the
     `scarab run` CLI binary (covers the one wired CLI command);
  2. **crash/resume at E2E grain**: kill `scarab-server` mid-DAG, restart,
     exactly-once completion — the wedge, proven end-to-end, not just at the
     scheduler layer;
  3. cancel mid-run tears down the Pod;
  4. rerun → takes/attempt evidence correct via API;
  5. webhook (fake forge) → run → status posted back;
  6. workspace/CAS restart: rerun after server restart fails fast instead of
     hanging at Init (known helm-dogfood escape).
- **Helm mode: minimal** — `helm lint` + `helm template` golden check per-PR;
  one nightly `helm install` onto the same kind cluster + a single smoke run.
  No colima in CI ever (the deploy.sh context guard stays a local-only tool).

### Phase 3 — frontend, run-detail only

Two layers, both new (there is nothing today):

- **Vitest on the pure derivation seam.** The run-detail screen is mostly
  pure data derivation and it's exactly where display bugs live:
  `takes.ts` (`deriveTakes`, `replayTake`, `attemptCauses`,
  `ofRecordAttemptId`), DAG `layers()`/`depth()` (extract from `Dag.tsx`),
  `StepPane` attempt-window scoping (`attemptsOf`/`scoped`/`ofRecordTry`),
  `events.ts` categorisation, `AttemptsDropdown` cause/outcome/tone mappers,
  version tally/bucketing. Cases: empty event log, no rerun, multiple
  reruns, rerun while running, superseded/not_run boundaries, cascade +
  readopted, DAG diamonds + cycle guard. Prerequisite: extract the inline
  closures following the existing pure-module precedent (`takes.ts`,
  `events.ts`, `fmt.ts`) — `layers()`/`depth()` → `src/dag-layout.ts`;
  RunDetail take/attempt-window closures and StepPane scoping →
  `takes.ts`/`attempts.ts`, taking explicit args instead of closing over
  signals; components keep thin wrappers (`tsc` guards the refactor).
  Vitest joins the typecheck job in ci.yml — **required from day one**,
  path-filtered to `ui/**`.
- **Strict pyramid — no-DOM ≫ DOM ≫ Playwright.** The bulk of UI testing is
  the no-DOM tier above; total vitest+playwright footprint stays small.
  - **DOM tier (few):** only where rendering *is* the logic and extraction
    can't reach it — e.g. DAG edge/chip placement per fixture. A handful of
    vitest + `@solidjs/testing-library` cases, added reluctantly and only
    after a real rendering bug or a derivation that can't be extracted.
  - **Playwright tier (minimal, ~2 specs):** one boot smoke in mock mode
    (app renders at all — catches the top-level-await class of escape) and
    one run-detail walkthrough on a single *rich* fixture (multi-take with
    superseded/shadowed + services/sidecars + gated step) asserting the
    load-bearing surfaces: DAG renders nodes/chips, version-dropdown
    language, activity rail order, browse tabs reachable. Extend the acme
    fixture only as far as this one scenario needs — not a fixture per
    state. In UI tests the server *is* the external, so mocking it is
    classical, not mockist. In CI: path-filtered to `ui/**`, advisory for
    two weeks (new toolchain), then required. **Text/role-based assertions
    only — no pixel/screenshot snapshots ever** (highest-flake, lowest-
    signal genre; fails the cynical bar).
- One nightly Playwright smoke against the real E2E stack (happy path +
  rerun) to catch fixture drift. No separate harness: `scarab-server`
  serves the SPA as its fallback route, so the smoke rides the `just e2e`
  nightly job after the API scenarios pass.

## Frameworks (decisions)

| Layer | Pick | Why |
|---|---|---|
| Rust runner | **cargo-nextest** | process-per-test isolation, JUnit, flake retries, llvm-cov integration |
| Rust coverage | **cargo-llvm-cov** | works with nextest, region coverage, no tarpaulin flakiness |
| PG provisioning | container-first: compose `postgres:16` locally (`just test` auto-starts, exports URL), identical service container in CI; **rejected: PGlite, `pg-embed`, `postgresql_embedded`, testcontainers** | Docker is already a hard dev dependency (k8s-only product; colima/kind required for everything past `cargo check`), so "no Docker" — the embedded crates' only benefit — buys nothing. nextest's process-per-test model makes an embedded instance boot per *test* (initdb 2-3s × ~200 tests) unless wrapped — and the wrapper is just the just-recipe again. `postgres:16` container = exact prod/helm image parity, which matters most for the durability suite. PGlite additionally single-connection (can't test racing schedulers/leases). IDE runners: `SCARAB_TEST_DATABASE_URL` in `.env.local`/direnv. Revisit embedded only if Docker-less external contributors materialize |
| `InMemoryDb` fate | keep + mechanical parity (Phase 1.4); **rejected: PGlite** | PGlite is single-connection (single-user mode; multi-conn still roadmap as of 2026-06), so it cannot express the multi-connection semantics the durability suite exists to test (racing schedulers, leases, optimistic concurrency); Rust bindings are 0.1.x socket-proxy wrappers. If parity keeps catching the fake lying, the exit is real-PG-everywhere, not PGlite |
| k8s E2E | **kind in GHA** + existing `local-proc` scripts | already the local harness; CI parity with `just up` |
| E2E driver | new `crates/scarab-e2e` (reqwest + typed DTOs) | replaces curl/python demo.sh assertions; compiler checks the contract |
| UI unit | **Vitest**, no-DOM by default | Vite-native, Solid-compatible; the bulk of UI coverage lives here on extracted pure derivations |
| UI E2E | **Playwright** over mock mode, capped at ~2 specs + 1 nightly | pyramid: no-DOM ≫ DOM ≫ Playwright; deterministic, no backend |

## Visibility metrics

1. **Executed vs skipped** — nextest JUnit counts in the CI summary; CI fails
   if any PG-gated test skipped (`SCARAB_TEST_REQUIRE_PG`). Silent skip was
   the root deception; this metric exists to make it impossible.
2. **Coverage ratchet** — per-crate line coverage from llvm-cov; PR fails if
   a crate drops >0.5pt below `docs/audits/coverage-baseline.toml`
   (human-custody, see Phase 0). UI vitest coverage is report-only, never a
   gate — a UI gate would fight the minimal pyramid.
3. **Command × scenario matrix** — a checked-in table using only canonical
   glossary verbs (create-run, dispatch, cancel, rerun, retry, approve,
   logs, artifacts, workspace-browse, attach/debug, services, events)
   mapping each to its covering test function. "Restart" appears in exactly
   one test: the deprecated alias route behaves identically to `/rerun`
   (pins the compat contract until the alias is deleted). CI validates each
   row with `cargo nextest list -E 'test(<name>)'` — existence + not
   filtered out; passing is implied since the whole suite runs.
   Meaningfulness stays the orchestrator's job. CLI verbs enter the matrix
   only when un-stubbed (feature-acceptance rule applies at wiring time);
   today that is `run` alone, covered by driving the nightly e2e happy-path
   scenario through the CLI binary instead of raw HTTP.
4. **Escape ratio** — every git-bug fix must reference a regression test
   (grow-from-bugs, already the philosophy; the orchestrator enforces it).
5. **Flake rate** — nextest retry stats surfaced in the job summary.

## Test-case orchestrator & enforcement

`.claude/agents/test-orchestrator.md` — a purpose-built, cynical-by-default
agent that plans/audits test work: refuses to count a test that doesn't run
in CI, demands red-first evidence for regressions, rejects tautological
assertions, enforces the classical rules above. Use it to drive Phases 1–3.

Enforcement in practice (an agent is a review tool, not a gate):

- **Red-first is a recorded artifact**: fix commits/PRs carry a line —
  `Red-first: reverted <sha>, test fails with <one-line output>` — checked
  by the orchestrator's review. Greppable, no memory required.
- **Orchestrator cadence**: (a) reviewer on every PR that adds/modifies
  tests; (b) periodic (~monthly) cynical audit of the suite against this
  doc, producing keep/cut/holes.
- **Mechanical backstop — mutation testing**: scheduled (~monthly,
  manual-dispatch) `cargo-mutants` over `scarab-engine`, `scarab-db-postgres`
  and `scarab-executor-k8s`, **report-only, never a PR gate** (hours-long,
  and equivalent mutants produce false alarms). Surviving mutants are
  automated red-first evidence and become the orchestrator's work queue —
  including whatever among today's ~5% tautological tests actually matters.

## Phasing summary

| Phase | What | Gate it creates |
|---|---|---|
| 0 | CI runs nextest+PG, loud skips, coverage ratchet | existing suite finally bites |
| 1 | field-preservation, QA-bug regressions, command matrix, fake/real parity | API/engine behavior |
| 2 | kind-in-CI executor tests + `scarab-e2e` nightly (crash/resume E2E) | real k8s wiring |
| 3 | Vitest on takes/DAG derivations + Playwright fixtures | run-detail display |
