//! # scarab-engine — the durable execution core
//!
//! This crate is a **pure domain crate**: it has ZERO infrastructure
//! dependencies. Its only dependencies are `serde`, `serde_json`,
//! `thiserror` and `async-trait`. There is no database driver, no HTTP
//! client, no Kubernetes client and no async runtime linked in here.
//!
//! That purity is deliberate and is the whole point of the architecture:
//!
//!  * The scheduler / reconciler logic that will live here is expressed
//!    purely in terms of the [`Db`], [`Clock`] and [`Executor`] ports.
//!  * Because those ports are `dyn`-safe (via `async-trait`) and the crate
//!    links no real clock, database or executor, the engine can be driven
//!    entirely by fakes. This is where **deterministic simulation testing
//!    (DST)** will live: virtual time from a fake clock, an in-memory db,
//!    and an executor whose handles can be told to fail or die on demand.
//!
//! Everything below is a compiling skeleton — method bodies are stubs.

pub mod ports;
pub mod scheduler;

pub use ports::{Clock, Db, Executor, LogChunks, SnapshotRetention, WorkspaceSnapshots};
pub use scheduler::{
    cancel_run_request, pin_run_snapshots, plan_rerun, record_gate_approval, release_gate,
    rerun_step, rerun_step_widened, retry_step, retry_step_widened, unpin_run_snapshots,
    ExpiredInput, RerunError, RerunPlan, Scheduler, SchedulerError, SupersedeTeardown,
    SupersededAttempt, Supervision, TickHealth, CANCEL_RUN, LAUNCH_STEP, MAX_DELIVERY_ATTEMPTS,
    RUN_STATUS_CHANGED, SUPERSEDE_TEARDOWN,
};

use serde::{Deserialize, Serialize};

/// Schema version stamped onto every [`EventKind`] this build emits.
///
/// Per ADR-0022 the event log is version-tolerant: older events keep their
/// lower stamp and are upcast on read; new payloads bump this constant.
pub const EVENT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Identity of a single durable run (one execution of a pipeline).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub String);

/// Identity of a logical step within a run's DAG.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StepId(pub String);

/// Identity of one attempt at executing a step (retries mint new attempts).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttemptId(pub String);

/// A logical timestamp in unix milliseconds. Kept as a plain integer so the
/// domain crate need not depend on `chrono`/`time` (infra-adjacent) — the
/// [`Clock`] port is the only source of "now".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

// ---------------------------------------------------------------------------
// State machine enums
// ---------------------------------------------------------------------------

/// Lifecycle status of a whole run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Pending,
    Running,
    Suspended,
    Succeeded,
    Failed,
    Cancelled,
    DeadLettered,
}

/// A one-line summary of a run for a list view (`GET /v1/runs`): identity,
/// current status, and creation time. Deliberately lean — the full run + its
/// steps come from `run_status` / `steps_of_run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run: RunId,
    pub status: RunStatus,
    pub created_at: Timestamp,
    /// Last status-transition time (epoch millis). For a terminal run this is
    /// its finish; `updated_at - created_at` is the run's duration.
    #[serde(default)]
    pub updated_at: Timestamp,
    /// The owning tenant `(org, project)` (ADR-0049), if the run was stamped
    /// at creation — the tenancy filter for the list view.
    #[serde(default)]
    pub tenant: Option<(String, String)>,
    /// The **run number** (ADR-0057 amendment) — a per-repo sequential `#N`, the
    /// human handle for the run. `None` for untenanted inline runs and runs
    /// created before run-number allocation. Distinct from the opaque internal
    /// [`RunId`]; display/reference only, never a key.
    #[serde(default)]
    pub run_number: Option<i64>,
    /// The run's **origin** — the trigger facts it was born from, stamped at
    /// creation from the normalized `Event`. Discrete and independently
    /// nullable (never a bundle): the facts are naturally sparse across trigger
    /// kinds, and runs created before origin-stamping carry all `None`.
    /// The trigger kind (`push`/`pull_request`/`tag`/…), always set for a
    /// trigger-created run.
    #[serde(default)]
    pub trigger_kind: Option<String>,
    /// The **Actor** login — who caused the trigger (`None` for cron/upstream
    /// or a pre-origin run).
    #[serde(default)]
    pub actor: Option<String>,
    /// The symbolic branch/tag ref the run ran on (`refs/heads/main`, a tag).
    #[serde(default)]
    pub git_ref: Option<String>,
    /// The resolved commit the run pinned to.
    #[serde(default)]
    pub sha: Option<String>,
    /// The pull-request number, for `pull_request` runs.
    #[serde(default)]
    pub pr_number: Option<i64>,
    /// The PR **base** branch (`origin_pr_base`, ADR-0057) — the branch a
    /// `pull_request` run targets, rendered `base ← head`. A discrete origin
    /// fact, `None` for non-PR runs and pre-stamping runs.
    #[serde(default)]
    pub pr_base: Option<String>,
    /// The bare name of the pipeline this run executed (the `.scarab/<name>`
    /// selection), stamped at creation for trigger/dispatch runs. `None` for
    /// inline runs (no named pipeline) and pre-stamping runs.
    #[serde(default)]
    pub pipeline: Option<String>,
    /// The run **Headline** (ADR-0057) — the one human line saying what this run
    /// is about, disambiguated by `trigger_kind` (a push's commit subject; later
    /// a PR title / dispatch reason). Display/audit only. `None` when the trigger
    /// carried no headline and on pre-stamping runs.
    #[serde(default)]
    pub trigger_title: Option<String>,
}

/// Lifecycle status of a single step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
}

/// How a concurrency group admits a new run when its single slot is already
/// held by another active run (ADR-0011, 0032).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConcurrencyPolicy {
    /// Wait: the new run stays Pending until the holder settles (serialize).
    Queue,
    /// Cancel the in-progress holder, then the new run takes the slot.
    CancelInProgress,
}

impl ConcurrencyPolicy {
    /// Wire token stored on the run.
    pub fn as_str(self) -> &'static str {
        match self {
            ConcurrencyPolicy::Queue => "queue",
            ConcurrencyPolicy::CancelInProgress => "cancel-in-progress",
        }
    }

    /// Parse the wire token; anything unrecognized defaults to the safe `Queue`.
    pub fn from_wire(s: &str) -> ConcurrencyPolicy {
        match s {
            "cancel-in-progress" => ConcurrencyPolicy::CancelInProgress,
            _ => ConcurrencyPolicy::Queue,
        }
    }
}

