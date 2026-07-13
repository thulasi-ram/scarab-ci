# Scarab

**A durable-execution, Kubernetes-native CI system** — a forge-integrated alternative to
GitHub Actions, written in Rust.

> *Your pipeline is a workflow that survives crashes, not a fire-and-forget batch job.*

Scarab treats a pipeline as a **resumable, inspectable, mutable-mid-flight workflow**. Because
the orchestrator is a crash-safe durable state machine on Postgres, **resume**,
**restart-a-step**, **time-travel**, and long-lived **approval gates** are native, not
bolted-on.

## Status

🌱 **Design + scaffolding.** The full design is captured — see below — and the workspace is a
compiling skeleton. Implementation proceeds in tracer-bullet vertical slices, starting with a
durable-core walking skeleton that survives a mid-run control-plane kill.

## Start here

- **[CONTEXT.md](CONTEXT.md)** — the thesis, the durability contract, non-goals, and the
  **ubiquitous language** every crate/API/UI must use.
- **[docs/adr/](docs/adr/)** — ~30 Architecture Decision Records (the *why* behind every
  load-bearing choice).
- **Issues** — tracked in [git-bug](https://github.com/git-bug/git-bug) (embedded in this
  repo): `git-bug bug` to list, `git-bug termui` for the TUI.

## What makes it different (the wedge)

Durable execution. Everything else — k8s-native execution, forge integration — is a *means*.
See [ADR-0001](docs/adr/0001-ci-as-durable-execution.md).

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

## Local dev stack

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

## License

[GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`). Running a
modified Scarab as a network service obliges you to offer users its source.
