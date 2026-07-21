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
| [0008](0008-step-contract.md) | Step contract: OCI image + convention + built-in kinds | Accepted (amended by 0058) |
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
| [0025](0025-cross-pipeline-orchestration.md) | Cross-pipeline orchestration: nest vs trigger | Accepted (amended by 0038) |
| [0026](0026-resource-and-placement.md) | Resource & placement model | Accepted (refined by 0055) |
| [0027](0027-restart-semantics.md) | Restart semantics: content-addressed smart invalidation | Accepted |
| [0028](0028-ui-stack.md) | UI stack: SolidJS + TS + generated client | Accepted |
| [0029](0029-workspace-cas.md) | Workspace content-addressing: per-file merkle CAS | Accepted |
| [0030](0030-operational-defaults.md) | Operational defaults (isolation, retention, notifications, install, DR, limits) | Accepted |
| [0031](0031-pure-computation-deps.md) | Purity = no I/O, not no deps: pure-computation crates allowed (amends 0016) | Accepted |
| [0032](0032-slices-3-5-implementation-decisions.md) | Slice 3–5 implementation decisions (forge/identity, scheduler/gates, secrets/OIDC/BuildKit) | Accepted |
| [0033](0033-transitive-skip.md) | Transitive skip: `when:false` step kept-but-skipped, descendants cascade | Accepted |
| [0034](0034-external-gate-token.md) | External gate release via HMAC token | Accepted |
| [0035](0035-explicit-outputs.md) | Explicit workspace `outputs:` | Accepted |
| [0036](0036-local-execution-dev-backend.md) | Local execution dev backend | Accepted |
| [0037](0037-environment-governance.md) | Environment governance: approval-as-gate, scoped secrets, admission at point-of-use | Accepted |
| [0038](0038-invoke-and-local-reuse.md) | `invoke` = compile-time inlining; local `.scarab/lib`, third-party via OCI images (amends 0025) | Accepted |
| [0039](0039-privileged-images.md) | Privileged step images: hardened baseline + governed capability grants per Environment | Accepted |
| [0041](0041-named-results-and-interpolation.md) | Named step results + launch-time interpolation (`${{ outputs.<id>.<name> }}`); makes 0038 outputs live | Accepted |
| [0042](0042-trusted-egress-sidecar.md) | Trusted per-Pod egress sidecar → fence-scoped results API → Postgres (how k8s captures results) | Accepted |
| [0043](0043-launch-parameters-and-manual-dispatch.md) | Launch parameters (typed `interface.inputs`) + manual dispatch as a repo-aware trigger | Accepted |
| [0044](0044-protection-ref-vs-read-ref.md) | Branch protection matches a symbolic ref, not the commit SHA (fixes `allowed_refs`) | Accepted |
| [0045](0045-source-provisioning.md) | Source provisioning: `clone` step kind — SHA-pinned `git` clone in a `scarab-clone` Pod, `.git` into CAS, read-only fork token | Accepted |
| [0046](0046-forge-auth-and-multi-adapter.md) | Forge auth is adapter-internal; GitHub + Forgejo adapters in v1; `ForgeConnection` registry; `Project` = governed repo (amends 0010) | Accepted |
| [0047](0047-retry-classification-and-attempt-model.md) | Retry classification (`FailureClass`), never-started vs post-start retry, new-fence-per-retry, dead-letter/timeout model (implements/amends 0020) | Accepted |
| [0048](0048-fail-closed-startup.md) | Fail-closed startup: validated config, boot refusal on unsafe/auth-off, mandatory Postgres (no API-only mode), opt-in `SCARAB_DEV_INSECURE` | Accepted |
| [0049](0049-identity-and-access.md) | Identity & access: forge-agnostic OAuth/OIDC authn + PG session; Scarab-native RBAC (Org/Project scope, forge-import bootstrap); tenancy scoping | Accepted |
| [0050](0050-retention-and-gc.md) | Retention & GC: mark-sweep workspace-CAS GC, eligibility keyed on run lifecycle (suspended runs never collected), per-class TTLs (implements 0030) | Accepted |
| [0051](0051-multi-replica-operation.md) | Multi-replica: per-step tail lease, replica-agnostic live-SSE (durable index), shared persistent OIDC key | Accepted |
| [0052](0052-artifacts.md) | Artifacts: dedicated per-run store (not CAS), convention-emitted (`/scarab/artifacts/` + globs), presigned download, own TTL | Accepted |
| [0053](0053-observability-and-lifecycle.md) | Observability & lifecycle: Prometheus `/metrics`, JSON logs + request-ids, `/readyz` vs `/healthz`, graceful shutdown (implements 0030) | Accepted |
| [0054](0054-product-surface-serving.md) | Product surface: embed UI in the binary, run-cancel API, OpenAPI drift gate, CLI truthfulness (stubs exit non-zero) | Accepted |
| [0055](0055-placement-profiles.md) | Placement profiles: named `placement_profiles` + control-plane baseline + governed `k8s_overlay` (refines 0026) | Accepted |
| [0056](0056-run-takes-and-attempt-grain-evidence.md) | Run Takes (derived, human-boundary version lens) + attempt-grain evidence (results/workspace/artifacts/consumption keyed by attempt; restart & re-adoption events) | Accepted |
| [0058](0058-runtime-service-containers.md) | Runtime service containers: co-located **Sidecar** (default, fenced-by-inheritance) + Run-scoped **Shared** (opt-in `uses:`, per-Take instance, unfenced) — not DAG nodes (amends 0008) | Accepted |
| [0059](0059-tick-fault-isolation.md) | Per-run scheduler-tick fault isolation (one poison Run can't stall the fleet) + bounded per-Run failures → dead-letter (generalizes 0058 Fix A/B) | Proposed |

> For *features* deliberately deferred until demand appears (as opposed to ADRs
> to be written), see [`docs/followups.md`](../followups.md).

## Open / deferred (to be written when their slice begins)

Workspace CAS internals (chunking) · multi-cluster remote-agent protocol · install Operator ·
OIDC login/session details · API rate-limit specifics · SLSA/cosign/SBOM export · notification providers.