/// Classifies a recorded attempt failure so the engine can decide retry vs.
/// dead-letter (ADR-0020/0047). Mirrors the executor port's
/// [`ports::FailureClass`] — the adapter classifies, the engine consumes.
///
/// - `Infra { never_started: true }` — the platform failed the step before its
///   main process ever ran (image pull, unschedulable, evicted-while-Pending):
///   no side effect is possible, so bounded auto-retry is always safe.
/// - `Infra { never_started: false }` — the platform killed a started process
///   (OOM, eviction, node loss): a side effect may exist; retry only on the
///   author's `retry:` assertion (ADR-0047, wired by the retry-loop slice).
/// - `Step` — the user's command exited non-zero; never auto-retried.
/// - `Timeout` — the step exceeded its deadline; post-start by definition.
/// - `Config` — the platform rejected the step's spec before it ran (bad
///   securityContext, invalid image name, missing mounted key): permanent and
///   author-fixable, so it fails fast as a developer verdict and is never
///   retried (retrying the identical spec cannot succeed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    Infra {
        never_started: bool,
    },
    Step,
    Timeout,
    /// The backend lost the execution (vanished Pod, node stopped reporting).
    /// Conservatively post-start — it cannot be proven the process never ran —
    /// so retry is assertion-gated and **counts against the attempt budget**
    /// (ADR-0047).
    Lost,
    /// Permanent, author-fixable configuration/admission rejection — see
    /// [`FailureClass::Config`](crate::ports::FailureClass::Config). Never
    /// auto-retried; settles the run as `Failed` (developer signal), not
    /// `DeadLettered`.
    Config,
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// A durable run aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub status: RunStatus,
    pub created_at: Timestamp,
}

/// The per-run projection of a single step.
///
/// Carries its `needs` (the step's DAG in-edges) so the DAG snapshot the
/// scheduler folds is self-contained: dependency-aware admission reads `needs`
/// directly rather than issuing a second query (ADR-0006, 0011).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRun {
    pub run: RunId,
    pub step: StepId,
    pub status: StepStatus,
    pub attempts: Vec<Attempt>,
    /// Upstream steps this step depends on. A step is admissible only once all
    /// of these have `Succeeded`.
    #[serde(default)]
    pub needs: Vec<StepId>,
    /// If set (`manual`/`timer`/`external`), this is a **gate** step: it launches
    /// no Pod; when its needs are satisfied the run suspends until released
    /// (ADR-0008). `None` for ordinary executed steps.
    #[serde(default)]
    pub gate_kind: Option<String>,
}

/// One attempt at executing a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: AttemptId,
    pub started_at: Timestamp,
    pub failure: Option<FailureKind>,
    /// The executor's human-readable cause for a `Failed` attempt (ticket
    /// 4cf03d7) — e.g. "cold tier refused: connection refused" from a
    /// lost-evidence drain — alongside the machine-consumed `failure` class.
    /// `#[serde(default)]` keeps rows/blobs written before the column existed
    /// decodable; `None` means the class alone is the whole story.
    #[serde(default)]
    pub failure_detail: Option<String>,
    /// Where this attempt's output workspace snapshot was **durable** when its
    /// verdict was granted (ADR-0064 s2): the Depot's self-reported tier wire
    /// string — `object` | `separate-volume` | `warm-only` — stamped by
    /// `set_step_output` beside the snapshot root. Per-attempt evidence, never
    /// recomputed: it answers "what did THIS `Succeeded` license?" after the
    /// deployment's tier has changed. `#[serde(default)]` keeps pre-s2
    /// rows/blobs decodable; `None` = no workspace, pre-s2 row, or unknown.
    #[serde(default)]
    pub output_durability: Option<String>,
    /// The recorded terminal (or in-flight) outcome of this attempt (ADR-0056
    /// amendment). `Running` until an outcome is written; `Superseded` and
    /// `Cancelled` are the non-failure terminations that must never render as a
    /// green `failed:false` attempt. `#[serde(default)]` keeps rows/blobs written
    /// before the column existed decodable — the storage read derives
    /// `Failed`/`Running` from the legacy `failure` column for such rows.
    #[serde(default)]
    pub outcome: AttemptOutcome,
}

/// The recorded outcome of a single [`Attempt`] (ADR-0056 amendment). Distinct
/// from [`FailureKind`], which only classifies *why* a `Failed` attempt failed:
/// this also names the non-failure terminations. In particular `Superseded` — a
/// rerun/retry re-armed the step while this attempt was still `Running`, so its
/// input is being replaced and it can never honestly finish (its Pod is torn
/// down) — was previously invisible and served as `failed:false`, rendering the
/// abandoned attempt green. `Running` is the in-flight state before any terminal
/// outcome is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// In flight — no terminal outcome recorded yet.
    #[default]
    Running,
    /// The attempt finished and its step succeeded.
    Succeeded,
    /// The attempt finished in a classified failure (see the attempt's `failure`).
    Failed,
    /// A rerun/retry re-armed this attempt's still-`Running` step: its input is
    /// being replaced so it can never honestly finish; its Pod is torn down. A
    /// wasted-compute stop, not a failure.
    Superseded,
    /// The attempt's run/step was cancelled (ADR-0054). Not a failure.
    Cancelled,
}

impl AttemptOutcome {
    /// The wire/column token (snake_case), mirroring the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            AttemptOutcome::Running => "running",
            AttemptOutcome::Succeeded => "succeeded",
            AttemptOutcome::Failed => "failed",
            AttemptOutcome::Superseded => "superseded",
            AttemptOutcome::Cancelled => "cancelled",
        }
    }

    /// Parse a stored column/wire token; `None` for an unknown value.
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "running" => AttemptOutcome::Running,
            "succeeded" => AttemptOutcome::Succeeded,
            "failed" => AttemptOutcome::Failed,
            "superseded" => AttemptOutcome::Superseded,
            "cancelled" => AttemptOutcome::Cancelled,
            _ => return None,
        })
    }
}

/// A **Run-scoped shared service** instance (ADR-0058): the durable projection
/// of one pipeline-level `services:` entry, born eagerly at Run start and torn
/// down when the Run/Take reaches terminal (namespace-per-run teardown, not a
/// refcount). Keyed `{run, take, name}` — a **fresh instance per Take**, so a
/// Rerun (a new Take) provisions a new instance that cannot see the prior Take's
/// writes. Not a `needs`-able DAG node; explicitly *unfenced* external state.
///
/// The `take` is materialized here as a stored generation integer. This is a
/// narrow, deliberate departure from ADR-0056 (which keeps Takes a *derived*
/// lens with "no take column anywhere"): a live k8s object cannot be re-derived
/// from event replay, and ADR-0058 explicitly keys the instance on `{run, take}`.
/// The engine's "current take" for a run is `max(take)` over its services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunService {
    pub run: RunId,
    /// The Take generation this instance belongs to (1 for the first Take).
    pub take: i64,
    /// The declared service name — also the cluster DNS hostname.
    pub name: String,
    pub status: ServiceStatus,
    /// The executor handle once launched (`None` before launch / after a crash
    /// before the handle was recorded). Lets teardown/readiness address the unit.
    pub handle: Option<String>,
    pub created_at: Timestamp,
}

/// The lifecycle of a [`RunService`] (ADR-0058): `starting → ready → running`,
/// terminal `torn-down | failed`. `Ready` is the readiness-gate release signal;
/// `Failed` (e.g. a ready-timeout) fails opt-in steps fail-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceStatus {
    /// Provisioning; the readiness probe has not yet passed.
    Starting,
    /// The readiness probe passed — opt-in steps may proceed.
    Ready,
    /// Ready and observed live (a post-ready liveness marker).
    Running,
    /// Torn down at Run/Take terminal (namespace-per-run teardown).
    TornDown,
    /// Failed to become ready within budget (fail-closed for opt-in steps).
    Failed,
}

