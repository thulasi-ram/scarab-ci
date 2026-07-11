# Architecture Decision Records

These ADRs record the **load-bearing decisions** for Scarab, captured in a design grilling
session on 2026-07-12. Each is written in a light [MADR](https://adr.github.io/madr/) style:
**Context → Decision → Consequences → Alternatives considered**.

They are the *why*. The *what* (ubiquitous language, system overview) lives in
[`../../CONTEXT.md`](../../CONTEXT.md).

## Status lifecycle

`Proposed` → `Accepted` → (`Superseded by NNNN` | `Deprecated`). All ADRs below are
`Accepted` unless noted. A decision changes by writing a **new** ADR that supersedes the
old one — never by editing history (mirrors our version-tolerance principle).

## Index

| ADR | Title | Status |
|---|---|---|
| [0001](0001-ci-as-durable-execution.md) | CI as durable execution (the wedge) + non-goals | Accepted |
| [0002](0002-durability-model.md) | Durability model: durable DAG orchestrator + at-least-once steps | Accepted |
| [0003](0003-durability-substrate-postgres.md) | Durability substrate: own it — DBOS pattern on Postgres (Rust) | Accepted |
| [0004](0004-execution-topology.md) | Execution topology: pod-per-step + content-addressed workspace | Accepted |
| [0005](0005-tenancy-and-k8s-only.md) | Tenancy & deployment; Kubernetes as the only backend | Accepted |
| [0006](0006-pipeline-ontology.md) | Pipeline ontology: flat recursive DAG | Accepted |
| [0007](0007-data-passing-model.md) | Data-passing model: Workspace / Result / Artifact / Cache | Accepted |
| [0008](0008-step-contract.md) | Step contract: OCI image + convention + built-in kinds | Accepted |
| [0009](0009-dsl-ir-yaml-cel.md) | DSL: typed Pipeline IR + YAML frontend + CEL | Accepted |
| [0010](0010-forge-integration.md) | Forge integration: agnostic core, OIDC identity, in-repo config | Accepted |
| [0011](0011-durable-scheduler.md) | Scheduling: durable admission control | Accepted |
| [0012](0012-api-surface.md) | API surface: REST/OpenAPI + SSE + internal gRPC | Accepted |
| [0013](0013-history-and-observability.md) | History & observability: state tables + append-only event log | Accepted |
| [0014](0014-secrets.md) | Secrets: envelope-encrypted PG + pluggable providers | Accepted |
| [0015](0015-supply-chain-oidc.md) | Supply-chain: Scarab as OIDC issuer + provenance-by-construction | Accepted |
| [0016](0016-code-architecture.md) | Code architecture: hexagonal + adapter crates + converged binary | Accepted |
| [0017](0017-testing-strategy.md) | Correctness & testing strategy | Accepted |
| [0018](0018-image-building.md) | Container image building: rootless BuildKit | Accepted |
| [0019](0019-local-execution.md) | Local execution: executor-local behind the Executor port | Accepted |
| [0020](0020-retry-and-failure.md) | Retry & failure taxonomy | Accepted |
| [0021](0021-double-effect-fencing.md) | Double-effect hazard: fencing tokens + idempotency contract | Accepted |
| [0022](0022-upgrades-and-versioning.md) | Upgrades & schema evolution: version-tolerant from day one | Accepted |
| [0023](0023-dag-shape.md) | DAG shape: static matrix v1, dynamic reserved | Accepted |
| [0024](0024-environments.md) | Environments: first-class with protection rules | Accepted |
| [0025](0025-cross-pipeline-orchestration.md) | Cross-pipeline orchestration: nest vs trigger | Accepted |
| [0026](0026-resource-and-placement.md) | Resource & placement model | Accepted |
| [0027](0027-restart-semantics.md) | Restart semantics: content-addressed smart invalidation | Accepted |
| [0028](0028-ui-stack.md) | UI stack: SolidJS + TS + generated client | Accepted |
| [0029](0029-workspace-cas.md) | Workspace content-addressing: per-file merkle CAS | Accepted |
| [0030](0030-operational-defaults.md) | Operational defaults (isolation, retention, notifications, install, DR, limits) | Accepted |

## Open / deferred (to be written when their slice begins)

Workspace CAS internals (chunking) · multi-cluster remote-agent protocol · install Operator ·
OIDC login/session details · API rate-limit specifics · SLSA/cosign/SBOM export · notification providers.
