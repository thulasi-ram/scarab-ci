# Architecture Decision Records

These ADRs record the main decisions behind Scarab. The first batch (0001 to 0032) came out of
a design review session on 2026-07-12; the rest were added one at a time as later work needed a
decision. Each is written in a light [MADR](https://adr.github.io/madr/) style:
**Context → Decision → Consequences → Alternatives considered**.

They explain *why*. The *what* (the domain vocabulary and a system overview) is in
[`../../CONTEXT.md`](../../CONTEXT.md).

## Status lifecycle

`Proposed` → `Accepted` → (`Superseded by NNNN` | `Deprecated`). All ADRs below are
`Accepted` unless noted. A decision changes when a **new** ADR supersedes or amends the old
one. We don't rewrite the old ADR's text. When a later ADR changes part of an earlier one, the
earlier ADR gets a dated note at the changed section (for example "Amended 2026-08-24 by 0067"),
so a reader sees the change where it applies and the original reasoning stays readable.

## Index

| ADR | Title | Status |
|---|---|---|
| [0001](0001-ci-as-durable-execution.md) | CI as durable execution + non-goals | Accepted |
| [0002](0002-durability-model.md) | Durability model: durable DAG orchestrator + at-least-once steps | Accepted |
| [0003](0003-durability-substrate-postgres.md) | Durability layer: build our own, DBOS-style on Postgres (Rust) | Accepted |
| [0004](0004-execution-topology.md) | Execution topology: pod-per-step + content-addressed workspace | Accepted (data path superseded by 0061) |
| [0005](0005-tenancy-and-k8s-only.md) | Tenancy & deployment; Kubernetes as the only backend | Accepted |
| [0006](0006-pipeline-ontology.md) | Pipeline model: flat recursive DAG | Accepted |
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
| [0024](0024-environments.md) | Environments: a built-in entity with protection rules | Accepted |
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
| [0059](0059-tick-fault-isolation.md) | Per-run scheduler-tick fault isolation (one poison Run can't stall the fleet) + bounded per-Run failures → dead-letter (generalizes 0058 Fix A/B) | Accepted |
| [0060](0060-org-settings-connections-and-secret-management.md) | Global Settings: connection lifecycle (Forgejo onboarding + GitHub observe/re-sync), org/env secret editing via the effective-status matrix, IaC-or-UI connections with single-owner precedence | Accepted |
| [0061](0061-workspace-data-path.md) | Workspace data path: per-AZ workspace service + Scarab node driver with lazy materialisation; control plane leaves the data path (supersedes 0004's data path, not its topology) | Accepted (**part 2, the node driver, superseded by 0062**; **part 4 amended by 0064**) |
| [0062](0062-workspace-export-lazy-without-node-driver.md) | Workspace Export: lazy materialisation with no DaemonSet — `overlayfs` over a **reflink-or-copy** Snapshot Farm on the Data Depot (hardlinks withdrawn on measurement), mounted by kubelet as a per-Step PVC; the change set is the upper layer; optional node-side write accelerator | Accepted |
| [0063](0063-step-logs-on-the-data-depot.md) | Step logs live on the **Data Depot**, not the object store and not the CAS: object storage stops being a silent prerequisite, external log systems are additional sinks only, eviction is loud and pinned until a sink acks; renames the workspace service → **Data Depot** | Accepted |
| [0064](0064-durability-tiering-and-the-write-path.md) | Durability tiering + write path: **warm-first with one batched flush** that gates `Succeeded`, a cold tier proven by `st_dev` (a second PVC counts, a `LocalDir` beside the CAS does not), graceful warm-only deployments, durability stamped per Attempt (**amends 0061 part 4**) | Accepted |
| [0065](0065-retention-cache-and-rederivation.md) | Retention, **Cache** (explicit keyed directory cache — implements 0007's concept), `RetentionProfile` (operator-defined, author-selectable), re-derivation is human-triggered via the widened rerun with full-cascade disclosure; no automatic Cache detection; checkout is not a special case | Accepted |
| [0068](0068-the-infra-observer.md) | The **infra observer**: a control-plane-side reader of the backend's account of an in-flight unit (`Executor::infra_condition`), appending to the run's **activity log** on *change* — the missing channel for "why is there no log", since a Pod that never started has no stdout to tail; the terminal cause also stamps `attempts.failure_detail` | Accepted |
| [0069](0069-identity-links.md) | **Identity links**: a `Principal` is a Scarab record holding one identity per provider, pinned by the provider's **immutable** id (never `login`); ids are readable (`github:alice`), minted once, never re-minted; several providers (legacy env vars = the one `unqualified_ids` provider); forge import resolves collaborators by external id and pre-creates **unclaimed** Principals; admin-typed handles must name an existing Principal; linking is explicit (never by email), merge only on proven control | Accepted |
| [0070](0070-deployments-as-a-vendor-neutral-record.md) | **Deployments as a vendor-neutral record**: a Deployment is an occurrence keyed by (DeploymentTarget, DeploymentVersion); six statuses (source asserts `in_progress`/`live`/`failed`, core derives `requested`/`stalled`/`superseded`); `deployment_requests` with an effective time; a core-resolved, forward-looking **DeploymentGate**; every gate can fail, holds only its dependents and has its own clock; INV-DEP-1..11 in `scarab-deployment` (approval becomes evidence, not the deployment) | Accepted |
| [0071](0071-org-level-environments.md) | **Org-level Environments**: an Environment is an org-wide place; EnvironmentProjectEligibility (fail-closed), tighten-only EnvironmentProjectOverlay, DeploymentTargetProjectOwnership mirrored from the source, EnvironmentDeploymentSources, two Environment secret scopes, Environment RBAC scope, per-Project privilege grants, concurrency per DeploymentTarget; behaviour-preserving migration (**revises 0037's per-repo scoping**) | Accepted |
| [0072](0072-plugins-a-governed-extension-point.md) | **Plugins**: a second extension point beside Steps (0008 and 0038's rejections stand) — digest-pinned out-of-process image + closed capability manifest, PluginPrincipal (two-key gate approval), PluginActions as `plugin_action` Steps, CredentialBroker with per-action write rules, `framed`/`trusted_dom` UI tiers, StepUpConfirmation, isolated `scarab-plugins` namespace with a fail-closed probe, signed publishers + operator ceiling + kill switch | Accepted |
| [0073](0073-step-up-confirmation.md) | **Step-up confirmation**: a passkey (WebAuthn) is the step-up (GitHub/Forgejo re-login never counts; enroll at first crown-jewel op, first passkey only after a fresh login); the crown-jewel list incl. irreversible destruction; fixed 5-min window + always-ask credential/channel minting; 403 `step_up_required`; StepUpConfirmationRequest (agents/CLI → one-time exact-action link confirmed by their own human); OrgSecurityEventLog incl. refusals; enforced from day one with `scarab security setup/status` + server break-glass; hand-rolled WebAuthn RP on `ring`, one classified route table | Accepted |
| [0074](0074-deployments-leave-core.md) | **Deployments leave core**: Scarab governs the Run that deploys and keeps no deployment state after it; a Deploy Run is a Run whose Pipeline names an `environment:` (admission, scoped secrets, no newest-wins cancel) and the Runs list filters by it; waiting until a change is live is a polling `plugin_action` Step (`argocd` `wait-live`); deletes the Deployment record, DeploymentGate, `deployment_requests`, the board and `report_deployments` (**supersedes 0070 except §9 and §13, and parts of 0071 and 0072**) | Proposed |

> For *features* deferred until demand appears (as opposed to ADRs
> to be written), see [`docs/followups.md`](../followups.md).

## Open / deferred (to be written when their slice begins)

Workspace CAS internals (chunking) · multi-cluster remote-agent protocol · install Operator ·
OIDC login/session details · API rate-limit specifics · SLSA/cosign/SBOM export · notification providers.