impl ServiceStatus {
    /// The wire token stored in the `status` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceStatus::Starting => "starting",
            ServiceStatus::Ready => "ready",
            ServiceStatus::Running => "running",
            ServiceStatus::TornDown => "torn-down",
            ServiceStatus::Failed => "failed",
        }
    }

    /// Parse a stored wire token; `None` for an unknown value.
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "starting" => ServiceStatus::Starting,
            "ready" => ServiceStatus::Ready,
            "running" => ServiceStatus::Running,
            "torn-down" => ServiceStatus::TornDown,
            "failed" => ServiceStatus::Failed,
            _ => return None,
        })
    }

    /// A terminal instance is never provisioned or gated on again.
    pub fn is_terminal(&self) -> bool {
        matches!(self, ServiceStatus::TornDown | ServiceStatus::Failed)
    }
}

/// The executable contract of a Step (ADR-0008): an OCI image + command, plus
/// environment. This is the minimal spec the [`Executor`] needs to launch one
/// Pod; the full IR (`scarab-pipeline`) compiles down to it. It is handed to the
/// executor at launch time rather than stored on the durable [`StepRun`], so the
/// durable instance stays lean and the "what to run" comes from the Run's IR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSpec {
    pub image: String,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Secret keys the step wants injected at launch (ADR-0037). Resolved (with
    /// inheritance) and merged into `env` by the launch path just before the Pod
    /// starts — never persisted with a value. Empty for most steps.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// The privilege escalations **admitted** for this step (ADR-0039) — already
    /// authorized against the run's Environment whitelist and fork-lockout at run
    /// creation. The executor applies *exactly* these above the hardened baseline
    /// and nothing more (fail-closed). Default = the restricted baseline.
    #[serde(default)]
    pub run_as_root: bool,
    #[serde(default)]
    pub add_capabilities: Vec<String>,
    #[serde(default)]
    pub privileged: bool,
    /// The step's authored execution deadline in seconds (ADR-0047), if any.
    /// `None` = the executor's configured global default (1h). The backend
    /// enforces it primarily (kubelet `activeDeadlineSeconds` / local
    /// kill-timer); the scheduler keeps an engine-side backstop.
    #[serde(default)]
    pub timeout_seconds: Option<u32>,
    /// The CAS tree roots of the workspaces this step consumes (ADR-0007/
    /// 0029/0045): the outputs of its `needs` (or its explicit `inputs:`
    /// subset), merged in order. Filled by the scheduler at launch; the
    /// executor materializes them into `/workspace` before the step starts.
    #[serde(default)]
    pub workspace_inputs: Vec<String>,
    /// The workspace-relative paths this step **publishes** downstream
    /// (ADR-0007), authored as `outputs:`. Empty = the whole workspace (the
    /// implicit default). Unlike [`workspace_inputs`](Self::workspace_inputs)
    /// these are authored at compile time, not resolved at launch: the backend
    /// prunes the post-step snapshot to exactly these paths, so a dependent
    /// receives a precise slice and the output hash is unaffected by unrelated
    /// files. A declared path the step did not produce fails the step
    /// (fail-closed — never a silently narrower publish).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_outputs: Vec<String>,
    /// Set when this is a **clone** step (ADR-0045): the engine runs the
    /// canonical scarab-clone image with this context instead of an authored
    /// image/command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone: Option<CloneConfig>,
    /// Set when this is a **build** step (ADR-0018): the engine runs rootless
    /// BuildKit with this context instead of an authored image/command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildConfig>,
    /// Artifact publication globs (ADR-0052): filters what the backend
    /// collects from `/scarab/artifacts/` post-step. Empty = everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    /// Placement profile names (ADR-0055) whose admin-defined k8s overlays the
    /// executor merges onto the Pod, in listed order, atop the operator baseline.
    /// Empty = the operator's `default` profile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placement_profiles: Vec<String>,
    /// Requested compute resources (ADR-0055), applied to the step container's
    /// requests/limits. Default = the operator baseline's defaults.
    #[serde(default)]
    pub resources: scarab_pipeline::Resources,
    /// The raw pod-spec overlay **admitted** for this step (ADR-0055) — already
    /// authorized against the run's Environment at run creation (a request carries
    /// no authority; fail-closed). The executor strategic-merges it onto the Pod
    /// last. `None` for almost every step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k8s_overlay: Option<serde_json::Value>,
    /// The per-attempt OIDC token for keyless cloud federation (ADR-0015),
    /// minted at LAUNCH — **never serialized** (in-memory enrichment only;
    /// delivery to the Pod is a tmpfs file, never env/argv). `None` when the
    /// issuer is not configured.
    #[serde(skip)]
    pub oidc_token: Option<String>,
    /// Sidecar services (ADR-0058): throwaway backing containers co-located in
    /// this Step's Pod, reachable at `localhost:<port>`. The executor injects
    /// each as a native sidecar (an `initContainer` with `restartPolicy: Always`,
    /// reusing the ADR-0042 machinery); an optional readiness probe gates the
    /// step's main container start. Fenced by inheritance — they die with the Pod
    /// and are re-created fresh on every Attempt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<scarab_pipeline::ServiceSpec>,
    /// Shared-service opt-in (ADR-0058): the names of pipeline-level shared
    /// services this Step reaches over the network. The executor stamps an
    /// opt-in label per name on the Step's Pod so the service's NetworkPolicy
    /// admits it (least-privilege: non-opt-in Pods cannot reach the service).
    /// The scheduler readiness-gates this Step on those services becoming ready.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// The concrete matrix coordinate this instance was expanded from (ADR-0023),
    /// e.g. `{features: all, toolchain: stable}`. Carried through from the compiled
    /// pipeline spec (`scarab_pipeline::StepSpec::matrix_values`) so launch-time CEL
    /// interpolation can resolve `${{ matrix.<dim> }}` in the image/command/env.
    /// Empty for steps authored without a matrix. Backward-compatible on the wire
    /// (absent in older stored specs → empty).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub matrix_values: std::collections::BTreeMap<String, String>,
}

/// The launch context of a `kind: build` step (ADR-0018): what to build
/// (workspace-relative context/dockerfile) and the image tag to build and
/// optionally push.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildConfig {
    /// Workspace-relative build context directory (default `.`).
    #[serde(default)]
    pub context: String,
    /// Dockerfile name within the context (default `Dockerfile`).
    #[serde(default)]
    pub dockerfile: String,
    /// The image reference to build (and push), e.g. `ghcr.io/acme/app:sha`.
    pub image: String,
    /// The run's repo coordinate (from the trigger), pinned at creation —
    /// what the scoped `REGISTRY_AUTH` secret resolves against and what the
    /// forge derives its registry credential for. Empty for inline dev runs.
    #[serde(default)]
    pub repo_owner: String,
    #[serde(default)]
    pub repo_name: String,
    /// Push after building.
    #[serde(default)]
    pub push: bool,
    /// Allow pushing to an insecure (plain-HTTP) registry — dev/test clusters
    /// only; never authored, set by the composition/test harness.
    #[serde(default)]
    pub insecure_push: bool,
    /// A scoped `REGISTRY_AUTH` secret's dockerconfigjson, resolved at
    /// LAUNCH — **never serialized**; mounted verbatim as the Pod's docker
    /// config (tmpfs), never env (ADR-0018/0037). Takes precedence over
    /// [`derived_auth`](Self::derived_auth).
    #[serde(skip)]
    pub registry_auth_json: Option<String>,
    /// A forge-derived registry credential for the forge's own registry
    /// (ADR-0018 amendment: zero-config GHCR/Forgejo push). Filled at LAUNCH,
    /// **never serialized**; used only when no scoped `REGISTRY_AUTH` secret
    /// resolves. Delivered to the Pod as a mounted dockerconfigjson, never env.
    #[serde(skip)]
    pub derived_auth: Option<RegistryCredential>,
}

