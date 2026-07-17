# Scarab

**A modern CI engine for Kubernetes — forge-native, on a durable core.** Written in Rust.

CI tools fall into two camps, and neither covers the whole job. **Workflow engines** — Argo,
Tekton, Temporal — have a durable, crash-safe core, but they aren't CI: the forge
integration, in-repo config, PR checks, identity, secrets, DSL, and UI are yours to build.
**Forge-native CIs** — GitHub Actions, GitLab, Woodpecker, Drone — hand you all of that, but
the orchestrator underneath is a job runner: if the control plane restarts mid-run, the run
is orphaned or starts from scratch.

Scarab is built to be **both at once** — the batteries-included feel of a forge-native CI, on
a control plane that treats a run as durable **state**, not a fire-and-forget process. The
engine is a crash-safe state machine on Postgres (the DBOS/Temporal pattern): a control-plane
restart resumes the run from its last completed step, with exactly-once step execution. Where
self-hosted CIs like Woodpecker and Drone strand in-flight builds on a restart — stuck
`running`, containers left dangling ([woodpecker-ci#3427](https://github.com/woodpecker-ci/woodpecker/issues/3427),
[drone#2189](https://github.com/harness/drone/issues/2189)) — a Scarab run picks up where it
left off.

Durability is the *architecture*, not the headline: resume, restart-a-step, durable approval
gates, and an inspectable event log all fall out of it. It earns its keep as pipelines get
longer and more autonomous — agents, multi-day workflows, long review gates — where "the job
died, click re-run" stops being good enough: Scarab holds a suspended run at near-zero cost
and resumes exactly where it paused.

**Lineage.** Scarab owes its shape to [Woodpecker](https://woodpecker-ci.org/) — lean,
forge-native CI, no enterprise ceremony — and is inspired as much by its *limits*: the
many-backend surface it carries, and the pace a volunteer project can sustain. Kubernetes-only
sheds the backend baggage on purpose ([ADR-0005](docs/adr/0005-tenancy-and-k8s-only.md)); the
pace is what a small team building AI-first can now hold.

## Status (honest)

Read this as *a proven core with the live-I/O edges being wired*, not a shipped product.

- **Proven (against real Postgres).** The durable engine: crash-mid-run → resume, exactly-once
  step execution, restart-a-step, durable gates, content-addressed workspace, `invoke`
  inlining, scheduler (concurrency/fairness/supersede), secrets + fork-PR lockout, and a
  self-hosted OIDC issuer. ~230 tests pass locally.
- **Proven against a *fake* forge.** The forge-native flow end-to-end: signed webhook →
  in-repo `.scarab` config → run → checks posted back → OAuth-gated reads.
- **In progress.** The live GitHub adapter (outbound calls are currently `unimplemented!()`),
  live-Kubernetes Pod execution and re-attach (tested only via `#[ignore]`d live-cluster
  paths), the results-egress sidecar image, and a CI job that runs the Rust suite on push.

So: the hard core (the durable state machine) is real and tested; the claims scoped to *a
live forge* and *a live cluster* are not yet demonstrated. Implementation proceeds in
tracer-bullet vertical slices. See [docs/positioning.md](docs/positioning.md) for what we do
and don't claim, and why.

## Start here

- **[CONTEXT.md](CONTEXT.md)** — the thesis, the durability contract, non-goals, and the
  **ubiquitous language** every crate/API/UI must use.
- **[docs/adr/](docs/adr/)** — ~30 Architecture Decision Records (the *why* behind every
  load-bearing choice).
- **Issues** — tracked in [git-bug](https://github.com/git-bug/git-bug) (embedded in this
  repo): `git-bug bug` to list, `git-bug termui` for the TUI.

## What makes it different

**Cohesion.** One forge-native CI product — DSL, secrets, approvals, identity, UI — on a
durable engine, rather than a workflow engine (Argo/Tekton/Temporal) you assemble a CI on top
of, or a job-runner CI (Actions/Woodpecker) with no durable engine underneath. The durable
core is the *architectural* wedge ([ADR-0001](docs/adr/0001-ci-as-durable-execution.md)) — the
thing every other decision is judged against — but the *public* pitch is the cohesion. See
[docs/positioning.md](docs/positioning.md) for how we talk about it, and the honest boundaries.

## Design at a glance

| Aspect | Choice |
|---|---|
| Substrate | DBOS-pattern durable state machine on **Postgres** (Rust); object store for blobs |
| Execution | **Pod-per-step**, content-addressed workspace (per-file merkle CAS); k8s-only |
| Ontology | **Flat recursive DAG** (`Pipeline → Step`); `invoke` = reuse; matrix = modifier |
| DSL | Typed **IR** (the real DSL) + YAML frontend + **CEL** expressions |
| API | **REST/OpenAPI + SSE** (dogfooded by UI & CLI) + internal gRPC |
| Code | **Hexagonal**, compiler-pure domain crates + per-vendor adapter crates; one converged binary |
| Security | Forge-agnostic OIDC identity; **Scarab as OIDC issuer** for keyless federation |
| UI | SolidJS + generated OpenAPI client |

## Workspace layout

```
CONTEXT.md            ubiquitous language + system overview
docs/adr/             architecture decision records
crates/
  scarab-engine       durable core (pure): DAG state machine, scheduler, ports
  scarab-pipeline     IR, YAML→IR, CEL, validation (pure)
  scarab-forge        ForgePort + canonical Event/Status (pure)
  scarab-identity     Authenticator, OidcIssuer, RBAC (pure)
  scarab-secrets      SecretProvider (pure)
  scarab-storage      ObjectStore + Cas (pure)
  scarab-projects     Org/Repo/Project/Environment (pure)
  scarab-*-{postgres,github,s3,k8s,local}   adapters (infra lives here)
  scarab-testkit      fakes: FakeClock / InMemoryDb / FakeExecutor
  scarab-server       composition root: axum + OpenAPI + SSE; role runner
  scarab-cli          CLI (generated from OpenAPI)
```

## Building

```sh
cargo check --workspace
cargo run -p scarab-server -- --help
```

## Local dev (cluster-free)

For laptop iteration you need only a local Postgres. Config is env-driven, so
set it once via direnv and run each process with no flags:

```sh
cp .env.local.example .env.local   # edit DB url etc.
direnv allow                       # exports .env.local on cd (see .envrc)

just server   # cargo run -p scarab-server   (binds SCARAB_ADDR, host executor)
just ui       # Vite dev server on http://localhost:5173, proxies /v1 → server
```

`just server` is just `cargo run -p scarab-server` — every knob
(`SCARAB_ADDR`, `SCARAB_DATABASE_URL`, `SCARAB_EXECUTOR`, `SCARAB_MASTER_KEY`, …)
comes from the environment. Serving is the default; pass `--dry-run` to report
the resolved role and exit without binding. Auth is disabled in dev, so every
request is an Owner.

## Full dev stack (k8s)

One command brings up the two stateful dependencies (Postgres + MinIO), a
[kind](https://kind.sigs.k8s.io/) cluster for step Pods, and `scarab-server` in
converged mode, then submits a pipeline and watches it complete:

```sh
just up      # Postgres + MinIO (docker compose) + kind + scarab-server (background)
just demo    # POST a one-step pipeline, poll until it succeeds, print logs
just down    # tear it all down
```

Requires `docker`, `kind`, `kubectl`, `cargo`, and `python3`. The kind cluster's
kubeconfig is written to `dev/.kubeconfig` and used only via `KUBECONFIG`, so
Scarab never touches an ambient (e.g. production) context. Postgres is published
on **55432** to avoid clashing with a host Postgres on 5432. Config is entirely
env-driven (`SCARAB_DATABASE_URL`, `SCARAB_S3_*`, `KUBECONFIG`,
`SCARAB_NAMESPACE`) — see `Justfile` and `dev/`.

Without Docker/kind, the server still runs API-only against a local Postgres and
a filesystem object store (`--object-dir`); the background driver is skipped
until a cluster is reachable.

## API contract workflow (ADR-0054)

`openapi.json` (committed at the repo root) and the generated TS client
(`ui/scarab-web-ui/src/api/schema.ts`) are **gated against drift in CI** —
the build fails if either is stale. After changing any route or DTO:

```sh
cargo run -p scarab-server -- --emit-openapi openapi.json
cd ui/scarab-web-ui && npm run gen && npm run typecheck
```

Full-route coverage is enforced by the test
`every_registered_route_is_in_the_openapi_spec` — a new `.route(...)` without
a `#[utoipa::path]` annotation fails the suite.

## License

[GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`). Running a
modified Scarab as a network service obliges you to offer users its source.