/// The in-memory half of a derived registry credential (mirrors the forge
/// port's type without a crate dependency; the engine stays forge-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryCredential {
    /// Registry host, e.g. `ghcr.io`.
    pub registry: String,
    pub username: String,
    pub token: String,
}

/// The launch context of a clone step (ADR-0045), resolved from the run's
/// trigger at creation (owner/name/sha/read_only + the authored knobs) and
/// enriched at launch (URL from the ForgeConnection registry; the short-TTL
/// checkout credential — in-memory only, never persisted).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneConfig {
    /// The forge coordinate, pinned at run creation.
    pub owner: String,
    pub name: String,
    /// The commit the run is pinned to (resolved ONCE at trigger time,
    /// ADR-0043/0044) — clone always fetches THIS, never re-resolves.
    pub sha: String,
    /// `depth: full` (complete history) vs the shallow default.
    #[serde(default)]
    pub depth_full: bool,
    #[serde(default)]
    pub submodules: bool,
    #[serde(default)]
    pub lfs: bool,
    /// Fork-PR runs get a READ-ONLY credential (ADR-0045 trust model), fixed
    /// immutably at run creation from the trigger.
    #[serde(default)]
    pub read_only: bool,
    /// The credential-free clone URL — filled at LAUNCH by the composition
    /// root (from the repo's ForgeConnection); empty in the stored spec.
    #[serde(default)]
    pub url: String,
    /// The short-TTL checkout credential — filled at LAUNCH, **never
    /// serialized** (in-memory enrichment only; delivery to the Pod is via a
    /// tmpfs file, never env/URL/argv — ADR-0045 §Token delivery).
    #[serde(skip)]
    pub credential: Option<CloneCredential>,
}

/// The in-memory half of a minted checkout credential (mirrors the forge
/// port's type without a crate dependency; the engine stays forge-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneCredential {
    pub username: String,
    pub token: String,
}

/// One artifact of record (ADR-0052): an output a step published to
/// `/scarab/artifacts/`, name-addressed per run, immutable once written.
/// The blob lives in the object store (NOT the workspace CAS — independent
/// lifecycle); this is its metadata row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub name: String,
    pub size: u64,
    pub content_type: String,
    /// Object-store key holding the bytes.
    pub object_key: String,
}

/// A persisted artifact version (ADR-0056): [`ArtifactMeta`] plus the
/// provenance that makes it immutable per attempt. A retry never overwrites a
/// prior attempt's version; the name-addressed "of record" download resolves
/// to the latest version whose attempt **succeeded** (a consumer must never
/// silently receive a failed attempt's partial file), while failed-attempt
/// versions are retained as evidence and swept with the run's TTL class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub meta: ArtifactMeta,
    /// The step that published this version.
    pub step: StepId,
    /// The attempt that published this version — the evidence key.
    pub attempt: AttemptId,
    /// Whether that attempt's verdict was success (of-record candidates only).
    pub succeeded: bool,
    pub created_at: Timestamp,
}

/// A manual/approval gate that suspends a run until released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    pub run: RunId,
    pub step: StepId,
    pub approved: bool,
}

// ---------------------------------------------------------------------------
// Append-only event log
// ---------------------------------------------------------------------------

/// An entry in the append-only event log that is the run's source of truth.
///
/// The `version` field makes the log **version-tolerant**: older events with
/// a lower `version` can still be folded by newer code, and new fields are
/// added with defaults keyed off the version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventKind {
    /// Schema version of this event's payload.
    pub version: u32,
    pub run: RunId,
    pub kind: EventPayload,
    pub at: Timestamp,
}

// ---------------------------------------------------------------------------
// Transactional outbox
// ---------------------------------------------------------------------------

/// The durable-store-assigned identity of an outbox row (a monotonic sequence).
/// `OutboxId(0)` marks a message not yet persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OutboxId(pub i64);

/// A message on the transactional outbox — the coordination bus between the
/// durable brain and the executor (ADR-0003). A state transition and the intent
/// to act on it are written in one transaction; a drainer later claims and
/// dispatches. `idempotency_key` is unique, so a logical effect is enqueued once
/// and any duplicate dispatch is neutralized by the fence at the consumer
/// (ADR-0021).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxMessage {
    /// Store-assigned id (`OutboxId(0)` before it is persisted).
    pub id: OutboxId,
    pub run: RunId,
    /// What kind of effect to perform (e.g. `"launch_step"`).
    pub kind: String,
    /// Effect-specific payload.
    pub payload: serde_json::Value,
    /// Unique key collapsing duplicate enqueues to a single effect.
    pub idempotency_key: String,
    pub at: Timestamp,
}

// ---------------------------------------------------------------------------
// Log index
// ---------------------------------------------------------------------------

/// The Postgres-side **index** of one persisted log chunk (ADR-0013). Log
/// *bodies* live only in the object store (chunked + compressed); Postgres keeps
/// just this offset metadata so the store never bloats with log text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogChunkMeta {
    /// 0-based position of this chunk within the step's log stream.
    pub seq: u64,
    /// Cumulative *uncompressed* byte offset where this chunk begins.
    pub byte_offset: u64,
    /// Uncompressed length of this chunk in bytes.
    pub len: u64,
    /// Object-store key holding the compressed chunk body.
    pub object_key: String,
}

/// A deploy run's durable target: the repo it deploys, the environment it
/// targets, and the git ref it ships (ADR-0037). Recorded at run creation for
/// runs whose pipeline declared an `environment:`; absent for ordinary CI runs.
/// Lets admission find the environment's protection rules at gate-approval time
/// without parsing the stored IR blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployContext {
    pub org: String,
    /// The owning Project's name — its repo's forge name, 1:1 in v1
    /// (ADR-0046: a Project IS the governed repo). Keys the protection-rule
    /// lookup at admission.
    #[serde(alias = "repo")]
    pub project: String,
    pub environment: String,
    pub git_ref: String,
    /// Whether this run is locked out of secrets (a fork PR, ADR-0015/0037). When
    /// true the launch path injects no secrets even for env-scoped steps.
    #[serde(default)]
    pub locked_out: bool,
}

/// The discriminated payload carried by an [`EventKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventPayload {
    RunCreated,
    RunTransitioned {
        from: RunStatus,
        to: RunStatus,
    },
    StepTransitioned {
        step: StepId,
        from: StepStatus,
        to: StepStatus,
    },
    AttemptStarted {
        step: StepId,
        attempt: AttemptId,
    },
    AttemptFinished {
        step: StepId,
        attempt: AttemptId,
        failure: Option<FailureKind>,
        /// The executor's human-readable cause for the failure, when it
        /// reported one (ticket 4cf03d7). Explicitly `#[serde(default)]`:
        /// this event IS persisted in the events table and replayed, so a row
        /// appended before the field existed must still deserialize.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<String>,
    },
    /// A single approval was recorded against a `manual` gate by the named
    /// principal (ADR-0037). Append-only, accumulating — the run stays suspended
    /// until enough distinct approvers satisfy the environment's rules, at which
    /// point [`GateReleased`](EventPayload::GateReleased) finalizes the gate.
    GateApproved {
        step: StepId,
        by: String,
    },
    GateReleased {
        step: StepId,
    },
    /// A step was skipped on restart because its inputs were unchanged — its
    /// prior output is carried forward rather than recomputed (ADR-0027). Surfaced
    /// explicitly so a "smart" skip is never mysterious.
    StepSkipped {
        step: StepId,
        reason: String,
    },
    /// The run was dead-lettered (ADR-0047): the system could not obtain a
    /// verdict (infra retries exhausted / a lost execution / poison). The
    /// **operator** signal — `reason` carries the diagnostics.
    RunDeadLettered {
        reason: String,
    },
    /// An operator requested a run cancel via the API (ADR-0054). Emitted
    /// BEFORE the cancel transitions so an operator-initiated cancel is
    /// attributable and distinguishable from the system's concurrency
    /// auto-cancel (`CancelInProgress`, which emits no such event). `by` is the
    /// acting principal's subject (`None` only when auth is off), mirroring
    /// [`RunRerunRequested`](EventPayload::RunRerunRequested).
    RunCancelRequested {
        by: Option<String>,
    },
    /// An unapproved gate outlived its opt-in `gate_expires_after` deadline and
    /// failed the run (ADR-0047).
    GateExpired {
        step: StepId,
    },
    /// A human requested a step rerun (ADR-0056) — the **Take boundary**.
    /// Emitted before any re-arming, so a Take view is a pure replay up to
    /// this event. `invalidated` is the resolved invalidation set (target +
    /// transitive descendants) recorded at press time — deterministic record,
    /// not re-derivation; `by` is the acting principal's subject (the
    /// who-reran-this audit fact, `None` only when auth is off).
    ///
    /// Serialized tag is `RunRerunRequested`; the `RunRestartRequested` alias
    /// keeps events persisted before the restart→rerun rename (2026-07-23)
    /// deserializable — zero-migration, since the DB read path is pure serde
    /// and nothing keys SQL on the tag string.
    #[serde(rename = "RunRerunRequested", alias = "RunRestartRequested")]
    RunRerunRequested {
        target: StepId,
        invalidated: Vec<StepId>,
        by: Option<String>,
        /// The members of `invalidated` that are **upstream** of the target and
        /// are there only because a Workspace Snapshot they produced is gone
        /// (ADR-0061 s5): the rerun was *widened* to regenerate expired inputs
        /// instead of failing a step that could never be provisioned. Empty on
        /// an ordinary rerun, and on every event recorded before widening
        /// existed — hence `default`, which is what keeps the read path
        /// zero-migration (ADR-0022 expand-contract).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        widened: Vec<StepId>,
    },
    /// A human retried a **Failed** step (ADR-0056 amendment 2026-07-22) — an
    /// attribution/audit fact, **NOT a Take boundary**. A Retry re-executes the
    /// target (and its dependent cascade) as fresh Attempts *within the current
    /// Take* — `deriveTakes` deliberately ignores this event, so only
    /// `RunRerunRequested` splits Takes. `invalidated` is the resolved cascade
    /// (target + transitive descendants); `by` is the acting principal.
    StepRetryRequested {
        target: StepId,
        invalidated: Vec<StepId>,
        by: Option<String>,
        /// As on [`RunRerunRequested`](EventPayload::RunRerunRequested): the
        /// upstream steps dragged in to regenerate expired Workspace Snapshots
        /// (ADR-0061 s5). Empty on an ordinary retry.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        widened: Vec<StepId>,
    },
    /// A control plane that did not launch this attempt resumed supervising
    /// it (ADR-0047 re-adoption, surfaced by ADR-0056). Same attempt, same
    /// fence, no re-execution, no budget consumed — this is the durability
    /// wedge made visible, and it must never render as a new execution.
    AttemptReadopted {
        step: StepId,
        attempt: AttemptId,
    },
    /// A rerun/retry re-armed a step while this attempt was still `Running`
    /// (ADR-0056 amendment): the attempt's input is being replaced, so it can
    /// never honestly finish and its Pod is torn down. Recorded as a fact
    /// (mirroring [`AttemptReadopted`](EventPayload::AttemptReadopted)) so the
    /// supersession is legible rather than a silently-abandoned green attempt;
    /// `by` is the acting principal (`None` only when auth is off).
    AttemptSuperseded {
        step: StepId,
        attempt: AttemptId,
        by: Option<String>,
    },
    /// The run's opt-in active-time `budget:` was exhausted (ADR-0047);
    /// in-flight steps were cancelled and the run fails.
    RunBudgetExhausted {
        active_ms: i64,
        budget_ms: i64,
    },
    /// An opt-in step (ADR-0058 `uses:`) was failed **fail-closed** because a
    /// shared service it depends on never became ready (a ready-timeout). The
    /// unbound-dependency diagnostic — `reason` names the service(s).
    StepServicesUnready {
        step: StepId,
        reason: String,
    },
    /// A human **pinned** this Run's Workspace Snapshots (ADR-0061 s5): keep
    /// them past the cold tier's retention TTL, for an investigation. The pin is
    /// a durable fact on the run row; this event is its audit half — who asked
    /// for the exception and when — mirroring [`GateApproved`](EventPayload::GateApproved).
    /// `by` is the acting principal (`None` only when auth is off).
    RunSnapshotsPinned {
        by: Option<String>,
    },
    /// A human released the pin, returning this Run's Workspace Snapshots to the
    /// ordinary TTL. Recorded for the same reason as the pin: an exception that
    /// costs storage should be attributable in both directions.
    RunSnapshotsUnpinned {
        by: Option<String>,
    },
    /// Escape hatch for forward-compatible payloads not yet modelled.
    Raw(serde_json::Value),
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by the [`Db`] port.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("durable store unavailable")]
    Unavailable,
    #[error("optimistic concurrency conflict")]
    Conflict,
    #[error("db error: {0}")]
    Other(String),
}

/// Errors returned by the [`Executor`] port.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("failed to launch step: {0}")]
    Launch(String),
    #[error("execution backend unavailable")]
    Unavailable,
    #[error("exec error: {0}")]
    Other(String),
}

/// A transition the state machine refused because it is not legal from the
/// current state. This is the pure guard behind the *forward-progress* and
/// *exactly-once* invariants: terminal states are sinks, and a state can only
/// move along an edge the machine declares.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    #[error("illegal run transition {from:?} -> {to:?}")]
    IllegalRun { from: RunStatus, to: RunStatus },
    #[error("illegal step transition {from:?} -> {to:?}")]
    IllegalStep { from: StepStatus, to: StepStatus },
    #[error("run is already in terminal state {0:?}")]
    RunTerminal(RunStatus),
    #[error("step is already in terminal state {0:?}")]
    StepTerminal(StepStatus),
    #[error("no in-flight attempt to finish")]
    NoAttempt,
}

// ---------------------------------------------------------------------------
// Pure state machine
// ---------------------------------------------------------------------------
//
// Every public method below is a *pure* function of `(self, args)` — it never
// touches a port, a clock, or the outside world. It mutates the aggregate and
// returns the [`EventKind`]s a caller must durably append (state tables are the
// source of truth, the event log is the derived-but-durable record — ADR-0013).
// A caller wires these into the [`Db`] port; the machine itself is I/O-free so
// it can be exhaustively unit-tested with no infra (ADR-0002 / ADR-0017).

/// The dependencies a step actually consumes: its explicit `inputs:` subset when
/// declared, else all of its `needs` (implicit-by-default). `workspace_inputs`
/// and `input_signature` MUST resolve consumption through this one function, so
/// the workspace a step materializes and the signature that decides whether it
/// re-runs are always computed over *exactly* the same set (ADR-0007).
fn consumed<'a>(needs: &'a [StepId], inputs: Option<&'a [StepId]>) -> &'a [StepId] {
    inputs.unwrap_or(needs)
}

/// Resolve a step's **input workspace** from its dependencies.
///
/// A step consumes its explicit `inputs:` subset when declared, else all of its
/// `needs` (implicit-by-default — ADR-0007, 0029). Returns those dependencies'
/// output snapshots (CAS merkle-root hashes), in dependency order, skipping any
/// that produced no workspace. `inputs = None` is the inherit-all default;
/// `Some(subset)` restricts the workspace to just the named needs.
///
/// (The sibling `outputs:` per-path publishing selection is deliberately
/// unimplemented — see the CAS snapshot site and `docs/followups.md`.)
pub fn workspace_inputs(
    needs: &[StepId],
    inputs: Option<&[StepId]>,
    output_of: &std::collections::HashMap<StepId, String>,
) -> Vec<String> {
    consumed(needs, inputs)
        .iter()
        .filter_map(|n| output_of.get(n).cloned())
        .collect()
}

/// A deterministic signature of the workspace a step will consume: the output
/// snapshots of the dependencies it consumes (its explicit `inputs:` subset, or
/// all `needs` — resolved identically to [`workspace_inputs`]), in sorted order.
/// Two runs of a step with the same upstream outputs produce the same signature —
/// the basis for restart skip-if-unchanged (ADR-0027). A consumed need that has
/// produced no output contributes an empty slot, so "an upstream that gained/lost
/// an output" also changes the signature. Because the subset is resolved the same
/// way as the materialized workspace, the signature covers *exactly* the inputs
/// the step consumes: a change in a need it does not consume does not force a
/// re-run (ADR-0007).
pub fn input_signature(
    needs: &[StepId],
    inputs: Option<&[StepId]>,
    output_of: &std::collections::HashMap<StepId, String>,
) -> String {
    let mut parts: Vec<String> = consumed(needs, inputs)
        .iter()
        .map(|n| {
            format!(
                "{}={}",
                n.0,
                output_of.get(n).map(|s| s.as_str()).unwrap_or("")
            )
        })
        .collect();
    parts.sort();
    parts.join(";")
}

/// The set of steps invalidated by restarting `target`: `target` itself plus
/// every step that (transitively) `needs` it. Computed by reverse reachability
/// over the DAG edges — so smart restart re-runs the target and its descendants,
/// leaving siblings and ancestors intact (ADR-0027).
pub fn invalidation_set(target: &StepId, steps: &[StepRun]) -> std::collections::HashSet<StepId> {
    let mut invalid = std::collections::HashSet::new();
    invalid.insert(target.clone());
    // Fixpoint: a step joins the set once any of its needs is in the set.
    let mut changed = true;
    while changed {
        changed = false;
        for s in steps {
            if !invalid.contains(&s.step) && s.needs.iter().any(|n| invalid.contains(n)) {
                invalid.insert(s.step.clone());
                changed = true;
            }
        }
    }
    invalid
}

/// Build an event stamped with the current [`EVENT_VERSION`].
fn event(run: &RunId, kind: EventPayload, at: Timestamp) -> EventKind {
    EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind,
        at,
    }
}

impl RunStatus {
    /// Terminal states are sinks: no transition leaves them. This is what makes
    /// "forward progress or explicit dead-letter" (invariant 1) hold — a run
    /// cannot loop back out of `Succeeded`/`Failed`/`Cancelled`/`DeadLettered`.
    pub fn is_terminal(self) -> bool {
        use RunStatus::*;
        matches!(self, Succeeded | Failed | Cancelled | DeadLettered)
    }

    /// The declared legal edges of the run state machine.
    fn can_transition_to(self, to: RunStatus) -> bool {
        use RunStatus::*;
        matches!(
            (self, to),
            (Pending, Running)
                | (Pending, Cancelled)
                | (Running, Suspended)
                | (Running, Succeeded)
                | (Running, Failed)
                | (Running, Cancelled)
                | (Running, DeadLettered)
                | (Suspended, Running)
                | (Suspended, Cancelled)
        )
    }
}

impl Run {
    /// Mint a fresh run in `Pending` together with its `RunCreated` event.
    pub fn new(id: RunId, created_at: Timestamp) -> (Run, EventKind) {
        let run = Run {
            id: id.clone(),
            status: RunStatus::Pending,
            created_at,
        };
        let ev = event(&id, EventPayload::RunCreated, created_at);
        (run, ev)
    }

    /// Move the run to `to`, returning the transition event to append.
    ///
    /// Rejected (leaving `self` untouched) if `self` is already terminal or the
    /// edge is not declared legal — including a no-op `from == to`, so a crashed
    /// worker replaying the same transition is refused rather than double-counted.
    pub fn transition(
        &mut self,
        to: RunStatus,
        at: Timestamp,
    ) -> Result<EventKind, TransitionError> {
        let from = self.status;
        if from.is_terminal() {
            return Err(TransitionError::RunTerminal(from));
        }
        if !from.can_transition_to(to) {
            return Err(TransitionError::IllegalRun { from, to });
        }
        self.status = to;
        Ok(event(
            &self.id,
            EventPayload::RunTransitioned { from, to },
            at,
        ))
    }
}

impl StepStatus {
    /// Terminal step states — no attempt may start from here.
    pub fn is_terminal(self) -> bool {
        use StepStatus::*;
        matches!(self, Succeeded | Failed | Skipped | Cancelled)
    }
}

impl StepRun {
    /// A step with no attempts yet and no dependencies, in `Pending`.
    pub fn new(run: RunId, step: StepId) -> StepRun {
        StepRun {
            run,
            step,
            status: StepStatus::Pending,
            attempts: Vec::new(),
            needs: Vec::new(),
            gate_kind: None,
        }
    }

    /// Whether this step is a gate (suspends the run rather than launching).
    pub fn is_gate(&self) -> bool {
        self.gate_kind.is_some()
    }

    /// Mark a `Pending` step as `Ready` for admission.
    pub fn mark_ready(&mut self, at: Timestamp) -> Result<EventKind, TransitionError> {
        let from = self.status;
        if from != StepStatus::Pending {
            return Err(TransitionError::IllegalStep {
                from,
                to: StepStatus::Ready,
            });
        }
        self.status = StepStatus::Ready;
        Ok(self.step_transition(from, StepStatus::Ready, at))
    }

    /// Begin a (re)attempt: push a fresh [`Attempt`] and move to `Running`.
    ///
    /// Legal only from `Pending` or `Ready` (a fresh admission or a retry that
    /// re-armed the step). Returns the `StepTransitioned` + `AttemptStarted`
    /// events. Each restart mints a *new* attempt — the at-least-once unit.
    pub fn start_attempt(
        &mut self,
        attempt: AttemptId,
        at: Timestamp,
    ) -> Result<Vec<EventKind>, TransitionError> {
        let from = self.status;
        match from {
            StepStatus::Pending | StepStatus::Ready => {}
            s if s.is_terminal() => return Err(TransitionError::StepTerminal(s)),
            other => {
                return Err(TransitionError::IllegalStep {
                    from: other,
                    to: StepStatus::Running,
                })
            }
        }
        self.status = StepStatus::Running;
        self.attempts.push(Attempt {
            id: attempt.clone(),
            started_at: at,
            failure: None,
            failure_detail: None,
            output_durability: None,
            outcome: AttemptOutcome::Running,
        });
        Ok(vec![
            self.step_transition(from, StepStatus::Running, at),
            event(
                &self.run,
                EventPayload::AttemptStarted {
                    step: self.step.clone(),
                    attempt,
                },
                at,
            ),
        ])
    }

    /// Finish the in-flight attempt with an optional [`FailureKind`].
    ///
    /// Outcome — and hence the *bounded* retry that guarantees forward progress:
    /// - success (`None`)            → `Succeeded`.
    /// - `Step` / `Timeout` failure  → `Failed` (a verdict was produced; retry
    ///   is only ever the author's opt-in `retry:` assertion, ADR-0047).
    /// - never-started `Infra`, attempts left (`< max_attempts`) → back to
    ///   `Ready` for an auto-retry (no side effect was possible).
    /// - never-started `Infra`, attempts exhausted → `Failed` (poison; caller
    ///   dead-letters the run).
    /// - post-start `Infra` → `Failed` (a side effect may exist; assertion-gated
    ///   retry lands with the ADR-0047 retry-loop slice).
    pub fn finish_attempt(
        &mut self,
        failure: Option<FailureKind>,
        max_attempts: u32,
        at: Timestamp,
    ) -> Result<Vec<EventKind>, TransitionError> {
        if self.status != StepStatus::Running {
            return Err(TransitionError::IllegalStep {
                from: self.status,
                to: StepStatus::Succeeded,
            });
        }
        let attempt = match self.attempts.last_mut() {
            Some(a) => {
                a.failure = failure;
                a.id.clone()
            }
            None => return Err(TransitionError::NoAttempt),
        };
        let from = StepStatus::Running;
        let to = match failure {
            None => StepStatus::Succeeded,
            // A verdict (Step/Timeout) or a permanent config rejection (Config)
            // fails the step outright — never auto-retried.
            Some(FailureKind::Step | FailureKind::Timeout | FailureKind::Config) => {
                StepStatus::Failed
            }
            Some(FailureKind::Infra {
                never_started: true,
            }) => {
                if (self.attempts.len() as u32) < max_attempts {
                    StepStatus::Ready
                } else {
                    StepStatus::Failed
                }
            }
            // A started (or possibly-started, for Lost) process may have
            // side-effected: retry only on the author's `retry:` assertion —
            // the scheduler's settle path owns that decision (ADR-0047).
            Some(
                FailureKind::Infra {
                    never_started: false,
                }
                | FailureKind::Lost,
            ) => StepStatus::Failed,
        };
        self.status = to;
        Ok(vec![
            event(
                &self.run,
                EventPayload::AttemptFinished {
                    step: self.step.clone(),
                    attempt,
                    failure,
                    // The pure state machine has no executor to ask; the
                    // scheduler's settle path is where causes are known.
                    cause: None,
                },
                at,
            ),
            self.step_transition(from, to, at),
        ])
    }

    /// Cancel a non-terminal step (e.g. its run was cancelled).
    pub fn cancel(&mut self, at: Timestamp) -> Result<EventKind, TransitionError> {
        let from = self.status;
        if from.is_terminal() {
            return Err(TransitionError::StepTerminal(from));
        }
        self.status = StepStatus::Cancelled;
        Ok(self.step_transition(from, StepStatus::Cancelled, at))
    }

    /// Number of attempts made so far.
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }

    /// The in-flight (latest) attempt, i.e. the current fence for execution.
    pub fn current_attempt(&self) -> Option<&Attempt> {
        self.attempts.last()
    }

    fn step_transition(&self, from: StepStatus, to: StepStatus, at: Timestamp) -> EventKind {
        event(
            &self.run,
            EventPayload::StepTransitioned {
                step: self.step.clone(),
                from,
                to,
            },
            at,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn workspace_inputs_inherit_needs_outputs_in_order() {
        let outputs = HashMap::from([
            (StepId("a".into()), "hash-a".to_string()),
            (StepId("b".into()), "hash-b".to_string()),
        ]);
        let needs = vec![StepId("a".into()), StepId("b".into())];
        // `None` = implicit-by-default: inherit every need's output workspace.
        assert_eq!(
            workspace_inputs(&needs, None, &outputs),
            vec!["hash-a", "hash-b"]
        );
    }

    #[test]
    fn workspace_inputs_skip_needs_that_produced_nothing() {
        // `b` never produced a workspace → it contributes no input.
        let outputs = HashMap::from([(StepId("a".into()), "hash-a".to_string())]);
        let needs = vec![StepId("a".into()), StepId("b".into())];
        assert_eq!(workspace_inputs(&needs, None, &outputs), vec!["hash-a"]);
    }

    #[test]
    fn workspace_inputs_explicit_subset_selects_only_named_needs() {
        // D needs [a, b] but declares `inputs: [b]` — only B's output workspace
        // flows in, in declared order, and A's is excluded.
        let outputs = HashMap::from([
            (StepId("a".into()), "hash-a".to_string()),
            (StepId("b".into()), "hash-b".to_string()),
        ]);
        let needs = vec![StepId("a".into()), StepId("b".into())];
        let inputs = vec![StepId("b".into())];
        assert_eq!(
            workspace_inputs(&needs, Some(&inputs), &outputs),
            vec!["hash-b"],
            "explicit inputs: [b] excludes A's workspace"
        );
    }

    #[test]
    fn workspace_inputs_explicit_subset_preserves_order_and_skips_empty() {
        // Declared order is honored (c before a), and a selected need that
        // produced nothing is skipped — same skip-empty rule as the None path.
        let outputs = HashMap::from([
            (StepId("a".into()), "hash-a".to_string()),
            (StepId("c".into()), "hash-c".to_string()),
        ]);
        let needs = vec![StepId("a".into()), StepId("b".into()), StepId("c".into())];
        let inputs = vec![StepId("c".into()), StepId("b".into()), StepId("a".into())];
        assert_eq!(
            workspace_inputs(&needs, Some(&inputs), &outputs),
            vec!["hash-c", "hash-a"],
            "c then a in declared order; b skipped (no output)"
        );
    }

    #[test]
    fn input_signature_is_stable_and_order_independent() {
        let outputs = HashMap::from([
            (StepId("a".into()), "hash-a".to_string()),
            (StepId("b".into()), "hash-b".to_string()),
        ]);
        // Order of `needs` does not change the signature (it is sorted).
        let ab = input_signature(&[StepId("a".into()), StepId("b".into())], None, &outputs);
        let ba = input_signature(&[StepId("b".into()), StepId("a".into())], None, &outputs);
        assert_eq!(ab, ba);

        // A changed upstream output changes the signature (→ cascade on rerun).
        let changed = HashMap::from([
            (StepId("a".into()), "hash-a2".to_string()),
            (StepId("b".into()), "hash-b".to_string()),
        ]);
        assert_ne!(
            ab,
            input_signature(&[StepId("a".into()), StepId("b".into())], None, &changed)
        );

        // A need that produced no output contributes an empty slot, so gaining an
        // output also changes the signature.
        let missing = HashMap::from([(StepId("b".into()), "hash-b".to_string())]);
        assert_ne!(
            ab,
            input_signature(&[StepId("a".into()), StepId("b".into())], None, &missing)
        );
    }

    #[test]
    fn input_signature_reflects_only_selected_inputs() {
        let needs = [StepId("a".into()), StepId("b".into())];
        // A step declaring `inputs: [b]` signs over B only — its signature is
        // computed over exactly the selected subset (ADR-0007), so it is
        // independent of A's output entirely.
        let base = HashMap::from([
            (StepId("a".into()), "hash-a".to_string()),
            (StepId("b".into()), "hash-b".to_string()),
        ]);
        let selected = [StepId("b".into())];
        let sig = input_signature(&needs, Some(&selected), &base);

        // Change A's output: a signature over `inputs: [b]` MUST NOT move.
        let a_changed = HashMap::from([
            (StepId("a".into()), "hash-a2".to_string()),
            (StepId("b".into()), "hash-b".to_string()),
        ]);
        assert_eq!(
            sig,
            input_signature(&needs, Some(&selected), &a_changed),
            "a need outside the `inputs:` subset must not affect the signature"
        );

        // Change B's output: the signature MUST move (B is consumed).
        let b_changed = HashMap::from([
            (StepId("a".into()), "hash-a".to_string()),
            (StepId("b".into()), "hash-b2".to_string()),
        ]);
        assert_ne!(
            sig,
            input_signature(&needs, Some(&selected), &b_changed),
            "a change in the consumed input must change the signature"
        );

        // And the subset signature differs from the inherit-all signature.
        assert_ne!(
            sig,
            input_signature(&needs, None, &base),
            "signing over [b] differs from signing over all needs"
        );
    }

    /// Diamond: A -> {B, C} -> D. Rerunning B invalidates B and its descendant
    /// D, but not its sibling C nor its ancestor A.
    #[test]
    fn invalidation_set_is_target_plus_transitive_dependents() {
        fn step(id: &str, needs: &[&str]) -> StepRun {
            let mut s = StepRun::new(RunId("r".into()), StepId(id.into()));
            s.needs = needs.iter().map(|n| StepId((*n).into())).collect();
            s
        }
        let steps = vec![
            step("A", &[]),
            step("B", &["A"]),
            step("C", &["A"]),
            step("D", &["B", "C"]),
        ];
        let invalid = invalidation_set(&StepId("B".into()), &steps);
        assert_eq!(invalid.len(), 2);
        assert!(invalid.contains(&StepId("B".into())));
        assert!(invalid.contains(&StepId("D".into())));
        assert!(!invalid.contains(&StepId("A".into())), "ancestor untouched");
        assert!(!invalid.contains(&StepId("C".into())), "sibling untouched");
    }

    /// Guards the serde alias behind the restart→rerun rename (2026-07-23): an
    /// event PERSISTED under the old tag `RunRestartRequested` must still
    /// deserialize into the renamed `RunRerunRequested` variant. The DB read
    /// path is pure serde and nothing keys SQL on the tag string, so this alias
    /// is the whole zero-migration contract — do not drop
    /// `#[serde(alias = "RunRestartRequested")]` without a data migration.
    #[test]
    fn legacy_run_restart_requested_tag_deserializes_as_rerun() {
        let legacy = serde_json::json!({
            "RunRestartRequested": { "target": "b", "invalidated": ["b"], "by": null }
        });
        let payload: EventPayload =
            serde_json::from_value(legacy).expect("legacy tag deserializes via alias");
        match payload {
            EventPayload::RunRerunRequested {
                target,
                invalidated,
                by,
                widened,
            } => {
                assert_eq!(target, StepId("b".into()));
                assert_eq!(invalidated, vec![StepId("b".into())]);
                assert_eq!(by, None);
                // A pre-ADR-0061 event carries no `widened` key at all; the
                // `#[serde(default)]` is what keeps the read path zero-migration.
                assert!(widened.is_empty());
            }
            other => panic!("expected RunRerunRequested, got {other:?}"),
        }
    }

    /// ADR-0064 / ticket 4cf03d7: `AttemptFinished` gained `cause`, and the
    /// events table replays rows written before it existed. A literal
    /// pre-`cause` JSON row must still deserialize (with `cause: None`) —
    /// kills removing the field's `#[serde(default)]`, which would make every
    /// replay of an old run's log fail at the first finished attempt.
    #[test]
    fn attempt_finished_without_cause_still_deserializes() {
        let old = r#"{"AttemptFinished":{"step":"build","attempt":"a1","failure":{"Infra":{"never_started":false}}}}"#;
        let payload: EventPayload = serde_json::from_str(old).expect("pre-cause row deserializes");
        match payload {
            EventPayload::AttemptFinished {
                step,
                attempt,
                failure,
                cause,
            } => {
                assert_eq!(step, StepId("build".into()));
                assert_eq!(attempt, AttemptId("a1".into()));
                assert_eq!(
                    failure,
                    Some(FailureKind::Infra {
                        never_started: false
                    })
                );
                assert_eq!(cause, None, "an old row simply has no recorded cause");
            }
            other => panic!("expected AttemptFinished, got {other:?}"),
        }
    }

    /// And a cause that IS recorded survives the round-trip — kills a
    /// `skip_serializing` (unconditional) mutation that would accept the cause
    /// in memory and lose it in the persisted event.
    #[test]
    fn attempt_finished_cause_round_trips() {
        let payload = EventPayload::AttemptFinished {
            step: StepId("build".into()),
            attempt: AttemptId("a2".into()),
            failure: Some(FailureKind::Infra {
                never_started: false,
            }),
            cause: Some("cold tier refused: connection refused".into()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: EventPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, payload);
    }
}
