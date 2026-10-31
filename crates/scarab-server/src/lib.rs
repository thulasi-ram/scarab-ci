//! # scarab-server — the dogfooded REST API + composition root.
//!
//! Code-first `axum` + `utoipa` (ADR-0012): the Rust request/response types are
//! the source of truth and OpenAPI is generated from them. The pipeline request
//! schema **is** the IR subset (ADR-0009) — [`PipelineDto`]/[`StepDto`] mirror
//! `scarab_pipeline`'s `PipelineIr`/`StepSpec`, so the one type system runs from
//! IR → API → generated clients. SSE (not WebSockets) carries server→client
//! streams (ADR-0012); `/logs` tails the run's append-only event log (ADR-0013).
//!
//! Handlers speak only the `Db` port, so the same code serves any adapter. The
//! background scheduler loop and full converged wiring land in the next slice.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json, Router};
use futures::stream::{self, BoxStream, Stream, StreamExt};
use futures::SinkExt;
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

/// Mint a fresh internal run id (ADR-0057 amendment). The single source of run
/// ids: an opaque, unguessable **UUIDv7** whose hex string is lexicographically
/// time-sortable, so the `runs` TEXT primary key keeps insert locality (v4 was
/// fully random — index fragmentation). v7 embeds wall-clock time, so it lives
/// here in the server (infra) rather than the engine, which sources time only
/// via the `Clock` port. Session/CSRF/state ids stay v4 — they aren't keys.
fn new_run_id() -> RunId {
    RunId(Uuid::now_v7().to_string())
}

use scarab_engine::{
    AttemptId, Clock, ConcurrencyPolicy, Db, DbError, EventKind, EventPayload, RerunError, RunId,
    RunStatus, StepId, StepSpec, StepStatus, Timestamp, EVENT_VERSION, MAX_DELIVERY_ATTEMPTS,
    RUN_STATUS_CHANGED,
};
use scarab_identity::{Action, Principal, Session};

/// ADR-0072: the CredentialBroker's plugin-facing router — its own listener,
/// not part of the public API.
pub mod broker;
pub mod broker_proxy;
/// ADR-0062 part 3: the **change set** of a Workspace Export — the exact paths an
/// Attempt wrote, read out of an `overlayfs` upper layer rather than inferred.
/// The Export lifecycle it fed was deleted when ADR-0066 cancelled lazy delivery
/// (git-bug `0ec3b39`); this module survives as the revival package, exercised
/// by `tests/changeset_overlay.rs`.
pub mod changeset;
pub mod forge_token;
pub mod config;
pub mod connections_config;
pub mod converged;
pub mod depot_expiry;
pub mod forge_router;
pub mod infra_observer;
pub mod libraries;
pub mod library_fetch;
pub mod log_tail;
pub mod logs;
pub mod metrics;
pub mod oauth;
pub mod oidc;
/// ADR-0071: the minimal org-level Environment — its admin/read API.
pub mod org_environments;
/// ADR-0072 §9: the plugin runtime's leased reconcile loop — namespace
/// baseline, isolation probe, one workload per installation.
pub mod plugin_runtime;
/// ADR-0072: PluginInstallations — the first-party catalog, install,
/// uninstall and the kill switch.
pub mod plugin_gates;
pub mod plugins;
/// ADR-0075: the Policy API — org Policies, Project typed settings, the
/// decision logs and the policy test API.
pub mod policies;
pub mod retention;
mod refused_runs;
mod routes;
pub mod run_origin;
/// Slice b, criterion 5: the Run page's gate resolution per step.
mod run_gate_view;
pub use routes::{route_table, RouteEntry};
pub mod secret_executor;
pub mod step_up;
/// `--role workspace`: the ADR-0061 workspace service. Its own router, its own
/// `/readyz`, and NO durable core — see the module docs.
pub mod workspaced;

pub use infra_observer::{live_fences, InfraObserver};
pub use log_tail::{pump_log_stream, LogTailer};
pub use logs::LogService;
pub use secret_executor::SecretInjectingExecutor;

/// The cold-tier Workspace-Snapshot retention default, in days — mirrors
/// `config::Config::retention_workspace_days`'s default so a hand-built
/// [`AppState`] and a booted server quote the same promise.
pub const DEFAULT_SNAPSHOT_RETENTION_DAYS: u32 = 14;

/// A wall-clock [`Clock`] for production wiring (tests inject `FakeClock`).
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    async fn now(&self) -> Timestamp {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Timestamp(ms)
    }
}

/// Shared handler state: the durable store, a clock, and the log pipeline.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<dyn Db>,
    pub clock: Arc<dyn Clock>,
    pub logs: Arc<LogService>,
    /// HMAC secret for verifying inbound GitHub webhooks (ADR-0032). `None`
    /// disables the ingest endpoint (rejects with 401).
    pub github_webhook_secret: Option<Vec<u8>>,
    /// HMAC secret for verifying inbound Forgejo webhooks (ADR-0046 — each
    /// forge endpoint binds its own secret). `None` disables `/webhooks/forgejo`.
    pub forgejo_webhook_secret: Option<Vec<u8>>,
    /// The ForgeConnection registry (ADR-0046): RepoRef→Project resolution,
    /// installation auto-registration, and the webhook delivery-id replay
    /// guard. `None` skips dedup/registration (dev without a registry).
    pub connections: Option<Arc<dyn scarab_forge::ForgeConnectionStore>>,
    /// The forge port used to read in-repo `.scarab` config on a trigger. `None`
    /// means the webhook ingest can verify+normalize but not start config-driven
    /// runs.
    pub forge: Option<Arc<dyn scarab_forge::ForgePort>>,
    /// Builds the adapter for a **specific connection** (ADR-0060) — what the
    /// onboarding endpoints need, because enumerating a credential's repos and
    /// registering a hook both apply to a connection with nothing bound yet, so
    /// `forge`'s repo-routed resolution has nothing to route on. `None` falls back
    /// to [`forge`](AppState::forge), which is what a test wiring a single fake
    /// adapter wants.
    pub forge_adapters: Option<Arc<dyn scarab_forge::ForgeAdapters>>,
    /// Login provider (OAuth/OIDC). `None` leaves `/v1/auth/login` disabled.
    pub auth: Option<Arc<dyn scarab_identity::Authenticator>>,
    /// Session store. When `None`, API authz is **disabled** (dev/test default —
    /// every request is allowed); when `Some`, run endpoints require a valid
    /// session with a sufficient role.
    pub sessions: Option<Arc<dyn scarab_identity::SessionStore>>,
    /// Issued API tokens (ADR-0049 amendment) — the credential a machine can
    /// hold. `None` leaves `/v1/orgs/{org}/tokens` disabled and makes any
    /// `scarab_pat_…` bearer a 401: an unwired store must refuse tokens, never
    /// fall through to treating one as a session id.
    pub api_tokens: Option<Arc<dyn scarab_identity::ApiTokenStore>>,
    /// The legacy per-Project environment store (ADR-0037), kept for the
    /// legacy approval history (`record_deployment` on every gate release and
    /// its read endpoint). Admission never reads it since ADR-0071 — see
    /// [`environment_governance`](AppState::environment_governance). `None`
    /// disables the history endpoint.
    pub environments: Option<Arc<dyn scarab_project::EnvironmentStore>>,
    /// Org-level Environment governance (ADR-0071 §2, §3, §8): what admits
    /// and governs every Deploy Run — eligibility, effective protection,
    /// privilege grants — at Run creation, at gate approval and at secret
    /// launch. `main.rs` always wires it. `None` is a hand-built state only (a
    /// test or the inline stack): deploys are then admitted as an undefined
    /// environment was before ADR-0071, and the per-repo environment reads
    /// answer 404.
    pub environment_governance: Option<Arc<dyn scarab_project::EnvironmentGovernanceStore>>,
    /// Stored Policies and their decisions (ADR-0075). Admission consults
    /// the `run.create` Policies through it. `main.rs` always wires it; `None`
    /// (a hand-built state) consults no Policies, as before ADR-0075.
    pub policies: Option<Arc<dyn scarab_policy::store::PolicyStore>>,
    /// Installed libraries, their versions and yanks (ADR-0076). Compiling
    /// resolves `lib:` targets through it, and admission, launch and gate
    /// release refuse yanked versions. `None` (a hand-built state) resolves
    /// no `lib:` target and disables `/v1/orgs/{org}/libraries` (404).
    pub libraries: Option<Arc<dyn scarab_policy::library::LibraryStore>>,
    /// Reads a library from a public forge (ADR-0076 §4). `None`: only
    /// installs through a ForgeConnection work.
    pub library_fetcher: Option<Arc<dyn library_fetch::AnonymousFetcher>>,
    /// The release manifest that says which library versions are
    /// first-party (ADR-0076 §5). Off unless wired.
    pub first_party: Arc<libraries::FirstParty>,
    /// Org-level Environments (ADR-0071, MMP step 1). `None` disables
    /// `/v1/orgs/{org}/environments[/{env}]` (404).
    pub org_environments: Option<Arc<dyn scarab_project::OrgEnvironmentStore>>,
    /// The PluginInstallation registry (ADR-0072). `None` disables the plugin
    /// endpoints (404) and the broker's authenticated routes.
    pub plugins: Option<Arc<dyn scarab_plugin::PluginStore>>,
    /// The same registry on the broker's **own small connection pool**
    /// (ADR-0072, red-team F-1): whatever plugins send, they can exhaust
    /// only that pool — answered 503 — never the one the API, the engine
    /// and an admin's kill switch use. `None` = the broker uses `plugins`
    /// (a hand-built state).
    pub broker_plugins: Option<Arc<dyn scarab_plugin::PluginStore>>,
    /// How many entries the CredentialBroker's journal queue holds between
    /// the read proxy and its writer (ADR-0072 §5). Full, the proxy refuses
    /// to forward (503) rather than forward what it cannot journal.
    pub broker_journal_queue: usize,
    /// Proves who calls the broker (ADR-0072 §11): TokenReview in production
    /// (slice d2), static dev tokens under `SCARAB_DEV_INSECURE`. `None` makes
    /// every authenticated broker call a 401 — nothing proven, nothing taken.
    pub plugin_tokens: Option<Arc<dyn scarab_plugin::PluginTokenVerifier>>,
    /// Proves which plugin calls the gate API (ADR-0076 §7): TokenReview for
    /// the audience `scarab-api` in production, static dev tokens under
    /// `SCARAB_DEV_INSECURE`. `None`: no plugin can list or resolve a gate.
    pub plugin_api_tokens: Option<Arc<dyn scarab_plugin::PluginTokenVerifier>>,
    /// Whether installs are on, the plugin namespace, the first-party
    /// catalog's repository and the dev tag knob. Off by default.
    pub plugin_policy: Arc<plugins::PluginPolicy>,
    /// "Intentionally unset" markers for the secret coverage matrix (ADR-0037 D).
    /// Advisory annotations only — `None` disables marking, and the matrix simply
    /// reports nothing silenced. Never consulted on a run path.
    pub secret_coverage: Option<Arc<dyn scarab_project::SecretCoverageStore>>,
    /// The OIDC issuer. When set, serves JWKS + discovery for keyless federation.
    pub oidc: Option<Arc<oidc::Rs256Issuer>>,
    /// HMAC secret for external-gate release tokens (ADR-0034). `None` disables
    /// the token-release endpoint (rejects with 404).
    pub gate_token_secret: Option<Vec<u8>>,
    /// Secret store (envelope-encrypted, ADR-0014). `None` disables the secrets
    /// management endpoints.
    pub secrets: Option<Arc<dyn scarab_secrets::SecretProvider>>,
    /// HMAC secret for the fence-scoped step-results ingest token (ADR-0042).
    /// `None` disables the results-ingest endpoint (rejects with 404). Shared with
    /// the k8s executor, which mints the per-step token the egress sidecar presents.
    pub results_token_secret: Option<Vec<u8>>,
    /// The artifact blob store (ADR-0052): serves downloads. `None` disables
    /// the artifact endpoints (404).
    pub artifact_store: Option<Arc<dyn scarab_storage::ObjectStore>>,
    /// The workspace content-addressed store (ADR-0029): reads a step's output
    /// snapshot for the read-only workspace browser. `None` disables the
    /// workspace-browse endpoints (404) — e.g. the local executor doesn't
    /// snapshot workspaces.
    pub workspace_cas: Option<Arc<dyn scarab_storage::Cas>>,
    /// The `Cas` handle the rerun-widening oracle answers over (git-bug 4afaa3e,
    /// amendment F1). Unlike `workspace_cas` — the Browse/GC read handle, whose
    /// tiered composition falls through to cold on ANY warm error — this one
    /// must let a warm-leg error PROPAGATE: the control plane's warm leg is the
    /// Depot (warm cache + pack index), so during a Depot outage a fall-through
    /// read consults only loose cold objects and answers a definitive NotFound
    /// for packed content — laundering an outage into "expired" and widening a
    /// rerun back to `clone`. With the error surfaced, `CasSnapshots`' "a
    /// transient error is not proof of expiry" guard assumes present instead.
    /// `None` = fall back to `workspace_cas` (correct where the two coincide,
    /// e.g. a cold-only deployment with no warm leg to launder).
    pub widening_cas: Option<Arc<dyn scarab_storage::Cas>>,
    /// The step attacher (debug shell): opens an interactive TTY into a running
    /// step's Pod. `None` disables the attach endpoint (404) — only the k8s
    /// executor can exec; the local executor runs no Pods.
    pub attacher: Option<Arc<dyn scarab_executor_k8s::StepAttacher>>,
    /// The debug-pod launcher: reproduces a finished step in a fresh ephemeral
    /// Pod (its image + re-materialized workspace snapshot) to shell into.
    /// `None` disables the debug-pod endpoint (404) — k8s executor only.
    pub debug_launcher: Option<Arc<dyn scarab_executor_k8s::DebugLauncher>>,
    /// The built SPA's dist directory (ADR-0054). `Some` serves the web UI at
    /// `/` with an SPA fallback — same-origin with the API, so no CORS layer
    /// exists or is needed. `None` (dev) leaves non-API paths 404.
    pub ui_dir: Option<std::path::PathBuf>,
    /// Scoped role bindings (ADR-0049 C2): the native RBAC model authorize()
    /// consults per request against the path's Org/Project. `None` = only the
    /// principal's flat global roles decide (the C1 bootstrap).
    pub rbac: Option<Arc<dyn scarab_identity::RbacStore>>,
    /// Principals and their pinned provider identities (ADR-0069). `Some` makes
    /// every admin-typed subject (a grant, an approver) resolve to a Principal
    /// that exists; `None` (dev/tests) leaves typed strings unchecked.
    pub identities: Option<Arc<dyn scarab_identity::IdentityStore>>,
    /// The configured login providers (ADR-0049, several since ADR-0069): the
    /// browser redirect + callback and the API/CLI exchange. Empty leaves only
    /// the credential-exchange `POST /v1/auth/login` through [`Self::auth`].
    pub oauth_providers: Vec<Arc<oauth::OAuthAuthenticator>>,
    /// Scarab's public base URL — where the outside world reaches this server.
    /// Two things are built from it, and **neither depends on login being on**:
    /// the OAuth callback `redirect_uri` (`{public_url}/v1/auth/callback`) and
    /// the forge-webhook callback every registered hook posts to
    /// (`{public_url}/webhooks/{forge}`, ADR-0046). Set it with
    /// [`with_public_url`](AppState::with_public_url); the dev default only
    /// works when the forge shares this host's loopback.
    pub public_url: String,
    /// Deployment-supplied forge-connection credentials (ADR-0060 part D): the
    /// env-override half of the one credential-resolution path. The API needs
    /// it so a connection whose credential comes from configuration is not
    /// reported as MISSING merely because it is absent from `SecretProvider`.
    /// Empty by default (every credential resolves from the secret store).
    pub credential_overrides: Arc<connections_config::CredentialOverrides>,
    /// How long a TERMINAL run's **Workspace Snapshots** are retained in the cold
    /// tier, in days (ADR-0061 s5 / ADR-0050) — the same number the GC sweeper
    /// runs on, surfaced so the API can *state the promise* instead of leaving it
    /// implicit in a sweeper's config. Defaults to the config default (14); set it
    /// from the boot config with
    /// [`with_snapshot_retention_days`](AppState::with_snapshot_retention_days)
    /// so the UI never quotes a different number than the sweeper enforces.
    pub snapshot_retention_days: u32,
    /// Step-up confirmation (ADR-0073). [`AppState::new`] leaves it
    /// [`Off`](step_up::StepUpPolicy::Off) — a hand-built state, or
    /// `SCARAB_DEV_INSECURE`; production wires
    /// [`with_step_up`](AppState::with_step_up) in `main.rs`.
    pub step_up: step_up::StepUpPolicy,
}

impl AppState {
    pub fn new(db: Arc<dyn Db>, clock: Arc<dyn Clock>, logs: Arc<LogService>) -> Self {
        Self {
            db,
            clock,
            logs,
            github_webhook_secret: None,
            forgejo_webhook_secret: None,
            connections: None,
            forge: None,
            forge_adapters: None,
            auth: None,
            sessions: None,
            api_tokens: None,
            environments: None,
            environment_governance: None,
            policies: None,
            libraries: None,
            library_fetcher: None,
            first_party: Arc::new(libraries::FirstParty::new(libraries::ManifestSource::Off)),
            org_environments: None,
            plugins: None,
            broker_plugins: None,
            broker_journal_queue: broker_proxy::JOURNAL_QUEUE,
            plugin_tokens: None,
            plugin_api_tokens: None,
            plugin_policy: Arc::new(plugins::PluginPolicy::off()),
            secret_coverage: None,
            oidc: None,
            gate_token_secret: None,
            secrets: None,
            results_token_secret: None,
            artifact_store: None,
            workspace_cas: None,
            widening_cas: None,
            attacher: None,
            debug_launcher: None,
            ui_dir: None,
            rbac: None,
            identities: None,
            oauth_providers: Vec::new(),
            public_url: "http://localhost:8080".into(),
            credential_overrides: Arc::new(connections_config::CredentialOverrides::new()),
            snapshot_retention_days: DEFAULT_SNAPSHOT_RETENTION_DAYS,
            step_up: step_up::StepUpPolicy::Off,
        }
    }

    /// Enforce step-up confirmation (ADR-0073): every crown-jewel route needs
    /// a passkey StepUpConfirmation, and the passkey ceremony endpoints are
    /// live.
    pub fn with_step_up(mut self, service: step_up::StepUpService) -> Self {
        self.step_up = step_up::StepUpPolicy::Enforce(Arc::new(service));
        self
    }

    /// The cold-tier Workspace-Snapshot retention window the sweeper actually
    /// enforces (ADR-0061 s5). Wire it from `config.retention_workspace_days` so
    /// what the UI promises and what GC does are one number.
    pub fn with_snapshot_retention_days(mut self, days: u32) -> Self {
        self.snapshot_retention_days = days;
        self
    }

    /// Deployment-supplied connection credentials (ADR-0060 part D): the
    /// env-override half of the one credential-resolution path, built at boot
    /// from the `connections:` block plus `SCARAB_GITHUB_APP_PEM[_FILE]`.
    pub fn with_credential_overrides(
        mut self,
        overrides: Arc<connections_config::CredentialOverrides>,
    ) -> Self {
        self.credential_overrides = overrides;
        self
    }

    /// Set the HMAC secret for external-gate release tokens (ADR-0034).
    pub fn with_gate_token_secret(mut self, secret: Vec<u8>) -> Self {
        self.gate_token_secret = Some(secret);
        self
    }

    /// Set the HMAC secret for fence-scoped step-results ingest tokens (ADR-0042).
    pub fn with_results_token_secret(mut self, secret: Vec<u8>) -> Self {
        self.results_token_secret = Some(secret);
        self
    }

    /// Enable the secrets management endpoints, backed by `secrets`.
    pub fn with_secrets(mut self, secrets: Arc<dyn scarab_secrets::SecretProvider>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Enable the OIDC issuer (JWKS + discovery endpoints).
    pub fn with_oidc(mut self, issuer: Arc<oidc::Rs256Issuer>) -> Self {
        self.oidc = Some(issuer);
        self
    }

    /// Enable the legacy approval history (ADR-0037, ADR-0070 §1).
    pub fn with_environments(
        mut self,
        environments: Arc<dyn scarab_project::EnvironmentStore>,
    ) -> Self {
        self.environments = Some(environments);
        self
    }

    /// Govern Deploy Runs by org-level Environments (ADR-0071): eligibility
    /// and effective protection at Run creation and gate approval, and the
    /// per-repo environment reads.
    pub fn with_environment_governance(
        mut self,
        governance: Arc<dyn scarab_project::EnvironmentGovernanceStore>,
    ) -> Self {
        self.environment_governance = Some(governance);
        self
    }

    /// Consult stored Policies at Run creation (ADR-0075).
    pub fn with_policies(mut self, policies: Arc<dyn scarab_policy::store::PolicyStore>) -> Self {
        self.policies = Some(policies);
        self
    }

    /// Resolve `lib:` targets and refuse yanked versions (ADR-0076).
    pub fn with_libraries(
        mut self,
        libraries: Arc<dyn scarab_policy::library::LibraryStore>,
    ) -> Self {
        self.libraries = Some(libraries);
        self
    }

    /// Fetch libraries from public forges (ADR-0076 §4).
    pub fn with_library_fetcher(
        mut self,
        fetcher: Arc<dyn library_fetch::AnonymousFetcher>,
    ) -> Self {
        self.library_fetcher = Some(fetcher);
        self
    }

    /// Where the first-party release manifest is read from (ADR-0076 §5).
    pub fn with_first_party(mut self, source: libraries::ManifestSource) -> Self {
        self.first_party = Arc::new(libraries::FirstParty::new(source));
        self
    }

    /// The stores Run admission reads ([`AdmissionPorts`]).
    pub fn admission_ports(&self) -> AdmissionPorts<'_> {
        AdmissionPorts {
            governance: self.environment_governance.as_deref(),
            policies: self.policies.as_deref(),
            libraries: self.libraries.as_deref(),
            plugins: self.plugins.as_deref(),
            links: self.actor_links(),
            connections: self.connections.as_deref(),
        }
    }

    /// The stores that link a webhook actor to a Principal ([`ActorLinks`]).
    pub(crate) fn actor_links(&self) -> ActorLinks<'_> {
        ActorLinks {
            connections: self.connections.as_deref(),
            identities: self.identities.as_deref(),
            providers: &self.oauth_providers,
        }
    }

    /// Enable the org-level Environment endpoints (ADR-0071).
    pub fn with_org_environments(
        mut self,
        org_environments: Arc<dyn scarab_project::OrgEnvironmentStore>,
    ) -> Self {
        self.org_environments = Some(org_environments);
        self
    }

    /// Enable the plugin registry and its endpoints (ADR-0072) under
    /// `policy`.
    pub fn with_plugins(
        mut self,
        plugins: Arc<dyn scarab_plugin::PluginStore>,
        policy: plugins::PluginPolicy,
    ) -> Self {
        self.plugins = Some(plugins);
        self.plugin_policy = Arc::new(policy);
        self
    }

    /// Serve the broker from `store` — the registry on the broker's own
    /// small pool — instead of [`Self::with_plugins`]'s.
    pub fn with_broker_plugin_store(mut self, store: Arc<dyn scarab_plugin::PluginStore>) -> Self {
        self.broker_plugins = Some(store);
        self
    }

    /// Size the CredentialBroker's journal queue (default
    /// [`broker_proxy::JOURNAL_QUEUE`]). A test makes it small to reach
    /// "journal full" without thousands of requests.
    pub fn with_broker_journal_queue(mut self, entries: usize) -> Self {
        self.broker_journal_queue = entries;
        self
    }

    /// Set how broker callers are verified (ADR-0072 §11).
    pub fn with_plugin_tokens(
        mut self,
        verifier: Arc<dyn scarab_plugin::PluginTokenVerifier>,
    ) -> Self {
        self.plugin_tokens = Some(verifier);
        self
    }

    /// Set how plugins calling the gate API are verified (ADR-0076 §7).
    pub fn with_plugin_api_tokens(
        mut self,
        verifier: Arc<dyn scarab_plugin::PluginTokenVerifier>,
    ) -> Self {
        self.plugin_api_tokens = Some(verifier);
        self
    }

    /// Enable "intentionally unset" coverage annotations (ADR-0037 D).
    pub fn with_secret_coverage(
        mut self,
        store: Arc<dyn scarab_project::SecretCoverageStore>,
    ) -> Self {
        self.secret_coverage = Some(store);
        self
    }

    /// Set the GitHub webhook HMAC secret (from `SCARAB_GITHUB_WEBHOOK_SECRET`).
    pub fn with_github_webhook_secret(mut self, secret: Vec<u8>) -> Self {
        self.github_webhook_secret = Some(secret);
        self
    }

    /// Set the Forgejo webhook HMAC secret (from `SCARAB_FORGEJO_WEBHOOK_SECRET`).
    pub fn with_forgejo_webhook_secret(mut self, secret: Vec<u8>) -> Self {
        self.forgejo_webhook_secret = Some(secret);
        self
    }

    /// Enable the ForgeConnection registry (dedup, resolution, auto-registration).
    pub fn with_forge_connections(
        mut self,
        connections: Arc<dyn scarab_forge::ForgeConnectionStore>,
    ) -> Self {
        self.connections = Some(connections);
        self
    }

    /// Set the forge port used to read in-repo config on a trigger.
    pub fn with_forge(mut self, forge: Arc<dyn scarab_forge::ForgePort>) -> Self {
        self.forge = Some(forge);
        self
    }

    /// Set the connection-scoped adapter factory (ADR-0060) used by the
    /// onboarding endpoints — repo enumeration and webhook registration on a
    /// connection that has no bindings yet.
    pub fn with_forge_adapters(mut self, adapters: Arc<dyn scarab_forge::ForgeAdapters>) -> Self {
        self.forge_adapters = Some(adapters);
        self
    }

    /// Enable login + session-based API authz.
    pub fn with_auth(
        mut self,
        auth: Arc<dyn scarab_identity::Authenticator>,
        sessions: Arc<dyn scarab_identity::SessionStore>,
    ) -> Self {
        self.auth = Some(auth);
        self.sessions = Some(sessions);
        self
    }

    /// Enable issued API tokens (ADR-0049 amendment): the mint/list/revoke
    /// endpoints, and `scarab_pat_…` as a bearer credential. Separate from
    /// [`with_auth`](AppState::with_auth) because a deployment may want browser
    /// login without ever issuing a machine credential.
    pub fn with_api_tokens(mut self, tokens: Arc<dyn scarab_identity::ApiTokenStore>) -> Self {
        self.api_tokens = Some(tokens);
        self
    }

    /// Serve the built web UI from `dir` (ADR-0054): `/` + SPA fallback.
    pub fn with_ui_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.ui_dir = Some(dir.into());
        self
    }

    /// Enable the artifact endpoints (ADR-0052), serving blobs from `store`.
    pub fn with_artifact_store(mut self, store: Arc<dyn scarab_storage::ObjectStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    /// Enable the read-only workspace browser, backed by the workspace CAS
    /// (ADR-0029). Serves a step's output snapshot tree + file bytes.
    pub fn with_workspace_cas(mut self, cas: Arc<dyn scarab_storage::Cas>) -> Self {
        self.workspace_cas = Some(cas);
        self
    }

    /// Give the rerun-widening oracle its own `Cas` handle — one whose warm-leg
    /// errors PROPAGATE instead of falling through to cold (git-bug 4afaa3e
    /// amendment F1: a Depot outage must never be laundered into "expired").
    /// Without this the oracle answers over `workspace_cas`.
    pub fn with_widening_cas(mut self, cas: Arc<dyn scarab_storage::Cas>) -> Self {
        self.widening_cas = Some(cas);
        self
    }

    /// Enable the debug-shell attach endpoint, backed by an executor that can
    /// exec into a running step's Pod (the k8s executor).
    pub fn with_attacher(mut self, attacher: Arc<dyn scarab_executor_k8s::StepAttacher>) -> Self {
        self.attacher = Some(attacher);
        self
    }

    /// Enable the debug-pod endpoint, backed by an executor that can reproduce a
    /// finished step in an ephemeral Pod (the k8s executor).
    pub fn with_debug_launcher(
        mut self,
        launcher: Arc<dyn scarab_executor_k8s::DebugLauncher>,
    ) -> Self {
        self.debug_launcher = Some(launcher);
        self
    }

    /// Enable scoped RBAC (ADR-0049 C2): authorize() consults these bindings
    /// per request against the path's Org/Project scope.
    pub fn with_rbac(mut self, rbac: Arc<dyn scarab_identity::RbacStore>) -> Self {
        self.rbac = Some(rbac);
        self
    }

    /// Resolve admin-typed subjects against the Principal store (ADR-0069 §3)
    /// and serve the principals typeahead.
    pub fn with_identities(mut self, store: Arc<dyn scarab_identity::IdentityStore>) -> Self {
        self.identities = Some(store);
        self
    }

    /// Set Scarab's public base URL (`SCARAB_PUBLIC_URL`) — the one place the
    /// server learns how the outside world reaches it.
    ///
    /// Deliberately **independent of authn**: it used to ride along on
    /// [`with_oauth_login`](AppState::with_oauth_login), so a deployment with
    /// login off silently kept the `http://localhost:8080` dev default and every
    /// webhook it registered was dead on arrival — the forge accepted the
    /// registration and then delivered to its own loopback. Wire this
    /// unconditionally.
    pub fn with_public_url(mut self, public_url: impl Into<String>) -> Self {
        self.public_url = public_url.into();
        self
    }

    /// Enable the browser OAuth login flow (ADR-0049): `GET /v1/auth/login`
    /// redirects to the provider; the callback lands on
    /// `{public_url}/v1/auth/callback`.
    ///
    /// Auth only — the public URL comes from
    /// [`with_public_url`](AppState::with_public_url), because webhook
    /// registration needs it whether or not login is configured.
    ///
    /// Call once per provider (ADR-0069); they share the one callback URL, the
    /// state cookie naming which provider the flow went to.
    pub fn with_oauth_login(mut self, login: Arc<oauth::OAuthAuthenticator>) -> Self {
        self.oauth_providers.push(login);
        self
    }

    /// The provider a login names, or the default when it names none
    /// (ADR-0069 §4): the only provider, else the `unqualified_ids` one. With
    /// several and no default, the caller must choose — guessing would sign
    /// someone into the wrong identity. `Ok(None)` = no OAuth provider at all.
    fn oauth_provider(
        &self,
        id: Option<&str>,
    ) -> Result<Option<&Arc<oauth::OAuthAuthenticator>>, ApiError> {
        if self.oauth_providers.is_empty() {
            return Ok(None);
        }
        match id.filter(|i| !i.is_empty()) {
            Some(id) => self
                .oauth_providers
                .iter()
                .find(|p| p.provider_id() == id)
                .map(Some)
                .ok_or_else(|| ApiError::BadRequest(format!("unknown login provider `{id}`"))),
            None if self.oauth_providers.len() == 1 => Ok(self.oauth_providers.first()),
            None => self
                .oauth_providers
                .iter()
                .find(|p| p.unqualified_ids())
                .map(Some)
                .ok_or_else(|| {
                    ApiError::BadRequest(
                        "several login providers are configured — name one with `provider`".into(),
                    )
                }),
        }
    }

    /// The login provider `subject` signs in again through (git-bug 584bd03),
    /// `None` when there is no OAuth provider or none can be told.
    ///
    /// A fresh login must be THIS Principal's: an unnamed login answers 400
    /// with several providers and no default, and with a default it would
    /// send a non-default user through the default provider — signing them
    /// in as someone else. So, among the configured providers:
    /// 1. the providers the identity store has pinned to `subject` — the one
    ///    its id is prefixed with if it is among them, else the first;
    /// 2. without a store (or a pinned configured one): the id's
    ///    `provider:` prefix (ADR-0069 §2 mints `provider:handle`);
    /// 3. else the default — a bare id is the `unqualified_ids` provider's.
    pub async fn login_provider_for(&self, subject: &str) -> Option<String> {
        let configured = |id: &str| self.oauth_providers.iter().any(|p| p.provider_id() == id);
        let prefix = subject
            .split_once(':')
            .map(|(p, _)| p)
            .filter(|p| configured(p));
        if let Some(store) = self.identities.as_ref() {
            // A store error falls through to the id's spelling: this only
            // picks where the login goes, never who it signs in as.
            let pinned: Vec<String> = store
                .identities_of(subject)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|i| i.provider)
                .filter(|p| configured(p))
                .collect();
            if let Some(p) = prefix.filter(|p| pinned.iter().any(|q| q == p)) {
                return Some(p.to_string());
            }
            if let Some(p) = pinned.into_iter().next() {
                return Some(p);
            }
        }
        if let Some(p) = prefix {
            return Some(p.to_string());
        }
        self.oauth_provider(None)
            .ok()
            .flatten()
            .map(|p| p.provider_id().to_string())
    }

    /// The `fresh_login_required` refusal's `login_url` for `subject`:
    /// [`FRESH_LOGIN_URL`] naming the provider it signs in with.
    pub async fn fresh_login_url(&self, subject: &str) -> String {
        match self.login_provider_for(subject).await {
            Some(p) => format!("{FRESH_LOGIN_URL}&provider={}", oauth::percent_encode(&p)),
            None => FRESH_LOGIN_URL.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// DTOs — the IR subset (ADR-0009). These carry the OpenAPI schema; the pure
// `scarab-pipeline` IR types cannot derive `ToSchema` (that would pull infra
// into a pure crate), so the server mirrors the subset and converts.
// ---------------------------------------------------------------------------

/// `POST /v1/runs` body: an inline pipeline to run immediately, plus any launch
/// parameters (ADR-0043) declared by the pipeline's `interface`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateRunRequest {
    pub pipeline: PipelineDto,
    /// Supplied launch parameters, `name → value` (ADR-0043). Resolved against
    /// `pipeline.interface.inputs` at creation: coerced to the declared types,
    /// defaults applied, validated fail-closed. Absent = none supplied.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub params: std::collections::BTreeMap<String, serde_json::Value>,
    /// Optional caller-supplied **reason** for this inline run (ADR-0057 §3),
    /// stamped as the run **Headline** (`trigger_title`) for this `api`-style
    /// dispatch. Accepted and stamped verbatim, no requiredness check. Absent =
    /// no headline.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The inline pipeline (IR subset).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PipelineDto {
    /// IR schema version (ADR-0022).
    pub ir_version: u32,
    /// The pipeline's launch/reuse interface (ADR-0038, 0043) — its declared
    /// typed parameters (`inputs`) and exposed outputs. Carried as the pure
    /// `scarab_pipeline::Interface` (opaque object in the schema; the pure crate
    /// cannot derive `ToSchema`). Absent = no parameters.
    #[serde(default, skip_serializing_if = "scarab_pipeline::Interface::is_empty")]
    #[schema(value_type = Object)]
    pub interface: scarab_pipeline::Interface,
    /// Opt-in run budget in seconds (ADR-0047): the run fails once its
    /// **active** time (gate-suspended time excluded) exceeds this. No default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u32>,
    pub steps: Vec<StepDto>,
}

/// One step (IR subset): the step contract is an OCI `image` + `command`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StepDto {
    pub id: String,
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Secret keys to resolve and inject at launch (ADR-0037).
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    /// Explicit input workspaces (ADR-0007): the subset of `needs` whose output
    /// workspace this step consumes. Absent = implicit-by-default (inherit every
    /// need's workspace). Naming a subset both restricts what flows in and
    /// sharpens restart invalidation — the skip-if-unchanged signature is then
    /// computed over exactly these inputs (mirrors `scarab_pipeline::StepSpec`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<String>>,
    /// Explicit output workspace paths (ADR-0007): the workspace-relative paths
    /// this step publishes downstream. Absent = the whole workspace. A declared
    /// path the step did not produce fails the step (fail-closed), so this is a
    /// contract, not a filter (mirrors `scarab_pipeline::StepSpec`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
    /// Privilege escalation requested above the hardened baseline (ADR-0039).
    /// On an inline API run only the self-service `run_as_root` grant is admitted
    /// (it stays inside the sandbox); governed grants (add-capabilities /
    /// privileged) require a target Environment and are rejected fail-closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub security: Option<scarab_pipeline::StepSecurity>,
    /// Opt-in retry policy `{on, max}` (ADR-0047). ⚠ At-least-once: retry
    /// re-runs the whole step at-least-once; enable only if the step is
    /// idempotent or fenced against a cooperating sink. Never-started infra
    /// failures auto-retry regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub retry: Option<scarab_pipeline::Retry>,
    /// Per-step execution deadline in seconds (ADR-0047). Absent = the global
    /// default (1h). Exceeding it is a `Timeout` failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
    /// PlacementProfile names this step runs on (ADR-0055); their admin-defined
    /// k8s overlays merge onto the Pod in listed order. Empty = the default profile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placement_profiles: Vec<String>,
    /// Requested compute resources (ADR-0055): exact `cpu_millis`/`memory_mib`.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub resources: scarab_pipeline::Resources,
    /// Governed raw pod-spec overlay (ADR-0055). Carries no authority; an inline
    /// API run targets no Environment, so any overlay is rejected fail-closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub k8s_overlay: Option<serde_json::Value>,
    /// Sidecar services (ADR-0058): throwaway backing containers co-located in
    /// this step's Pod, reachable at `localhost:<port>`, with an optional
    /// readiness probe gating the step's main container start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Object)]
    pub services: Vec<scarab_pipeline::ServiceSpec>,
    /// Shared-service opt-in (ADR-0058): names of pipeline-level shared services
    /// this step reaches over the network (DNS `<name>:<port>` + readiness gate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// NOT part of the inline IR subset: read only so [`create_run`] can
    /// refuse the field ADR-0074 removed with the Deployment record, naming
    /// its replacement, instead of silently dropping it. Never serialized
    /// into the stored IR; not in the schema.
    #[serde(default, skip_serializing)]
    #[schema(ignore)]
    pub deployment_requests: Option<serde_json::Value>,
    /// NOT part of the inline IR subset: read only so [`create_run`] can
    /// refuse a `gate: deployment` step (removed by ADR-0074). Never
    /// serialized; not in the schema.
    #[serde(default, skip_serializing)]
    #[schema(ignore)]
    pub gate: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreateRunResponse {
    pub id: String,
    pub status: String,
}

/// `POST /v1/repos/{org}/{repo}/dispatch` body: dispatch a **named** pipeline at
/// a `ref`, supplying its declared launch parameters (ADR-0043 "World B"). Unlike
/// the inline `POST /v1/runs` escape hatch, this rides the read-at-ref → compile
/// → admission machinery — a dispatched deploy hits Environment governance.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DispatchRequest {
    /// The ref to dispatch at (branch/tag/sha). Resolved to a concrete commit;
    /// the run pins to that commit.
    pub r#ref: String,
    /// The pipeline to run — a `.scarab` name (e.g. `deploy`) or a full
    /// `.scarab/*.yaml` path.
    pub pipeline: String,
    /// Supplied launch parameters, `name → value` (ADR-0043). Resolved against
    /// the pipeline's `interface.inputs` at the resolved commit: coerced,
    /// defaulted, validated fail-closed. Absent = none supplied.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub params: std::collections::BTreeMap<String, serde_json::Value>,
    /// Which trigger to dispatch: `manual` (default) or `api`. The pipeline must
    /// declare the matching `on:`.
    #[serde(default)]
    pub kind: DispatchKind,
    /// Optional operator-supplied **reason** for this dispatch (ADR-0057 §3),
    /// stamped as the run **Headline** (`trigger_title`). The endpoint accepts and
    /// stamps it verbatim — it performs **no** requiredness check; requiredness is
    /// an Environment `ProtectionRule` enforced at admission (thread D). Absent =
    /// no headline.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Query for the manual-dispatch catalog + interface describe endpoints
/// (ADR-0043 §4): the ref to read the config at. Resolved to a concrete commit
/// SHA, which is echoed back so the form and the eventual dispatch see
/// byte-identical config. Absent = `HEAD`.
#[derive(Debug, Deserialize)]
pub struct PipelineRefQuery {
    #[serde(default = "default_pipeline_ref")]
    pub r#ref: String,
}

fn default_pipeline_ref() -> String {
    "HEAD".to_string()
}

/// `GET /v1/repos/{org}/{repo}/pipelines?ref=` body: the manually-dispatchable
/// catalog at a ref (ADR-0043 §4). Lightweight — each entry is an `on:`-only
/// read, not a full compile. `sha` is the ref resolved to a concrete commit.
#[derive(Debug, Serialize, ToSchema)]
pub struct PipelineCatalogResponse {
    /// The ref resolved to a concrete commit — the catalog reflects this exact
    /// commit, and a subsequent describe/dispatch should pin to it.
    pub sha: String,
    /// Every `.scarab/*.{yaml,yml}` at the commit (excluding `.scarab/lib/**`
    /// and files marked `kind: library`), path-sorted.
    pub pipelines: Vec<CatalogEntry>,
}

/// One pipeline in the dispatch catalog: its selection `name` and whether it
/// opts into `manual` / `api` dispatch (its `on:` includes that trigger).
#[derive(Debug, Serialize, ToSchema)]
pub struct CatalogEntry {
    /// The bare selection name (e.g. `deploy`) — what a describe/dispatch call
    /// passes as `pipeline`.
    pub name: String,
    /// This pipeline declares `on: manual` — it appears in the human catalog.
    pub manual: bool,
    /// This pipeline declares `on: api` — its programmatic dispatch sibling.
    pub api: bool,
    /// Set when this single file failed to parse — the rest of the catalog still
    /// lists (a broken sibling does not fail the whole listing, ADR-0043 §4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Query for the ref picker (ADR-0046): an optional case-insensitive substring
/// the ref name must contain. Absent/blank returns every branch and tag.
#[derive(Debug, Deserialize)]
pub struct RefsQuery {
    #[serde(default)]
    pub q: Option<String>,
}

/// `GET /v1/repos/{org}/{repo}/refs?q=` body: the repo's branches and tags for a
/// searchable ref picker (ADR-0046). Scoped to branches + tags — recent commits
/// and open-PR head-refs are a deliberate follow-up, not part of v1.
#[derive(Debug, Serialize, ToSchema)]
pub struct RefsResponse {
    /// Branches first, then tags; each group name-sorted.
    pub refs: Vec<RefDto>,
}

/// One branch or tag: its kind, bare name, and the short SHA of the commit it
/// points at (a resolved-SHA hint for the picker row). The full SHA is
/// intentionally omitted — selecting a ref re-resolves it through the pipelines
/// endpoint, which is the authority on the pinned commit.
#[derive(Debug, Serialize, ToSchema)]
pub struct RefDto {
    /// `branch` or `tag`.
    pub kind: String,
    /// The bare ref name (no `refs/{heads,tags}/` prefix), e.g. `main`, `v0.3.1`.
    pub name: String,
    /// The 7-char short SHA of the commit the ref points at.
    pub short_sha: String,
}

/// `GET /v1/repos/{org}/{repo}/pipelines/{name}/interface?ref=` body: the
/// compiled, typed launch-parameter schema for ONE selected pipeline (ADR-0043
/// §4). A pure function of the compiled IR — the same compile path dispatch
/// rides — so the form and the run validate against byte-identical specs.
#[derive(Debug, Serialize, ToSchema)]
pub struct PipelineInterfaceResponse {
    /// The ref resolved to a concrete commit — the interface reflects this exact
    /// commit; a subsequent dispatch should pin to it.
    pub sha: String,
    /// This pipeline opts into `manual` dispatch (`on: manual`).
    pub manual: bool,
    /// This pipeline opts into `api` dispatch (`on: api`).
    pub api: bool,
    /// The declared typed launch parameters (name, type, required, default,
    /// options, validate, description) — the compiled `interface.inputs`.
    #[schema(value_type = Vec<Object>)]
    pub inputs: Vec<scarab_pipeline::ParamSpec>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RunStatusResponse {
    pub id: String,
    pub status: String,
    /// The **run number** (ADR-0057 amendment) — the per-repo sequential `#N`,
    /// the human handle shown in the run-detail breadcrumb + gutter. Absent for
    /// untenanted inline runs and pre-allocation runs. Distinct from `id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_number: Option<i64>,
    pub steps: Vec<StepStatusDto>,
    /// The run's frozen launch parameters, `name → typed value` (ADR-0043 §5).
    /// A run-level constant resolved once at creation; non-secret by contract
    /// (§6), so safe to expose. Empty when the run took none. Lets the UI's
    /// re-run flow pre-fill the form from the prior run's params.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    #[schema(value_type = Object)]
    pub params: std::collections::BTreeMap<String, serde_json::Value>,
    /// The pipeline this run executed (the bare `.scarab/<name>` selection).
    /// Absent for inline runs and runs created before pipeline-stamping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    /// The run **Headline** (ADR-0057) — the one human line saying what this run
    /// is about (a push's commit subject; later a PR title / dispatch reason),
    /// disambiguated by the trigger kind. Display/audit only; absent when the
    /// trigger carried no headline and on runs created before headline-stamping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_title: Option<String>,
    /// The PR **base** branch (ADR-0057) — the branch a `pull_request` run
    /// targets, rendered `base ← head` in the ref cluster. A discrete origin
    /// fact; absent for non-PR runs and runs created before base-stamping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_pr_base: Option<String>,
    /// This run's **Workspace Snapshot** retention promise (ADR-0061 s5) — the
    /// cold tier's time bound, made explicit rather than left implicit in a
    /// sweeper's config, plus any manual pin over it.
    pub snapshot_retention: SnapshotRetentionDto,
    /// The library versions this Run uses that are yanked in its org now
    /// (ADR-0076 §6). A Rerun or Retry of a Step from one is refused at
    /// launch: it reuses the Steps the Run was compiled with. Empty when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub yanked_libraries: Vec<run_origin::RunYankDto>,
}

/// The cold-tier retention promise over one Run's **Workspace Snapshots**
/// (ADR-0061 s5).
///
/// ADR-0061 gives Workspace Snapshots two tiers with two policies. The warm
/// workspace service is bounded by **space** (LRU) and promises nothing — a miss
/// there is slower, never wrong. Object storage is the cold tier, bounded by
/// **time**, and *that* is the guarantee a user is given. Everything in this DTO
/// describes the cold tier only; nothing here can or should say anything about
/// the warm tier.
#[derive(Debug, Serialize, ToSchema)]
pub struct SnapshotRetentionDto {
    /// The retention window in days — what the GC sweeper actually enforces.
    pub retention_days: u32,
    /// When this run's Workspace Snapshots stop being promised (epoch millis).
    /// `None` while the run is **non-terminal**: a run that has not settled —
    /// including one suspended on a gate for weeks — is never GC-eligible
    /// regardless of age (ADR-0050), so no expiry applies yet. Also `None` while
    /// **pinned**, because a pin holds the window open indefinitely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// The promise has **lapsed**: this run is terminal, unpinned, and past its
    /// window, so its snapshots may already have been swept. Not the same as
    /// "gone" — GC is periodic and content is shared, so data often outlives the
    /// promise. The honest word for it is *expired*, not *deleted*, and a rerun
    /// resolves the difference by checking the store.
    pub expired: bool,
    /// A human **pinned** this run's Workspace Snapshots — hold them past the
    /// window for an investigation.
    pub pinned: bool,
    /// Who pinned it (`None` when unpinned, or when auth is off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_by: Option<String>,
    /// When it was pinned, epoch millis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<i64>,
}

/// Build the retention promise for a run from its durable facts plus the
/// deployment's window. Pure, so the UI copy and the sweeper's behaviour cannot
/// drift apart by arithmetic.
fn snapshot_retention_dto(
    r: &scarab_engine::SnapshotRetention,
    retention_days: u32,
    now: Timestamp,
) -> SnapshotRetentionDto {
    let pinned = r.pinned_at.is_some();
    let window_ms = (retention_days as i64) * 24 * 60 * 60 * 1000;
    // Only a settled, unpinned run is on the clock at all.
    let expires_at = (!pinned && r.terminal).then(|| r.settled_at.0 + window_ms);
    SnapshotRetentionDto {
        retention_days,
        expires_at,
        expired: expires_at.is_some_and(|e| now.0 >= e),
        pinned,
        pinned_by: r.pinned_by.clone(),
        pinned_at: r.pinned_at.map(|t| t.0),
    }
}

/// `GET /v1/runs` body: the most recent runs, newest first.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunListResponse {
    pub runs: Vec<RunSummaryDto>,
}

/// One run in the list view: identity, status, creation time (epoch millis),
/// and — when the run was stamped at creation (ADR-0049) — its owning tenant.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunSummaryDto {
    pub id: String,
    pub status: String,
    pub created_at: i64,
    /// Run duration in millis: `updated_at - created_at`. For a terminal run
    /// this is its total wall time; for an in-flight run, elapsed-to-last-update.
    /// Drives the dashboard's per-run bar heights (ADR-0046).
    pub duration_ms: i64,
    /// The owning org, if the run is tenanted (trigger-created).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    /// The owning project (repo name), if tenanted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The **run number** (ADR-0057 amendment) — the per-repo sequential `#N`,
    /// the human handle shown in the run-list gutter. Absent for untenanted
    /// inline runs and runs created before run-number allocation. Distinct from
    /// the opaque, unguessable `id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_number: Option<i64>,
    /// The run's **origin** — the trigger facts it was born from, each stamped at
    /// creation and independently nullable (sparse across trigger kinds; all
    /// absent on runs created before origin-stamping). The trigger kind
    /// (`push`/`pull_request`/`tag`/…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_kind: Option<String>,
    /// The **Actor** login — who caused the trigger (UI labels it "author").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The symbolic branch/tag ref the run ran on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// The resolved commit the run pinned to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The pull-request number, for `pull_request` runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<i64>,
    /// The PR **base** branch (ADR-0057) — the branch a `pull_request` run
    /// targets, rendered `base ← head` in the ref cluster. A discrete origin
    /// fact; absent for non-PR runs and runs created before base-stamping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_pr_base: Option<String>,
    /// The pipeline this run executed (the bare `.scarab/<name>` selection).
    /// Absent for inline runs and runs created before pipeline-stamping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    /// The run **Headline** (ADR-0057) — the one human line saying what this run
    /// is about (a push's commit subject; later a PR title / dispatch reason),
    /// disambiguated by the trigger kind. Display/audit only; absent when the
    /// trigger carried no headline and on runs created before headline-stamping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_title: Option<String>,
    /// The Environment this run deploys into (ADR-0037) — present only for a
    /// deploy run, whose pipeline declared `environment:`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Whether this is a **deploy Run** (ADR-0074 §2): its pipeline, or one of
    /// its Steps, is marked `deploy: true`. Independent of `environment`.
    pub deploy: bool,
    /// The run's **waiting gates** (git-bug `c489346`): gate steps that are
    /// `Pending`, reached (every need `Succeeded`) and have an open Attempt —
    /// the engine's "waiting gate". Since a waiting gate holds only its
    /// dependents (ADR-0070 §9), a Run with one can still be `running` while
    /// independent steps run, so this — not `status == suspended` — is what
    /// says a Run waits on a gate. Omitted when empty (and always for a
    /// terminal or queued Run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_gates: Vec<WaitingGateDto>,
}

/// One waiting gate of a run in the list view ([`RunSummaryDto::waiting_gates`]).
#[derive(Debug, Serialize, ToSchema)]
pub struct WaitingGateDto {
    /// The gate step's id.
    pub step: String,
    /// The gate kind: `manual` / `timer` / `external`.
    pub kind: String,
    /// When its wait opened (epoch millis): its open Attempt's start.
    pub since: i64,
}

/// The list view's runs, each with its waiting gates. Steps are read only for
/// a `Running` or `Suspended` run — the only ones that can have a waiting gate.
async fn run_summaries(
    st: &AppState,
    runs: Vec<scarab_engine::RunSummary>,
) -> Result<Vec<RunSummaryDto>, ApiError> {
    // Only an active run can wait on a gate. Their steps are read in ONE
    // batched query for the page (review fit-F8): the list endpoints are
    // polled, and one `steps_of_run` per active run grew with the page.
    let active: Vec<RunId> = runs
        .iter()
        .filter(|r| matches!(r.status, RunStatus::Running | RunStatus::Suspended))
        .map(|r| r.run.clone())
        .collect();
    let mut steps_by_run = if active.is_empty() {
        std::collections::HashMap::new()
    } else {
        st.db.steps_of_runs(&active).await?
    };
    let mut out = Vec::with_capacity(runs.len());
    for r in runs {
        let active = matches!(r.status, RunStatus::Running | RunStatus::Suspended);
        let run = r.run.clone();
        let mut dto = RunSummaryDto::from(r);
        if active {
            let steps = steps_by_run.remove(&run).unwrap_or_default();
            dto.waiting_gates = run_gate_view::waiting_gates(&steps)
                .into_iter()
                .filter_map(|s| {
                    Some(WaitingGateDto {
                        step: s.step.0.clone(),
                        kind: s.gate_kind.clone()?,
                        since: s.attempts.last()?.started_at.0,
                    })
                })
                .collect();
        }
        out.push(dto);
    }
    Ok(out)
}

impl From<scarab_engine::RunSummary> for RunSummaryDto {
    fn from(s: scarab_engine::RunSummary) -> Self {
        RunSummaryDto {
            id: s.run.0,
            status: run_status_name(s.status).to_string(),
            created_at: s.created_at.0,
            // Total wall time for a terminal run; for an in-flight run this is
            // elapsed-to-last-transition (the UI shows a live ticking elapsed
            // for running runs instead of trusting this frozen value).
            duration_ms: (s.updated_at.0 - s.created_at.0).max(0),
            org: s.tenant.as_ref().map(|(o, _)| o.clone()),
            project: s.tenant.as_ref().map(|(_, p)| p.clone()),
            run_number: s.run_number,
            trigger_kind: s.trigger_kind,
            actor: s.actor,
            git_ref: s.git_ref,
            sha: s.sha,
            pr_number: s.pr_number,
            origin_pr_base: s.pr_base,
            pipeline: s.pipeline,
            trigger_title: s.trigger_title,
            deploy: s.deploy,
            environment: s.environment,
            waiting_gates: Vec::new(),
        }
    }
}

/// `POST /v1/secrets` body: define (or overwrite) a secret at a scope. The
/// `value` is **write-only** — no endpoint ever returns it.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PutSecretRequest {
    /// Owning org (required).
    pub org: String,
    /// Repo, for a repo- or environment-scoped secret.
    #[serde(default)]
    pub repo: Option<String>,
    /// Environment, for an environment-scoped secret (requires `repo`).
    #[serde(default)]
    pub environment: Option<String>,
    /// Secret name (the key steps reference).
    pub name: String,
    /// Secret value — stored envelope-encrypted, never returned.
    pub value: String,
}

/// Scope selector for listing/deleting secrets (query params).
#[derive(Debug, Deserialize)]
pub struct SecretScopeQuery {
    pub org: String,
    pub repo: Option<String>,
    pub environment: Option<String>,
    /// Secret name — required for delete, ignored for list.
    pub name: Option<String>,
}

/// `GET /v1/secrets` body: the secret **names** at a scope. Values are never
/// listed.
#[derive(Debug, Serialize, ToSchema)]
pub struct SecretListResponse {
    pub names: Vec<String>,
}

/// The column id of the repo-scope default in the coverage matrix (ADR-0060).
///
/// A reserved id rather than a separate field, so a row is one flat
/// `column -> status` map that the UI can index uniformly. An environment named
/// `""` is impossible, so it cannot collide.
pub const REPO_DEFAULT_COLUMN: &str = "";

/// `GET /v1/repos/{org}/{repo}/secrets/matrix` body: the advisory coverage view
/// (ADR-0037 D) and — since ADR-0060 — the model behind the *editor* for repo-
/// and environment-scoped values. For each key, its **effective** status per
/// column after inheritance; never a value.
#[derive(Debug, Serialize, ToSchema)]
pub struct SecretMatrix {
    /// Column ids in render order: [`REPO_DEFAULT_COLUMN`] first (the repo-scope
    /// default that the environments fall through to), then each environment.
    pub columns: Vec<String>,
    /// The repo's environments, in column order. A subset of `columns` — kept
    /// separate so a client can tell an environment column from the repo one
    /// without reasoning about the reserved id.
    pub environments: Vec<String>,
    pub keys: Vec<SecretMatrixRow>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SecretMatrixRow {
    pub key: String,
    /// `column id -> "set" | "inherited" | "unset" | "silenced"`.
    ///
    /// `set` = a value lives at exactly that scope · `inherited` = none here, but
    /// it resolves from a broader scope · `unset` = resolves to nothing ·
    /// `silenced` = unset **on purpose** (an ADR-0037 marker). Only a genuinely
    /// unset cell can be silenced: a marker never hides a real value.
    pub status: std::collections::BTreeMap<String, String>,
    /// For each `inherited` cell, the scope it resolves from — `"repo"` or
    /// `"org"`. Lets a cell say *what* it would be overriding, so an edit reads
    /// as "override the repo default" rather than an unexplained write.
    pub inherited_from: std::collections::BTreeMap<String, String>,
}

/// `PUT …/secrets/matrix/silenced` body — and, as a query, the `DELETE`
/// selector: the one cell to annotate. Omitting `environment` addresses the
/// repo-scope default column. The Project is in the path, so unlike
/// [`SecretScopeQuery`] this carries no org/repo.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SilenceCellRequest {
    pub key: String,
    #[serde(default)]
    pub environment: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StepStatusDto {
    pub id: String,
    pub status: String,
    pub attempts: usize,
    /// Per-attempt detail (ADR-0047) — the rerun/retry history in append order.
    /// `attempts` is the count; this is the list, so the UI can show retries (a
    /// failed attempt followed by a succeeding one — the durable-execution story)
    /// and fold a step's logs per attempt. Empty until the step first launches.
    #[serde(default)]
    pub attempt_list: Vec<AttemptDto>,
    /// Upstream step ids this step depends on — the DAG in-edges (ADR-0006). The
    /// UI folds these into the run's graph view.
    #[serde(default)]
    pub needs: Vec<String>,
    /// `manual`/`timer`/`external` if this step is a gate (ADR-0008), else
    /// absent. Gates launch no pod; a waiting
    /// gate holds only the steps that need it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// How this gate's latest wait resolved — its latest `GateResolved` in
    /// the run's journal, never re-derived. Absent for a step that is not a
    /// gate or a gate that has never resolved. A gate re-armed since (a
    /// retry or rerun) keeps showing the old verdict here; compare `attempt`
    /// with the latest of `attempt_list` to tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_resolution: Option<GateResolutionDto>,
    /// Co-located sidecar services (ADR-0058) this step declares — rendered as
    /// sidecar nodes attached to the step, each with its own log stream (fetched
    /// via `?sidecar=<index>` on the step-logs endpoint). Empty for a step with
    /// no `services:`.
    #[serde(default)]
    pub services: Vec<StepServiceDto>,
    /// Names of pipeline-level shared services this step opts into via `uses:`
    /// (ADR-0058) — the DAG's service edges. Empty for a step that uses none.
    #[serde(default)]
    pub uses: Vec<String>,
    /// This Step is marked `deploy: true` (ADR-0074 §2) — the Step of a deploy
    /// Run that deploys. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deploy: bool,
    /// The invoke chain this Step was inlined through, outermost first
    /// (ADR-0076): the `origin` the compiler recorded, each library version
    /// marked when it is yanked in the Run's org. Empty for a Step written in
    /// the Pipeline file itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origin: Vec<run_origin::OriginLinkDto>,
    /// For a `gate: plugin` node (ADR-0076 §7): who releases it, its expiry,
    /// its rendered inputs and its typed outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_gate: Option<run_origin::PluginGateViewDto>,
}

/// How a gate's wait resolved: its `GateResolved` journal entry.
#[derive(Debug, Serialize, ToSchema)]
pub struct GateResolutionDto {
    /// `true`: the gate passed and its dependents may run.
    pub passed: bool,
    /// Why — a timer elapsing, an expiry, a rejection's reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Unix-ms the verdict was journalled.
    pub at: i64,
    /// The gate Attempt the verdict closed (`a1`, `a2`, …).
    pub attempt: String,
    /// Who rejected it, for a rejection by request (`external:{step}` for an
    /// external gate's token). Absent for a verdict the engine reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// One of a step's co-located **sidecar services** (ADR-0058) — a throwaway
/// backing container in the step's own Pod, reachable at `localhost:<port>`.
/// Positional: identified by its `index` in the step's authored `services:`,
/// which is also its container ordinal (`service-{index}`) and the `?sidecar=`
/// selector for its logs. Not a `needs`-able DAG node.
#[derive(Debug, Serialize, ToSchema)]
pub struct StepServiceDto {
    /// Index in the step's `services:` list — the log selector and container ord.
    pub index: usize,
    /// The service OCI image (e.g. `postgres:16`).
    pub image: String,
    /// Container ports the service listens on (all reachable at `localhost:<p>`).
    #[serde(default)]
    pub ports: Vec<u16>,
}

/// One attempt at executing a step (ADR-0047) — the rerun unit. A step with more
/// than one attempt is one that failed-and-retried (or was restarted); the last
/// attempt carries the current outcome.
#[derive(Debug, Serialize, ToSchema)]
pub struct AttemptDto {
    pub id: String,
    /// When this attempt started (unix-ms).
    pub started_at: i64,
    /// `true` if this attempt ended in a classified failure. A later attempt may
    /// still have succeeded — that divergence is exactly the retry story worth
    /// showing. Retained for back-compat; prefer `outcome` for the full picture
    /// (a `superseded`/`cancelled` attempt is `failed:false` but NOT green).
    pub failed: bool,
    /// Coarse failure kind when `failed`: `infra` | `step` | `timeout` | `lost`
    /// | `config` | `missing-inputs` (an input snapshot gone from every tier —
    /// Rerun/Retry regenerates it; ticket e140121) | `gate` (a gate's wait
    /// resolved failed — e.g. not released within `gate_expires_after`; the
    /// reason is in `failure_detail`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// The executor's human-readable cause for a failed attempt (ticket
    /// 4cf03d7) — e.g. "cold tier refused: connection refused" — alongside the
    /// machine-consumed `failure` class. Absent = the class is the whole story
    /// (or a pre-0041 row). Stored since migration 0041.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    /// The attempt's recorded outcome (ADR-0056 amendment): `running` |
    /// `succeeded` | `failed` | `superseded` | `cancelled`. Unlike `failed` this
    /// distinguishes a *superseded* attempt (a rerun/retry replaced its input
    /// while it was still running and its Pod was torn down) from a genuine
    /// success — so the UI never renders an abandoned attempt green.
    pub outcome: String,
}

/// Project a durable [`scarab_engine::Attempt`] to its wire DTO.
fn attempt_dto(a: &scarab_engine::Attempt) -> AttemptDto {
    let failure = a.failure.as_ref().map(|f| {
        match f {
            scarab_engine::FailureKind::Infra { .. } => "infra",
            scarab_engine::FailureKind::Step => "step",
            scarab_engine::FailureKind::Timeout => "timeout",
            scarab_engine::FailureKind::Lost => "lost",
            scarab_engine::FailureKind::Config => "config",
            scarab_engine::FailureKind::MissingInputs => "missing-inputs",
            scarab_engine::FailureKind::Gate => "gate",
        }
        .to_string()
    });
    AttemptDto {
        id: a.id.0.clone(),
        started_at: a.started_at.0,
        failed: a.failure.is_some(),
        failure,
        failure_detail: a.failure_detail.clone(),
        // The durable read (storage boundary) already resolved back-compat, so
        // `a.outcome` is authoritative here.
        outcome: a.outcome.as_str().to_string(),
    }
}

/// A **shared service** instance of a run (ADR-0058) — the evidence a shared
/// service exists and its lifecycle state. A shared service is NOT a DAG node
/// (it has no `needs`, no rerun action), so it is surfaced in a Services panel
/// beside the DAG, never inside it. Keyed `{run, take, name}`; a Rerun's new
/// Take is a fresh instance.
#[derive(Debug, Serialize, ToSchema)]
pub struct ServiceStatusDto {
    /// The declared service name — also its cluster DNS hostname (`<name>:<port>`).
    pub name: String,
    /// Lifecycle: `starting` | `ready` | `running` | `torn-down` | `failed`.
    pub status: String,
    /// The Take generation this instance belongs to (a Rerun opens a new one).
    pub take: i64,
    /// When the instance was born (unix-ms).
    pub created_at: i64,
}

/// Project a durable [`scarab_engine::RunService`] to its wire DTO.
fn service_status_dto(s: &scarab_engine::RunService) -> ServiceStatusDto {
    ServiceStatusDto {
        name: s.name.clone(),
        status: s.status.as_str().to_string(),
        take: s.take,
        created_at: s.created_at.0,
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

pub enum ApiError {
    NotFound,
    Unauthorized,
    Forbidden,
    BadRequest(String),
    /// The request conflicts with the resource's current state (e.g. rerunning a
    /// step whose dependency has not succeeded, or retrying a non-failed step).
    Conflict(String),
    /// The request is well-formed but the capability does not exist here — e.g. a
    /// forge adapter that cannot enumerate its repos (ADR-0060). Distinct from
    /// `NotFound` (wrong resource) and `BadRequest` (wrong request).
    NotImplemented(String),
    /// Refused for a reason the caller should read — not a missing role
    /// (that is [`ApiError::Forbidden`]) but a policy: a third-party plugin,
    /// a killed installation (ADR-0072).
    Denied(String),
    /// The resource used to exist here and was retired on purpose; the text
    /// says where it went (410). The per-repo environment writes, replaced by
    /// the org-level Environment (ADR-0071).
    Gone(String),
    Db(DbError),
    /// `403 step_up_required` (ADR-0073 §5): a crown-jewel operation without
    /// a valid StepUpConfirmation. JSON body: [`step_up::StepUpRefusal`].
    StepUpRequired(Box<step_up::StepUpRefusal>),
    /// `403 fresh_login_required` (ADR-0073 §2): a FIRST passkey can be
    /// enrolled only from a browser login made in the last 5 minutes.
    /// `login_url` is that login, naming the caller's provider
    /// ([`AppState::fresh_login_url`]).
    FreshLoginRequired {
        login_url: String,
    },
    /// A passkey / step-up ceremony was refused; `code` names the check.
    /// JSON body `{code, message}`.
    Ceremony {
        status: StatusCode,
        code: &'static str,
        message: String,
    },
    /// Too many of something pending (429) — a requester's live
    /// StepUpConfirmationRequests (ADR-0073 §6). The text says what.
    TooManyRequests(String),
    /// A dependency that must work for the request to be safe is down (503):
    /// a crown jewel whose authorisation cannot be recorded does not run.
    Unavailable(String),
    /// A Pipeline that does not compile (400), with each error's fields.
    /// JSON body: [`CompileErrorResponse`].
    Compile(Box<CompileErrorResponse>),
    /// A log body read failed while the index says chunks exist — the bytes are
    /// gone or unreachable, and the reader must be told rather than handed a
    /// plausible blank pane (ADR-0063 part 5: absence is authoritative only when
    /// the index records no chunks).
    LogRead(crate::logs::LogError),
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        ApiError::Db(e)
    }
}

impl From<crate::logs::LogError> for ApiError {
    fn from(e: crate::logs::LogError) -> Self {
        ApiError::LogRead(e)
    }
}

impl From<DispatchError> for ApiError {
    fn from(e: DispatchError) -> Self {
        match e {
            // A DB failure is a 500; everything else is a caller-facing 4xx
            // (fail-closed, no run created).
            DispatchError::Db(d) => ApiError::Db(d),
            DispatchError::PipelineNotFound(_) => ApiError::NotFound,
            // ADR-0071 §2, §10: the caller may dispatch, but the Environment
            // does not let this Project deploy — a policy refusal (403) with
            // the actionable text, not a malformed request.
            refused @ (DispatchError::UnknownEnvironment { .. }
            | DispatchError::NotEligible { .. }
            | DispatchError::PolicyDenied { .. }
            | DispatchError::LibraryYanked(_)
            | DispatchError::ForgeToken(_)) => ApiError::Denied(refused.to_string()),
            other => ApiError::BadRequest(other.to_string()),
        }
    }
}

/// Where a `fresh_login_required` refusal sends the browser: a fresh login
/// that lands on passkey enrollment (ADR-0073 §2, §10). The refusal adds the
/// caller's `provider` ([`AppState::fresh_login_url`]).
pub const FRESH_LOGIN_URL: &str = "/v1/auth/login?next=/account/security%3Fenroll%3D1";

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            ApiError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "authentication required").into_response()
            }
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "insufficient role").into_response(),
            ApiError::Denied(m) => (StatusCode::FORBIDDEN, m).into_response(),
            ApiError::Gone(m) => (StatusCode::GONE, m).into_response(),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m).into_response(),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m).into_response(),
            ApiError::NotImplemented(m) => (StatusCode::NOT_IMPLEMENTED, m).into_response(),
            ApiError::Db(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response()
            }
            ApiError::LogRead(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("log storage read failed: {e}"),
            )
                .into_response(),
            ApiError::StepUpRequired(refusal) => refusal.into_response(),
            ApiError::FreshLoginRequired { login_url } => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "code": "fresh_login_required",
                    "message": "Log in again to enroll your first passkey (within 5 minutes of \
                                signing in).",
                    "login_url": login_url,
                })),
            )
                .into_response(),
            ApiError::Ceremony {
                status,
                code,
                message,
            } => (
                status,
                Json(serde_json::json!({ "code": code, "message": message })),
            )
                .into_response(),
            ApiError::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m).into_response(),
            ApiError::Compile(body) => (StatusCode::BAD_REQUEST, Json(*body)).into_response(),
            ApiError::TooManyRequests(m) => (StatusCode::TOO_MANY_REQUESTS, m).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Create and durably persist a new run from an inline pipeline.
#[utoipa::path(
    post,
    path = "/v1/runs",
    request_body = CreateRunRequest,
    responses((status = 201, description = "Run created", body = CreateRunResponse))
)]
async fn create_run(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateRunRequest>,
) -> Result<(StatusCode, Json<CreateRunResponse>), ApiError> {
    authorize(&st, &headers, Action::Write).await?;
    // No `run.create` Policy is consulted (ADR-0075): an inline Run names no org or Project.

    // Resolve launch parameters against the declared interface BEFORE creating
    // the run (ADR-0043): coerce to declared types, apply defaults, validate
    // fail-closed, reject unknown/missing. A bad supply is a 400 with a
    // per-parameter message and creates **no** run.
    let resolved = scarab_pipeline::params::resolve_params(&req.pipeline.interface, &req.params)
        .map_err(|e| ApiError::BadRequest(format!("invalid launch parameters: {e}")))?;

    // ADR-0074 removed DeploymentRequests and DeploymentGates with the
    // Deployment record: refuse them with the replacement, before any run
    // exists, rather than drop them silently.
    for step in &req.pipeline.steps {
        let requests = step
            .deployment_requests
            .as_ref()
            .is_some_and(|v| !(v.is_null() || v.as_array().is_some_and(Vec::is_empty)));
        let deployment_gate = step.gate.as_ref().and_then(|g| g.as_str()) == Some("deployment");
        if requests || deployment_gate {
            return Err(ApiError::BadRequest(format!(
                "step `{}`: {}",
                step.id,
                scarab_pipeline::RETIRED_DEPLOYMENTS
            )));
        }
    }

    // Same retry bound the YAML compiler enforces (ADR-0047): a liveness bound,
    // rejected before any run exists.
    for step in &req.pipeline.steps {
        if let Some(retry) = &step.retry {
            if !(1..=10).contains(&retry.max) {
                return Err(ApiError::BadRequest(format!(
                    "step `{}`: `retry.max` must be between 1 and 10 (got {}) — retry re-runs \
                     the whole step at-least-once; enable only if the step is idempotent or \
                     fenced against a cooperating sink",
                    step.id, retry.max
                )));
            }
        }
    }

    // Admit + translate EVERY step before persisting anything: a request the
    // API rejects (4xx) must create **no** run. Admitting inside the persist
    // loop used to leave a rejected multi-step request half-created — its
    // already-persisted steps schedulable despite the caller's 400.
    let mut admitted_steps: Vec<(StepId, StepSpec, Vec<StepId>)> = Vec::new();
    for step in &req.pipeline.steps {
        // Admit the step's privilege request (ADR-0039), same as the trigger path.
        // An inline API run carries no Environment, so governed grants
        // (add-capabilities/privileged) are rejected fail-closed — but the
        // self-service `run_as_root` grant is honored (it stays inside the
        // caps-dropped, unprivileged sandbox). The hardened floor always applies.
        let admitted = admit_step_grants(None, step.security.as_ref(), &step.image, false)
            .map_err(|v| {
                ApiError::BadRequest(format!(
                    "step `{}`: privilege request rejected: {}",
                    step.id,
                    v.join("; ")
                ))
            })?;
        // ADR-0055: a raw k8s_overlay carries no authority. An inline API run
        // targets no Environment, so any overlay is rejected fail-closed.
        let k8s_overlay = admit_k8s_overlay(None, step.k8s_overlay.as_ref())
            .map_err(|v| ApiError::BadRequest(format!("step `{}`: {}", step.id, v.join("; "))))?;
        let spec = StepSpec {
            image: step.image.clone(),
            command: step.command.clone(),
            env: step
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            secrets: step.secrets.clone(),
            run_as_root: admitted.run_as_root,
            add_capabilities: admitted.add_capabilities,
            privileged: admitted.privileged,
            timeout_seconds: step.timeout,
            workspace_inputs: vec![],
            workspace_outputs: step.outputs.clone().unwrap_or_default(),
            // No cache on the inline `POST /v1/runs` escape hatch: such runs
            // are untenanted (no `runs.project`), and the cache mapping is
            // project-namespaced — the scheduler would disable it anyway.
            cache: None,
            forge_token: None,
            build: None,
            artifacts: vec![],
            placement_profiles: step.placement_profiles.clone(),
            resources: step.resources.clone(),
            k8s_overlay,
            oidc_token: None,
            services: step.services.clone(),
            uses: step.uses.clone(),
            // The inline `POST /v1/runs` escape hatch takes steps directly (no
            // matrix expansion), so there is never a coordinate here.
            matrix_values: Default::default(),
            scope: None,
        };
        let needs: Vec<StepId> = step.needs.iter().map(|n| StepId(n.clone())).collect();
        admitted_steps.push((StepId(step.id.clone()), spec, needs));
    }

    let now = st.clock.now().await;
    let run = new_run_id();

    // Out of the scheduler's work list until its steps and their `inputs:`
    // are all written (`end_run_creation` below), as in `persist_run_from_ir`.
    st.db
        .begin_run_creation(&run, req.pipeline.ir_version, EVENT_VERSION, now)
        .await?;
    let written: Result<(), DbError> = async {
        // Store the compiled IR on the run — self-describing (ADR-0022).
        let ir = serde_json::to_value(&req.pipeline).map_err(|e| DbError::Other(e.to_string()))?;
        st.db.store_run_ir(&run, &ir).await?;
        // Freeze the resolved params on the run so every step's interpolation
        // (`${{ inputs.… }}`) and `SCARAB_PARAM_*` env re-derive deterministically.
        st.db.set_run_params(&run, &resolved).await?;
        // The Headline (ADR-0057 §3): an optional dispatch reason for this inline
        // `api`-style run. This path builds no forge `Event`, so the reason is capped
        // + stamped directly (the same cap `Event::trigger_title` applies). Stamped
        // only when supplied; no requiredness check here (thread D). Display/audit
        // only — never in the CEL/interpolation context.
        if let Some(title) = req
            .reason
            .as_deref()
            .and_then(scarab_forge::cap_trigger_title)
        {
            st.db.set_run_trigger_title(&run, &title).await?;
        }
        st.db
            .append_event(&EventKind {
                version: EVENT_VERSION,
                run: run.clone(),
                kind: EventPayload::RunCreated,
                at: now,
            })
            .await?;

        for (id, spec, needs) in &admitted_steps {
            st.db
                .create_step_run(&run, id, Some(spec), needs, now)
                .await?;
        }
        // Explicit input workspaces (ADR-0007): thread the authored `inputs:` subset
        // so the scheduler honors it at launch (workspace materialization) and in the
        // skip-if-unchanged signature — the same threading the trigger path does in
        // `persist_run_from_ir`. Absent = implicit-by-default (all needs). Without
        // this the inline `POST /v1/runs` path silently dropped the selection.
        for step in &req.pipeline.steps {
            if let Some(inputs) = &step.inputs {
                let inputs: Vec<StepId> = inputs.iter().map(|i| StepId(i.clone())).collect();
                st.db
                    .set_step_inputs(&run, &StepId(step.id.clone()), &inputs)
                    .await?;
            }
        }
        Ok(())
    }
    .await;
    end_run_creation(st.db.as_ref(), &run, now, written).await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateRunResponse {
            id: run.0,
            status: run_status_name(RunStatus::Pending).to_string(),
        }),
    ))
}

/// Dispatch a named repo pipeline at a ref (ADR-0043 "World B"). Authorized as a
/// write (like [`create_run`]); a dispatch is a *trigger*, so a deploy pipeline
/// still hits its Environment's protection rules at admission. Returns the new
/// run id (mirrors [`CreateRunResponse`]); creates **no** run on any error.
#[utoipa::path(
    post,
    path = "/v1/repos/{org}/{repo}/dispatch",
    params(
        ("org" = String, Path, description = "repo owner"),
        ("repo" = String, Path, description = "repo name")
    ),
    request_body = DispatchRequest,
    responses(
        (status = 201, description = "Run created", body = CreateRunResponse),
        (status = 403, description = "the caller may not dispatch, the pipeline's Environment does not exist in the org or does not admit this project (ADR-0071), or a run.create Policy denied the Run (ADR-0075); the body says why. No run is created")
    )
)]
async fn dispatch(
    State(st): State<AppState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<DispatchRequest>,
) -> Result<(StatusCode, Json<CreateRunResponse>), ApiError> {
    let scope = scarab_identity::Scope::Project {
        org: org.clone(),
        name: repo.clone(),
    };
    let principal = authorize_scoped(&st, &headers, Action::Write, Some(&scope)).await?;

    // Dispatch rides the read-at-ref machinery, so a forge must be wired.
    let forge = st
        .forge
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no forge configured".into()))?;

    let run = dispatch_run(
        forge.as_ref(),
        st.db.as_ref(),
        st.clock.as_ref(),
        st.admission_ports(),
        principal.subject,
        scarab_forge::RepoRef {
            owner: org,
            name: repo,
        },
        req.r#ref,
        req.pipeline,
        req.params,
        req.kind,
        req.reason,
    )
    .await
    .map_err(ApiError::from)?;

    Ok((
        StatusCode::CREATED,
        Json(CreateRunResponse {
            id: run.0,
            status: run_status_name(RunStatus::Pending).to_string(),
        }),
    ))
}

/// The bare selection name of a `.scarab/<name>.{yaml,yml}` pipeline (ADR-0043
/// catalog): strip the `.scarab/` prefix and the extension. This is what the
/// catalog reports and what the describe/dispatch calls accept as `pipeline`
/// (the path-addressable interface endpoint needs a slash-free segment, and
/// [`dispatch_candidate_paths`] re-derives the `.yaml`/`.yml` candidates).
fn bare_pipeline_name(path: &str) -> String {
    let p = path.strip_prefix(&format!("{CONFIG_DIR}/")).unwrap_or(path);
    p.strip_suffix(".yaml")
        .or_else(|| p.strip_suffix(".yml"))
        .unwrap_or(p)
        .to_string()
}

/// The manually-dispatchable catalog at a ref (ADR-0043 §4). Resolves `ref` to a
/// concrete commit (echoed back), enumerates every `.scarab/*.{yaml,yml}` at it
/// (direct children only — `.scarab/lib/**` is excluded, mirroring webhook
/// discovery, and so is a file marked `kind: library`), and reports each
/// pipeline's `manual`/`api` opt-in from a
/// **lightweight `on:`-only parse** (no `invoke:` pre-fetch, no full compile —
/// that cost is deferred to the on-selection interface read). A single file that
/// fails to parse is flagged with an `error` rather than failing the whole list;
/// an absent `.scarab/` yields an empty catalog, not an error. Read capability.
#[utoipa::path(
    get,
    path = "/v1/repos/{org}/{repo}/pipelines",
    params(
        ("org" = String, Path, description = "repo owner"),
        ("repo" = String, Path, description = "repo name"),
        ("ref" = Option<String>, Query, description = "ref to read the config at (default HEAD)")
    ),
    responses((status = 200, description = "dispatch catalog at the resolved commit", body = PipelineCatalogResponse))
)]
async fn list_pipelines(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
    Query(q): Query<PipelineRefQuery>,
) -> Result<Json<PipelineCatalogResponse>, ApiError> {
    let scope = scarab_identity::Scope::Project {
        org: org.clone(),
        name: repo.clone(),
    };
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let forge = st
        .forge
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no forge configured".into()))?;
    let repo = scarab_forge::RepoRef {
        owner: org,
        name: repo,
    };

    // Resolve the ref to a concrete commit; the catalog reflects exactly it.
    let sha = forge
        .latest_commit(&repo, &q.r#ref)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .sha;

    // Enumerate the pipeline files. An absent `.scarab/` (a forge API miss) is an
    // empty catalog, not an error — the same tolerance webhook discovery has.
    let entries = match forge.list_dir_at_ref(&repo, &sha, CONFIG_DIR).await {
        Ok(e) => e,
        Err(scarab_forge::ForgeError::Api(_)) => Vec::new(),
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };
    let mut paths: Vec<String> = entries
        .into_iter()
        .filter(|p| is_pipeline_file(p))
        .collect();
    paths.sort();

    let mut pipelines = Vec::with_capacity(paths.len());
    for path in paths {
        let name = bare_pipeline_name(&path);
        let entry = match forge.read_file_at_ref(&repo, &sha, &path).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                // A library is not dispatchable (ADR-0076 §2): leave it out.
                Ok(yaml) if scarab_pipeline::is_library(&yaml) => continue,
                // Lightweight: parse only the `on:` block (ADR-0043 §4).
                Ok(yaml) => match scarab_pipeline::triggers_of(&yaml) {
                    Ok(triggers) => CatalogEntry {
                        name,
                        manual: triggers.0.contains_key("manual"),
                        api: triggers.0.contains_key("api"),
                        error: None,
                    },
                    Err(e) => CatalogEntry {
                        name,
                        manual: false,
                        api: false,
                        error: Some(e.to_string()),
                    },
                },
                Err(_) => CatalogEntry {
                    name,
                    manual: false,
                    api: false,
                    error: Some("config is not valid UTF-8".into()),
                },
            },
            // A file that vanished between list and read → skip it.
            Err(scarab_forge::ForgeError::Api(_)) => continue,
            Err(e) => return Err(ApiError::BadRequest(e.to_string())),
        };
        pipelines.push(entry);
    }

    Ok(Json(PipelineCatalogResponse { sha, pipelines }))
}

/// `GET /v1/repos/{org}/{repo}/refs?q=` — the repo's branches and tags for a
/// searchable ref picker (ADR-0046), backed by the repo's `ForgeConnection`.
/// `q`, when set, is a case-insensitive substring the ref name must contain
/// (applied by the adapter, since neither forge's list API takes a search
/// param). Branches sort before tags, each group name-ascending, so the picker
/// order is stable regardless of forge return order. Scoped to branches + tags.
/// Read capability.
#[utoipa::path(
    get,
    path = "/v1/repos/{org}/{repo}/refs",
    params(
        ("org" = String, Path, description = "repo owner"),
        ("repo" = String, Path, description = "repo name"),
        ("q" = Option<String>, Query, description = "case-insensitive substring the ref name must contain")
    ),
    responses((status = 200, description = "the repo's branches and tags", body = RefsResponse))
)]
async fn list_refs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
    Query(q): Query<RefsQuery>,
) -> Result<Json<RefsResponse>, ApiError> {
    let scope = scarab_identity::Scope::Project {
        org: org.clone(),
        name: repo.clone(),
    };
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let forge = st
        .forge
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no forge configured".into()))?;
    let repo = scarab_forge::RepoRef {
        owner: org,
        name: repo,
    };

    let mut refs = forge
        .list_refs(&repo, q.q.as_deref())
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Branches before tags, each group name-sorted — a stable picker order.
    let rank = |k: &scarab_forge::RefKind| matches!(k, scarab_forge::RefKind::Tag) as u8;
    refs.sort_by(|a, b| {
        rank(&a.kind)
            .cmp(&rank(&b.kind))
            .then_with(|| a.name.cmp(&b.name))
    });

    let refs = refs
        .into_iter()
        .map(|r| RefDto {
            kind: match r.kind {
                scarab_forge::RefKind::Branch => "branch",
                scarab_forge::RefKind::Tag => "tag",
            }
            .to_string(),
            name: r.name,
            short_sha: r.sha.chars().take(7).collect(),
        })
        .collect();

    Ok(Json(RefsResponse { refs }))
}

/// The compiled, typed launch-parameter schema for one selected pipeline
/// (ADR-0043 §4) — the on-selection describe. Resolves `ref` to a concrete
/// commit (echoed back), reads the named pipeline there, and **fully compiles**
/// it (reusing the shared [`prefetch_libs_and_compile`] — this is where the
/// lib-prefetch cost is justified), returning `interface.inputs` as typed
/// [`ParamSpec`](scarab_pipeline::ParamSpec)s. The response is a pure function of
/// the compiled IR — the same path dispatch rides, no parallel parser — so a
/// compile error surfaces as a structured 4xx (never a 500), identical to
/// dispatch. Read capability.
#[utoipa::path(
    get,
    path = "/v1/repos/{org}/{repo}/pipelines/{name}/interface",
    params(
        ("org" = String, Path, description = "repo owner"),
        ("repo" = String, Path, description = "repo name"),
        ("name" = String, Path, description = "pipeline name (bare, e.g. `deploy`)"),
        ("ref" = Option<String>, Query, description = "ref to read the config at (default HEAD)")
    ),
    responses(
        (status = 200, description = "the compiled typed parameter schema", body = PipelineInterfaceResponse),
        (status = 404, description = "no pipeline by that name at the ref"),
        (status = 400, description = "the pipeline failed to compile: each error with its fields", body = CompileErrorResponse)
    )
)]
async fn pipeline_interface(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, name)): Path<(String, String, String)>,
    Query(q): Query<PipelineRefQuery>,
) -> Result<Json<PipelineInterfaceResponse>, ApiError> {
    let scope = scarab_identity::Scope::Project {
        org: org.clone(),
        name: repo.clone(),
    };
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let forge = st
        .forge
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no forge configured".into()))?;
    let forge = forge.as_ref();
    let repo = scarab_forge::RepoRef {
        owner: org,
        name: repo,
    };

    // Resolve the ref → SHA (echoed back), read the named pipeline there, and
    // fully compile — mapping each failure to a structured 4xx via DispatchError
    // (the same shape dispatch uses: not-found → 404, compile/forge → 400).
    let mut resolved: Option<String> = None;
    let interface = async {
        let sha = forge
            .latest_commit(&repo, &q.r#ref)
            .await
            .map_err(DispatchError::Forge)?
            .sha;
        resolved = Some(sha.clone());
        let (_, yaml) = read_named_pipeline(forge, &repo, &sha, &name).await?;
        let installed =
            installed_libraries(st.libraries.as_deref(), st.plugins.as_deref(), &repo.owner)
            .await
            .map_err(DispatchError::Db)?;
        let ir = prefetch_libs_and_compile(forge, &repo, &sha, &yaml, &installed).await?;
        Ok::<_, DispatchError>(PipelineInterfaceResponse {
            sha,
            manual: ir.triggers.0.contains_key("manual"),
            api: ir.triggers.0.contains_key("api"),
            inputs: ir.interface.inputs,
        })
    }
    .await;

    match interface {
        Ok(i) => Ok(Json(i)),
        // A compile failure answers with each error's fields (ADR-0076), so
        // the form can show what is wrong and the fix line by line.
        Err(DispatchError::Pipeline(text)) => Err(ApiError::Compile(Box::new(
            CompileErrorResponse::from_text(format!("pipeline: {text}"), resolved),
        ))),
        Err(e) => Err(e.into()),
    }
}

/// One compile error with the fields a reader acts on (ADR-0076): the Step,
/// the command index and the template it reads, and for a library Step the
/// library and the caller's binding. `message` is the compiler's text.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompileErrorDto {
    /// `command_value` (a `command` argument reads free text from outside
    /// the Pipeline), `script_template` (a `${{ … }}` in a script) or
    /// `other`.
    pub kind: String,
    /// The Step in the Pipeline file; for a library Step, the invoke Step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// For a library Step: the library (`<publisher>/<name>` or a path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    /// For a library Step: its id inside the library.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_index: Option<u32>,
    /// The `${{ … }}` the argument reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<String>,
    /// For a library Step: the library input the caller binds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// For a library Step: the caller's `with:` value for `input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<String>,
    pub message: String,
}

impl From<scarab_pipeline::diagnostic::Diagnostic> for CompileErrorDto {
    fn from(d: scarab_pipeline::diagnostic::Diagnostic) -> Self {
        use scarab_pipeline::diagnostic::DiagnosticKind;
        CompileErrorDto {
            kind: match d.kind {
                DiagnosticKind::CommandValue => "command_value",
                DiagnosticKind::ScriptTemplate => "script_template",
                DiagnosticKind::Other => "other",
            }
            .to_string(),
            step: d.step,
            library: d.library,
            library_step: d.library_step,
            command_index: d.command_index.and_then(|i| u32::try_from(i).ok()),
            read: d.read,
            input: d.input,
            bound: d.bound,
            message: d.message,
        }
    }
}

/// `400` body for a Pipeline that does not compile: the whole text, as a
/// plain-text 400 carried it before, and each error with its fields.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CompileErrorResponse {
    /// Always `compile_failed`.
    pub code: String,
    pub message: String,
    /// The commit the Pipeline was read at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    pub errors: Vec<CompileErrorDto>,
}

impl CompileErrorDto {
    /// A refusal that is not a compile error: its message only.
    pub fn other(message: String) -> Self {
        CompileErrorDto {
            kind: "other".into(),
            step: None,
            library: None,
            library_step: None,
            command_index: None,
            read: None,
            input: None,
            bound: None,
            message,
        }
    }
}

impl CompileErrorResponse {
    /// The response for a compile failure's text.
    pub fn from_text(message: String, sha: Option<String>) -> Self {
        CompileErrorResponse {
            code: "compile_failed".into(),
            errors: compile_errors(&message),
            message,
            sha,
        }
    }
}

/// Each compile error in a compile failure's text, with its fields.
pub fn compile_errors(text: &str) -> Vec<CompileErrorDto> {
    let text = text.strip_prefix("pipeline: ").unwrap_or(text);
    scarab_pipeline::diagnostic::diagnostics(text)
        .into_iter()
        .map(Into::into)
        .collect()
}

/// Query for [`list_runs`]: an optional page size.
#[derive(Debug, Deserialize)]
pub struct ListRunsQuery {
    /// Max runs to return (default [`DEFAULT_RUNS_LIMIT`], capped at [`MAX_RUNS_LIMIT`]).
    pub limit: Option<u32>,
}

/// Query for [`list_repo_runs`]: a page size plus an optional Environment.
#[derive(Debug, Deserialize)]
pub struct ListRepoRunsQuery {
    /// Max runs to return (default [`DEFAULT_RUNS_LIMIT`], capped at [`MAX_RUNS_LIMIT`]).
    pub limit: Option<u32>,
    /// Only the deploy runs into this Environment (ADR-0074 §2). Absent = every
    /// run; present but empty is a 400, never a silent "match nothing".
    pub environment: Option<String>,
    /// `true`: only deploy Runs; `false`: only the rest (ADR-0074 §2). Absent =
    /// every run.
    pub deploy: Option<bool>,
}

/// Default page size for `GET /v1/runs`.
const DEFAULT_RUNS_LIMIT: u32 = 50;
/// Upper bound so a client can't request an unbounded scan.
const MAX_RUNS_LIMIT: u32 = 200;

/// The most recent runs, newest first — the runs-list view (ADR-0013, 0028).
#[utoipa::path(
    get,
    path = "/v1/runs",
    params(("limit" = Option<u32>, Query, description = "max runs to return (default 50, max 200)")),
    responses((status = 200, body = RunListResponse))
)]
async fn list_runs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListRunsQuery>,
) -> Result<Json<RunListResponse>, ApiError> {
    // Tenancy scoping (ADR-0049): authenticate, then filter the list to what
    // the caller may see — global roles see everything; scoped callers see the
    // runs of orgs/projects their bindings grant Read on, and an issued API
    // token sees only those its own scope also covers. Untenanted runs (inline
    // dev submissions) are visible to global roles only.
    let caller = authenticate(&st, &headers, Action::Read).await?;
    let limit = q.limit.unwrap_or(DEFAULT_RUNS_LIMIT).min(MAX_RUNS_LIMIT);
    let mut runs = st.db.list_runs(limit).await?;
    if !caller.may_globally(Action::Read) {
        // Resolve each distinct tenant once, never per run.
        let mut allowed: std::collections::HashMap<(String, String), bool> =
            std::collections::HashMap::new();
        let mut visible = Vec::with_capacity(runs.len());
        for r in runs {
            let Some(tenant) = r.tenant.clone() else {
                continue;
            };
            let ok = match allowed.get(&tenant) {
                Some(ok) => *ok,
                None => {
                    let scope = scarab_identity::Scope::Project {
                        org: tenant.0.clone(),
                        name: tenant.1.clone(),
                    };
                    let ok = caller.may(&st, Action::Read, &scope).await?;
                    allowed.insert(tenant.clone(), ok);
                    ok
                }
            };
            if ok {
                visible.push(r);
            }
        }
        runs = visible;
    }
    Ok(Json(RunListResponse {
        runs: run_summaries(&st, runs).await?,
    }))
}

/// Current status of a run and its steps.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}",
    params(("id" = String, Path, description = "run id")),
    responses((status = 200, body = RunStatusResponse), (status = 404, description = "no such run"))
)]
async fn get_run(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<RunStatusResponse>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    let status = st.db.run_status(&run).await?.ok_or(ApiError::NotFound)?;
    let steps = st.db.steps_of_run(&run).await?;
    let params = st.db.run_params(&run).await?;
    let pipeline = st.db.run_pipeline(&run).await?;
    let trigger_title = st.db.run_trigger_title(&run).await?;
    let origin_pr_base = st.db.run_pr_base(&run).await?;
    let run_number = st.db.run_number(&run).await?;
    // The cold tier's TIME bound, stated rather than implied (ADR-0061 s5). Cheap
    // by construction: one row read plus arithmetic, no object-store round-trip —
    // the *promise* is a function of the run's lifecycle, and whether the bytes
    // happen to still be there is a separate (and more expensive) question the
    // rerun plan answers.
    let retention = st
        .db
        .run_snapshot_retention(&run)
        .await?
        .ok_or(ApiError::NotFound)?;
    let snapshot_retention =
        snapshot_retention_dto(&retention, st.snapshot_retention_days, st.clock.now().await);
    // Enrich each step with its sidecar services + shared-service opt-ins (ADR-
    // 0058) from the stored spec, so the FE can render sidecar nodes and `uses`
    // edges and address per-sidecar logs by index. A step with no stored spec
    // (e.g. a gate) carries empty vecs.
    let mut gate_views = run_gate_view::step_gate_views(&st, &run, &steps).await?;
    let (mut origin_views, yanked_libraries) = run_origin::step_views(&st, &run, &steps).await?;
    let deploy_steps = st.db.deploy_steps_of_run(&run).await?;
    let mut step_dtos = Vec::with_capacity(steps.len());
    for s in steps {
        let view = gate_views.remove(&s.step.0).unwrap_or_default();
        let origin_view = origin_views.remove(&s.step.0).unwrap_or_default();
        let deploy = deploy_steps.contains(&s.step);
        let (services, uses) = match st.db.step_spec(&run, &s.step).await? {
            Some(spec) => (
                spec.services
                    .iter()
                    .enumerate()
                    .map(|(index, svc)| StepServiceDto {
                        index,
                        image: svc.image.clone(),
                        ports: svc.ports.clone(),
                    })
                    .collect(),
                spec.uses.clone(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        step_dtos.push(StepStatusDto {
            id: s.step.0,
            status: step_status_name(s.status).to_string(),
            attempts: s.attempts.len(),
            attempt_list: s.attempts.iter().map(attempt_dto).collect(),
            needs: s.needs.into_iter().map(|n| n.0).collect(),
            gate: s.gate_kind,
            gate_resolution: view.gate_resolution,
            services,
            uses,
            deploy,
            origin: origin_view.origin,
            plugin_gate: origin_view.plugin_gate,
        });
    }
    Ok(Json(RunStatusResponse {
        id: run.0,
        status: run_status_name(status).to_string(),
        run_number,
        steps: step_dtos,
        params,
        pipeline,
        trigger_title,
        origin_pr_base,
        snapshot_retention,
        yanked_libraries,
    }))
}

/// Server-Sent-Events tail of the run's append-only event log — the status
/// timeline (ADR-0013): RunCreated, transitions, attempt start/finish.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/events",
    params(("id" = String, Path, description = "run id")),
    responses((status = 200, description = "SSE stream of the run's event log"))
)]
async fn get_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    if st.db.run_status(&run).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let events = st.db.events(&run).await?;
    let items: Vec<Result<Event, Infallible>> = events
        .into_iter()
        .map(|e| {
            Ok(Event::default()
                .json_data(&e)
                .unwrap_or_else(|_| Event::default().data("{}")))
        })
        .collect();
    Ok(Sse::new(stream::iter(items)))
}

/// Server-Sent-Events of step **log bodies** (ADR-0013): replays every
/// committed chunk (decompressed from the object store, indexed by Postgres
/// offsets), then — while the run is still going — live-tails new chunks via the
/// log pipeline's broadcast. A terminal run yields the full log and closes.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/logs",
    params(("id" = String, Path, description = "run id")),
    responses((status = 200, description = "SSE stream of step log output"))
)]
async fn get_logs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Sse<BoxStream<'static, Result<Event, Infallible>>>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    let status = st.db.run_status(&run).await?.ok_or(ApiError::NotFound)?;
    let steps = st.db.steps_of_run(&run).await?;

    // Replay everything committed so far, remembering how far each stream was
    // consumed (the live tail resumes from these seqs — never duplicating).
    let mut replay: Vec<Result<Event, Infallible>> = Vec::new();
    let mut seen: std::collections::HashMap<(String, String), u64> =
        std::collections::HashMap::new();
    for s in &steps {
        for a in &s.attempts {
            // `Ok` with no chunks is the truthful empty pane; `Err` means the
            // index promised bytes the store could not produce (or the index
            // itself failed) — surface it, never a plausible blank (ADR-0063
            // part 5).
            let (body, next) = st.logs.read_from(&run, &s.step, &a.id, 0).await?;
            if !body.is_empty() {
                replay.push(Ok(Event::default().data(String::from_utf8_lossy(&body))));
            }
            seen.insert((s.step.0.clone(), a.id.0.clone()), next);
        }
    }

    let replay_stream = stream::iter(replay);
    if status.is_terminal() {
        // Nothing more will be written: replay and close.
        Ok(Sse::new(replay_stream.boxed()))
    } else {
        // Live tail (ADR-0051): poll the DURABLE index for new chunks, so ANY
        // replica serves live logs regardless of which replica tails the Pod.
        // (The in-process broadcast is only a same-replica fast-path, not the
        // source of truth — this path never depends on it.) The stream ends
        // one poll after the run settles.
        let live = futures::stream::unfold(
            (st.clone(), run.clone(), seen, false),
            |(st, run, mut seen, done)| async move {
                if done {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let terminal = matches!(
                    st.db.run_status(&run).await,
                    Ok(Some(s)) if s.is_terminal()
                );
                let steps = st.db.steps_of_run(&run).await.unwrap_or_default();
                let mut body = Vec::new();
                let mut failed: Option<crate::logs::LogError> = None;
                'read: for s in &steps {
                    for a in &s.attempts {
                        let key = (s.step.0.clone(), a.id.0.clone());
                        let from = seen.get(&key).copied().unwrap_or(0);
                        match st.logs.read_from(&run, &s.step, &a.id, from).await {
                            Ok((bytes, next)) => {
                                if !bytes.is_empty() {
                                    body.extend(bytes);
                                }
                                seen.insert(key, next);
                            }
                            Err(e) => {
                                failed = Some(e);
                                break 'read;
                            }
                        }
                    }
                }
                // Chunks the index promised could not be read: tell the reader
                // and end the stream instead of tailing a plausible blank
                // (ADR-0063 part 5).
                if let Some(e) = failed {
                    let event = Event::default()
                        .event("error")
                        .data(format!("log storage read failed: {e}"));
                    return Some((Ok(event), (st, run, seen, true)));
                }
                let event = if body.is_empty() {
                    Event::default().comment("keepalive")
                } else {
                    Event::default().data(String::from_utf8_lossy(&body))
                };
                Some((Ok(event), (st, run, seen, terminal)))
            },
        );
        Ok(Sse::new(replay_stream.chain(live).boxed()))
    }
}

/// The `?attempt=` scope for a per-step log stream. Absent = the whole step
/// (all attempts, in order — the default the fold header shows).
#[derive(Debug, Deserialize)]
struct StepLogsQuery {
    #[serde(default)]
    attempt: Option<String>,
    /// Restrict to one of the step's co-located sidecar services (ADR-0058), by
    /// its index in the step's authored `services:` — reads the `service-{index}`
    /// container's stream instead of the step's own main container. Combines with
    /// `attempt` (a sidecar shares the step's attempts). Absent = the step's log.
    #[serde(default)]
    sidecar: Option<usize>,
}

/// SSE of ONE step's log bodies (ADR-0013), the source for the run detail's
/// per-step fold. Same machinery as the run-wide `/logs`, scoped to the step —
/// and, with `?attempt=`, to a single attempt so a rerun's earlier (failed)
/// output can be read in isolation. Replays committed chunks then live-tails
/// only when the run is still going AND the latest attempt is in scope
/// (historical attempts are immutable). Read at the run's tenant.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/steps/{step}/logs",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id"),
        ("attempt" = Option<String>, Query, description = "restrict to one attempt id (default = all)"),
        ("sidecar" = Option<usize>, Query, description = "restrict to a sidecar service by its index in the step's services (default = the step's main container)")
    ),
    responses((status = 200, description = "SSE stream of this step's log output"), (status = 404, description = "no such run or step"))
)]
async fn get_step_logs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    Query(q): Query<StepLogsQuery>,
) -> Result<Sse<BoxStream<'static, Result<Event, Infallible>>>, ApiError> {
    let run = RunId(id);
    let step = StepId(step);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    let status = st.db.run_status(&run).await?.ok_or(ApiError::NotFound)?;
    let sr = st
        .db
        .steps_of_run(&run)
        .await?
        .into_iter()
        .find(|s| s.step == step)
        .ok_or(ApiError::NotFound)?;

    // The attempts to stream, in order: one requested, else all this step ran.
    let selected: Vec<scarab_engine::AttemptId> = match &q.attempt {
        Some(a) => {
            let aid = scarab_engine::AttemptId(a.clone());
            if !sr.attempts.iter().any(|x| x.id == aid) {
                return Err(ApiError::NotFound);
            }
            vec![aid]
        }
        None => sr.attempts.iter().map(|a| a.id.clone()).collect(),
    };

    // Sidecar logs (ADR-0058) are stored under a synthetic step id
    // (`{step}::service-{i}`) but keyed on the step's REAL attempt ids (a sidecar
    // shares the step's Pod + Attempt), so the attempt resolution above is
    // unchanged — only the read key swaps to the sidecar's synthetic stream.
    let read_step = match q.sidecar {
        Some(i) => crate::logs::sidecar_stream_key(&step, i),
        None => step.clone(),
    };

    // Replay each selected attempt in order, remembering how far it was read.
    let mut replay: Vec<Result<Event, Infallible>> = Vec::new();
    let mut seen: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for aid in &selected {
        // As on the run-wide route: no chunks is a truthful empty pane, a
        // failed read of promised chunks is an error the reader must see.
        let (body, next) = st.logs.read_from(&run, &read_step, aid, 0).await?;
        if !body.is_empty() {
            replay.push(Ok(Event::default().data(String::from_utf8_lossy(&body))));
        }
        seen.insert(aid.0.clone(), next);
    }
    let replay_stream = stream::iter(replay);

    // Live-tail only if the run is going AND the latest attempt is in scope.
    let latest_in_scope = sr
        .attempts
        .last()
        .map(|a| a.id.clone())
        .filter(|latest| selected.iter().any(|s| s == latest))
        .is_some();
    if status.is_terminal() || !latest_in_scope {
        return Ok(Sse::new(replay_stream.boxed()));
    }
    let live = futures::stream::unfold(
        (
            st.clone(),
            run.clone(),
            read_step.clone(),
            selected,
            seen,
            false,
        ),
        |(st, run, step, attempts, mut seen, done)| async move {
            if done {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let terminal = matches!(
                st.db.run_status(&run).await,
                Ok(Some(s)) if s.is_terminal()
            );
            let mut body = Vec::new();
            let mut failed: Option<crate::logs::LogError> = None;
            for aid in &attempts {
                let from = seen.get(&aid.0).copied().unwrap_or(0);
                match st.logs.read_from(&run, &step, aid, from).await {
                    Ok((bytes, next)) => {
                        if !bytes.is_empty() {
                            body.extend(bytes);
                        }
                        seen.insert(aid.0.clone(), next);
                    }
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            // Promised chunks that would not read: error out and end the
            // stream rather than tailing a blank (ADR-0063 part 5).
            if let Some(e) = failed {
                let event = Event::default()
                    .event("error")
                    .data(format!("log storage read failed: {e}"));
                return Some((Ok(event), (st, run, step, attempts, seen, true)));
            }
            let event = if body.is_empty() {
                Event::default().comment("keepalive")
            } else {
                Event::default().data(String::from_utf8_lossy(&body))
            };
            Some((Ok(event), (st, run, step, attempts, seen, terminal)))
        },
    );
    Ok(Sse::new(replay_stream.chain(live).boxed()))
}

/// The shared services of a run's **current Take** (ADR-0058), name-ordered —
/// each with its lifecycle status, for the run detail's Services panel beside the
/// DAG. A shared service is not a DAG node, so it is never folded into the step
/// list. Read at the run's tenant. Empty when the pipeline declares no shared
/// services.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/services",
    params(("id" = String, Path, description = "run id")),
    responses((status = 200, body = [ServiceStatusDto]), (status = 404, description = "no such run"))
)]
async fn get_services(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<ServiceStatusDto>>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    if st.db.run_status(&run).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let services = st.db.run_services(&run).await?;
    // Surface the current Take's instances (a Rerun's prior Take is being torn
    // down); `run_services` folds every Take, so pick `max(take)`.
    let current = services.iter().map(|s| s.take).max();
    Ok(Json(
        services
            .iter()
            .filter(|s| Some(s.take) == current)
            .map(service_status_dto)
            .collect(),
    ))
}

/// The `?take=` scope for a shared-service log stream. Absent = the current Take.
#[derive(Debug, Deserialize)]
struct ServiceLogsQuery {
    #[serde(default)]
    take: Option<i64>,
}

/// SSE of ONE shared service's log output (ADR-0058 evidence), the source for the
/// run detail's Services panel "logs" view. Best-effort, the SAME reliability
/// class and pipeline as step logs (ADR-0013): replays committed chunks then
/// live-tails while the run is still going. `?take=` reads an older Take's
/// instance in isolation; absent = the current Take. Read at the run's tenant.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/services/{service}/logs",
    params(
        ("id" = String, Path, description = "run id"),
        ("service" = String, Path, description = "declared service name"),
        ("take" = Option<i64>, Query, description = "restrict to one Take generation (default = current)")
    ),
    responses((status = 200, description = "SSE stream of this service's log output"), (status = 404, description = "no such run or service"))
)]
async fn get_service_logs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, service)): Path<(String, String)>,
    Query(q): Query<ServiceLogsQuery>,
) -> Result<Sse<BoxStream<'static, Result<Event, Infallible>>>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    let status = st.db.run_status(&run).await?.ok_or(ApiError::NotFound)?;
    let services = st.db.run_services(&run).await?;
    // Resolve the instance: a requested Take, else the current (`max`) one. 404
    // if the run declares no such service (or not at that Take).
    let take = match q.take {
        Some(t) => t,
        None => services
            .iter()
            .filter(|s| s.name == service)
            .map(|s| s.take)
            .max()
            .ok_or(ApiError::NotFound)?,
    };
    if !services.iter().any(|s| s.name == service && s.take == take) {
        return Err(ApiError::NotFound);
    }
    let (step, attempt) = crate::logs::service_stream_key(&service, take);

    // Replay committed chunks, remembering how far the stream was consumed.
    let mut replay: Vec<Result<Event, Infallible>> = Vec::new();
    // No chunks is a truthful empty pane; a failed read of promised chunks
    // is an error the reader must see (ADR-0063 part 5).
    let (body, next) = st.logs.read_from(&run, &step, &attempt, 0).await?;
    if !body.is_empty() {
        replay.push(Ok(Event::default().data(String::from_utf8_lossy(&body))));
    }
    let replay_stream = stream::iter(replay);
    if status.is_terminal() {
        // Nothing more will be written: replay and close.
        return Ok(Sse::new(replay_stream.boxed()));
    }
    // Live tail: poll the durable index for new chunks (ADR-0051), replica-
    // agnostic like the step-log tail; the stream ends one poll after settle. The
    // next seq to read rides in the fold state (like the step-log tail's `seen`).
    let live = futures::stream::unfold(
        (st.clone(), run.clone(), step, attempt, next, false),
        |(st, run, step, attempt, mut next, done)| async move {
            if done {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let terminal = matches!(
                st.db.run_status(&run).await,
                Ok(Some(s)) if s.is_terminal()
            );
            let mut body = Vec::new();
            match st.logs.read_from(&run, &step, &attempt, next).await {
                Ok((bytes, n)) => {
                    if !bytes.is_empty() {
                        body.extend(bytes);
                    }
                    next = n;
                }
                // Promised chunks that would not read: error out and end the
                // stream rather than tailing a blank (ADR-0063 part 5).
                Err(e) => {
                    let event = Event::default()
                        .event("error")
                        .data(format!("log storage read failed: {e}"));
                    return Some((Ok(event), (st, run, step, attempt, next, true)));
                }
            }
            let event = if body.is_empty() {
                Event::default().comment("keepalive")
            } else {
                Event::default().data(String::from_utf8_lossy(&body))
            };
            Some((Ok(event), (st, run, step, attempt, next, terminal)))
        },
    );
    Ok(Sse::new(replay_stream.chain(live).boxed()))
}

/// Map a rerun/retry outcome to the API's error space (the success half rides
/// through untouched — the POST handlers return the **executed plan** as their
/// `202` body, git-bug 4afaa3e). `404` for an unknown step; `409` when the
/// request conflicts with the step's state (ADR-0056 amendment: rerunning a step
/// whose dependency has not succeeded, or retrying a non-failed step). Generic
/// so the error mapping is never forked per handler.
fn rerun_outcome<T>(res: Result<T, RerunError>) -> Result<T, ApiError> {
    match res {
        Ok(v) => Ok(v),
        Err(RerunError::StepNotFound(_)) => Err(ApiError::NotFound),
        Err(e @ RerunError::DependencyNotSatisfied { .. }) => {
            Err(ApiError::Conflict(e.to_string()))
        }
        Err(e @ RerunError::NotFailed { .. }) => Err(ApiError::Conflict(e.to_string())),
        Err(e @ RerunError::CreationUnfinished) => Err(ApiError::Conflict(e.to_string())),
        // Gate-approval-only variants (never produced by rerun/retry); mapped to
        // 409 for exhaustiveness.
        Err(
            e @ (RerunError::NotAManualGate(_)
            | RerunError::GateNotPending { .. }
            | RerunError::GateNotReached { .. }
            | RerunError::NotARejectableGate(_)),
        ) => Err(ApiError::Conflict(e.to_string())),
        Err(RerunError::Db(e)) => Err(ApiError::Db(e)),
    }
}

/// Rerun a step and its transitive descendants (ADR-0027 smart invalidation) —
/// **forks a new Take** (ADR-0056). The target and every step depending on it are
/// re-armed and re-run in dependency order; siblings and ancestors are left
/// as-is. Rejected `409` if the target's dependencies have not all succeeded (it
/// could not run).
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/steps/{step}/rerun",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id")
    ),
    responses(
        (status = 202, description = "rerun accepted — the body is the EXECUTED plan", body = RerunPlanResponse),
        (status = 404, description = "no such run or step"),
        (status = 409, description = "target's dependencies have not succeeded, or the run's creation failed")
    )
)]
async fn rerun_step(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<(StatusCode, Json<RerunPlanResponse>), ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    // Bind the principal (ADR-0056): a rerun is a Take boundary and the
    // event it emits carries WHO pressed it — the same attribution pattern as
    // gate approval.
    let principal = authorize_scoped(&st, &headers, Action::Write, scope.as_ref()).await?;
    let oracle = snapshot_oracle(&st);
    // The EXECUTED plan comes back as the body (git-bug 4afaa3e): the preview →
    // confirm window is a real TOCTOU (a snapshot can expire between the two),
    // and returning what actually happened lets the UI disclose a divergence
    // instead of silently showing a stale scope.
    let plan = rerun_outcome(
        scarab_engine::rerun_step_widened(
            &*st.db,
            &*st.clock,
            oracle.as_deref(),
            &run,
            &StepId(step),
            Some(principal.subject),
        )
        .await,
    )?;
    Ok((StatusCode::ACCEPTED, Json(plan_response(plan))))
}

/// The ADR-0061 s5 snapshot-availability oracle, when this deployment has a
/// workspace CAS wired. `None` (the local executor, which snapshots nothing)
/// means no presence check and therefore no widening — the pre-0061 behaviour,
/// which is correct there because there are no snapshots to expire.
///
/// Answers over `widening_cas` when one is wired (git-bug 4afaa3e amendment F1:
/// the handle whose warm-leg errors propagate, so a Depot outage reaches
/// `CasSnapshots`' transient-error guard as an ERROR — assumed present — and is
/// never laundered into a definitive cold-leg NotFound, i.e. "expired").
fn snapshot_oracle(st: &AppState) -> Option<Box<dyn scarab_engine::WorkspaceSnapshots>> {
    st.widening_cas
        .clone()
        .or_else(|| st.workspace_cas.clone())
        .map(|cas| {
            Box::new(retention::CasSnapshots(cas)) as Box<dyn scarab_engine::WorkspaceSnapshots>
        })
}

/// Retry a **Failed** step (ADR-0056 amendment) — another Attempt **in the
/// current Take** (no fork). Re-arms the target and its dependent cascade;
/// rejected `409` if the step is not Failed (use rerun instead).
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/steps/{step}/retry",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id")
    ),
    responses(
        (status = 202, description = "retry accepted — the body is the EXECUTED plan", body = RerunPlanResponse),
        (status = 404, description = "no such run or step"),
        (status = 409, description = "step is not failed, or the run's creation failed")
    )
)]
async fn retry_step(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<(StatusCode, Json<RerunPlanResponse>), ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    let principal = authorize_scoped(&st, &headers, Action::Write, scope.as_ref()).await?;
    let oracle = snapshot_oracle(&st);
    let plan = rerun_outcome(
        scarab_engine::retry_step_widened(
            &*st.db,
            &*st.clock,
            oracle.as_deref(),
            &run,
            &StepId(step),
            Some(principal.subject),
        )
        .await,
    )?;
    Ok((StatusCode::ACCEPTED, Json(plan_response(plan))))
}

/// `GET …/steps/{step}/rerun-plan` body: what a rerun/retry of this step would
/// actually execute — **before** the user confirms it (ADR-0061 s5, and
/// ADR-0027's rule that smart never means mysterious).
///
/// A read-only dry run. The interesting case is a widened one: a Run reopened
/// after its Workspace Snapshots expired cannot rerun just the step you pointed
/// at, because the inputs that step consumes no longer exist — so the scope
/// expands upstream to regenerate them, in the limit back to `clone`. That is a
/// bigger action than the button implies, so it has to be visible first.
#[derive(Debug, Serialize, ToSchema)]
pub struct RerunPlanResponse {
    /// The step the plan is for.
    pub target: String,
    /// Every step that would re-execute, sorted — the invalidation set after any
    /// widening.
    pub invalidated: Vec<String>,
    /// The subset of `invalidated` present only because a Workspace Snapshot
    /// expired: upstream steps dragged in to regenerate data. Empty on an
    /// ordinary rerun.
    pub widened: Vec<String>,
    /// The steps the rerun effectively **starts from** (no dependency inside the
    /// set). `[target]` on an ordinary rerun; on a widened one this is the phrase
    /// the affordance says out loud — "this re-runs from *clone*".
    pub starts_from: Vec<String>,
    /// Each expired input that caused the widening: the consuming step, the step
    /// that produced the missing snapshot, and its CAS root.
    pub expired_inputs: Vec<ExpiredInputDto>,
    /// The same set as `invalidated`, **execution-ordered** and carrying each
    /// member's WHY (git-bug 4afaa3e) — computed by the engine, identically for
    /// this preview and for the plan a POST rerun/retry executes and returns.
    pub steps: Vec<PlannedStepDto>,
}

/// Why a step is in a rerun plan (git-bug 4afaa3e).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanReasonDto {
    /// The step the human pointed at — always re-runs.
    Target,
    /// A transitive descendant of the target (ADR-0027 smart invalidation).
    Cascade,
    /// Dragged in upstream: its output Workspace Snapshot is gone (ADR-0061 s5).
    Regenerate,
    /// A descendant of a regenerate root the target's cascade did not contain.
    RegenerateCascade,
}

/// One member of a rerun plan, with its WHY (git-bug 4afaa3e).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PlannedStepDto {
    /// The step that re-runs (a gate re-arms and pauses instead of running).
    pub step: String,
    /// Why it is in the set.
    pub reason: PlanReasonDto,
    /// The in-set step this one is here because of: the cascade parent
    /// (minimum in-set need by id, deterministic) or, for `regenerate`, the
    /// consumer whose expired input dragged it in. Absent only on the target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub because_of: Option<String>,
    /// A gate (ADR-0008): the rerun pauses for approval here — the plan never
    /// claims a gate will run.
    pub is_gate: bool,
}

/// The one `RerunPlan → RerunPlanResponse` mapping — shared by the GET preview
/// and both POST bodies, so a shape drift between them is unrepresentable.
fn plan_response(plan: scarab_engine::RerunPlan) -> RerunPlanResponse {
    RerunPlanResponse {
        target: plan.target.0,
        invalidated: plan.invalidated.into_iter().map(|s| s.0).collect(),
        widened: plan.widened.into_iter().map(|s| s.0).collect(),
        starts_from: plan.starts_from.into_iter().map(|s| s.0).collect(),
        expired_inputs: plan
            .expired
            .into_iter()
            .map(|e| ExpiredInputDto {
                consumer: e.consumer.0,
                produced_by: e.produced_by.0,
                root: e.root,
            })
            .collect(),
        steps: plan
            .steps
            .into_iter()
            .map(|s| PlannedStepDto {
                step: s.step.0,
                reason: match s.reason {
                    scarab_engine::PlanReason::Target => PlanReasonDto::Target,
                    scarab_engine::PlanReason::Cascade => PlanReasonDto::Cascade,
                    scarab_engine::PlanReason::Regenerate => PlanReasonDto::Regenerate,
                    scarab_engine::PlanReason::RegenerateCascade => {
                        PlanReasonDto::RegenerateCascade
                    }
                },
                because_of: s.because_of.map(|b| b.0),
                is_gate: s.is_gate,
            })
            .collect(),
    }
}

/// One expired Workspace Snapshot behind a widened rerun (ADR-0061 s5).
#[derive(Debug, Serialize, ToSchema)]
pub struct ExpiredInputDto {
    /// The step whose input workspace is incomplete.
    pub consumer: String,
    /// The upstream step that produced the missing snapshot.
    pub produced_by: String,
    /// The CAS merkle root the store no longer holds.
    pub root: String,
}

/// Preview a rerun/retry of a step: the resolved scope, and whether expired
/// Workspace Snapshots widened it (ADR-0061 s5).
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/steps/{step}/rerun-plan",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id")
    ),
    responses(
        (status = 200, body = RerunPlanResponse),
        (status = 404, description = "no such run or step")
    )
)]
async fn rerun_plan(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<Json<RerunPlanResponse>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    // A preview mutates nothing, so Read is the right authority — the same
    // principal who can see the run can see what reruning it would cost.
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    let oracle = snapshot_oracle(&st);
    let plan = scarab_engine::plan_rerun(&*st.db, oracle.as_deref(), &run, &StepId(step))
        .await
        .map_err(|e| match e {
            RerunError::StepNotFound(_) => ApiError::NotFound,
            RerunError::Db(e) => ApiError::Db(e),
            other => ApiError::Conflict(other.to_string()),
        })?;
    Ok(Json(plan_response(plan)))
}

/// **Pin** this run's Workspace Snapshots (ADR-0061 s5): keep them past the cold
/// tier's retention TTL, so an investigation is not raced by the GC sweeper.
///
/// The pin acts on the cold tier's **time** bound only. It says nothing about the
/// warm workspace service, which is bounded by space and evicts least-recently-
/// used by design — a warm miss is slower, never wrong, so there is nothing there
/// to protect. Idempotent; recorded with who pinned it and when.
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/snapshots-pin",
    params(("id" = String, Path, description = "run id")),
    responses(
        (status = 200, body = SnapshotRetentionDto, description = "pinned"),
        (status = 404, description = "no such run")
    )
)]
async fn pin_run_snapshots(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<SnapshotRetentionDto>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    let principal = authorize_scoped(&st, &headers, Action::Write, scope.as_ref()).await?;
    if !scarab_engine::pin_run_snapshots(&*st.db, &*st.clock, &run, Some(principal.subject.clone()))
        .await?
    {
        return Err(ApiError::NotFound);
    }
    current_snapshot_retention(&st, &run).await
}

/// Release the [pin](pin_run_snapshots), returning this run's Workspace Snapshots
/// to the ordinary retention window. Idempotent.
#[utoipa::path(
    delete,
    path = "/v1/runs/{id}/snapshots-pin",
    params(("id" = String, Path, description = "run id")),
    responses(
        (status = 200, body = SnapshotRetentionDto, description = "pin released"),
        (status = 404, description = "no such run")
    )
)]
async fn unpin_run_snapshots(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<SnapshotRetentionDto>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    let principal = authorize_scoped(&st, &headers, Action::Write, scope.as_ref()).await?;
    if !scarab_engine::unpin_run_snapshots(
        &*st.db,
        &*st.clock,
        &run,
        Some(principal.subject.clone()),
    )
    .await?
    {
        return Err(ApiError::NotFound);
    }
    current_snapshot_retention(&st, &run).await
}

/// Re-read and project one run's retention promise — what both pin endpoints
/// return, so a client never has to re-fetch the whole run to see the effect.
async fn current_snapshot_retention(
    st: &AppState,
    run: &RunId,
) -> Result<Json<SnapshotRetentionDto>, ApiError> {
    let r = st
        .db
        .run_snapshot_retention(run)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(snapshot_retention_dto(
        &r,
        st.snapshot_retention_days,
        st.clock.now().await,
    )))
}

/// Cancel a run (ADR-0054): drive its non-terminal steps and the run to
/// `Cancelled` durably and enqueue the Pod-teardown intent the driver
/// executes (SIGTERM + grace via the backend). Idempotent — cancelling a
/// terminal run is a 409; an unknown run is a 404.
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/cancel",
    params(("id" = String, Path, description = "run id")),
    responses(
        (status = 202, description = "cancellation recorded; Pods tear down asynchronously"),
        (status = 404, description = "no such run"),
        (status = 409, description = "run already terminal")
    )
)]
async fn cancel_run(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    let principal = authorize_scoped(&st, &headers, Action::Write, scope.as_ref()).await?;
    let Some(status) = st.db.run_status(&run).await? else {
        return Err(ApiError::NotFound);
    };
    match scarab_engine::cancel_run_request(&*st.db, &*st.clock, &run, Some(principal.subject))
        .await
    {
        Ok(true) => Ok(StatusCode::ACCEPTED),
        Ok(false) => {
            // Known run, nothing to cancel: already terminal.
            let _ = status;
            Ok(StatusCode::CONFLICT)
        }
        Err(e) => Err(ApiError::BadRequest(e.to_string())),
    }
}

/// One registered project (governed repo, ADR-0046) in the repos list.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectDto {
    pub org: String,
    pub project: String,
    /// The forge coordinate backing it (1:1 in v1).
    pub owner: String,
    pub name: String,
    /// The forge WEB base for this repo (e.g. `https://github.com/owner/name`) —
    /// what the UI appends `/commit/<sha>` or `/pull/<n>` to for deep links.
    /// Derived from the connection's kind + base_url (the stored base is the API
    /// host, which differs from the web host on GitHub).
    pub repo_url: String,
    /// Epoch millis of the project's most recent run, or `null` if it has never
    /// run — the dashboard's recency signal (ADR-0046). The domain carries no
    /// push/created_at yet, so never-run repos have no ordering key here.
    pub last_run_at: Option<i64>,
}

/// The authenticated principal (ADR-0049) — powers the UI's identity menu.
#[derive(Debug, Serialize, ToSchema)]
pub struct MeResponse {
    /// Stable, forge-agnostic identity subject.
    pub subject: String,
    /// Human display name, when the identity provides one.
    pub display_name: Option<String>,
    /// The principal's Scarab-native roles (e.g. `["Owner"]`).
    pub roles: Vec<String>,
    /// May this principal administer org-level settings (ADR-0060)? The gate on
    /// the global Settings area — the UI hides the nav entry when false, since
    /// nothing there is actionable or informative without `Administer`.
    ///
    /// True for a globally-`Administer` role, or an `Admin`+ binding on the Org
    /// scope of any org the caller can see. Deliberately *not* implied by a
    /// Project-scoped `Administer`: administering one repo does not grant the
    /// org's secrets or its forge connections (ADR-0049 — Org inherits down,
    /// never up).
    pub can_administer: bool,
    /// The orgs this principal may administer, for the Settings area to act on.
    /// One entry in practice (single implicit Org, ADR-0060) — a list because
    /// `Org` remains the model's real inheritance root, not a collapsed
    /// constant. Empty on a fresh install: an org only exists once a Project is
    /// bound to it, so `can_administer` can be true with nothing to administer
    /// yet.
    pub admin_orgs: Vec<String>,
    /// The provider accounts this Principal can sign in with (ADR-0069).
    /// Empty when no identity store is wired (dev).
    pub identities: Vec<PrincipalIdentityDto>,
}

/// Return the current authenticated principal. In dev (auth disabled) this is
/// the synthetic Owner.
#[utoipa::path(
    get,
    path = "/v1/me",
    responses(
        (status = 200, body = MeResponse),
        (status = 401, description = "not authenticated")
    )
)]
async fn me(State(st): State<AppState>, headers: HeaderMap) -> Result<Json<MeResponse>, ApiError> {
    let caller = authenticate(&st, &headers, Action::Read).await?;
    let admin_orgs = administrable_orgs(&st, &caller).await?;
    let can_administer = caller.may_globally(Action::Administer) || !admin_orgs.is_empty();
    let identities = match st.identities.as_ref() {
        Some(store) => store
            .identities_of(&caller.principal.subject)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?
            .into_iter()
            .map(|i| PrincipalIdentityDto {
                provider: i.provider,
                handle: i.handle,
            })
            .collect(),
        None => Vec::new(),
    };
    // `roles` stays the caller's GLOBAL roles, which for a token caller is the
    // owner's C1 bootstrap and nothing else — so an API token never reports an
    // administrable org it could not actually act on.
    Ok(Json(MeResponse {
        subject: caller.principal.subject,
        display_name: caller.principal.display_name,
        roles: caller
            .principal
            .roles
            .iter()
            .map(|r| format!("{r:?}"))
            .collect(),
        can_administer,
        admin_orgs,
        identities,
    }))
}

/// The orgs `principal` may administer (ADR-0060), sorted and deduplicated.
///
/// An org exists only as the coordinate of a bound Project — there is no `orgs`
/// table (ADR-0046) — so the candidate set is exactly the orgs of the registry's
/// bindings. A globally-`Administer` role takes all of them; anyone else needs an
/// `Admin`+ binding on that specific `Scope::Org`.
async fn administrable_orgs(st: &AppState, caller: &Caller) -> Result<Vec<String>, ApiError> {
    let Some(connections) = st.connections.as_ref() else {
        return Ok(Vec::new()); // no registry wired (dev): no orgs exist yet
    };
    let mut orgs = std::collections::BTreeSet::new();
    let conns = connections
        .list_connections()
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    for conn in conns {
        let repos = connections
            .repos_of(&conn.id)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        for repo in repos {
            if let Some(resolved) = connections
                .resolve(&repo)
                .await
                .map_err(|e| ApiError::BadRequest(e.to_string()))?
            {
                orgs.insert(resolved.org);
            }
        }
    }
    if caller.may_globally(Action::Administer) {
        return Ok(orgs.into_iter().collect());
    }
    let mut allowed = Vec::new();
    for org in orgs {
        let scope = scarab_identity::Scope::Org(org.clone());
        if caller.may(st, Action::Administer, &scope).await? {
            allowed.push(org);
        }
    }
    Ok(allowed)
}

/// The forge WEB (html) base for a repo — what the UI appends `/commit/<sha>`
/// or `/pull/<n>` to. The connection's stored `base_url` is the API base, which
/// on GitHub differs from the web host (`api.github.com` vs `github.com`; a GHES
/// host carries an `/api/v3` suffix). Forgejo's `base_url` is already the web
/// host (its API lives under `/api/v1`).
fn web_repo_url(kind: scarab_forge::ForgeKind, base_url: &str, owner: &str, name: &str) -> String {
    format!("{}/{owner}/{name}", forge_web_host(kind, base_url))
}

/// The forge's **web** host derived from a connection's API `base_url` — what a
/// deep link out of the UI is built on (a repo page, or a GitHub App's
/// installation settings).
fn forge_web_host(kind: scarab_forge::ForgeKind, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match kind {
        scarab_forge::ForgeKind::GitHub => {
            if base.contains("api.github.com") {
                "https://github.com".to_string()
            } else {
                base.strip_suffix("/api/v3").unwrap_or(base).to_string()
            }
        }
        scarab_forge::ForgeKind::Forgejo => base.to_string(),
    }
}

/// List the registered projects (ADR-0046 registry — what the dashboard's
/// repo cards render). Scoped: global roles see all; otherwise only orgs/
/// projects the caller's bindings grant Read on.
#[utoipa::path(
    get,
    path = "/v1/repos",
    responses((status = 200, body = [ProjectDto]))
)]
async fn list_projects(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProjectDto>>, ApiError> {
    let caller = authenticate(&st, &headers, Action::Read).await?;
    let Some(connections) = st.connections.as_ref() else {
        return Ok(Json(Vec::new())); // no registry wired (dev): empty, honest
    };
    let conns = connections
        .list_connections()
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let mut out = Vec::new();
    for conn in conns {
        let repos = connections
            .repos_of(&conn.id)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        for repo in repos {
            let Some(resolved) = connections
                .resolve(&repo)
                .await
                .map_err(|e| ApiError::BadRequest(e.to_string()))?
            else {
                continue;
            };
            // Tenancy (ADR-0049): scoped callers see only their projects, and
            // an issued API token only those its own scope covers.
            if !caller.may_globally(Action::Read) {
                let scope = scarab_identity::Scope::Project {
                    org: resolved.org.clone(),
                    name: resolved.project.clone(),
                };
                if !caller.may(&st, Action::Read, &scope).await? {
                    continue;
                }
            }
            // Recency signal (ADR-0046): the tenant's most recent run, if any.
            let last_run_at = st
                .db
                .list_runs_for_tenant(&resolved.org, &resolved.project, &Default::default(), 1)
                .await?
                .first()
                .map(|r| r.created_at.0);
            let repo_url = web_repo_url(
                resolved.connection.kind,
                &resolved.connection.base_url,
                &repo.owner,
                &repo.name,
            );
            out.push(ProjectDto {
                org: resolved.org,
                project: resolved.project,
                owner: repo.owner,
                name: repo.name,
                repo_url,
                last_run_at,
            });
        }
    }
    // Most-recently-active first; never-run repos fall back to (org, project)
    // alphabetical (no push/created_at exists in the domain yet).
    out.sort_by(|a, b| {
        b.last_run_at
            .cmp(&a.last_run_at)
            .then_with(|| (&a.org, &a.project).cmp(&(&b.org, &b.project)))
    });
    Ok(Json(out))
}

/// A repo's most recent runs, newest first (ADR-0046) — the dashboard's per-repo
/// history and pass/fail chart source. Scoped to the repo's tenant.
#[utoipa::path(
    get,
    path = "/v1/repos/{org}/{repo}/runs",
    params(
        ("org" = String, Path, description = "org slug"),
        ("repo" = String, Path, description = "project (repo) name"),
        ("limit" = Option<u32>, Query, description = "max runs (default 50, capped 200)"),
        ("environment" = Option<String>, Query, description = "only runs governed by this Environment (ADR-0074); applied before `limit`; must not be empty"),
        ("deploy" = Option<bool>, Query, description = "true: only deploy Runs (pipeline or a Step marked `deploy: true`); false: only the rest; applied before `limit`")
    ),
    responses(
        (status = 200, body = RunListResponse),
        (status = 400, description = "`environment` present but empty, or `deploy` not a boolean")
    )
)]
async fn list_repo_runs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
    Query(q): Query<ListRepoRunsQuery>,
) -> Result<Json<RunListResponse>, ApiError> {
    let scope = scarab_identity::Scope::Project {
        org: org.clone(),
        name: repo.clone(),
    };
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    if q.environment.as_deref() == Some("") {
        return Err(ApiError::BadRequest(
            "`environment` must name an Environment; omit it to list every run".into(),
        ));
    }
    let limit = q.limit.unwrap_or(DEFAULT_RUNS_LIMIT).min(MAX_RUNS_LIMIT);
    let filter = scarab_engine::RunListFilter {
        environment: q.environment,
        deploy: q.deploy,
    };
    let runs = st
        .db
        .list_runs_for_tenant(&org, &repo, &filter, limit)
        .await?;
    Ok(Json(RunListResponse {
        runs: run_summaries(&st, runs).await?,
    }))
}

/// One artifact **version** in a run's list (ADR-0052, immutable per attempt
/// by ADR-0056). `step`/`attempt` are the publishing provenance (empty
/// strings on pre-ADR-0056 rows); `succeeded` is that attempt's verdict;
/// `of_record` marks the version the bare name-addressed download resolves
/// to — the latest successful version of that name.
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactDto {
    pub name: String,
    pub size: u64,
    pub content_type: String,
    pub step: String,
    pub attempt: String,
    pub succeeded: bool,
    pub of_record: bool,
}

/// The of-record version per name (ADR-0056): the latest **successful**
/// version — a consumer fetching by bare name must never silently receive a
/// failed attempt's partial file. `artifacts_of_run` returns rows
/// name-then-created_at ordered, so the last successful row per name wins.
fn of_record_index(
    artifacts: &[scarab_engine::ArtifactRecord],
) -> std::collections::HashMap<String, usize> {
    let mut idx = std::collections::HashMap::new();
    for (i, a) in artifacts.iter().enumerate() {
        if a.succeeded {
            idx.insert(a.meta.name.clone(), i);
        }
    }
    idx
}

/// List a run's artifact versions (ADR-0052/0056). Read at the run's tenant.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/artifacts",
    params(("id" = String, Path, description = "run id")),
    responses((status = 200, body = [ArtifactDto]), (status = 404, description = "no such run"))
)]
async fn list_artifacts(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<ArtifactDto>>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    if st.db.run_status(&run).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let artifacts = st.db.artifacts_of_run(&run).await?;
    let of_record = of_record_index(&artifacts);
    Ok(Json(
        artifacts
            .iter()
            .enumerate()
            .map(|(i, a)| ArtifactDto {
                name: a.meta.name.clone(),
                size: a.meta.size,
                content_type: a.meta.content_type.clone(),
                step: a.step.0.clone(),
                attempt: a.attempt.0.clone(),
                succeeded: a.succeeded,
                of_record: of_record.get(&a.meta.name) == Some(&i),
            })
            .collect(),
    ))
}

/// Version selector for an artifact download (ADR-0056): omitted → the
/// of-record resolution (latest successful version); `step`+`attempt` → that
/// exact version (how a Take view reads a shadowed or failed-attempt file).
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ArtifactVersionQuery {
    pub step: Option<String>,
    pub attempt: Option<String>,
}

/// Download one artifact's bytes (ADR-0052). Streams through the server (a
/// presigned-URL fast path can replace this when the store backend supports
/// signing). Read at the run's tenant; immutable content. The bare name
/// resolves to the latest SUCCESSFUL version (ADR-0056); `?step=&attempt=`
/// pins an exact version.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/artifacts/{name}",
    params(
        ("id" = String, Path, description = "run id"),
        ("name" = String, Path, description = "artifact name (may contain slashes)"),
        ArtifactVersionQuery
    ),
    responses((status = 200, description = "the artifact bytes"), (status = 404, description = "no such artifact"))
)]
async fn download_artifact(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, name)): Path<(String, String)>,
    Query(version): Query<ArtifactVersionQuery>,
) -> Result<Response, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    let store = st.artifact_store.as_ref().ok_or(ApiError::NotFound)?;
    let artifacts = st.db.artifacts_of_run(&run).await?;
    let artifact = match (&version.step, &version.attempt) {
        (Some(step), Some(attempt)) => artifacts
            .iter()
            .find(|a| a.meta.name == name && a.step.0 == *step && a.attempt.0 == *attempt),
        _ => of_record_index(&artifacts)
            .get(&name)
            .map(|&i| &artifacts[i]),
    }
    .ok_or(ApiError::NotFound)?;
    let bytes = store
        .get(&artifact.meta.object_key)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let content_type = artifact.meta.content_type.clone();
    let mut resp = bytes.into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&content_type) {
        resp.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, v);
    }
    Ok(resp)
}

/// One named result a step published (ADR-0041) — the `${{ outputs.<step>.<name> }}`
/// values, exposed read-only for the run detail Inspector. `type_name` is a
/// coarse JSON kind (string/number/bool/object/array/null) so the UI can badge it
/// without re-deriving.
#[derive(Debug, Serialize, ToSchema)]
pub struct StepResultDto {
    pub name: String,
    #[schema(value_type = Object)]
    pub value: serde_json::Value,
    pub type_name: String,
}

/// The coarse JSON kind of a result value, for the UI badge.
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// The `?attempt=` selector shared by the attempt-scoped evidence reads
/// (ADR-0056): omitted → the step's latest evidence; present → that attempt's
/// immutable copy.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AttemptQuery {
    pub attempt: Option<String>,
}

/// A step's named results (ADR-0041). The read side of the results-ingest
/// write path (`POST …/steps/{step}/results`): the Inspector's Results tab,
/// and the source the Outputs view derives from. Bare = latest evidence;
/// `?attempt=` = that attempt's immutable copy (ADR-0056). Read at the run's
/// tenant.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/steps/{step}/results",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id"),
        AttemptQuery
    ),
    responses((status = 200, body = [StepResultDto]), (status = 404, description = "no such run"))
)]
async fn get_step_results(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    Query(q): Query<AttemptQuery>,
) -> Result<Json<Vec<StepResultDto>>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    if st.db.run_status(&run).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let step = StepId(step);
    // `?attempt=` scopes to one attempt's immutable evidence (ADR-0056) — how
    // a Take view reads a superseded attempt; bare reads the latest evidence.
    let results = match q.attempt {
        Some(a) => st.db.attempt_results(&run, &step, &AttemptId(a)).await?,
        None => st.db.step_results(&run, &step).await?,
    };
    Ok(Json(
        results
            .into_iter()
            .map(|(name, value)| StepResultDto {
                type_name: json_type_name(&value).to_string(),
                name,
                value,
            })
            .collect(),
    ))
}

/// What an attempt consumed (ADR-0056): the map `upstream step id → attempt
/// id` stamped at its launch — the durable answer to "which generation of
/// `build` did `test` actually build on?" after a mid-run restart leaves the
/// run a patchwork of attempt generations. `attempt` names the attempt the
/// map belongs to; empty map = nothing recorded (no upstream evidence, or a
/// pre-ADR-0056 attempt).
#[derive(Debug, Serialize, ToSchema)]
pub struct ConsumedDto {
    pub attempt: String,
    pub consumed: std::collections::BTreeMap<String, String>,
}

/// An attempt's consumption map (ADR-0056). Bare = the attempt behind the
/// step's current evidence; `?attempt=` = that attempt. Read at the run's
/// tenant. Fetched lazily by the step pane's Outputs tab — deliberately NOT
/// part of the polled run-status DTO.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/steps/{step}/consumed",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id"),
        AttemptQuery
    ),
    responses((status = 200, body = ConsumedDto), (status = 404, description = "no such run or no attempt"))
)]
async fn get_attempt_consumed(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    Query(q): Query<AttemptQuery>,
) -> Result<Json<ConsumedDto>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    if st.db.run_status(&run).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let step = StepId(step);
    let attempt = match q.attempt {
        Some(a) => AttemptId(a),
        None => st
            .db
            .step_evidence_attempt(&run, &step)
            .await?
            .ok_or(ApiError::NotFound)?,
    };
    let consumed = st.db.attempt_consumed(&run, &step, &attempt).await?;
    Ok(Json(ConsumedDto {
        attempt: attempt.0,
        consumed,
    }))
}

/// One entry in a workspace directory listing — the merkle-tree children under a
/// step's output snapshot (ADR-0029).
#[derive(Debug, Serialize, ToSchema)]
pub struct WorkspaceEntryDto {
    pub name: String,
    /// `"dir"` (a sub-tree), `"file"` (a blob), or `"symlink"` (a blob whose
    /// content is the link target path, marked by the symlink file-type bits in
    /// the entry mode — ADR-0061 s7 keeps git's representation in the CAS).
    pub kind: String,
    /// The link target path — present only when `kind` is `"symlink"`. The
    /// target is reported verbatim from the snapshot; it may point outside the
    /// snapshot or at nothing (a dangling link).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// A directory listing within a step's output workspace snapshot.
#[derive(Debug, Serialize, ToSchema)]
pub struct WorkspaceListing {
    /// The path listed (empty string = snapshot root).
    pub path: String,
    /// `false` when the step produced no snapshot (e.g. a still-running step, a
    /// gate, or the local executor which doesn't snapshot). `entries` is empty.
    pub available: bool,
    pub entries: Vec<WorkspaceEntryDto>,
}

/// The `?path=` sub-path within a step's workspace snapshot (default = root),
/// plus the `?attempt=` evidence selector (ADR-0056).
#[derive(Debug, Deserialize)]
struct WorkspacePathQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    attempt: Option<String>,
}

/// Split a browse path into clean, non-empty segments — rejecting any `.`/`..`
/// so a listing/fetch can never escape the snapshot root.
fn workspace_segments(path: &str) -> Result<Vec<String>, ApiError> {
    let mut segs = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(ApiError::NotFound);
        }
        segs.push(part.to_string());
    }
    Ok(segs)
}

/// Walk `segments` down from `root`, returning the [`scarab_storage::TreeTarget`]
/// they name (a sub-tree or a blob) plus whether the final entry is a symlink
/// (a blob whose content is the link target path, ADR-0061 s7 — `false` for the
/// root itself), or `NotFound` if any segment is missing.
async fn workspace_walk(
    cas: &Arc<dyn scarab_storage::Cas>,
    root: scarab_storage::TreeHash,
    segments: &[String],
) -> Result<(scarab_storage::TreeTarget, bool), ApiError> {
    use scarab_storage::TreeTarget;
    let mut cursor = TreeTarget::Tree(root);
    let mut symlink = false;
    for seg in segments {
        let tree = match cursor {
            TreeTarget::Tree(h) => h,
            // A path component descends into a file — no such directory. A
            // symlink is a blob too, so a path THROUGH one is refused rather
            // than resolved (the snapshot walk never follows links).
            TreeTarget::Blob(_) => return Err(ApiError::NotFound),
        };
        let entries = cas
            .tree_entries(&tree)
            .await
            .map_err(|_| ApiError::NotFound)?;
        let entry = entries
            .into_iter()
            .find(|e| &e.name == seg)
            .ok_or(ApiError::NotFound)?;
        symlink = entry.is_symlink();
        cursor = entry.target;
    }
    Ok((cursor, symlink))
}

/// Read a step's output snapshot root hash, or `None` if it produced none.
/// With `attempt`, reads that attempt's immutable root (ADR-0056) instead of
/// the latest evidence.
async fn step_snapshot_root(
    st: &AppState,
    run: &RunId,
    step: &StepId,
    attempt: Option<&str>,
) -> Result<Option<scarab_storage::TreeHash>, ApiError> {
    let root = match attempt {
        Some(a) => {
            st.db
                .attempt_output(run, step, &AttemptId(a.to_string()))
                .await?
        }
        None => st.db.step_output(run, step).await?,
    };
    Ok(root.map(scarab_storage::TreeHash))
}

/// List a directory inside a step's output workspace snapshot (ADR-0029). The
/// live Pod workspace is gone once the step ends (`restartPolicy: Never`); what
/// survives is the content-addressed snapshot, walked read-only here. Read at
/// the run's tenant.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/steps/{step}/workspace",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id"),
        ("path" = Option<String>, Query, description = "sub-path within the snapshot (default = root)"),
        ("attempt" = Option<String>, Query, description = "attempt id — that attempt's immutable snapshot instead of the latest (ADR-0056)")
    ),
    responses((status = 200, body = WorkspaceListing), (status = 404, description = "no such run/path or browse disabled"))
)]
async fn list_workspace(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    Query(q): Query<WorkspacePathQuery>,
) -> Result<Json<WorkspaceListing>, ApiError> {
    use scarab_storage::TreeTarget;
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    let cas = st.workspace_cas.as_ref().ok_or(ApiError::NotFound)?;
    if st.db.run_status(&run).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let path = q.path.unwrap_or_default();
    let segments = workspace_segments(&path)?;
    let root = match step_snapshot_root(&st, &run, &StepId(step), q.attempt.as_deref()).await? {
        Some(r) => r,
        // No snapshot yet — a running step, a gate, or a non-snapshotting
        // backend. Report unavailable rather than 404 so the UI can explain.
        None => {
            return Ok(Json(WorkspaceListing {
                path,
                available: false,
                entries: Vec::new(),
            }))
        }
    };
    let tree = match workspace_walk(cas, root, &segments).await?.0 {
        TreeTarget::Tree(h) => h,
        // The path names a file, not a directory.
        TreeTarget::Blob(_) => return Err(ApiError::NotFound),
    };
    let mut entries: Vec<WorkspaceEntryDto> = Vec::new();
    for e in cas
        .tree_entries(&tree)
        .await
        .map_err(|_| ApiError::NotFound)?
    {
        // A symlink is a blob whose CONTENT is the link target path, marked by
        // the mode file-type bits (ADR-0061 s7, git-bug 1344d1d) — name it for
        // what the filesystem it represents held, never as a plain file. The
        // target blob is a path (tiny), so resolve it inline for the listing.
        let (kind, target) = match (&e.target, e.is_symlink()) {
            (TreeTarget::Tree(_), _) => ("dir", None),
            (TreeTarget::Blob(h), true) => {
                let target = cas
                    .get_blob(h)
                    .await
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).into_owned());
                ("symlink", target)
            }
            (TreeTarget::Blob(_), false) => ("file", None),
        };
        entries.push(WorkspaceEntryDto {
            name: e.name,
            kind: kind.to_string(),
            target,
        });
    }
    // Directories first, then lexicographic — a conventional file listing
    // (symlinks group with files).
    entries.sort_by(|a, b| (a.kind != "dir", &a.name).cmp(&(b.kind != "dir", &b.name)));
    Ok(Json(WorkspaceListing {
        path,
        available: true,
        entries,
    }))
}

/// Stream one file's bytes from a step's output workspace snapshot (ADR-0029).
/// Read at the run's tenant; immutable content.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/steps/{step}/workspace/file",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id"),
        ("path" = String, Query, description = "file path within the snapshot"),
        ("attempt" = Option<String>, Query, description = "attempt id — that attempt's immutable snapshot instead of the latest (ADR-0056)")
    ),
    responses(
        (status = 200, description = "the file bytes. For a symlink the body is the link \
            TARGET PATH (that is all the snapshot stores for it), flagged by the \
            `X-Scarab-Symlink: 1` response header — it is never the target's content"),
        (status = 404, description = "no such file or browse disabled")
    )
)]
async fn get_workspace_file(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    Query(q): Query<WorkspacePathQuery>,
) -> Result<Response, ApiError> {
    use scarab_storage::TreeTarget;
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    let cas = st.workspace_cas.as_ref().ok_or(ApiError::NotFound)?;
    let path = q.path.unwrap_or_default();
    let segments = workspace_segments(&path)?;
    if segments.is_empty() {
        return Err(ApiError::NotFound);
    }
    let root = step_snapshot_root(&st, &run, &StepId(step), q.attempt.as_deref())
        .await?
        .ok_or(ApiError::NotFound)?;
    let (blob, symlink) = match workspace_walk(cas, root, &segments).await? {
        (TreeTarget::Blob(h), symlink) => (h, symlink),
        (TreeTarget::Tree(_), _) => return Err(ApiError::NotFound),
    };
    let bytes = cas.get_blob(&blob).await.map_err(|_| ApiError::NotFound)?;
    // Serve text inline where it looks like UTF-8 (logs, source, configs); fall
    // back to octet-stream so a binary triggers a download rather than mojibake.
    let content_type = if std::str::from_utf8(&bytes).is_ok() {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    let mut resp = bytes.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(content_type),
    );
    // A symlink's blob is the link TARGET PATH, not the target's content
    // (ADR-0061 s7). Streaming it is honest — that IS everything the snapshot
    // stores for the entry — but only with the flag that says so, so a client
    // never presents a path as if it were file contents (git-bug 1344d1d).
    if symlink {
        resp.headers_mut().insert(
            axum::http::HeaderName::from_static("x-scarab-symlink"),
            axum::http::HeaderValue::from_static("1"),
        );
    }
    Ok(resp)
}

/// WebSocket: an interactive debug shell into a **running** step's Pod (the
/// debug surface). Gated behind `Administer` — exec is the most privileged
/// surface. Only a running step has a live Pod (they are `restartPolicy: Never`
/// and gone once done), so a terminal/pending step is refused. Bridges the
/// Pod's TTY to the socket: client text/binary → shell stdin, shell output →
/// client. Disabled (404) unless an attacher is wired (k8s executor only).
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/steps/{step}/attach",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id")
    ),
    responses(
        (status = 101, description = "WebSocket upgrade — interactive TTY into the running step's Pod"),
        (status = 400, description = "step is not running (no live Pod)"),
        (status = 404, description = "no such run/step, or attach disabled")
    )
)]
async fn attach_step(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Administer, scope.as_ref()).await?;
    let attacher = st.attacher.as_ref().ok_or(ApiError::NotFound)?.clone();
    let step_id = StepId(step);
    let sr = st
        .db
        .steps_of_run(&run)
        .await?
        .into_iter()
        .find(|s| s.step == step_id)
        .ok_or(ApiError::NotFound)?;
    if sr.status != StepStatus::Running {
        return Err(ApiError::BadRequest(
            "step is not running — a debug shell needs a live Pod".into(),
        ));
    }
    let io = attacher
        .attach(&sr)
        .await
        .map_err(|e| ApiError::BadRequest(format!("attach failed: {e}")))?;
    Ok(ws.on_upgrade(move |socket| bridge_attach(socket, io)))
}

/// Pump bytes both ways between a WebSocket and an attached step shell until
/// either end closes. `_process` in the `AttachIo` keeps the Pod exec alive for
/// the lifetime of this task.
async fn bridge_attach(socket: WebSocket, io: scarab_executor_k8s::AttachIo) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut ws_tx, mut ws_rx) = socket.split();
    let scarab_executor_k8s::AttachIo {
        mut output,
        mut input,
        _process,
    } = io;
    let mut buf = [0u8; 8192];
    loop {
        tokio::select! {
            read = output.read(&mut buf) => match read {
                Ok(0) | Err(_) => break, // shell exited or stream error
                Ok(n) => {
                    if ws_tx.send(Message::Binary(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                        break;
                    }
                }
            },
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if input.write_all(t.as_bytes()).await.is_err() { break; }
                    let _ = input.flush().await;
                }
                Some(Ok(Message::Binary(b))) => {
                    if input.write_all(&b).await.is_err() { break; }
                    let _ = input.flush().await;
                }
                Some(Ok(Message::Close(_))) | None => break,
                _ => {} // ping/pong handled by axum
            },
        }
    }
    // Hold the exec process until the pump ends.
    drop(_process);
}

/// WebSocket: reproduce a **finished** step in a fresh ephemeral Pod — its
/// image, its output workspace snapshot re-materialized at `/workspace` —
/// running `sleep` so the operator can shell in and debug (ADR-0039 world). The
/// live-attach surface needs a still-running step; this one works after the fact.
/// Gated behind `Administer`; the debug Pod is TTL-bounded and torn down when
/// the socket closes. Disabled (404) unless a debug launcher is wired (k8s only).
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/steps/{step}/debug-pod",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id")
    ),
    responses(
        (status = 101, description = "WebSocket upgrade — interactive TTY into a fresh reproduction Pod"),
        (status = 404, description = "no such run/step, no step spec, or debug-pod disabled"),
        (status = 409, description = "the step was inlined from a library version yanked in the run's org")
    )
)]
async fn debug_pod_step(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Administer, scope.as_ref()).await?;
    let launcher = st
        .debug_launcher
        .as_ref()
        .ok_or(ApiError::NotFound)?
        .clone();
    let step = StepId(step);
    let (sr, image, snapshot) = debug_pod_plan(st.db.as_ref(), &run, &step).await?;
    // ADR-0076 §6: a debug Pod runs the Step's image, so a Step inlined from
    // a yanked library version gets none, as a launch would not.
    refuse_yanked_step(&st, &run, &step, "no debug Pod starts").await?;
    Ok(ws.on_upgrade(move |mut socket| async move {
        let _ = socket
            .send(Message::Text(
                "provisioning debug pod (re-materializing workspace)…\r\n".into(),
            ))
            .await;
        match launcher
            .launch_debug(&sr, &image, snapshot.as_deref(), 3600)
            .await
        {
            Ok(dp) => {
                match launcher.attach_debug(&dp.name).await {
                    Ok(io) => {
                        let _ = socket.send(Message::Text("\r\n".into())).await;
                        bridge_attach(socket, io).await;
                    }
                    Err(e) => {
                        let _ = socket
                            .send(Message::Text(format!("attach failed: {e}\r\n").into()))
                            .await;
                    }
                }
                // Throwaway: tear the reproduction Pod down when the shell ends.
                let _ = launcher.teardown_debug(&dp.name).await;
            }
            Err(e) => {
                let _ = socket
                    .send(Message::Text(format!("debug pod failed: {e}\r\n").into()))
                    .await;
            }
        }
    }))
}

/// What a debug Pod reproduces of a finished step: the step, the image it ran
/// and its output snapshot. The image is the stored spec's, rendered the way
/// its launch rendered it (ADR-0041, ADR-0076): a stored image may hold
/// `${{ … }}`, which is a template, not an image.
async fn debug_pod_plan(
    db: &dyn Db,
    run: &RunId,
    step: &StepId,
) -> Result<(scarab_engine::StepRun, String, Option<String>), ApiError> {
    let sr = db
        .steps_of_run(run)
        .await?
        .into_iter()
        .find(|s| &s.step == step)
        .ok_or(ApiError::NotFound)?;
    let spec = db
        .step_spec(run, step)
        .await?
        .ok_or_else(|| ApiError::BadRequest("no step spec — cannot reproduce this step".into()))?;
    let spec = scarab_engine::render::render_launch_spec(db, run, step, spec)
        .await?
        .map_err(|reason| {
            ApiError::BadRequest(format!("the step's image does not render: {reason}"))
        })?;
    let snapshot = db.step_output(run, step).await?;
    Ok((sr, spec.image, snapshot))
}

/// In-repo directory holding pipeline definitions (ADR-0010). Every
/// `*.yaml`/`*.yml` directly under it is a pipeline, discovered and evaluated on
/// a trigger.
pub const CONFIG_DIR: &str = ".scarab";

/// Is `path` a pipeline definition file under [`CONFIG_DIR`] (a `.yaml`/`.yml`)?
fn is_pipeline_file(path: &str) -> bool {
    path.ends_with(".yaml") || path.ends_with(".yml")
}

/// Error building a triggered run from a forge event.
#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    #[error(transparent)]
    Forge(scarab_forge::ForgeError),
    #[error("pipeline: {0}")]
    Pipeline(String),
    #[error("config is not valid UTF-8")]
    NotUtf8,
    /// A target Environment's `require_reason` guardrail (ADR-0057 §3) blocked a
    /// reasonless human dispatch at the admission gate — fail-closed, no run
    /// created. Carries the joined violation(s), surfaced like a missing approval.
    #[error("{0}")]
    ReasonRequired(String),
    #[error(transparent)]
    Db(#[from] DbError),
}

/// The `(repo, ref, pipeline)` auto-cancel key for an event, or `None` for
/// events that shouldn't auto-cancel (cron/manual/api/…). Keyed by pipeline
/// (`pipeline` = its `.scarab/*.yaml` path) so a newer run of one pipeline
/// supersedes only older runs of the *same* pipeline on the same ref, not its
/// siblings (ADR-0011, 0032).
fn supersede_key(event: &scarab_forge::Event, pipeline: &str) -> Option<String> {
    use scarab_forge::Event;
    let repo = event.repo()?;
    let scope = match event {
        Event::Push { r#ref, .. } => r#ref.clone(),
        Event::PullRequest { number, .. } => format!("pr-{number}"),
        _ => return None,
    };
    Some(format!("{}/{}:{scope}:{pipeline}", repo.owner, repo.name))
}

/// The ref/SHA to read the pipeline config at — the event's commit where
/// available (immutable, ADR-0032), else a branch/tag ref.
fn config_ref(event: &scarab_forge::Event) -> String {
    use scarab_forge::Event;
    match event {
        Event::Push { after, .. } => after.clone(),
        Event::PullRequest { head, .. } => head.clone(),
        Event::Tag { tag, .. } | Event::Release { tag, .. } => format!("refs/tags/{tag}"),
        // A manual/api dispatch carries the resolved commit it runs against as
        // `sha` (ADR-0043): the config is read — and the Run pinned — at that
        // commit, exactly like Push reads at `after`. The symbolic `ref` is used
        // only for allowed_refs matching (see `Event::protection_ref`).
        Event::Manual { sha, .. } | Event::Api { sha, .. } => sha.clone(),
        _ => "HEAD".to_string(),
    }
}

/// The resolved commit an event pinned to, for the run's `origin_sha` — only
/// the events that carry a concrete commit. A `Tag`/`Release` normalizes to a
/// tag name (no SHA in the payload) and repo-less events have none, so both are
/// `None`; the symbolic ref still lands in `origin_ref` via [`Event::protection_ref`].
fn origin_sha(event: &scarab_forge::Event) -> Option<String> {
    use scarab_forge::Event;
    match event {
        Event::Push { after, .. } => Some(after.clone()),
        Event::PullRequest { head, .. } => Some(head.clone()),
        Event::Manual { sha, .. } | Event::Api { sha, .. } => Some(sha.clone()),
        _ => None,
    }
}

/// The pull-request number for a `pull_request` event, for `origin_pr_number`.
fn origin_pr_number(event: &scarab_forge::Event) -> Option<i64> {
    match event {
        scarab_forge::Event::PullRequest { number, .. } => Some(*number as i64),
        _ => None,
    }
}

/// The PR **base** branch for a `pull_request` event, for `origin_pr_base`
/// (ADR-0057, the `base ← head` display). `None` for every other kind, and when
/// the payload carried no base ref.
fn origin_pr_base(event: &scarab_forge::Event) -> Option<String> {
    match event {
        scarab_forge::Event::PullRequest { base, .. } if !base.is_empty() => Some(base.clone()),
        _ => None,
    }
}

/// "Commit a file, done" (ADR-0010): on a normalized `event`, discover every
/// pipeline under `.scarab/` at the triggering ref, compile each, and durably
/// create a Run for each whose `on:` matches this event. Returns the new run ids
/// (empty when there is no config, or no pipeline's trigger matches).
///
/// Pipelines are evaluated independently and in a deterministic (path-sorted)
/// order; a file that fails to compile fails the whole trigger (a broken
/// pipeline is a submit-time error, ADR-0009), so a repo's CI is all-or-nothing
/// per delivery rather than silently partial. A pipeline that admission
/// **refuses** (a disallowed ref, an unknown Environment, an ineligible
/// Project — ADR-0037, ADR-0071) is skipped on its own and never aborts the
/// delivery: the other pipelines still run.
///
/// `ports` are the stores admission reads ([`AdmissionPorts`]).
///
/// The run ids only; [`trigger_runs_from_event`] also reports the
/// pipelines the Environment or a Policy refused.
pub async fn trigger_run_from_event(
    forge: &dyn scarab_forge::ForgePort,
    db: &dyn Db,
    clock: &dyn Clock,
    ports: AdmissionPorts<'_>,
    event: &scarab_forge::Event,
) -> Result<Vec<RunId>, TriggerError> {
    Ok(trigger_runs_from_event(forge, db, clock, ports, event)
        .await?
        .runs)
}

/// The stores Run admission reads, beside the run store itself. Each `None`
/// is a hand-built state (a test, or the inline dev stack); `main.rs` wires
/// both.
#[derive(Clone, Copy, Default)]
pub struct AdmissionPorts<'a> {
    /// Org-level Environment governance (ADR-0071). `None`: a deploy is
    /// admitted as an undefined environment was before ADR-0071
    /// ([`GovernedRules::Unwired`]).
    pub governance: Option<&'a dyn scarab_project::EnvironmentGovernanceStore>,
    /// Stored Policies (ADR-0075), consulted at `run.create`. `None`: no
    /// Policy is consulted and no decision recorded.
    pub policies: Option<&'a dyn scarab_policy::store::PolicyStore>,
    /// The org's installed libraries (ADR-0076): compiling resolves `lib:`
    /// against them, and admission refuses a yanked version. `None`: no
    /// `lib:` target resolves, and nothing is checked for yanks.
    pub libraries: Option<&'a dyn scarab_policy::library::LibraryStore>,
    /// The org's plugin installations (ADR-0076 §7): compiling resolves
    /// `plugin:<installation>/<gate>` against the gates of those that may
    /// resolve gates. `None`: no `plugin:` target resolves.
    pub plugins: Option<&'a dyn scarab_plugin::PluginStore>,
    /// What links a webhook actor to a Principal (ADR-0069) for the
    /// `run.create` input. Unset: a webhook Run has no `principal`.
    pub links: ActorLinks<'a>,
    /// The forge connection registry, which `event.repo.clone_url` is
    /// computed from at Run creation (ADR-0076 §9). Unset: a repo is read
    /// from public github.com.
    pub connections: Option<&'a dyn scarab_forge::ForgeConnectionStore>,
}

/// The stores that resolve a webhook actor to a Principal (ADR-0069): the
/// forge connection serving the repo, the login provider whose accounts are
/// that forge's, and the identity pinned by the actor's immutable id.
#[derive(Clone, Copy, Default)]
pub struct ActorLinks<'a> {
    pub connections: Option<&'a dyn scarab_forge::ForgeConnectionStore>,
    pub identities: Option<&'a dyn scarab_identity::IdentityStore>,
    pub providers: &'a [Arc<oauth::OAuthAuthenticator>],
}

impl ActorLinks<'_> {
    /// The Principal linked to `event`'s actor: `None` for a dispatch (its
    /// actor is already a Principal, ADR-0075 §2), for an actor with no
    /// immutable id, and when the repo has no forge connection, the forge no
    /// login provider, or the account no Principal. A store failure is an
    /// error, so a Policy that names Principals is never evaluated as if the
    /// actor had none.
    async fn principal_of(
        &self,
        event: &scarab_forge::Event,
        repo: &scarab_forge::RepoRef,
    ) -> Result<Option<String>, TriggerError> {
        let (Some(actor_id), Some(connections), Some(identities)) =
            (event.actor_id(), self.connections, self.identities)
        else {
            return Ok(None);
        };
        let store_err = |what: &str, e: String| {
            TriggerError::Db(DbError::Other(format!(
                "{what}: {e}; the Run was not created because its actor could not be resolved"
            )))
        };
        let Some(resolved) = connections
            .resolve(repo)
            .await
            .map_err(|e| store_err("forge connections", e.to_string()))?
        else {
            return Ok(None);
        };
        let base_url = resolved.connection.base_url.trim_end_matches('/');
        let Some(provider) = self
            .providers
            .iter()
            .find(|p| p.forge_url() == Some(base_url))
        else {
            return Ok(None);
        };
        let Some(pinned) = identities
            .pinned(provider.provider_id(), actor_id)
            .await
            .map_err(|e| store_err("identities", e.to_string()))?
        else {
            return Ok(None);
        };
        scarab_identity::resolve_principal(identities, &pinned)
            .await
            .map_err(|e| store_err("identities", e.to_string()))
    }
}

impl<'a> AdmissionPorts<'a> {
    /// Environment governance only; no Policies.
    pub fn governance(
        governance: Option<&'a dyn scarab_project::EnvironmentGovernanceStore>,
    ) -> Self {
        AdmissionPorts {
            governance,
            policies: None,
            libraries: None,
            plugins: None,
            links: ActorLinks::default(),
            connections: None,
        }
    }

    /// The same ports, resolving `lib:` against `libraries` (ADR-0076).
    pub fn with_libraries(
        mut self,
        libraries: &'a dyn scarab_policy::library::LibraryStore,
    ) -> Self {
        self.libraries = Some(libraries);
        self
    }

    /// The same ports, consulting `policies` at `run.create`.
    pub fn with_policies(mut self, policies: &'a dyn scarab_policy::store::PolicyStore) -> Self {
        self.policies = Some(policies);
        self
    }
}

/// A pipeline a delivery did not run because its Environment does not exist
/// in the Org or does not admit this Project (ADR-0071 §2, §10), or because a
/// `run.create` Policy denied it (ADR-0075). Reported in the webhook delivery
/// body: for a pipeline whose name the ADR-0071 migration could never see (it
/// had never run), this is the only place the refusal surfaces besides the
/// server log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefusedPipeline {
    /// The pipeline file, e.g. `.scarab/deploy.yaml`.
    pub pipeline: String,
    /// The Environment it named; absent for a Policy refusal of a Pipeline
    /// that names none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// What is missing and who can fix it, or what the Policies denied.
    pub reason: String,
    /// The recorded PolicyDecision, in the org's decision log, for a Policy
    /// refusal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    /// Each Policy deny message, naming its Policy, for a Policy refusal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
}

/// What one delivery did: the Runs it created and the deploy pipelines the
/// Environment refused. A `RefNotAllowed` skip is not listed — a
/// branch-scoped Environment ignoring other branches is the rule working.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TriggerOutcome {
    pub runs: Vec<RunId>,
    pub refused: Vec<RefusedPipeline>,
}

/// [`trigger_run_from_event`], also reporting every pipeline refused as
/// `UnknownEnvironment` / `NotEligible` or by a Policy.
pub async fn trigger_runs_from_event(
    forge: &dyn scarab_forge::ForgePort,
    db: &dyn Db,
    clock: &dyn Clock,
    ports: AdmissionPorts<'_>,
    event: &scarab_forge::Event,
) -> Result<TriggerOutcome, TriggerError> {
    // Repo-less events (cron/manual/api) don't carry in-repo config here.
    let Some(repo) = event.repo() else {
        return Ok(TriggerOutcome::default());
    };
    let git_ref = config_ref(event);

    // Discover the pipeline files under `.scarab/`. An absent directory yields an
    // empty listing → nothing to run (not an error).
    let entries = match forge.list_dir_at_ref(repo, &git_ref, CONFIG_DIR).await {
        Ok(e) => e,
        Err(scarab_forge::ForgeError::Api(_)) => return Ok(TriggerOutcome::default()),
        Err(e) => return Err(TriggerError::Forge(e)),
    };
    let mut paths: Vec<String> = entries
        .into_iter()
        .filter(|p| is_pipeline_file(p))
        .collect();
    paths.sort();

    let ctx = event.context();
    let kind = event.trigger_kind();
    let mut outcome = TriggerOutcome::default();
    // The org's installed libraries, read once for the whole delivery.
    let installed = installed_libraries(ports.libraries, ports.plugins, &repo.owner)
        .await
        .map_err(TriggerError::Db)?;
    for path in &paths {
        let bytes = match forge.read_file_at_ref(repo, &git_ref, path).await {
            Ok(b) => b,
            // A listed file that vanished between list and read → skip it.
            Err(scarab_forge::ForgeError::Api(_)) => continue,
            Err(e) => return Err(TriggerError::Forge(e)),
        };
        let yaml = String::from_utf8(bytes).map_err(|_| TriggerError::NotUtf8)?;
        // A library (`kind: library`, ADR-0076 §2) runs only where it is
        // invoked, wherever it sits under `.scarab/`.
        if scarab_pipeline::is_library(&yaml) {
            continue;
        }

        // Pre-fetch transitive `invoke:` libraries at this ref and compile
        // (ADR-0038) — the shared read-at-ref → compile primitive also used by
        // the manual/api dispatch path.
        let ir = record_compile_failure(
            db,
            clock,
            event,
            path,
            prefetch_libs_and_compile(forge, repo, &git_ref, &yaml, &installed).await,
        )
        .await?;

        let matched = scarab_pipeline::matches_trigger(&ir, kind.as_str(), &ctx)
            .map_err(|e| TriggerError::Pipeline(e.to_string()))?;
        if !matched {
            continue;
        }

        // Step-level `when:` guards against the event context (ADR-0009, 0033):
        // guarded-off steps are kept in the DAG but marked Skipped, so the engine
        // transitively skips their descendants. A pipeline whose every step is
        // excluded starts no run.
        let excluded = scarab_pipeline::excluded_steps(&ir, &ctx)
            .map_err(|e| TriggerError::Pipeline(e.to_string()))?;
        if excluded.len() == ir.steps.len() {
            continue;
        }

        // Resolve governance, refuse what may not deploy, admit privilege
        // grants, and durably create the run — the shared admission primitive
        // the dispatch path also rides (ADR-0037/0039, ADR-0071). A refusal
        // skips this pipeline only (the dispatch path turns the same refusal
        // into a fail-closed error). Webhook-triggered runs supply no launch
        // parameters.
        match admit_and_create_run(forge, db, clock, ports, event, &ir, path, &excluded, None)
            .await?
        {
            Admission::Created(run) => outcome.runs.push(run),
            // A branch-scoped Environment ignoring every other branch is the
            // rule working, not news: silent, as it always was.
            Admission::Refused(AdmissionRefusal::RefNotAllowed) => {}
            // ADR-0071 §2, §10: an Environment that does not exist or does not
            // admit this Project is a configuration gap an operator must see.
            // Logged AND returned: the delivery body is where a forge's
            // "recent deliveries" view shows it, and for a name the migration
            // could not see (never run before ADR-0071) it is the first sign.
            Admission::Refused(
                ref refusal @ (AdmissionRefusal::UnknownEnvironment { ref environment }
                | AdmissionRefusal::NotEligible { ref environment }),
            ) => {
                let reason = admission_refusal_message(refusal, &repo.owner, &repo.name);
                tracing::warn!(
                    org = %repo.owner,
                    project = %repo.name,
                    pipeline = %path,
                    trigger = event.trigger_kind().as_str(),
                    "deploy pipeline not run: {reason}"
                );
                outcome.refused.push(RefusedPipeline {
                    pipeline: path.clone(),
                    environment: Some(environment.clone()),
                    reason,
                    decision_id: None,
                    messages: Vec::new(),
                });
            }
            // ADR-0075 §9: a `run.create` Policy denied this Run. Its
            // decision is in the org's decision log; the body names it and
            // carries every message.
            Admission::Refused(
                ref refusal @ AdmissionRefusal::PolicyDenied {
                    ref decision_id,
                    ref messages,
                },
            ) => {
                let reason = admission_refusal_message(refusal, &repo.owner, &repo.name);
                tracing::warn!(
                    org = %repo.owner,
                    project = %repo.name,
                    pipeline = %path,
                    trigger = event.trigger_kind().as_str(),
                    decision = %decision_id,
                    "pipeline not run: {reason}"
                );
                outcome.refused.push(RefusedPipeline {
                    pipeline: path.clone(),
                    environment: ir.environment.clone(),
                    reason,
                    decision_id: Some(decision_id.clone()),
                    messages: messages.clone(),
                });
            }
            // ADR-0076 §6: a yanked library version, refused whatever the
            // Policies say.
            Admission::Refused(
                ref refusal @ (AdmissionRefusal::EventField { ref messages }
                | AdmissionRefusal::ForgeToken { ref messages }),
            ) => {
                let reason = admission_refusal_message(refusal, &repo.owner, &repo.name);
                tracing::warn!(
                    org = %repo.owner,
                    project = %repo.name,
                    pipeline = %path,
                    "pipeline not run: {reason}"
                );
                outcome.refused.push(RefusedPipeline {
                    pipeline: path.clone(),
                    environment: ir.environment.clone(),
                    reason,
                    decision_id: None,
                    messages: messages.clone(),
                });
            }
            Admission::Refused(ref refusal @ AdmissionRefusal::LibraryYanked { ref messages }) => {
                let reason = admission_refusal_message(refusal, &repo.owner, &repo.name);
                tracing::warn!(
                    org = %repo.owner,
                    project = %repo.name,
                    pipeline = %path,
                    "pipeline not run: {reason}"
                );
                outcome.refused.push(RefusedPipeline {
                    pipeline: path.clone(),
                    environment: ir.environment.clone(),
                    reason,
                    decision_id: None,
                    messages: messages.clone(),
                });
            }
        }
    }
    Ok(outcome)
}

/// Pre-fetch a pipeline's transitive `invoke:` library sources at `read_ref`
/// (ADR-0038) and compile the authored `yaml` against them. Compilation is pure;
/// the I/O — fetching the referenced `.scarab/**` sources at the ref — happens
/// here. `invoke_refs` returns only path-safe keys; the fetch is **transitive**
/// (a library referenced by a library is fetched too) via a `seen`-guarded
/// worklist so an invoke cycle terminates (the cycle itself is reported by
/// compile). A library that vanished between list and read surfaces as a compile
/// diagnostic ("no library found at …"), not a fetch error.
///
/// `lib:` targets resolve against `installed`, the org's installed libraries
/// ([`installed_libraries`]); nothing is fetched for them (ADR-0076 §1).
///
/// Shared by [`trigger_run_from_event`] (looping over discovered pipelines) and
/// [`dispatch_run`] (one named pipeline) so both produce byte-identical IR.
async fn prefetch_libs_and_compile(
    forge: &dyn scarab_forge::ForgePort,
    repo: &scarab_forge::RepoRef,
    read_ref: &str,
    yaml: &str,
    installed: &scarab_pipeline::Libraries,
) -> Result<scarab_pipeline::PipelineIr, TriggerError> {
    let mut libs = std::collections::BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut worklist: Vec<String> = scarab_pipeline::invoke_refs(yaml);
    while let Some(lib_path) = worklist.pop() {
        if !seen.insert(lib_path.clone()) {
            continue;
        }
        match forge.read_file_at_ref(repo, read_ref, &lib_path).await {
            Ok(bytes) => {
                let src = String::from_utf8(bytes).map_err(|_| TriggerError::NotUtf8)?;
                worklist.extend(scarab_pipeline::invoke_refs(&src));
                libs.insert(lib_path, src);
            }
            Err(scarab_forge::ForgeError::Api(_)) => continue,
            Err(e) => return Err(TriggerError::Forge(e)),
        }
    }
    let libraries = scarab_pipeline::Libraries {
        files: libs,
        ..installed.clone()
    };
    let (ir, warnings) = compile_bounded(yaml.to_string(), libraries)
        .await
        .map_err(TriggerError::Pipeline)?;
    // Non-fatal lint diagnostics (ADR-0045), surfaced on the compile path —
    // warnings only, never failures.
    for warning in warnings {
        tracing::warn!(lint = %warning, "pipeline lint");
    }
    Ok(ir)
}

/// What `lib:` and `plugin:` resolve to in `org` (ADR-0076 §4, §7): its
/// installed libraries, and the gates of its plugin installations that may
/// resolve gates. `files` is left empty for the caller. A store failure is an
/// error, so a Pipeline is never compiled as if something were not
/// installed.
pub(crate) async fn installed_libraries(
    libraries: Option<&dyn scarab_policy::library::LibraryStore>,
    plugins: Option<&dyn scarab_plugin::PluginStore>,
    org: &str,
) -> Result<scarab_pipeline::Libraries, DbError> {
    let installed = match libraries {
        Some(l) => l
            .installed_for_compile(org)
            .await
            .map_err(|e| DbError::Other(format!("library store: {e}")))?,
        None => Default::default(),
    };
    let plugin_gates = match plugins {
        Some(p) => plugins::gates_for_compile(p, org).await?,
        None => Default::default(),
    };
    Ok(scarab_pipeline::Libraries {
        files: Default::default(),
        installed,
        plugin_gates,
    })
}

/// The longest the server waits for one Pipeline compile.
pub(crate) const COMPILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Compile `yaml` against `libraries` (the `file:` sources read at the
/// caller's ref and the org's installed libraries) and lint it, on a
/// blocking thread so a compile never holds an async worker, waiting at
/// most [`COMPILE_TIMEOUT`]. Every compile the server runs goes through
/// here. The compiler bounds its own work (source and IR size, matrix and
/// invoke limits), so a compile that times out still ends; its thread is
/// left to finish and its result dropped. A compile error, a timeout and a
/// panic in the compiler are each an `Err` with the reason.
pub(crate) async fn compile_bounded(
    yaml: String,
    libraries: scarab_pipeline::Libraries,
) -> Result<(scarab_pipeline::PipelineIr, Vec<String>), String> {
    let compiled = off_runtime(COMPILE_TIMEOUT, move || {
        scarab_pipeline::compile_with_libraries(&yaml, &libraries).map(|ir| {
            let warnings = scarab_pipeline::lint(&ir);
            (ir, warnings)
        })
    })
    .await
    .map_err(|why| format!("the pipeline compiler {why}"))?;
    compiled.map_err(|e| e.to_string())
}

/// Run `f` on a blocking thread, waiting at most `timeout`. `Err` says why
/// there is no result: `f` panicked, or did not finish in time (its thread
/// is left to finish).
async fn off_runtime<T: Send + 'static>(
    timeout: std::time::Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("stopped: {e}")),
        Err(_) => Err(format!(
            "did not finish within {} seconds",
            timeout.as_secs_f64()
        )),
    }
}

/// Why admission refused to create a Deploy Run. Every refusal is decided
/// **before** the Run is persisted, so a refused Run never launches a Pod or
/// resolves a secret (ADR-0071 §2: eligibility is checked at Run creation).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AdmissionRefusal {
    /// The event's symbolic ref is not allowed by the effective allowed refs
    /// (ADR-0037), or it has none and some layer restricts refs.
    RefNotAllowed,
    /// The pipeline names an Environment its Org does not have. Before
    /// ADR-0071 such a name deployed with no rules at all; §10 ends that
    /// coexistence — the migration created an Environment for every name it
    /// could see, so this is a name no Run had used before.
    UnknownEnvironment { environment: String },
    /// The Environment exists but its EnvironmentProjectEligibility does not
    /// admit this Project (ADR-0071 §2).
    NotEligible { environment: String },
    /// A `run.create` Policy denied the Run (ADR-0075). `decision_id` is the
    /// recorded PolicyDecision; `messages` name the Policy each came from.
    PolicyDenied {
        decision_id: String,
        messages: Vec<String>,
    },
    /// The Run uses a library version yanked in its org (ADR-0076 §6), a
    /// PolicyInvariant: checked whether or not any Policy exists, and no
    /// Policy can allow it. Each message names the version and the reason.
    LibraryYanked { messages: Vec<String> },
    /// The Pipeline reads an `event` field the trigger does not have, or
    /// one whose value fails its check (ADR-0076 §9).
    EventField { messages: Vec<String> },
    /// A Step asks for a forge credential it may not have (ADR-0076 §9), a
    /// PolicyInvariant like a yank.
    ForgeToken { messages: Vec<String> },
}

/// What admission fixed for a Run besides its governance (ADR-0076 §9),
/// which [`persist_run_from_ir`] stores.
struct AdmittedFacts<'a> {
    /// The checked `event` values every launch renders.
    event: &'a serde_json::Map<String, serde_json::Value>,
    /// Steps that hold a long-lived forge credential only because their
    /// library is trusted, each with that library's content hash: they
    /// consume no workspace.
    trusted_only: &'a std::collections::BTreeMap<String, String>,
}

/// The outcome of admitting one compiled pipeline (ADR-0037, ADR-0071 §2):
/// the Run created, or why not. A missing `require_reason` reason is an
/// error rather than a refusal — an answerable request, surfaced like a
/// missing approval (ADR-0057 §3).
#[derive(Debug)]
enum Admission {
    Created(RunId),
    Refused(AdmissionRefusal),
}

/// Governance resolved once for a run from its `environment:` reference
/// (ADR-0037/0039, ADR-0071), so `ir.environment` is unpacked a single time and
/// every downstream admission/persistence site reads intent instead of
/// re-deriving it.
///
/// The variant is the **deploy vs ordinary-CI** discriminator: targeting an
/// Environment makes a run a deploy (opts out of newest-wins auto-cancel,
/// ADR-0032, and records a deploy context). What governs the deploy is the
/// nested [`GovernedRules`].
// Built once per admitted pipeline and dropped with it, never held in bulk,
// so the size gap between the variants costs nothing worth a Box.
#[allow(clippy::large_enum_variant)]
enum RunGovernance {
    /// No `environment:` referenced: an ordinary CI run. Permissive on refs,
    /// fail-closed on governed grants, eligible for auto-cancel.
    Ungoverned,
    /// The pipeline targets an Environment — a deploy — that admitted this
    /// Project.
    Governed {
        environment: String,
        /// A fork PR locked out of this environment's secrets is also locked out
        /// of its governed privilege grants (ADR-0039).
        locked_out: bool,
        rules: GovernedRules,
    },
}

/// What a Deploy Run is governed by.
enum GovernedRules {
    /// No governance store is wired. Only a hand-built `AppState` (a test,
    /// or the inline dev stack) can be in this state — `main.rs` always wires
    /// the Postgres store. It decides exactly as an environment that was
    /// referenced but undefined did before ADR-0071: any ref, no reason
    /// check, no governed grants and no raw overlay.
    Unwired,
    /// The Environment's baseline merged with this Project's overlay
    /// (ADR-0071 §3), and the Project's own privilege grant there (§8 —
    /// never merged with anything).
    Resolved {
        protection: scarab_project::EffectiveProtection,
        privilege_grant: Option<scarab_project::EnvironmentProjectPrivilegeGrant>,
    },
}

/// The privilege grant a step's request is decided against (ADR-0039,
/// ADR-0071 §8): `None` when the run targets no Environment that can grant
/// anything (ordinary CI, an inline API run, the unwired legacy path) —
/// governed grants then "require a target Environment" — and `Some(grant)`
/// when it does, `grant` being the Project's grant there, if it holds one.
type PrivilegeGrantScope<'a> = Option<Option<&'a scarab_project::EnvironmentProjectPrivilegeGrant>>;

impl RunGovernance {
    /// The effective protection rules, when an Environment resolved them.
    /// `None` for an ordinary run and for the unwired legacy path — both
    /// permissive on refs.
    fn protection(&self) -> Option<&scarab_project::EffectiveProtection> {
        match self {
            RunGovernance::Governed {
                rules: GovernedRules::Resolved { protection, .. },
                ..
            } => Some(protection),
            _ => None,
        }
    }

    /// Where this run's step privileges are decided ([`PrivilegeGrantScope`]).
    fn privilege_grant_scope(&self) -> PrivilegeGrantScope<'_> {
        match self {
            RunGovernance::Governed {
                rules:
                    GovernedRules::Resolved {
                        privilege_grant, ..
                    },
                ..
            } => Some(privilege_grant.as_ref()),
            _ => None,
        }
    }

    /// Whether a fork PR is locked out of the target environment's secrets and,
    /// by extension, its governed privilege grants (ADR-0039).
    fn locked_out(&self) -> bool {
        matches!(
            self,
            RunGovernance::Governed {
                locked_out: true,
                ..
            }
        )
    }
}

/// A governance store failure: the store is down or holds a row this binary
/// cannot read. A 500, never a silent "no rules" — that would be fail-open.
fn governance_store_err(e: scarab_project::OrgEnvironmentError) -> TriggerError {
    TriggerError::Db(DbError::Other(format!("environment governance store: {e}")))
}

/// Resolve a run's [`RunGovernance`] from the pipeline's `environment:`
/// reference — or the reason it may not deploy at all — reading the
/// Environment, the Project's eligibility, overlay and privilege grant in one
/// consistent read ([`EnvironmentGovernanceStore::deploy_governance`]).
///
/// **Fail-closed** (ADR-0071 §2, §10): a name the Org has no Environment for
/// is [`AdmissionRefusal::UnknownEnvironment`], and an Environment that does
/// not admit this Project is [`AdmissionRefusal::NotEligible`] — both before
/// anything is persisted. Only an unwired store ([`GovernedRules::Unwired`])
/// keeps the pre-ADR-0071 permissive reading.
///
/// [`EnvironmentGovernanceStore::deploy_governance`]: scarab_project::EnvironmentGovernanceStore::deploy_governance
async fn resolve_governance(
    store: Option<&dyn scarab_project::EnvironmentGovernanceStore>,
    event: &scarab_forge::Event,
    ir: &scarab_pipeline::PipelineIr,
    repo: &scarab_forge::RepoRef,
) -> Result<Result<RunGovernance, AdmissionRefusal>, TriggerError> {
    let Some(env_name) = &ir.environment else {
        return Ok(Ok(RunGovernance::Ungoverned));
    };
    let rules = match store {
        None => GovernedRules::Unwired,
        Some(store) => {
            let Some(governance) = store
                .deploy_governance(&repo.owner, &repo.name, env_name)
                .await
                .map_err(governance_store_err)?
            else {
                return Ok(Err(AdmissionRefusal::UnknownEnvironment {
                    environment: env_name.clone(),
                }));
            };
            if !governance.eligible {
                return Ok(Err(AdmissionRefusal::NotEligible {
                    environment: env_name.clone(),
                }));
            }
            GovernedRules::Resolved {
                protection: governance.effective_protection(),
                privilege_grant: governance.privilege_grant,
            }
        }
    };
    Ok(Ok(RunGovernance::Governed {
        environment: env_name.clone(),
        locked_out: fork_policy(event, env_name).secrets_locked_out,
        rules,
    }))
}

/// Resolve the target Environment's governance, refuse an unknown
/// Environment, an ineligible Project or a disallowed ref, enforce
/// `require_reason`, admit per-step privilege grants, and durably create the
/// run for a single compiled pipeline — the admission primitive shared by the
/// webhook trigger path and [`dispatch_run`]. A refusal is
/// [`Admission::Refused`] (the caller decides whether it is a warned skip or
/// a hard error); nothing is persisted for it.
///
/// A dispatch is a *trigger, never authority* (ADR-0043 §6): it rides the exact
/// same Environment protection (approvers, wait timer, allowed-refs, concurrency)
/// as a webhook deploy — there is no gate-bypass here. `params`, when supplied,
/// are the already-resolved launch parameters frozen onto the run (ADR-0043 §5).
#[allow(clippy::too_many_arguments)] // a cohesive admission routine; splitting hides the flow
/// [`admit_unrecorded`], recording a refusal (other than a ref an
/// Environment does not allow, which is the rule working) so the Runs list
/// can show it (ADR-0076).
#[allow(clippy::too_many_arguments)]
async fn admit_and_create_run(
    forge: &dyn scarab_forge::ForgePort,
    db: &dyn Db,
    clock: &dyn Clock,
    ports: AdmissionPorts<'_>,
    event: &scarab_forge::Event,
    ir: &scarab_pipeline::PipelineIr,
    path: &str,
    excluded: &[String],
    params: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<Admission, TriggerError> {
    let admission =
        admit_unrecorded(forge, db, clock, ports, event, ir, path, excluded, params).await?;
    if let (Admission::Refused(refusal), Some(repo)) = (&admission, event.repo()) {
        let (kind, messages): (&str, &[String]) = match refusal {
            AdmissionRefusal::RefNotAllowed => ("", &[]),
            AdmissionRefusal::UnknownEnvironment { .. }
            | AdmissionRefusal::NotEligible { .. } => ("environment", &[]),
            AdmissionRefusal::PolicyDenied { messages, .. } => ("policy", messages),
            AdmissionRefusal::LibraryYanked { messages } => ("yanked", messages),
            AdmissionRefusal::EventField { messages } => ("event_field", messages),
            AdmissionRefusal::ForgeToken { messages } => ("forge_token", messages),
        };
        if !kind.is_empty() {
            let message = admission_refusal_message(refusal, &repo.owner, &repo.name);
            let errors: Vec<CompileErrorDto> = if messages.is_empty() {
                vec![CompileErrorDto::other(message.clone())]
            } else {
                messages.iter().cloned().map(CompileErrorDto::other).collect()
            };
            record_refusal(db, clock, event, path, kind, message, &errors).await;
        }
    }
    Ok(admission)
}

/// Record that `event` did not become a Run of `pipeline` (ADR-0076). Best
/// effort: a failed write is logged and changes nothing about the refusal.
async fn record_refusal(
    db: &dyn Db,
    clock: &dyn Clock,
    event: &scarab_forge::Event,
    pipeline: &str,
    reason_kind: &str,
    message: String,
    errors: &[CompileErrorDto],
) {
    let Some(repo) = event.repo() else {
        return;
    };
    let refused = scarab_engine::RefusedRun {
        id: 0,
        org: repo.owner.clone(),
        project: repo.name.clone(),
        pipeline: pipeline.to_string(),
        trigger_kind: event.trigger_kind().as_str().to_string(),
        actor: event.actor().map(str::to_string),
        git_ref: event.protection_ref(),
        sha: origin_sha(event),
        title: event.trigger_title(),
        reason_kind: reason_kind.to_string(),
        message,
        errors: serde_json::to_value(errors).unwrap_or(serde_json::Value::Array(Vec::new())),
        created_at: clock.now().await,
    };
    if let Err(e) = db.record_refused_run(&refused).await {
        tracing::warn!(
            org = %refused.org,
            project = %refused.project,
            pipeline = %refused.pipeline,
            error = %e,
            "recording a refused Run failed"
        );
    }
}

/// Record a Pipeline that did not compile for `event`, and pass the error on.
async fn record_compile_failure<T>(
    db: &dyn Db,
    clock: &dyn Clock,
    event: &scarab_forge::Event,
    pipeline: &str,
    compiled: Result<T, TriggerError>,
) -> Result<T, TriggerError> {
    if let Err(TriggerError::Pipeline(text)) = &compiled {
        let message = format!("pipeline: {text}");
        let errors = compile_errors(&message);
        record_refusal(db, clock, event, pipeline, "compile", message, &errors).await;
    }
    compiled
}

#[allow(clippy::too_many_arguments)]
async fn admit_unrecorded(
    forge: &dyn scarab_forge::ForgePort,
    db: &dyn Db,
    clock: &dyn Clock,
    ports: AdmissionPorts<'_>,
    event: &scarab_forge::Event,
    ir: &scarab_pipeline::PipelineIr,
    path: &str,
    excluded: &[String],
    params: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<Admission, TriggerError> {
    // Both callers pass a repo-aware event (webhook events carry a repo; dispatch
    // builds a repo+ref Manual/Api event, ADR-0043).
    let repo = event
        .repo()
        .ok_or_else(|| TriggerError::Pipeline("event carries no repository".into()))?;

    let gov = match admission_before_policies(ports.governance, event, ir, repo).await? {
        Ok(gov) => gov,
        Err(refusal) => return Ok(Admission::Refused(refusal)),
    };

    // ADR-0076 §6: a yanked library version is a PolicyInvariant, checked
    // before (and whether or not) any Policy is consulted.
    if let Some(libraries) = ports.libraries {
        let items: Vec<(String, &[scarab_pipeline::OriginEntry])> = ir
            .steps
            .iter()
            .map(|s| (format!("`{}`", s.id), s.origin.as_slice()))
            .collect();
        let messages = libraries::yank_refusals_for(libraries, &repo.owner, &items)
            .await
            .map_err(|e| TriggerError::Db(DbError::Other(format!("library store: {e}"))))?;
        if !messages.is_empty() {
            return Ok(Admission::Refused(AdmissionRefusal::LibraryYanked {
                messages,
            }));
        }
    }

    // ADR-0076 §9: the `event` values the Pipeline's templates read, checked
    // and fixed now, and which Steps may receive a forge credential. Both are
    // fixed checks, decided before any Policy is consulted.
    let clone_url = forge_token::repo_clone_url(event, ports.connections)
        .await
        .map_err(|e| TriggerError::Db(DbError::Other(e)))?;
    let facts = match forge_token::event_facts(event, clone_url.as_deref(), ir) {
        Ok(facts) => facts,
        Err(messages) => {
            return Ok(Admission::Refused(AdmissionRefusal::EventField { messages }))
        }
    };
    let write = match gov.protection() {
        Some(p) => forge_token::WriteGovernance {
            restricts_refs: !p.unrestricted_refs(),
            has_approvers: !p.approvers.is_empty(),
        },
        None => forge_token::WriteGovernance::default(),
    };
    let tokens = forge_token::forge_token_refusals(
        forge,
        ports.libraries,
        &repo.owner,
        repo,
        clone_url.as_deref(),
        ir,
        event.is_fork_pr() || gov.locked_out(),
        write,
    )
    .await
    .map_err(|e| TriggerError::Db(DbError::Other(e)))?;
    if !tokens.refusals.is_empty() {
        return Ok(Admission::Refused(AdmissionRefusal::ForgeToken {
            messages: tokens.refusals,
        }));
    }

    let now = clock.now().await;
    let run = new_run_id();
    // ADR-0075: the `run.create` Policies, after the Environment's ref and
    // reason checks and before anything is persisted (`persist_run_from_ir`
    // is not atomic, so a deny after it would leave a partial Run). The run
    // id is minted first so an allow decision names the Run it admitted.
    if let Some(policies) = ports.policies {
        if let Err(refusal) = consult_run_create(
            policies,
            &ports.links,
            event,
            ir,
            path,
            repo,
            &gov,
            &run,
            now,
            params,
            &facts,
        )
        .await?
        {
            return Ok(Admission::Refused(refusal));
        }
    }
    let admitted = AdmittedFacts {
        event: &facts,
        trusted_only: &tokens.trusted_only,
    };
    persist_run_from_ir(db, &run, ir, event, path, &gov, excluded, params, &admitted, now).await?;
    Ok(Admission::Created(run))
}

/// The admission checks a Run passes before its `run.create` Policies are
/// consulted: resolve the target Environment's governance (refusing an
/// unknown Environment or an ineligible Project), then the Environment's
/// allowed refs and `require_reason`. Shared by [`admit_and_create_run`] and
/// the policy test API, so both answer for the same Run the same way.
async fn admission_before_policies(
    governance: Option<&dyn scarab_project::EnvironmentGovernanceStore>,
    event: &scarab_forge::Event,
    ir: &scarab_pipeline::PipelineIr,
    repo: &scarab_forge::RepoRef,
) -> Result<Result<RunGovernance, AdmissionRefusal>, TriggerError> {
    // ADR-0037/0039, ADR-0071: resolve the run's governance once (a deploy
    // pipeline targets an Environment; an ordinary CI run does not) — refusing
    // an unknown Environment or an ineligible Project outright. Then reject a
    // disallowed ref at creation (enforced even without an approver gate) and
    // admit per-step privilege grants against the Project's grant.
    let gov = match resolve_governance(governance, event, ir, repo).await? {
        Ok(gov) => gov,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if let Some(p) = gov.protection() {
        // ADR-0037: match allowed_refs against the *symbolic* branch/tag ref
        // (`refs/heads/main`, `refs/tags/*`, …), NOT the immutable commit the
        // config is read at (`git_ref` above is a SHA for push/dispatch). An event
        // with no symbolic ref (cron/comment/upstream) is admitted only where no
        // layer restricts refs — fail-closed.
        if !p.admits_protection_ref(event.protection_ref().as_deref()) {
            return Ok(Err(AdmissionRefusal::RefNotAllowed));
        }

        // ADR-0057 §3: a `require_reason` environment blocks a reasonless human
        // dispatch (manual/api) at the admission gate — a third guardrail beside
        // allowed_refs/approvers. `trigger_title()` is `None` exactly when a
        // manual/api dispatch carried no non-blank reason (slice C's canonical
        // empty-reason signal); push/PR/tag/release/cron/upstream are exempt via
        // `is_human_dispatch == false`. Unlike a disallowed ref (a refusal the
        // webhook path skips), a missing reason is an answerable request —
        // surface it fail-closed with a clear diagnostic (like a missing approval).
        let is_human_dispatch = matches!(
            event.trigger_kind(),
            scarab_forge::TriggerKind::Manual | scarab_forge::TriggerKind::Api
        );
        if let Err(violations) = p.admits_reason(is_human_dispatch, event.trigger_title().is_some())
        {
            return Err(TriggerError::ReasonRequired(violations.join("; ")));
        }
    }
    Ok(Ok(gov))
}

/// A PolicyStore failure: refuse the Run (fail closed) rather than admit it
/// unchecked. A 500 on the webhook and dispatch paths, like a governance
/// store failure.
fn policy_store_err(e: scarab_policy::store::PolicyStoreError) -> TriggerError {
    TriggerError::Db(DbError::Other(format!(
        "policy store: {e}; the Run was not created because its Policies could not be checked"
    )))
}

/// The `run.create` trigger of `event` (ADR-0075 §2). For a manual or API
/// dispatch the actor is the authenticated Principal (the dispatch handler
/// passes `principal.subject`), so it is both `trigger.principal` and the
/// input's `principal`. A webhook actor is a forge login: its `principal`
/// is `linked`, the Principal its immutable account id is pinned to through
/// identity links (ADR-0069, [`ActorLinks::principal_of`]), or none.
fn run_create_trigger(
    event: &scarab_forge::Event,
    linked: Option<String>,
) -> (scarab_policy::run_create::Trigger, Option<String>) {
    use scarab_forge::{Event, TriggerKind as F};
    use scarab_policy::run_create::TriggerKind as P;
    let kind = match event.trigger_kind() {
        F::Push => P::Push,
        F::PullRequest => P::PullRequest,
        F::Tag => P::Tag,
        F::Release => P::Release,
        F::Comment => P::Comment,
        F::Cron => P::Cron,
        F::Manual => P::Manual,
        F::Api => P::Api,
        F::Upstream => P::Upstream,
    };
    let sha = match event {
        Event::Push { after, .. } => Some(after.clone()),
        Event::PullRequest { head, .. } => Some(head.clone()),
        Event::Manual { sha, .. } | Event::Api { sha, .. } => Some(sha.clone()),
        _ => None,
    }
    .filter(|s| !s.is_empty());
    let (principal, reason) = match event {
        Event::Manual { actor, reason, .. } | Event::Api { actor, reason, .. } => (
            (!actor.is_empty()).then(|| actor.clone()),
            reason
                .clone()
                .filter(|r| !r.trim().is_empty())
                .map(scarab_policy::schema::FreeText),
        ),
        _ => (linked, None),
    };
    let trigger = scarab_policy::run_create::Trigger {
        kind,
        r#ref: event.protection_ref(),
        sha,
        fork: event.is_fork_pr(),
        actor: event.actor().map(str::to_string),
        principal: principal.clone(),
        reason,
    };
    (trigger, principal)
}

/// What a `run.create` consultation of `event` knows besides the Pipeline,
/// and what the Policies' `match` is evaluated against (ADR-0075 §2). The
/// facts are an `Err` when `now_ms` is out of range. Shared by admission
/// and the policy test API.
fn run_create_facts(
    event: &scarab_forge::Event,
    linked: Option<String>,
    path: &str,
    repo: &scarab_forge::RepoRef,
    gov: &RunGovernance,
    now_ms: i64,
    params: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> (
    Result<scarab_policy::run_create::RunCreateFacts, String>,
    scarab_policy::evaluate::MatchContext,
) {
    let environment = match gov {
        RunGovernance::Governed { environment, .. } => Some(environment.clone()),
        RunGovernance::Ungoverned => None,
    };
    let (trigger, principal) = run_create_trigger(event, linked);
    let ctx = scarab_policy::evaluate::MatchContext {
        project: repo.name.clone(),
        environment: environment.clone(),
        trigger_kind: trigger.kind,
    };
    let facts = scarab_policy::schema::Timestamp::from_unix_millis(now_ms)
        .ok_or_else(|| format!("the time {now_ms} is out of range"))
        .map(|time| scarab_policy::run_create::RunCreateFacts {
            org: repo.owner.clone(),
            project: repo.name.clone(),
            trigger,
            pipeline_path: path.to_string(),
            environment,
            principal,
            time,
            params: params.cloned().unwrap_or_default(),
            event: serde_json::Map::new(),
        });
    (facts, ctx)
}

/// The `run.create` input of `ir`, as a PolicyDecision records it; an `Err`
/// names the list or string cap the Run does not fit. CPU work: call it off
/// the async workers.
fn run_create_input(
    ir: &scarab_pipeline::PipelineIr,
    facts: scarab_policy::run_create::RunCreateFacts,
) -> Result<serde_json::Value, String> {
    scarab_policy::run_create::input_value(ir, facts).map_err(|e| e.to_string())
}

/// Consults the `run.create` Policies of `repo`'s org and Project for a
/// compiled Pipeline about to become Run `run` (ADR-0075 §2, §7, §9).
///
/// - No Policy at `run.create` and no plugin gate: nothing is evaluated or
///   recorded. A plugin gate is denied by the closed plugin default
///   ([`plugins_closed_default`](scarab_policy::evaluate::plugins_closed_default))
///   unless an org-level, enforced `plugins_allowed` setting lists it.
/// - Otherwise the decision is recorded: an allow with `run`, a deny with no
///   Run (the org's decision log). A deny is an
///   [`AdmissionRefusal::PolicyDenied`] whose messages name their Policy.
/// - A stored Policy that no longer compiles denies when enforced and
///   matched ([`evaluate_stored`](scarab_policy::evaluate::evaluate_stored)).
/// - A Run over an input list or string cap cannot be checked: it is
///   refused, naming the cap, when an enforced Policy matches it, and
///   admitted otherwise. The plugin default reads the gates from `ir`, so it
///   denies an unlisted plugin gate either way.
/// - A store failure (loading or recording) is an error: no Run.
#[allow(clippy::too_many_arguments)]
async fn consult_run_create(
    policies: &dyn scarab_policy::store::PolicyStore,
    links: &ActorLinks<'_>,
    event: &scarab_forge::Event,
    ir: &scarab_pipeline::PipelineIr,
    path: &str,
    repo: &scarab_forge::RepoRef,
    gov: &RunGovernance,
    run: &RunId,
    now: scarab_engine::Timestamp,
    params: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
    event_facts: &serde_json::Map<String, serde_json::Value>,
) -> Result<Result<(), AdmissionRefusal>, TriggerError> {
    use scarab_policy::evaluate::{evaluate_stored, PolicyOutcome};
    use scarab_policy::policy::{Checkpoint, Mode};
    use scarab_policy::run_create;

    let stored = policies
        .effective(&repo.owner, Some(&repo.name), Checkpoint::RunCreate)
        .await
        .map_err(policy_store_err)?;
    // A plugin gate is consulted with no Policy stored: the closed plugin
    // default denies it until an org lists the plugin (ADR-0075 §7).
    if stored.is_empty() && !run_create::has_plugin_gate(ir) {
        return Ok(Ok(()));
    }

    let linked = links.principal_of(event, repo).await?;
    let (facts, ctx) = run_create_facts(event, linked, path, repo, gov, now.0, params);
    let facts = facts.map(|mut f| {
        f.event = event_facts.clone();
        f
    });
    // Building the input and evaluating it are CPU work; both run off the
    // async workers. The Run-level caps are checked here first, so a Run
    // that cannot be checked is not copied to get there.
    let job = facts.and_then(|facts| {
        run_create::check_run_caps(ir)
            .map(|()| (ir.clone(), facts))
            .map_err(|e| e.to_string())
    });
    let plugin_gates = run_create::plugin_gates(ir);
    let decision = tokio::task::spawn_blocking(move || {
        let input = job.and_then(|(ir, facts)| run_create_input(&ir, facts));
        evaluate_stored(
            Checkpoint::RunCreate,
            run_create::SCHEMA_VERSION,
            input.as_ref().map_err(String::as_str),
            Some(&plugin_gates),
            &ctx,
            &stored,
        )
    })
    .await
    .map_err(|e| policy_store_err(scarab_policy::store::PolicyStoreError::Store(e.to_string())))?;

    let denied = decision.is_denied();
    // Each message with the Policy it came from, for the refusal.
    let mut messages = Vec::new();
    if denied {
        for r in &decision.results {
            if r.mode != Mode::Enforce {
                continue;
            }
            match &r.outcome {
                PolicyOutcome::Denied { messages: m } => {
                    messages.extend(m.iter().map(|m| format!("policy `{}`: {m}", r.name)))
                }
                PolicyOutcome::Error { message } => {
                    messages.push(format!("policy `{}`: {message}", r.name))
                }
                _ => {}
            }
        }
    }
    let record = scarab_policy::store::DecisionRecord {
        id: Uuid::now_v7().to_string(),
        org: repo.owner.clone(),
        project: Some(repo.name.clone()),
        run_id: (!denied).then(|| run.0.clone()),
        step_id: None,
        attempt: None,
        decision,
        at: now.0,
    };
    let decision_id = policies
        .record_decision(&record)
        .await
        .map_err(policy_store_err)?;
    if denied {
        return Ok(Err(AdmissionRefusal::PolicyDenied {
            decision_id,
            messages,
        }));
    }
    Ok(Ok(()))
}

/// Which trigger a dispatch opts into: a human [`Manual`](DispatchKind::Manual)
/// dispatch (the default) or its programmatic [`Api`](DispatchKind::Api) sibling
/// (ADR-0043 §4). The named pipeline must declare the matching `on:` trigger to
/// be dispatchable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DispatchKind {
    #[default]
    Manual,
    Api,
}

impl DispatchKind {
    fn trigger_kind(self) -> scarab_forge::TriggerKind {
        match self {
            DispatchKind::Manual => scarab_forge::TriggerKind::Manual,
            DispatchKind::Api => scarab_forge::TriggerKind::Api,
        }
    }

    /// Build the repo+ref-aware dispatch event (ADR-0043). Mirrors a push: `r#ref`
    /// is the **symbolic** dispatch ref (canonicalized, used for `allowed_refs`
    /// matching, ADR-0037), `sha` the **resolved commit** the config is read at and
    /// the Run pins to.
    fn into_event(
        self,
        actor: String,
        repo: scarab_forge::RepoRef,
        r#ref: String,
        sha: String,
        reason: Option<String>,
    ) -> scarab_forge::Event {
        match self {
            DispatchKind::Manual => scarab_forge::Event::Manual {
                actor,
                repo,
                r#ref,
                sha,
                reason,
            },
            DispatchKind::Api => scarab_forge::Event::Api {
                actor,
                repo,
                r#ref,
                sha,
                reason,
            },
        }
    }
}

/// Error dispatching a named pipeline (ADR-0043 "World B"). Distinct from
/// [`TriggerError`] so the HTTP layer can map each cause to a precise status and
/// a caller-facing, fail-closed message; a dispatch creates **no** run on any
/// error.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error(transparent)]
    Forge(scarab_forge::ForgeError),
    /// No `.scarab` pipeline by that name exists at the resolved commit.
    #[error("no pipeline `{0}` found at the requested ref")]
    PipelineNotFound(String),
    #[error("config is not valid UTF-8")]
    NotUtf8,
    #[error("pipeline: {0}")]
    Pipeline(String),
    /// The pipeline does not opt into this dispatch trigger — no matching
    /// `on: manual` / `on: api` (ADR-0043 §4). Not dispatchable, fail-closed.
    #[error("pipeline `{pipeline}` is not dispatchable: it declares no matching `on: {kind}`")]
    NotDispatchable {
        pipeline: String,
        kind: &'static str,
    },
    /// A supplied launch parameter failed coercion/validation (ADR-0043 §6). The
    /// inner error carries the per-parameter detail the form renders.
    #[error("invalid launch parameters: {0}")]
    Params(scarab_pipeline::PipelineError),
    /// The dispatch ref is not permitted by the target Environment's allowed-refs
    /// (ADR-0043 §6) — the same guardrail a webhook deploy hits, fail-closed.
    #[error("ref `{0}` is not allowed to deploy to this environment")]
    RefNotAllowed(String),
    /// A target Environment requires a reason for human dispatches (ADR-0057 §3)
    /// and none was supplied — the same admission guardrail a `require_reason`
    /// environment applies, fail-closed. Surfaces the violation to the dispatcher.
    #[error("{0}")]
    ReasonRequired(String),
    /// The pipeline names an Environment its Org does not have (ADR-0071
    /// §10) — fail-closed; the message says who can fix it.
    #[error("{}", unknown_environment_message(.org, .project, .environment))]
    UnknownEnvironment {
        org: String,
        project: String,
        environment: String,
    },
    /// The Environment does not admit this Project (ADR-0071 §2) —
    /// fail-closed; the message says who can fix it.
    #[error("{}", not_eligible_message(.org, .project, .environment))]
    NotEligible {
        org: String,
        project: String,
        environment: String,
    },
    /// A `run.create` Policy denied the Run (ADR-0075) — fail-closed; the
    /// message carries every deny and the recorded decision's id.
    #[error("{}", policy_denied_message(.decision_id, .messages))]
    PolicyDenied {
        decision_id: String,
        messages: Vec<String>,
    },
    /// The Run uses a yanked library version (ADR-0076 §6).
    #[error("{}", library_yanked_message(.0))]
    LibraryYanked(Vec<String>),
    /// The Pipeline reads an `event` field the dispatch cannot give it
    /// (ADR-0076 §9).
    #[error("{}", event_field_message(.0))]
    EventField(Vec<String>),
    /// A Step asks for a forge credential it may not have (ADR-0076 §9).
    #[error("{}", forge_token_message(.0))]
    ForgeToken(Vec<String>),
    #[error(transparent)]
    Db(DbError),
}

/// The actionable text for an eligibility refusal (ADR-0071 §2, §10): what
/// is missing and who can fix it — an org admin, since only an
/// Environment's admin may change who deploys into it (§7).
fn admission_refusal_message(refusal: &AdmissionRefusal, org: &str, project: &str) -> String {
    match refusal {
        AdmissionRefusal::UnknownEnvironment { environment } => {
            unknown_environment_message(org, project, environment)
        }
        AdmissionRefusal::NotEligible { environment } => {
            not_eligible_message(org, project, environment)
        }
        AdmissionRefusal::RefNotAllowed => {
            "the ref is not allowed to deploy to this environment".to_string()
        }
        AdmissionRefusal::PolicyDenied {
            decision_id,
            messages,
        } => policy_denied_message(decision_id, messages),
        AdmissionRefusal::LibraryYanked { messages } => library_yanked_message(messages),
        AdmissionRefusal::EventField { messages } => event_field_message(messages),
        AdmissionRefusal::ForgeToken { messages } => forge_token_message(messages),
    }
}

fn event_field_message(messages: &[String]) -> String {
    format!("this Run reads a trigger value it cannot use: {}", messages.join("; "))
}

fn forge_token_message(messages: &[String]) -> String {
    format!(
        "this Run asks for a forge credential it may not have: {}",
        messages.join("; ")
    )
}

fn library_yanked_message(messages: &[String]) -> String {
    format!("this Run uses a yanked library: {}", messages.join("; "))
}

fn policy_denied_message(decision_id: &str, messages: &[String]) -> String {
    format!(
        "a run.create Policy refused this Run (decision {decision_id}): {}",
        messages.join("; ")
    )
}

fn unknown_environment_message(org: &str, project: &str, environment: &str) -> String {
    format!(
        "environment `{environment}` does not exist in org `{org}`: an org admin must create \
         it and make `{project}` eligible for `{environment}` (ADR-0071)"
    )
}

fn not_eligible_message(org: &str, project: &str, environment: &str) -> String {
    format!(
        "project `{project}` may not deploy to environment `{environment}` in org `{org}`: an \
         org admin must make `{project}` eligible for `{environment}` (ADR-0071)"
    )
}

impl From<TriggerError> for DispatchError {
    fn from(e: TriggerError) -> Self {
        match e {
            TriggerError::Forge(f) => DispatchError::Forge(f),
            TriggerError::Pipeline(m) => DispatchError::Pipeline(m),
            TriggerError::NotUtf8 => DispatchError::NotUtf8,
            TriggerError::ReasonRequired(m) => DispatchError::ReasonRequired(m),
            TriggerError::Db(d) => DispatchError::Db(d),
        }
    }
}

/// Canonicalize a user-supplied dispatch ref into a **symbolic** ref for
/// Environment `allowed_refs` matching (ADR-0037):
/// - already a fully-qualified ref (`refs/…`) → taken verbatim;
/// - a 40-char lowercase-hex string (a raw commit SHA) → taken verbatim, so it
///   won't match a branch glob and is correctly denied from a branch-scoped
///   Environment;
/// - anything else → treated as a bare branch name → `refs/heads/<ref>`.
///
/// Pure and total; the resolved commit is looked up separately.
fn canonicalize_ref(r#ref: &str) -> String {
    let is_raw_sha = r#ref.len() == 40
        && r#ref
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if r#ref.starts_with("refs/") || is_raw_sha {
        // Already fully-qualified, or an opaque commit that must stay verbatim so
        // it never matches a branch glob.
        r#ref.to_string()
    } else {
        format!("refs/heads/{}", r#ref)
    }
}

/// Candidate `.scarab` paths a dispatch `pipeline` identifier resolves to. A full
/// `.scarab/**.yaml`/`.yml` path (or any path containing `/`) is taken verbatim;
/// a bare name maps to `.scarab/<name>.yaml` then `.yml`.
fn dispatch_candidate_paths(pipeline: &str) -> Vec<String> {
    if pipeline.contains('/') || is_pipeline_file(pipeline) {
        vec![pipeline.to_string()]
    } else {
        vec![
            format!("{CONFIG_DIR}/{pipeline}.yaml"),
            format!("{CONFIG_DIR}/{pipeline}.yml"),
        ]
    }
}

/// Read the authored YAML of a named pipeline at a resolved commit, trying each
/// [`dispatch_candidate_paths`] candidate in turn. Returns the `(path, yaml)`
/// that resolved, or [`DispatchError::PipelineNotFound`] if no candidate exists
/// at `sha`. Shared by [`dispatch_run`] (before compile+admit) and the interface
/// describe endpoint (before compile-for-interface) so both address a pipeline
/// identically (a bare name or a full `.scarab/*.yaml` path).
async fn read_named_pipeline(
    forge: &dyn scarab_forge::ForgePort,
    repo: &scarab_forge::RepoRef,
    sha: &str,
    pipeline: &str,
) -> Result<(String, String), DispatchError> {
    for candidate in dispatch_candidate_paths(pipeline) {
        match forge.read_file_at_ref(repo, sha, &candidate).await {
            Ok(bytes) => {
                let yaml = String::from_utf8(bytes).map_err(|_| DispatchError::NotUtf8)?;
                return Ok((candidate, yaml));
            }
            Err(scarab_forge::ForgeError::Api(_)) => continue,
            Err(e) => return Err(DispatchError::Forge(e)),
        }
    }
    Err(DispatchError::PipelineNotFound(pipeline.to_string()))
}

/// Dispatch a single named pipeline at a repo + ref — the manual/api trigger
/// path (ADR-0043 "World B"), the sibling of [`trigger_run_from_event`]. Both
/// share the read-at-ref → compile ([`prefetch_libs_and_compile`]) and
/// admission → create ([`admit_and_create_run`]) primitives, so a dispatched
/// deploy inherits Environment governance identically to a webhook deploy.
///
/// The flow: resolve `ref` to a concrete commit SHA (`forge.latest_commit`),
/// read and compile the **named** pipeline at that SHA (the Run pins to it,
/// reproducible and self-describing per ADR-0022), require the compiled pipeline
/// to opt in via a matching `on: manual` / `on: api` (else
/// [`DispatchError::NotDispatchable`]), resolve the supplied `params` against the
/// declared interface **fail-closed** before any run exists (else
/// [`DispatchError::Params`]), then create the run through the shared admission
/// path, freezing the resolved params on it.
///
/// A dispatch is a *trigger, never authority* (§6): there is no gate-bypass — a
/// disallowed ref, a missing approver, a wait timer all bite exactly as they
/// would on a webhook deploy.
#[allow(clippy::too_many_arguments)] // the dispatch coordinates are irreducible
pub async fn dispatch_run(
    forge: &dyn scarab_forge::ForgePort,
    db: &dyn Db,
    clock: &dyn Clock,
    ports: AdmissionPorts<'_>,
    actor: String,
    repo: scarab_forge::RepoRef,
    r#ref: String,
    pipeline: String,
    params: std::collections::BTreeMap<String, serde_json::Value>,
    kind: DispatchKind,
    reason: Option<String>,
) -> Result<RunId, DispatchError> {
    // Resolve the dispatch ref to a concrete commit — the form and the run see
    // byte-identical config (no branch-moved skew), and the Run is reproducible
    // (ADR-0043 §4).
    let commit = forge
        .latest_commit(&repo, &r#ref)
        .await
        .map_err(DispatchError::Forge)?;
    // The pinned commit reaches `git fetch` in the clone container as an
    // argument; a forge answer that is not a hex SHA never becomes a Run.
    if !scarab_forge::is_commit_sha(&commit.sha) {
        return Err(DispatchError::Forge(scarab_forge::ForgeError::Api(
            "the forge resolved the ref to a value that is not a commit SHA".into(),
        )));
    }
    let sha = commit.sha;

    // Read the named pipeline at the resolved commit.
    let (path, yaml) = read_named_pipeline(forge, &repo, &sha, &pipeline).await?;

    // Compile at the resolved commit (transitive `invoke:` libraries pre-fetched
    // there) — the shared read-at-ref → compile primitive.
    let installed = installed_libraries(ports.libraries, ports.plugins, &repo.owner)
        .await
        .map_err(DispatchError::Db)?;
    let compiled = prefetch_libs_and_compile(forge, &repo, &sha, &yaml, &installed).await;

    // Build the repo+ref-aware event: the config is read/pinned at the resolved
    // commit (`sha`), while allowed_refs (ADR-0037) matches the *symbolic* ref the
    // launcher named, canonicalized to a `refs/...` form. A user who dispatches a
    // raw SHA gets a SHA-shaped symbolic ref, which correctly won't match a
    // branch-scoped Environment.
    let sym = canonicalize_ref(&r#ref);
    // The optional dispatch reason (ADR-0057 §3) rides the Event as the run
    // Headline. The endpoint stays dumb — accept + stamp, no requiredness check
    // (that is an Environment ProtectionRule at admission, thread D). Excluded
    // from `context()`, so it never reaches trigger-matching / interpolation.
    let event = kind.into_event(actor, repo, sym, sha, reason);
    let ir = record_compile_failure(db, clock, &event, &path, compiled).await?;
    let ctx = event.context();

    // Opt-in (ADR-0043 §4): the pipeline is dispatchable only if it declares a
    // matching `on: manual` / `on: api` (a `when:` on that trigger is honoured
    // too). No opt-in ⇒ not dispatchable, fail-closed, no run.
    let dispatchable = scarab_pipeline::matches_trigger(&ir, kind.trigger_kind().as_str(), &ctx)
        .map_err(|e| DispatchError::Pipeline(e.to_string()))?;
    if !dispatchable {
        return Err(DispatchError::NotDispatchable {
            pipeline,
            kind: kind.trigger_kind().as_str(),
        });
    }

    // Validate launch parameters against the declared interface BEFORE creating
    // the run (ADR-0043 §6): coerce to declared types, apply defaults, run each
    // `validate:` predicate, reject unknown/missing — all fail-closed. A bad
    // supply creates **no** run.
    let resolved = scarab_pipeline::params::resolve_params(&ir.interface, &params)
        .map_err(DispatchError::Params)?;

    // Step-level `when:` guards, applied at creation exactly as the webhook path
    // does (ADR-0033).
    let excluded = scarab_pipeline::excluded_steps_with_params(&ir, &ctx, &resolved)
        .map_err(|e| DispatchError::Pipeline(e.to_string()))?;

    // Create the run through the shared admission path (Environment
    // eligibility and protection, privilege admission), freezing the resolved
    // params. A refusal is a fail-closed error here (the webhook path skips the
    // pipeline; a dispatch is an explicit, answerable request).
    let refusal = match admit_and_create_run(
        forge,
        db,
        clock,
        ports,
        &event,
        &ir,
        &path,
        &excluded,
        Some(&resolved),
    )
    .await?
    {
        Admission::Created(run) => return Ok(run),
        Admission::Refused(refusal) => refusal,
    };
    let repo = event
        .repo()
        .expect("a dispatch event always carries its repository");
    Err(match refusal {
        // Report the symbolic ref the launcher named (what allowed_refs is matched
        // against), not the opaque resolved commit.
        AdmissionRefusal::RefNotAllowed => DispatchError::RefNotAllowed(
            event.protection_ref().unwrap_or_else(|| config_ref(&event)),
        ),
        AdmissionRefusal::UnknownEnvironment { environment } => DispatchError::UnknownEnvironment {
            org: repo.owner.clone(),
            project: repo.name.clone(),
            environment,
        },
        AdmissionRefusal::NotEligible { environment } => DispatchError::NotEligible {
            org: repo.owner.clone(),
            project: repo.name.clone(),
            environment,
        },
        AdmissionRefusal::PolicyDenied {
            decision_id,
            messages,
        } => DispatchError::PolicyDenied {
            decision_id,
            messages,
        },
        AdmissionRefusal::LibraryYanked { messages } => DispatchError::LibraryYanked(messages),
        AdmissionRefusal::EventField { messages } => DispatchError::EventField(messages),
        AdmissionRefusal::ForgeToken { messages } => DispatchError::ForgeToken(messages),
    })
}

/// Admit a step's privilege request (ADR-0039) against the run's target
/// Environment, **fail-closed**. Returns the escalations the executor may apply,
/// or the violations (which must reject the run — never downgrade).
///
/// - No request (or a baseline one) → the restricted baseline (no grants).
/// - With an Environment → the Project's privilege grant there decides
///   ([`EnvironmentProjectPrivilegeGrant::admit_grants`]: digest whitelist,
///   fork-lockout, capability bounds); no grant decides as an empty whitelist
///   (ADR-0071 §8).
/// - Without an Environment → governed grants (`add-capabilities`/`privileged`)
///   are impossible ("privileged requires an Environment"); self-service
///   `run-as-root` is still allowed (it cannot escape the sandbox).
///
/// [`EnvironmentProjectPrivilegeGrant::admit_grants`]: scarab_project::EnvironmentProjectPrivilegeGrant::admit_grants
fn admit_step_grants(
    grant_scope: PrivilegeGrantScope<'_>,
    security: Option<&scarab_pipeline::StepSecurity>,
    image: &str,
    locked_out: bool,
) -> Result<scarab_project::AdmittedGrants, Vec<String>> {
    let Some(sec) = security.filter(|s| !s.is_baseline()) else {
        return Ok(scarab_project::AdmittedGrants::default());
    };
    let req = scarab_project::GrantRequest {
        run_as_root: sec.run_as_root,
        add_capabilities: sec.add_capabilities.clone(),
        privileged: sec.privileged,
    };
    match grant_scope {
        Some(grant) => scarab_project::EnvironmentProjectPrivilegeGrant::admit_grants(
            grant, &req, image, locked_out,
        ),
        None if req.privileged || !req.add_capabilities.is_empty() => Err(vec![
            "governed grants (add-capabilities/privileged) require a target Environment"
                .to_string(),
        ]),
        None => Ok(scarab_project::AdmittedGrants {
            run_as_root: req.run_as_root,
            ..Default::default()
        }),
    }
}

/// Admit a step's raw `k8s_overlay` (ADR-0055) against the target Environment.
/// A raw overlay carries no authority — mirroring a governed grant
/// ([`admit_step_grants`]), it is honored only where the Project's privilege
/// grant in the target Environment sets `permit_k8s_overlay` (ADR-0071 §8),
/// else the run is rejected **fail-closed**. An inline API run (no
/// Environment) therefore never gets one. Returns the overlay to persist (or
/// `None`).
/// Translate an authored `cache:` (ADR-0065 s1) into the launch spec's
/// [`scarab_engine::CacheConfig`]. Only the authored halves cross here —
/// `key`/`restore` are launch-time enrichment the scheduler owns.
fn cache_config(step: &scarab_pipeline::StepSpec) -> Option<scarab_engine::CacheConfig> {
    step.cache.as_ref().map(|c| scarab_engine::CacheConfig {
        dirs: c.dirs.clone(),
        key_files: c.key.clone(),
        key: None,
        restore: Vec::new(),
    })
}

fn admit_k8s_overlay(
    grant_scope: PrivilegeGrantScope<'_>,
    overlay: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>, Vec<String>> {
    match overlay {
        None => Ok(None),
        Some(o)
            if scarab_project::EnvironmentProjectPrivilegeGrant::permits_k8s_overlay(
                grant_scope.flatten(),
            ) =>
        {
            Ok(Some(o.clone()))
        }
        Some(_) => Err(vec![
            "raw `k8s_overlay` requires a target Environment that permits raw overlays for \
             this project (an org admin sets `permit_k8s_overlay` in the project's privilege \
             grant there)"
                .to_string(),
        ]),
    }
}

/// Durably materialize a compiled pipeline IR into a Run: store the IR on the
/// run (self-describing, ADR-0022), record RunCreated + the normalized trigger
/// on the event log, and create each step with its `needs`.
///
/// `admitted` is what admission fixed besides governance (ADR-0076 §9).
///
/// The run stays out of the scheduler's work list until every write has
/// landed ([`end_run_creation`]): a tick between two of them would launch a
/// step without its params, launch a `when:`-excluded step before it is
/// Skipped, or start the run before its concurrency group exists. A creation
/// that fails partway ends the run Failed, never schedulable.
#[allow(clippy::too_many_arguments)] // a cohesive persist routine; splitting hides the flow
async fn persist_run_from_ir(
    db: &dyn Db,
    run: &RunId,
    ir: &scarab_pipeline::PipelineIr,
    event: &scarab_forge::Event,
    pipeline: &str,
    gov: &RunGovernance,
    excluded: &[String],
    params: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
    admitted: &AdmittedFacts<'_>,
    now: Timestamp,
) -> Result<(), TriggerError> {
    let concurrency_group = concurrency_group_key(ir, event, params, admitted)?;
    db.begin_run_creation(run, ir.ir_version, EVENT_VERSION, now)
        .await?;
    let written = write_run_from_ir(
        db,
        run,
        ir,
        event,
        pipeline,
        gov,
        excluded,
        params,
        admitted,
        concurrency_group.as_deref(),
        now,
    )
    .await;
    end_run_creation(db, run, now, written).await
}

/// The concurrency group's key for a Run of `ir`, rendered before any row
/// of the Run is written ([`persist_run_from_ir`]).
fn concurrency_group_key(
    ir: &scarab_pipeline::PipelineIr,
    event: &scarab_forge::Event,
    params: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
    admitted: &AdmittedFacts<'_>,
) -> Result<Option<String>, TriggerError> {
    // The concurrency group's key, rendered before anything is written: the
    // group is interpolated against the run's launch inputs and its checked
    // `event` values (ADR-0076 §9: the fields a template may read, each
    // passing its check — never the raw trigger event, whose free text
    // anyone who can comment writes), so `deploy-${{ inputs.deploy_env }}`
    // resolves to a *distinct* key per environment (`deploy-staging` vs
    // `deploy-production`) rather than colliding on the literal template. A
    // `${{ … }}` that cannot be resolved fails closed — never store a literal
    // template as the slot key — and leaves no Run behind.
    Ok(match &ir.concurrency {
        Some(c) => {
            let inputs = params
                .map(|p| serde_json::Value::Object(p.clone().into_iter().collect()))
                .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
            let interp_ctx = serde_json::json!({ "inputs": inputs, "event": admitted.event });
            let group = scarab_pipeline::cel::interpolate(&c.group, &interp_ctx).map_err(|e| {
                TriggerError::Pipeline(format!("concurrency group `{}`: {e}", c.group))
            })?;
            // Tenancy scope (QA finding; git-bug 5120335): the author's group name is
            // an arbitrary string, so it is scoped to the owning repo here — at
            // construction, exactly like `supersede_key` below — as
            // `{owner}/{name}:{group}`. Without the prefix, two unrelated tenants
            // both naming their group `deploy` would share one global slot
            // (`concurrency_slots.group_key` is a global PRIMARY KEY), serializing
            // across the tenant boundary — and under `cancel-in-progress` one
            // tenant's run would cancel the other's. Untenanted runs (no repo —
            // inline dev submissions) take an `inline:` namespace instead; it can
            // never collide with a repo-scoped key because a repo prefix always
            // carries a `/` before its first `:`.
            Some(match event.repo() {
                Some(repo) => format!("{}/{}:{group}", repo.owner, repo.name),
                None => format!("inline:{group}"),
            })
        }
        None => None,
    })
}

/// End a run creation begun with [`Db::begin_run_creation`]. When every write
/// landed (`written` is `Ok`), make the run schedulable. Otherwise — or when
/// that last write fails — end the run `Failed` with the error as its reason
/// ([`scarab_engine::fail_run_creation`]), so a half-written run is never
/// scheduled and never sits `Pending` for good. The creation error is
/// returned either way.
async fn end_run_creation<E>(
    db: &dyn Db,
    run: &RunId,
    now: Timestamp,
    written: Result<(), E>,
) -> Result<(), E>
where
    E: std::fmt::Display + From<DbError>,
{
    let ended = match written {
        Ok(()) => db.finish_run_creation(run).await.map_err(E::from),
        Err(e) => Err(e),
    };
    if let Err(e) = &ended {
        if let Err(failed) = scarab_engine::fail_run_creation(db, run, e.to_string(), now).await {
            // The run stays Pending and out of the work list: never scheduled,
            // but shown Pending until someone clears it.
            tracing::warn!(
                run = %run.0,
                creation_error = %e,
                error = %failed,
                "run creation failed and the run could not be marked Failed"
            );
        }
    }
    ended
}

/// Every write of [`persist_run_from_ir`] after the run row exists.
#[allow(clippy::too_many_arguments)] // a cohesive persist routine; splitting hides the flow
async fn write_run_from_ir(
    db: &dyn Db,
    run: &RunId,
    ir: &scarab_pipeline::PipelineIr,
    event: &scarab_forge::Event,
    pipeline: &str,
    gov: &RunGovernance,
    excluded: &[String],
    params: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
    admitted: &AdmittedFacts<'_>,
    concurrency_group: Option<&str>,
    now: Timestamp,
) -> Result<(), TriggerError> {
    let locked_out = gov.locked_out();
    let no_params = std::collections::BTreeMap::new();
    let params_or_none = params.unwrap_or(&no_params);
    // Freeze the resolved launch parameters on the run so every step's
    // interpolation (`${{ inputs.… }}`) and `SCARAB_PARAM_*` env re-derive
    // deterministically (ADR-0043 §5).
    if let Some(params) = params {
        db.set_run_params(run, params).await?;
    }
    // Tenancy (ADR-0049): stamp the owning (org, project) from the trigger's
    // repo, so run reads/lists can be scoped by the caller's bindings.
    // Untenanted runs (no repo — inline dev submissions) stay global-only.
    if let Some(repo) = event.repo() {
        db.set_run_tenant(run, &repo.owner, &repo.name).await?;
        // Allocate the per-repo run number (ADR-0057 amendment) once the tenant
        // is known — the human `#N` handle. Untenanted inline runs skip this.
        db.allocate_run_number(run, &repo.owner, &repo.name).await?;
    }
    // Origin (the runs-list surface): stamp the trigger facts this run was born
    // from — trigger kind (always), Actor, symbolic ref, resolved commit, PR
    // number, and PR base branch — as discrete columns beside tenancy. Sparse by
    // nature (a cron run has no actor/ref/sha; only a PR has a number + base);
    // carries no scheduling authority, purely for display/audit.
    db.set_run_origin(
        run,
        event.trigger_kind().as_str(),
        event.actor(),
        event.protection_ref().as_deref(),
        origin_sha(event).as_deref(),
        origin_pr_number(event),
        origin_pr_base(event).as_deref(),
    )
    .await?;
    // The pipeline name (the runs-list/detail "which pipeline is this") — the
    // explicit `name:` from the IR when set, else the bare `.scarab/<name>`
    // selection. Stamped beside origin.
    let pipeline_name = ir
        .name
        .clone()
        .unwrap_or_else(|| bare_pipeline_name(pipeline));
    db.set_run_pipeline(run, &pipeline_name).await?;
    // The Headline (ADR-0057): the one human line saying what this run is about,
    // extracted from the trigger event (a push's commit subject; thread B/C fill
    // PR title / dispatch reason) — already subject-only + capped char-safe by
    // the extractor. Display/audit only, never in the CEL/interpolation context.
    // Only stamped when the trigger carried one (skipped otherwise, leaving NULL).
    if let Some(title) = event.trigger_title() {
        db.set_run_trigger_title(run, &title).await?;
    }
    db.store_run_ir(
        run,
        &serde_json::to_value(ir).unwrap_or(serde_json::Value::Null),
    )
    .await?;
    // The checked `event` values every launch renders (ADR-0076 §9), fixed
    // now so a rerun or retry renders the same ones.
    db.set_run_event_facts(run, admitted.event).await?;
    // ADR-0037: record the deploy context (repo + environment + git ref) so
    // gate-approval-time admission can look up the environment's protection
    // rules directly, without parsing the stored IR blob. Deploy runs only.
    if let (RunGovernance::Governed { environment, .. }, Some(repo)) = (gov, event.repo()) {
        db.set_run_deploy_context(
            run,
            &scarab_engine::DeployContext {
                org: repo.owner.clone(),
                project: repo.name.clone(),
                environment: environment.clone(),
                // ADR-0037: persist the *symbolic* ref, because gate-approval-time
                // admission re-runs `ProtectionRules::admits` (allowed_refs +
                // approvers) against this value and deployment history records it.
                // A SHA here would silently break the second allowed_refs check
                // (the gate would never release under a non-empty allowed_refs).
                // Refless deploy events fall back to the read ref.
                git_ref: event.protection_ref().unwrap_or_else(|| config_ref(event)),
                // A fork PR is locked out of this environment's secrets (ADR-0015)
                // and — by extension — its governed privilege grants (ADR-0039).
                locked_out,
            },
        )
        .await?;
    }
    // The deploy marker (ADR-0074 §2): a Run is a deploy Run when its pipeline
    // or any Step is marked `deploy: true` — read from the IR once, here. It
    // drives the Runs list filter and the Run page's deploy-Step mark, and
    // exempts the Run from newest-wins cancel (below).
    let is_deploy_run = ir.is_deploy();
    if is_deploy_run {
        let steps: Vec<StepId> = ir.deploy_steps().into_iter().map(StepId).collect();
        db.mark_deploy_run(run, &steps).await?;
    }
    // Newest-wins cancel applies only to a Run that is both ungoverned (no
    // Environment, ADR-0032) and not a deploy Run (ADR-0074 §2): neither an
    // in-flight Environment run nor an in-flight deploy is disposable.
    let cancellable = matches!(gov, RunGovernance::Ungoverned) && !is_deploy_run;
    // Concurrency group (ADR-0011, 0032): serialize this run against others in the
    // same group under its policy; the key was rendered before the Run was
    // created ([`concurrency_group_key`]).
    if let (Some(c), Some(group)) = (&ir.concurrency, concurrency_group) {
        // ADR-0032: a governed (environment-targeting) deploy still SERIALIZES
        // against its group, but must never silently cancel the current slot
        // holder — an in-flight production deploy is not disposable. So a governed
        // run always takes the queue-and-wait policy regardless of the pipeline's
        // declared `cancel-in-progress`; only ungoverned CI runs keep newest-wins
        // cancel. A deploy Run (ADR-0074 §2) queues the same way. (Mirrors the
        // supersede opt-out below.)
        let policy = if cancellable {
            ConcurrencyPolicy::from_wire(&c.policy)
        } else {
            ConcurrencyPolicy::Queue
        };
        db.set_run_concurrency(run, group, policy).await?;
    }
    // Newest-wins auto-cancel (ADR-0032): key non-deploy runs by (repo, ref,
    // pipeline) so a newer run supersedes older in-flight ones. A pipeline that
    // targets an Environment, or a deploy Run (ADR-0074 §2), opts out — a
    // superseded deploy must not be silently cancelled; no key means
    // `superseded_by` never returns it.
    if cancellable {
        if let Some(key) = supersede_key(event, pipeline) {
            db.set_supersede_key(run, &key).await?;
        }
    }
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::RunCreated,
        at: now,
    })
    .await?;
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::Raw(serde_json::json!({ "trigger": event.context() })),
        at: now,
    })
    .await?;
    for step in &ir.steps {
        let needs: Vec<StepId> = step.needs.0.iter().map(|n| StepId(n.clone())).collect();
        let step_id = StepId(step.id.clone());
        if let Some(kind) = &step.gate {
            // A gate step launches no unit — create it spec-less, then mark it a
            // durable suspend point of this kind (ADR-0008; engine: set_step_gate).
            // A `timer` gate carries its wait so the scheduler can auto-release.
            db.create_step_run(run, &step_id, None, &needs, now).await?;
            let timer = step.gate_after.map(|s| s as i64);
            db.set_step_gate(run, &step_id, kind, timer).await?;
            // A plugin gate names the installation that releases it
            // (ADR-0076 §7); nobody else can pass it.
            if let Some(g) = &step.plugin_gate {
                db.set_gate_releaser(run, &step_id, &g.releaser()).await?;
            }
        } else {
            // ADR-0039: admit the step's privilege request against the
            // Project's privilege grant in the target Environment (ADR-0071
            // §8), fail-closed. A rejected request aborts the
            // whole run creation with a diagnostic — never a silent downgrade.
            // Admitted against the image as Run creation knows it, rendered
            // by the launch's own renderer (ADR-0076): a stored template is
            // not an image, and a `${{` left in it pins no digest.
            let image = scarab_pipeline::preview_image(
                &step.image,
                step.scope.as_ref(),
                &step.matrix_values,
                params_or_none,
                admitted.event,
            );
            // A command the launch will refuse (a value from outside the
            // Pipeline that may not be in a command, ADR-0076) is reported
            // now, with the reason the Step will fail with. The Run is still
            // created: the refusal is the Step's, at launch, as for any value
            // that does not render.
            if let Err(reason) = scarab_pipeline::render(
                scarab_pipeline::Templates {
                    image: "",
                    command: &step.command,
                    env: &[],
                },
                step.scope.as_ref(),
                &step.matrix_values,
                scarab_pipeline::Facts::Preview {
                    params: params_or_none,
                    declared: &ir.interface.inputs,
                    event: admitted.event,
                },
            ) {
                tracing::warn!(step = %step.id, %reason, "step will fail when it launches");
            }
            let trusted_library = admitted.trusted_only.get(&step.id).cloned();
            let admitted = admit_step_grants(
                gov.privilege_grant_scope(),
                step.security.as_ref(),
                &image,
                locked_out,
            )
            .map_err(|v| {
                TriggerError::Pipeline(format!(
                    "step `{}`: privilege request rejected: {}",
                    step.id,
                    v.join("; ")
                ))
            })?;
            // A `kind: build` step (ADR-0018): the engine runs rootless
            // BuildKit with this context — never the author's image. The
            // registry credential resolves at LAUNCH (scoped REGISTRY_AUTH
            // secret, else the forge-derived credential).
            let build = step.build.as_ref().map(|b| scarab_engine::BuildConfig {
                context: if b.context.is_empty() {
                    ".".into()
                } else {
                    b.context.clone()
                },
                dockerfile: if b.dockerfile.is_empty() {
                    "Dockerfile".into()
                } else {
                    b.dockerfile.clone()
                },
                image: b.image.clone(),
                repo_owner: event.repo().map(|r| r.owner.clone()).unwrap_or_default(),
                repo_name: event.repo().map(|r| r.name.clone()).unwrap_or_default(),
                push: b.push && !locked_out, // fork-PR lockout never pushes
                insecure_push: false,
                registry_auth_json: None,
                derived_auth: None,
            });
            let spec = StepSpec {
                image: step.image.clone(),
                command: step.command.clone(),
                env: step.env.clone(),
                secrets: step.secrets.clone(),
                run_as_root: admitted.run_as_root,
                add_capabilities: admitted.add_capabilities,
                privileged: admitted.privileged,
                timeout_seconds: step.timeout,
                workspace_inputs: vec![],
                // Per-PATH output publishing (ADR-0007): the backend prunes the
                // post-step snapshot to exactly these workspace-relative paths.
                // Empty = publish the whole workspace (the implicit default).
                workspace_outputs: step.outputs.clone().unwrap_or_default(),
                // Keyed directory Cache (ADR-0065 s1): the authored dirs/key
                // files persist with the spec; the scheduler resolves the key
                // and restore hints at launch.
                cache: cache_config(step),
                // The forge credential (ADR-0076 §9), as admission decided
                // it: a fork pull request (or a Run locked out of its
                // Environment's secrets) gets `read` whatever it asked for.
                // What each launch checks again is stored with it: that
                // lock to `read`, and the library the org trusted when the
                // Step may hold a long-lived token.
                forge_token: step.forge_token.and_then(|asked| {
                    let repo = event.repo()?;
                    let read_only = locked_out || event.is_fork_pr();
                    Some(scarab_engine::ForgeTokenConfig {
                        access: if read_only {
                            scarab_pipeline::ForgeToken::Read
                        } else {
                            asked
                        },
                        owner: repo.owner.clone(),
                        name: repo.name.clone(),
                        read_only,
                        trusted_library: trusted_library.clone(),
                        credential: None,
                    })
                }),
                build,
                artifacts: step.artifacts.clone(),
                placement_profiles: step.placement_profiles.clone(),
                resources: step.resources.clone(),
                k8s_overlay: admit_k8s_overlay(
                    gov.privilege_grant_scope(),
                    step.k8s_overlay.as_ref(),
                )
                .map_err(|v| {
                    TriggerError::Pipeline(format!("step `{}`: {}", step.id, v.join("; ")))
                })?,
                oidc_token: None,
                // Sidecar services (ADR-0058) co-locate in this executed step's Pod.
                services: step.services.clone(),
                // Shared-service opt-in (ADR-0058): the executor labels this Pod
                // so each named service's NetworkPolicy admits it.
                uses: step.uses.clone(),
                // Matrix coordinate (ADR-0023): carried through so launch-time
                // interpolation resolves this leg's `${{ matrix.<dim> }}`.
                matrix_values: step.matrix_values.clone(),
                scope: step.scope.clone(),
            };
            db.create_step_run(run, &step_id, Some(&spec), &needs, now)
                .await?;
        }
        // A Step that holds a long-lived forge credential only because its
        // library is trusted consumes no workspace (ADR-0076 §9): no file
        // another Step wrote (a `.git/config`, a hook) is present while the
        // token is mounted.
        if admitted.trusted_only.contains_key(&step.id) {
            db.set_step_inputs(run, &step_id, &[]).await?;
        } else if let Some(inputs) = &step.inputs {
            // Explicit input workspaces (ADR-0007), when the step declares a
            // subset of its needs — sharpens restart skip-if-unchanged
            // (ADR-0027).
            let inputs: Vec<StepId> = inputs.iter().map(|i| StepId(i.clone())).collect();
            db.set_step_inputs(run, &step_id, &inputs).await?;
        }

        // A `when:`-excluded step is kept in the DAG (edges intact) but starts
        // Skipped, so the scheduler transitively skips its descendants (ADR-0033).
        // The transition and its journal entry are one commit (git-bug 586187a).
        if excluded.iter().any(|e| e == &step.id) {
            let transitioned = EventKind {
                version: EVENT_VERSION,
                run: run.clone(),
                kind: EventPayload::StepTransitioned {
                    step: step_id.clone(),
                    from: StepStatus::Pending,
                    to: StepStatus::Skipped,
                },
                at: now,
            };
            db.commit_step_transition(
                run,
                &step_id,
                StepStatus::Pending,
                StepStatus::Skipped,
                scarab_engine::AttemptFrontier::Any,
                None,
                std::slice::from_ref(&transitioned),
                &[],
            )
            .await?;
        }
    }
    Ok(())
}

/// Drain "run status changed" outbox notifications and post the matching commit
/// status back to the forge (ADR-0010, 0013). Exactly-once by the outbox: a
/// message is retired (`mark_dispatched`) only after a successful post; a post
/// that fails is left for redelivery (at-least-once, and `set_status` is
/// idempotent on the forge). Returns how many statuses were posted.
///
/// `public_url` is Scarab's public base URL: every status carries the
/// **required** deep-link back to its run (`{public_url}/runs/{id}`,
/// ADR-0046) — a status without a way back to its run is a dead end.
pub async fn drain_forge_statuses(
    forge: &dyn scarab_forge::ForgePort,
    db: &dyn Db,
    owner: &str,
    limit: u32,
    visibility_ms: i64,
    public_url: &str,
) -> Result<usize, DbError> {
    let msgs = db
        .claim_outbox(owner, Some(RUN_STATUS_CHANGED), limit, visibility_ms)
        .await?;
    let mut posted = 0;
    for msg in msgs {
        // A run with no forge trigger (API/manual) has nothing to post — retire it.
        let Some((repo, sha)) = run_forge_coords(db, &msg.run).await? else {
            db.mark_dispatched(msg.id).await?;
            continue;
        };
        let to = msg
            .payload
            .get("to")
            .and_then(|v| serde_json::from_value::<RunStatus>(v.clone()).ok());
        let Some(to) = to else {
            db.mark_dispatched(msg.id).await?; // malformed payload → drop, don't loop
            continue;
        };
        // A superseded post is retired unposted (red team R2-E5): delivery
        // is at-least-once, so a post that failed and is redelivered after
        // its lease can land AFTER a newer verdict and leave the forge on the
        // old one — a green rerun's required check stuck red. A post with no
        // generation (written before it was carried) cannot be ordered, and
        // is posted as before.
        if let Some(generation) = msg.payload.get("generation").and_then(|g| g.as_u64()) {
            if status_post_superseded(db, &msg.run, generation, to).await? {
                db.mark_dispatched(msg.id).await?;
                continue;
            }
        }
        let status = scarab_forge::Status {
            context: "scarab".into(),
            state: run_status_to_forge(to),
            target_url: format!("{}/runs/{}", public_url.trim_end_matches('/'), msg.run.0),
        };
        let commit = scarab_forge::Commit {
            sha,
            message: String::new(),
        };
        // Post the status. A failed post stays on the outbox for redelivery
        // (at-least-once; set_status is idempotent on the forge) but is no
        // longer SILENT: log it, count the failed delivery, and dead-letter the
        // MESSAGE after MAX_DELIVERY_ATTEMPTS so a permanently-rejected post —
        // e.g. HTTP 403 "Resource not accessible by integration" when the App
        // lacks statuses:write — surfaces instead of retrying forever. Mirrors
        // scheduler::reconcile poison handling (ADR-0047), but never dead-letters
        // the run: the run's verdict is independent of the forge accepting it.
        match forge.set_status(&repo, &commit, status).await {
            Ok(()) => {
                db.mark_dispatched(msg.id).await?;
                posted += 1;
            }
            Err(e) => {
                let failures = db.record_outbox_failure(msg.id).await?;
                crate::metrics::record_forge_status_failure();
                tracing::warn!(
                    run = %msg.run.0,
                    repo = %format!("{}/{}", repo.owner, repo.name),
                    sha = %commit.sha,
                    failures,
                    error = %e,
                    "forge set_status failed; left on outbox for redelivery"
                );
                if failures >= MAX_DELIVERY_ATTEMPTS {
                    db.dead_letter_outbox(msg.id).await?;
                    crate::metrics::record_forge_status_dead_lettered();
                    tracing::error!(
                        run = %msg.run.0,
                        repo = %format!("{}/{}", repo.owner, repo.name),
                        sha = %commit.sha,
                        failures,
                        error = %e,
                        "forge set_status dead-lettered as poison — status will NOT be \
                         posted; check the forge App's statuses:write permission"
                    );
                }
            }
        }
    }
    Ok(posted)
}

/// Whether a `to` post of `run` from reopen generation `generation` is
/// superseded by a newer verdict the forge already has (red team R2-E5):
///
/// - it is from an older generation than the run's current one — the run
///   was reopened since, and the newer generation's posts own the forge; or
/// - it is a non-terminal post of the current generation, the run has
///   settled in it, and that settle's terminal post is no longer pending
///   (delivered, or dead-lettered): a run goes non-terminal → terminal once
///   per generation, so the terminal post is the newer word. While the
///   terminal post is still pending the non-terminal one is posted first —
///   the ordinary in-order drain.
async fn status_post_superseded(
    db: &dyn Db,
    run: &RunId,
    generation: u64,
    to: RunStatus,
) -> Result<bool, DbError> {
    let current = db.run_reopen_generation(run).await?;
    if generation < current {
        return Ok(true);
    }
    if to.is_terminal() || generation != current {
        return Ok(false);
    }
    match db.run_status(run).await? {
        Some(settled) if settled.is_terminal() => Ok(!db
            .outbox_key_pending(&scarab_engine::run_status_post_key(
                run, generation, settled,
            ))
            .await?),
        _ => Ok(false),
    }
}

/// Map a run's lifecycle status to a forge commit-status state (ADR-0010).
fn run_status_to_forge(s: RunStatus) -> scarab_forge::StatusState {
    use scarab_forge::StatusState;
    match s {
        RunStatus::Pending | RunStatus::Running | RunStatus::Suspended => StatusState::Pending,
        RunStatus::Succeeded => StatusState::Success,
        RunStatus::Failed | RunStatus::DeadLettered => StatusState::Failure,
        RunStatus::Cancelled => StatusState::Error,
    }
}

/// Recover a run's `(repo, sha)` from the normalized trigger recorded on its
/// event log (persisted by [`persist_run_from_ir`]).
async fn run_forge_coords(
    db: &dyn Db,
    run: &RunId,
) -> Result<Option<(scarab_forge::RepoRef, String)>, DbError> {
    for e in db.events(run).await? {
        if let EventPayload::Raw(v) = &e.kind {
            let ev = &v["trigger"]["event"];
            if let (Some(owner), Some(name), Some(sha)) = (
                ev["repo"]["owner"].as_str(),
                ev["repo"]["name"].as_str(),
                ev["sha"].as_str(),
            ) {
                return Ok(Some((
                    scarab_forge::RepoRef {
                        owner: owner.to_string(),
                        name: name.to_string(),
                    },
                    sha.to_string(),
                )));
            }
        }
    }
    Ok(None)
}

/// Replay guard (ADR-0046): record the delivery id against the registry;
/// `Ok(false)` means this exact delivery was already processed (a replay —
/// even a correctly-signed one) and must be acknowledged without re-processing.
/// With no registry wired (dev) or an empty id, dedup is skipped.
async fn delivery_is_fresh(
    st: &AppState,
    forge: scarab_forge::ForgeKind,
    delivery_id: &str,
) -> Result<bool, ApiError> {
    let (Some(connections), false) = (st.connections.as_ref(), delivery_id.is_empty()) else {
        return Ok(true);
    };
    connections
        .record_delivery(forge, delivery_id)
        .await
        .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))
}

/// The shared tail of every webhook ingest: read in-repo `.scarab` config at
/// the event ref, compile, and start Runs if a pipeline's `on:` matches
/// (ADR-0010). Without a forge wired we can only acknowledge the delivery.
async fn ingest_event(
    st: &AppState,
    event: scarab_forge::Event,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let Some(forge) = st.forge.as_ref() else {
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "no forge configured" })),
        ));
    };
    match trigger_runs_from_event(
        forge.as_ref(),
        st.db.as_ref(),
        st.clock.as_ref(),
        st.admission_ports(),
        &event,
    )
    .await
    {
        // ADR-0071 §2: every deploy pipeline the Environment refused is named
        // in the body as `refused: [{pipeline, environment, reason}]` — beside
        // the runs that did start, or alone (200: nothing was accepted, but
        // "no matching pipeline" would misdescribe a pipeline that matched
        // and was refused).
        Ok(outcome) if !outcome.runs.is_empty() => {
            let run_ids: Vec<&str> = outcome.runs.iter().map(|r| r.0.as_str()).collect();
            let mut body = serde_json::json!({
                "run_ids": run_ids,
                "trigger": event.trigger_kind(),
            });
            if !outcome.refused.is_empty() {
                body["refused"] = serde_json::json!(outcome.refused);
            }
            Ok((StatusCode::ACCEPTED, Json(body)))
        }
        Ok(outcome) if !outcome.refused.is_empty() => Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "refused": outcome.refused,
                "trigger": event.trigger_kind(),
            })),
        )),
        Ok(_) => Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "no matching pipeline" })),
        )),
        Err(TriggerError::Db(e)) => Err(ApiError::Db(e)),
        Err(e) => Err(ApiError::BadRequest(e.to_string())),
    }
}

/// Inbound GitHub webhook ingest (ADR-0010, 0032, 0046): verify the HMAC
/// signature with the GitHub secret, guard against replays by delivery id,
/// auto-register installation events into the ForgeConnection registry
/// (installing the App IS registration), normalize the payload to a canonical
/// [`Event`](scarab_forge::Event), and durably create the triggered Runs.
/// Unverified deliveries are rejected; administrative events (e.g. `ping`)
/// are acknowledged and ignored.
#[utoipa::path(post, path = "/webhooks/github", summary = "GitHub webhook ingest (HMAC-verified, ADR-0046)", responses((status = 202, description = "delivery accepted"), (status = 401, description = "bad signature")))]
async fn github_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // Verify HMAC-SHA256 over the raw body (ADR-0032). No secret configured =>
    // the endpoint is closed.
    let secret = st
        .github_webhook_secret
        .as_deref()
        .ok_or(ApiError::Unauthorized)?;
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    scarab_forge_github::verify_signature(secret, body.as_ref(), sig)
        .map_err(|_| ApiError::Unauthorized)?;

    let payload: serde_json::Value = serde_json::from_slice(body.as_ref())
        .map_err(|e| ApiError::BadRequest(format!("invalid JSON: {e}")))?;
    let delivery = scarab_forge::WebhookDelivery {
        id: header_str(&headers, "x-github-delivery"),
        event: header_str(&headers, "x-github-event"),
        signature: sig.map(str::to_string),
        payload,
    };

    // Replay guard — only verified deliveries are recorded (ADR-0046).
    if !delivery_is_fresh(&st, scarab_forge::ForgeKind::GitHub, &delivery.id).await? {
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "duplicate delivery" })),
        ));
    }

    // Installation lifecycle → registry auto-registration (ADR-0046).
    if let Some(sync) = scarab_forge_github::installation_sync(&delivery) {
        if let Some(connections) = st.connections.as_ref() {
            let conn_id = format!("github-install-{}", sync.installation_id);
            connections
                .put_connection(&scarab_forge::ForgeConnection {
                    id: conn_id.clone(),
                    kind: scarab_forge::ForgeKind::GitHub,
                    base_url: "https://api.github.com".into(),
                    // The App credential is shared across installations; the
                    // composition root registers it under this handle.
                    credential_ref: "github-app".into(),
                })
                .await
                .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?;
            scarab_forge_github::apply_installation_sync(
                connections.as_ref(),
                &conn_id,
                &sync.account,
                &sync,
            )
            .await
            .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?;
            // A new org gets the first-party libraries (ADR-0076 §5).
            if !sync.added.is_empty() {
                libraries::offer_defaults_on_bind(&st, &sync.account).await;
            }
            return Ok((
                StatusCode::OK,
                Json(serde_json::json!({
                    "registered": { "added": sync.added.len(), "removed": sync.removed.len() },
                })),
            ));
        }
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "no registry configured" })),
        ));
    }

    let event = match scarab_forge_github::normalize(&delivery) {
        Ok(e) => e,
        // Acknowledge-and-ignore events we don't act on (ping, unsupported).
        Err(scarab_forge::ForgeError::UnsupportedEvent(_)) => {
            return Ok((StatusCode::OK, Json(serde_json::json!({ "ignored": true }))));
        }
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };
    ingest_event(&st, event).await
}

/// Inbound Forgejo webhook ingest (ADR-0046): the second per-forge endpoint,
/// bound to the Forgejo adapter's verification (plain-hex
/// `X-Forgejo/Gitea-Signature`) and its own secret — no payload sniffing on a
/// shared endpoint. Same replay guard, same canonical vocabulary downstream.
#[utoipa::path(post, path = "/webhooks/forgejo", summary = "Forgejo webhook ingest (HMAC-verified, ADR-0046)", responses((status = 202, description = "delivery accepted"), (status = 401, description = "bad signature")))]
async fn forgejo_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let secret = st
        .forgejo_webhook_secret
        .as_deref()
        .ok_or(ApiError::Unauthorized)?;
    let sig = headers
        .get("x-forgejo-signature")
        .or_else(|| headers.get("x-gitea-signature"))
        .and_then(|v| v.to_str().ok());
    scarab_forge_forgejo::verify_signature(secret, body.as_ref(), sig)
        .map_err(|_| ApiError::Unauthorized)?;

    let payload: serde_json::Value = serde_json::from_slice(body.as_ref())
        .map_err(|e| ApiError::BadRequest(format!("invalid JSON: {e}")))?;
    let event_header = {
        let forgejo = header_str(&headers, "x-forgejo-event");
        if forgejo.is_empty() {
            header_str(&headers, "x-gitea-event")
        } else {
            forgejo
        }
    };
    let delivery_id = {
        let forgejo = header_str(&headers, "x-forgejo-delivery");
        if forgejo.is_empty() {
            header_str(&headers, "x-gitea-delivery")
        } else {
            forgejo
        }
    };
    let delivery = scarab_forge::WebhookDelivery {
        id: delivery_id,
        event: event_header,
        signature: sig.map(str::to_string),
        payload,
    };

    if !delivery_is_fresh(&st, scarab_forge::ForgeKind::Forgejo, &delivery.id).await? {
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "duplicate delivery" })),
        ));
    }

    let event = match scarab_forge_forgejo::normalize(&delivery) {
        Ok(e) => e,
        Err(scarab_forge::ForgeError::UnsupportedEvent(_)) => {
            return Ok((StatusCode::OK, Json(serde_json::json!({ "ignored": true }))));
        }
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };
    ingest_event(&st, event).await
}

fn header_str(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// How long an issued session stays valid (24h).
const SESSION_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// `POST /v1/auth/login` body: an OAuth/OIDC credential (e.g. a GitHub code).
#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    pub credential: String,
    /// PKCE (RFC 7636) verifier, for an API/CLI client that ran its OWN
    /// authorize redirect and therefore holds one (ADR-0049 amendment). Absent
    /// = the pre-PKCE API shape, still accepted: this path never had a
    /// server-side browser flow to carry a verifier through.
    #[serde(default)]
    pub code_verifier: Option<String>,
    /// Which login provider issued `credential` (ADR-0069) — a provider id
    /// from `GET /v1/auth/providers`. Absent = the default provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// The `redirect_uri` the client's own authorize request used, replayed
    /// at the token endpoint (RFC 6749 §4.1.3 — strict providers such as
    /// Authentik refuse the exchange without it).
    #[serde(default)]
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LoginResponse {
    pub session: String,
    pub subject: String,
    /// The session's CSRF token — browsers echo it in `x-csrf-token` on every
    /// mutation (also delivered as the readable `scarab_csrf` cookie).
    pub csrf: String,
}

/// Exchange an OAuth/OIDC credential for a Scarab session (ADR-0010, 0032):
/// authenticate to a forge-agnostic [`Principal`], mint a server-side
/// [`Session`], and return its id (also set as an httpOnly cookie).
#[utoipa::path(post, path = "/v1/auth/login", summary = "Exchange an OAuth credential for a session (API/CLI, ADR-0049)", responses((status = 200, body = LoginResponse), (status = 401, description = "bad credential"), (status = 404, description = "login not configured")))]
async fn login(
    State(st): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let (Some(auth), Some(sessions)) = (st.auth.as_ref(), st.sessions.as_ref()) else {
        return Err(ApiError::NotFound); // login not configured
    };
    // A client that carries a verifier gets it replayed at the token endpoint;
    // otherwise this is exactly the pre-PKCE port call.
    let verifier = req.code_verifier.as_deref().filter(|v| !v.is_empty());
    let redirect_uri = req.redirect_uri.as_deref().filter(|r| !r.is_empty());
    // A named provider exchanges through itself. Unnamed: the default provider
    // — which is what the composition root wires as `auth` — refusing the
    // ambiguity when several are configured and none is the default.
    let principal = match req.provider.as_deref().filter(|p| !p.is_empty()) {
        Some(id) => {
            let oauth = st.oauth_provider(Some(id))?.ok_or(ApiError::NotFound)?;
            oauth
                .exchange(&req.credential, redirect_uri, verifier, None)
                .await
        }
        None => match st.oauth_provider(None)? {
            Some(oauth) if verifier.is_some() || redirect_uri.is_some() => {
                oauth
                    .exchange(&req.credential, redirect_uri, verifier, None)
                    .await
            }
            _ => auth.authenticate(&req.credential).await,
        },
    }
    .map_err(|e| {
        tracing::warn!(error = %e, "API login: credential exchange failed");
        ApiError::Unauthorized
    })?;
    let now = st.clock.now().await.0;
    // An API/CLI exchange: never a fresh browser login (ADR-0073 §2), even
    // though it also sets the browser cookies.
    let (session, csrf) = mint_session(principal.clone(), now, scarab_identity::LOGIN_KIND_API);
    sessions
        .put(&session)
        .await
        .map_err(|_| ApiError::Unauthorized)?;

    let mut resp = (
        StatusCode::OK,
        Json(LoginResponse {
            session: session.id.clone(),
            subject: principal.subject,
            csrf: csrf.clone(),
        }),
    )
        .into_response();
    set_session_cookies(resp.headers_mut(), &session, &csrf);
    Ok(resp)
}

/// Mint a fresh [`Session`] (opaque id + CSRF token, 24h TTL), stamped with
/// when the login completed and how (ADR-0073 §2): `login_kind` is
/// [`scarab_identity::LOGIN_KIND_BROWSER`] for the OAuth callback and
/// [`scarab_identity::LOGIN_KIND_API`] for the API/CLI exchange — only a
/// fresh browser login may enroll a first passkey.
///
/// Returns the CSRF token beside the session: the session keeps only its hash
/// (git-bug 51d6bcb), so this is the one place the plaintext exists — for the
/// cookie and the login response.
fn mint_session(principal: Principal, now_ms: i64, login_kind: &str) -> (Session, String) {
    let csrf = Uuid::new_v4().to_string();
    let session = Session {
        id: Uuid::new_v4().to_string(),
        principal,
        expires_at: now_ms + SESSION_TTL_MS,
        csrf_hash: scarab_identity::csrf_hash(&csrf),
        created_at: now_ms,
        login_kind: login_kind.to_string(),
    };
    (session, csrf)
}

/// Append the login cookies (ADR-0049): the session cookie is `HttpOnly` +
/// `Secure` + `SameSite=Lax` (script-unreadable); the CSRF cookie is
/// deliberately script-READABLE — the UI double-submits it as `x-csrf-token`,
/// which a cross-site attacker cannot do.
fn set_session_cookies(headers: &mut HeaderMap, session: &Session, csrf: &str) {
    for cookie in [
        format!(
            "scarab_session={}; HttpOnly; Secure; Path=/; SameSite=Lax",
            session.id
        ),
        format!("scarab_csrf={csrf}; Secure; Path=/; SameSite=Lax"),
    ] {
        if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
            headers.append(axum::http::header::SET_COOKIE, v);
        }
    }
}

/// The one registered callback every provider and every flow (sign-in and
/// link) returns to. The authorize redirect and the token exchange must carry
/// it byte-for-byte alike (RFC 6749 §4.1.3), so both build it here.
fn oauth_callback_url(st: &AppState) -> String {
    format!("{}/v1/auth/callback", st.public_url.trim_end_matches('/'))
}

/// `GET /v1/auth/login` (ADR-0049): begin the browser OAuth flow — mint the
/// login secrets (`state` + PKCE verifier + OIDC nonce), keep them in one
/// HttpOnly cookie (10 min), and redirect to the provider's authorize endpoint
/// with the `state` echo and the S256 `code_challenge`.
#[utoipa::path(get, path = "/v1/auth/login", summary = "Begin the browser OAuth flow (ADR-0049)", params(("provider" = Option<String>, Query, description = "login provider id (ADR-0069); default = the only, or the unqualified_ids, provider"), ("next" = Option<String>, Query, description = "a relative path to land on after the callback (ADR-0073); default `/`")), responses((status = 302, description = "redirect to the provider with a state cookie"), (status = 400, description = "unknown provider, several and none named, or a `next` that is not a relative path"), (status = 404, description = "OAuth not configured")))]
async fn oauth_login_redirect(
    State(st): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let Some(provider) = st.oauth_provider(q.get("provider").map(String::as_str))? else {
        return Err(ApiError::NotFound);
    };
    // The provider rides in the state cookie, so every provider shares the one
    // registered callback URL (ADR-0069).
    // `?next=` (ADR-0073 §10): where the browser lands after the callback —
    // e.g. `scarab security setup` sends a fresh login straight to passkey
    // enrollment. Only a same-origin relative path is kept; anything else
    // is refused rather than silently dropped.
    let next = match q.get("next").filter(|n| !n.is_empty()) {
        Some(n) => oauth::safe_next(n)
            .ok_or_else(|| ApiError::BadRequest("`next` must be a relative path".into()))?,
        None => "",
    };
    let flow = oauth::LoginFlow::generate()
        .for_provider(provider.provider_id())
        .with_next(next);
    let location = provider.authorize_redirect(&oauth_callback_url(&st), &flow);
    let mut resp = (StatusCode::FOUND, ()).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&location) {
        resp.headers_mut().insert(axum::http::header::LOCATION, v);
    }
    // The verifier never leaves the server except in this HttpOnly cookie —
    // that is what makes the challenge on the redirect worth anything.
    let cookie = format!(
        "scarab_oauth_state={}; HttpOnly; Secure; Path=/; SameSite=Lax; Max-Age=600",
        flow.to_cookie()
    );
    if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
    }
    Ok(resp)
}

/// `GET /v1/auth/callback?code&state` (ADR-0049): finish the browser OAuth
/// flow — verify the `state` echo against the cookie, exchange the code for a
/// [`Principal`], mint the PG session + CSRF token, and land on `/`.
#[utoipa::path(get, path = "/v1/auth/callback", summary = "Finish the browser OAuth flow (ADR-0049)", responses((status = 302, description = "session + CSRF cookies set; redirect to the login's `next`, else /"), (status = 401, description = "state mismatch or bad code")))]
async fn oauth_callback(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let (Some(auth), Some(sessions)) = (st.auth.as_ref(), st.sessions.as_ref()) else {
        return Err(ApiError::NotFound);
    };
    let (Some(code), Some(state)) = (q.get("code"), q.get("state")) else {
        return Err(ApiError::BadRequest("missing code/state".into()));
    };
    // The state cookie proves THIS browser started the flow (login-CSRF guard)
    // and carries the flow's PKCE verifier + expected nonce.
    let flow = cookie_value(&headers, "scarab_oauth_state")
        .as_deref()
        .and_then(oauth::LoginFlow::from_cookie);
    let Some(flow) = flow else {
        tracing::warn!("OAuth callback: no valid scarab_oauth_state cookie (expired, blocked, or a different browser)");
        return Err(ApiError::Unauthorized);
    };
    if flow.state != *state {
        tracing::warn!(provider = %flow.provider, "OAuth callback: state does not match the login cookie");
        return Err(ApiError::Unauthorized);
    }
    if !flow.link.is_empty() {
        return link_callback(&st, &headers, &flow, code).await;
    }
    // The OAuth provider is the one that can honour PKCE/nonce; a non-OAuth
    // authenticator (dev fakes) still goes through the port unchanged.
    let principal = match st.oauth_provider(Some(flow.provider.as_str()))? {
        Some(oauth) => {
            let redirect_uri = oauth_callback_url(&st);
            oauth
                .exchange(code, Some(&redirect_uri), flow.verifier(), flow.nonce())
                .await
        }
        None => auth.authenticate(code).await,
    }
    .map_err(|e| {
        tracing::warn!(provider = %flow.provider, error = %e, "OAuth callback: code exchange failed");
        ApiError::Unauthorized
    })?;
    let now = st.clock.now().await.0;
    let (session, csrf) = mint_session(principal, now, scarab_identity::LOGIN_KIND_BROWSER);
    sessions.put(&session).await.map_err(|e| {
        tracing::warn!(error = %e, "OAuth callback: could not store the new session");
        ApiError::Unauthorized
    })?;

    // Land where the login was started for (`?next=` on `GET
    // /v1/auth/login`, validated twice: when set and here), else `/`.
    let landing = oauth::safe_next(&flow.next).unwrap_or("/");
    let mut resp = (StatusCode::FOUND, ()).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(landing) {
        resp.headers_mut().insert(axum::http::header::LOCATION, v);
    } else {
        resp.headers_mut().insert(
            axum::http::header::LOCATION,
            axum::http::HeaderValue::from_static("/"),
        );
    }
    set_session_cookies(resp.headers_mut(), &session, &csrf);
    // The one-shot state cookie is spent.
    if let Ok(v) = axum::http::HeaderValue::from_str(
        "scarab_oauth_state=; HttpOnly; Secure; Path=/; SameSite=Lax; Max-Age=0",
    ) {
        resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
    }
    Ok(resp)
}

/// The hex SHA-256 of a session id — what a link flow's state cookie carries
/// (ADR-0069 §6) instead of the session id itself.
fn session_hash(sid: &str) -> String {
    scarab_identity::api_token_hash(sid)
}

/// The signed-in, cookie-borne session a link acts for, or a refusal. Linking
/// is a browser act on a person's own account: an API token cannot do it (it
/// is a narrowing of someone's authority, not them), and neither can a Bearer
/// session, which has no browser to come back through the callback.
async fn link_session(
    st: &AppState,
    headers: &HeaderMap,
    action: Action,
) -> Result<(String, Principal), ApiError> {
    let caller = authenticate(st, headers, action).await?;
    if caller.grant.is_some() {
        return Err(ApiError::Forbidden);
    }
    match session_id(headers) {
        Some((sid, AuthVia::Cookie)) => Ok((sid, caller.principal)),
        _ => Err(ApiError::Forbidden),
    }
}

/// `POST /v1/me/identities/link` body.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LinkIdentityRequest {
    /// The login provider to link an account from (`GET /v1/auth/providers`).
    pub provider: String,
}

/// Where the browser goes to prove the account.
#[derive(Debug, Serialize, ToSchema)]
pub struct LinkIdentityResponse {
    pub authorize_url: String,
}

/// Begin linking another provider's account to the signed-in Principal
/// (ADR-0069 §6). A POST (so the CSRF double-submit applies) that returns the
/// authorize URL and plants a one-shot state cookie bound to THIS session; the
/// shared callback then attaches the proven account, or merges in a Principal
/// whose only way in it was. Never by email.
#[utoipa::path(post, path = "/v1/me/identities/link", summary = "Begin linking another sign-in account (ADR-0069)", request_body = LinkIdentityRequest, responses((status = 200, body = LinkIdentityResponse), (status = 403, description = "not a browser session")))]
async fn begin_link_identity(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LinkIdentityRequest>,
) -> Result<Response, ApiError> {
    let (sid, _) = link_session(&st, &headers, Action::Write).await?;
    st.identities.as_ref().ok_or(ApiError::NotFound)?;
    let provider = st
        .oauth_provider(Some(&req.provider))?
        .ok_or(ApiError::NotFound)?;
    let flow = oauth::LoginFlow::generate().for_provider(provider.provider_id());
    // The flow lives no longer than the step-up window that authorised it.
    let expires_at = st.clock.now().await.0 + scarab_identity::STEP_UP_WINDOW_MS;
    // Enforced: a binding only this (guarded) POST can mint, over the
    // provider and the expiry too. Off: the bare session hash, as before —
    // the POST is unguarded then anyway.
    let link = match st.step_up.service() {
        Some(svc) => svc.link_binding(&flow.state, &session_hash(&sid), &flow.provider, expires_at),
        None => session_hash(&sid),
    };
    let flow = flow.for_link(&link, expires_at);
    let mut resp = Json(LinkIdentityResponse {
        authorize_url: provider.authorize_redirect(&oauth_callback_url(&st), &flow),
    })
    .into_response();
    let cookie = format!(
        "scarab_oauth_state={}; HttpOnly; Secure; Path=/; SameSite=Lax; Max-Age=600",
        flow.to_cookie()
    );
    if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
    }
    Ok(resp)
}

/// The callback half of a link (ADR-0069 §6). The session presenting the
/// callback must be the one that started it — the state cookie carries its
/// hash — so a code minted for one person cannot be landed in another's
/// account. Outcomes come back to the UI as a query parameter, not an error
/// page: this is a browser redirect, not an API call.
async fn link_callback(
    st: &AppState,
    headers: &HeaderMap,
    flow: &oauth::LoginFlow,
    code: &str,
) -> Result<Response, ApiError> {
    let (sid, principal) = link_session(st, headers, Action::Read).await?;
    // The link component must be the one the guarded link POST minted for
    // THIS flow, THIS session, THIS provider and THIS expiry (ADR-0073 §3:
    // identity links are a crown jewel, and the state cookie is unsigned
    // client input) — and the flow must not have outlived its step-up.
    let now = st.clock.now().await.0;
    let expires_at = flow.link_expires_at.unwrap_or(i64::MIN);
    let expected = match st.step_up.service() {
        Some(svc) => svc.link_binding(&flow.state, &session_hash(&sid), &flow.provider, expires_at),
        None => session_hash(&sid),
    };
    if now > expires_at || !scarab_identity::ct_eq(expected.as_bytes(), flow.link.as_bytes()) {
        if let Some(svc) = st.step_up.service() {
            let mut ev = step_up::event(
                scarab_identity::security_event_kind::STEP_UP_REFUSED,
                &principal.subject,
                "session:cookie".to_string(),
                None,
                now,
            );
            ev.op = Some("identity.link".to_string());
            ev.session_hash = Some(session_hash(&sid));
            ev.detail = serde_json::json!({
                "reason": "link flow not minted by the stepped-up POST, or expired",
            });
            svc.record(ev).await;
        }
        return Err(ApiError::Unauthorized);
    }
    let store = st.identities.as_ref().ok_or(ApiError::NotFound)?;
    let provider = st
        .oauth_provider(Some(flow.provider.as_str()))?
        .ok_or(ApiError::NotFound)?;
    let identity = provider
        .exchange_identity(
            code,
            Some(&oauth_callback_url(st)),
            flow.verifier(),
            flow.nonce(),
        )
        .await
        .map_err(|e| {
            tracing::warn!(provider = %flow.provider, error = %e, "OAuth link callback: code exchange failed");
            ApiError::Unauthorized
        })?;
    let now = st.clock.now().await.0;
    let location = match store.link(&principal.subject, &identity, now).await {
        Ok(scarab_identity::LinkOutcome::Merged { from }) => {
            format!(
                "/account?identity_linked={}&merged={}",
                flow.provider,
                query_escape(&from)
            )
        }
        Ok(_) => format!("/account?identity_linked={}", flow.provider),
        Err(scarab_identity::IdentityError::Conflict(why)) => {
            format!("/account?identity_link_error={}", query_escape(&why))
        }
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };
    let mut resp = (StatusCode::FOUND, ()).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&location) {
        resp.headers_mut().insert(axum::http::header::LOCATION, v);
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(
        "scarab_oauth_state=; HttpOnly; Secure; Path=/; SameSite=Lax; Max-Age=0",
    ) {
        resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
    }
    Ok(resp)
}

/// Percent-encode a query value (everything but RFC 3986 unreserved).
fn query_escape(v: &str) -> String {
    v.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Unlink the signed-in Principal's account on `provider` (ADR-0069 §6). The
/// roles imported through that account go FIRST, so a failure between the two
/// steps leaves less authority, never more. Refuses (409) to remove the last
/// way to sign in.
#[utoipa::path(delete, path = "/v1/me/identities/{provider}", summary = "Unlink a sign-in account (ADR-0069)", responses((status = 204, description = "unlinked"), (status = 409, description = "not linked, or the only way to sign in")))]
async fn unlink_identity(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (_, principal) = link_session(&st, &headers, Action::Write).await?;
    let store = st.identities.as_ref().ok_or(ApiError::NotFound)?;
    let held = store
        .identities_of(&principal.subject)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if !held.iter().any(|i| i.provider == provider) {
        return Err(ApiError::Conflict(format!(
            "no {provider} account is linked"
        )));
    }
    if held.len() == 1 {
        return Err(ApiError::Conflict(format!(
            "{provider} is your only way to sign in — link another account first"
        )));
    }
    if let Some(rbac) = st.rbac.as_ref() {
        rbac.drop_imports(&principal.subject, &provider)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    }
    match store.unlink(&principal.subject, &provider).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(scarab_identity::IdentityError::Conflict(why)) => Err(ApiError::Conflict(why)),
        Err(e) => Err(ApiError::BadRequest(e.to_string())),
    }
}

/// One login provider, as the login page offers it (ADR-0069).
#[derive(Debug, Serialize, ToSchema)]
pub struct AuthProviderDto {
    /// Pass as `provider` to `GET`/`POST /v1/auth/login`.
    pub id: String,
    /// The button label.
    pub name: String,
    /// The one used when a login names none.
    pub default: bool,
}

/// `GET /v1/auth/providers` (ADR-0069): the configured login providers, for
/// the login page. Unauthenticated by necessity — it is what you see before
/// you have signed in — and it carries nothing secret.
#[utoipa::path(get, path = "/v1/auth/providers", summary = "List login providers (ADR-0069)", responses((status = 200, body = [AuthProviderDto])))]
async fn auth_providers(State(st): State<AppState>) -> Json<Vec<AuthProviderDto>> {
    let default = st
        .oauth_provider(None)
        .ok()
        .flatten()
        .map(|p| p.provider_id().to_string());
    Json(
        st.oauth_providers
            .iter()
            .map(|p| AuthProviderDto {
                id: p.provider_id().to_string(),
                name: p.name().to_string(),
                default: default.as_deref() == Some(p.provider_id()),
            })
            .collect(),
    )
}

/// `POST /v1/auth/logout` (ADR-0049): revoke the presented session server-side
/// and expire the browser cookies. Exempt from the CSRF guard by design — the
/// worst a forged logout achieves is logging the victim out.
#[utoipa::path(post, path = "/v1/auth/logout", summary = "Revoke the session (ADR-0049)", responses((status = 204, description = "session revoked; cookies expired")))]
async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let Some(sessions) = st.sessions.as_ref() else {
        return Err(ApiError::NotFound);
    };
    if let Some((sid, _)) = session_id(&headers) {
        sessions
            .delete(&sid)
            .await
            .map_err(|_| ApiError::Unauthorized)?;
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    for cookie in [
        "scarab_session=; HttpOnly; Secure; Path=/; SameSite=Lax; Max-Age=0",
        "scarab_csrf=; Secure; Path=/; SameSite=Lax; Max-Age=0",
    ] {
        if let Ok(v) = axum::http::HeaderValue::from_str(cookie) {
            resp.headers_mut().append(axum::http::header::SET_COOKIE, v);
        }
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Role bindings (ADR-0049 C2): native grants/revokes + forge-permission import.
// ---------------------------------------------------------------------------

/// `PUT /v1/orgs/{org}/bindings` body — a native grant. `project` absent =
/// org-scoped (inherits down to every project of the org).
#[derive(Debug, Deserialize, ToSchema)]
pub struct BindingRequest {
    pub subject: String,
    /// `viewer` | `member` | `admin` | `owner`.
    pub role: String,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BindingQuery {
    subject: String,
    #[serde(default)]
    project: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BindingDto {
    pub subject: String,
    pub role: String,
    /// Empty = org-scoped.
    pub project: String,
    /// The Principal's display name, when the identity store knows it
    /// (ADR-0069). Absent for a pre-upgrade bare subject nobody has signed in
    /// as since.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// `false` = created by a forge import and not signed into yet — the
    /// Members panel's "not signed in yet". Absent when the store does not
    /// know the subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed: Option<bool>,
}

/// `POST /v1/repos/{org}/{repo}/bindings/import` body: the forge users whose
/// repo permissions to import as seed bindings.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ImportBindingsRequest {
    /// Forge logins to import, as a filter over the collaborator list. Empty
    /// or absent = everyone, which is a re-sync (ADR-0069 §5).
    #[serde(default)]
    pub subjects: Vec<String>,
}

fn parse_role(s: &str) -> Result<scarab_identity::Role, ApiError> {
    Ok(match s {
        "viewer" => scarab_identity::Role::Viewer,
        "member" => scarab_identity::Role::Member,
        "admin" => scarab_identity::Role::Admin,
        "owner" => scarab_identity::Role::Owner,
        other => {
            return Err(ApiError::BadRequest(format!(
                "unknown role `{other}` (want viewer|member|admin|owner)"
            )))
        }
    })
}

fn role_name(r: scarab_identity::Role) -> &'static str {
    match r {
        scarab_identity::Role::Viewer => "viewer",
        scarab_identity::Role::Member => "member",
        scarab_identity::Role::Admin => "admin",
        scarab_identity::Role::Owner => "owner",
    }
}

/// List an org's live bindings (org- and project-scoped). Administer.
#[utoipa::path(get, path = "/v1/orgs/{org}/bindings", summary = "List an org's role bindings (ADR-0049)", responses((status = 200, body = [BindingDto]), (status = 403, description = "not an org admin")))]
async fn list_bindings(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Vec<BindingDto>>, ApiError> {
    let scope = scarab_identity::Scope::Org(org.clone());
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let rbac = st.rbac.as_ref().ok_or(ApiError::NotFound)?;
    let bindings = rbac
        .bindings(&org)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let mut out = Vec::with_capacity(bindings.len());
    for b in bindings {
        let record = match st.identities.as_ref() {
            Some(store) => store.principal(&b.subject).await.ok().flatten(),
            None => None,
        };
        out.push(BindingDto {
            display_name: record.as_ref().and_then(|r| r.display_name.clone()),
            claimed: record.as_ref().map(|r| r.claimed_at.is_some()),
            subject: b.subject,
            role: role_name(b.role).to_string(),
            project: match b.scope {
                scarab_identity::Scope::Org(_) => String::new(),
                scarab_identity::Scope::Project { name, .. } => name,
            },
        });
    }
    Ok(Json(out))
}

/// Natively grant `subject` a role in the org (or one of its projects).
/// Native bindings are authoritative — no import ever overwrites them.
#[utoipa::path(put, path = "/v1/orgs/{org}/bindings", summary = "Natively grant a role (ADR-0049)", responses((status = 204, description = "granted")))]
async fn put_binding(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(req): Json<BindingRequest>,
) -> Result<StatusCode, ApiError> {
    let org_scope = scarab_identity::Scope::Org(org.clone());
    authorize_scoped(&st, &headers, Action::Administer, Some(&org_scope)).await?;
    let rbac = st.rbac.as_ref().ok_or(ApiError::NotFound)?;
    if req.subject.is_empty() {
        return Err(ApiError::BadRequest("subject is required".into()));
    }
    let binding = scarab_identity::Binding {
        subject: resolve_typed_subject(&st, &req.subject).await?,
        scope: match req.project.filter(|p| !p.is_empty()) {
            Some(name) => scarab_identity::Scope::Project { org, name },
            None => org_scope,
        },
        role: parse_role(&req.role)?,
    };
    rbac.grant(&binding, scarab_identity::BindingOrigin::Native)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// An admin-typed subject → the Principal id it names (ADR-0069 §3): a merged
/// id follows to its target; a string no Principal holds is refused. Granting
/// to one would hand the role to whoever next produced that string at sign-in —
/// the recycled-login takeover, re-opened for every pre-assigned surface.
async fn resolve_typed_subject(st: &AppState, subject: &str) -> Result<String, ApiError> {
    let Some(store) = st.identities.as_ref() else {
        return Ok(subject.to_string());
    };
    scarab_identity::resolve_principal(store.as_ref(), subject)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "`{subject}` is not a Scarab user. They must sign in once, or be imported \
                 from the forge (POST /v1/repos/{{org}}/{{repo}}/bindings/import), before \
                 a role or an approval can name them (ADR-0069)"
            ))
        })
}

/// `GET /v1/orgs/{org}/principals?q=` query.
#[derive(Debug, Deserialize)]
struct PrincipalQuery {
    #[serde(default)]
    q: String,
}

/// One linked identity of a [`PrincipalDto`].
#[derive(Debug, Serialize, ToSchema)]
pub struct PrincipalIdentityDto {
    pub provider: String,
    /// The person's handle there today (login / email) — display only.
    pub handle: String,
}

/// A Principal, as the grant/approver typeahead offers it (ADR-0069 §3).
#[derive(Debug, Serialize, ToSchema)]
pub struct PrincipalDto {
    /// What a grant or an approver entry stores.
    pub id: String,
    pub display_name: Option<String>,
    /// `false` = created by a forge import, not signed into yet.
    pub claimed: bool,
    pub identities: Vec<PrincipalIdentityDto>,
}

/// Search Principals for the grant/approver typeahead (ADR-0069 §3). Principals
/// are install-wide, so this is Administer-gated and capped: an org admin needs
/// to find someone who holds no role here yet, which no org-scoped list could
/// contain.
#[utoipa::path(get, path = "/v1/orgs/{org}/principals", summary = "Search users for a grant or approver entry (ADR-0069)", params(("q" = String, Query, description = "id or handle prefix, or a display-name fragment")), responses((status = 200, body = [PrincipalDto]), (status = 403, description = "not an org admin")))]
async fn search_principals(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    axum::extract::Query(q): axum::extract::Query<PrincipalQuery>,
) -> Result<Json<Vec<PrincipalDto>>, ApiError> {
    let org_scope = scarab_identity::Scope::Org(org);
    authorize_scoped(&st, &headers, Action::Administer, Some(&org_scope)).await?;
    let store = st.identities.as_ref().ok_or(ApiError::NotFound)?;
    let q = q.q.trim();
    if q.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let found = store
        .search(q, PRINCIPAL_SEARCH_LIMIT)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(
        found
            .into_iter()
            .map(|m| PrincipalDto {
                id: m.id,
                display_name: m.display_name,
                claimed: m.claimed,
                identities: m
                    .identities
                    .into_iter()
                    .map(|i| PrincipalIdentityDto {
                        provider: i.provider,
                        handle: i.handle,
                    })
                    .collect(),
            })
            .collect(),
    ))
}

/// A typeahead shows a handful; more is enumeration, not search.
const PRINCIPAL_SEARCH_LIMIT: usize = 20;

/// Natively revoke `subject`'s binding at the scope — a durable tombstone a
/// later forge import cannot resurrect.
#[utoipa::path(delete, path = "/v1/orgs/{org}/bindings", summary = "Natively revoke a role - a durable tombstone (ADR-0049)", responses((status = 204, description = "revoked")))]
async fn delete_binding(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    axum::extract::Query(q): axum::extract::Query<BindingQuery>,
) -> Result<StatusCode, ApiError> {
    let org_scope = scarab_identity::Scope::Org(org.clone());
    authorize_scoped(&st, &headers, Action::Administer, Some(&org_scope)).await?;
    let rbac = st.rbac.as_ref().ok_or(ApiError::NotFound)?;
    let scope = match q.project.filter(|p| !p.is_empty()) {
        Some(name) => scarab_identity::Scope::Project { org, name },
        None => org_scope,
    };
    rbac.revoke(&q.subject, &scope)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Import the repo's forge collaborators as **seed** bindings (ADR-0049,
/// re-founded on pinned identities by ADR-0069 §5): admin→Admin, write→Member,
/// read→Viewer, none→skipped.
///
/// Each collaborator is resolved by the forge's **immutable** user id through
/// the login provider that IS this forge (`forge_url` = the connection's
/// `base_url`). Someone nobody has signed in as yet becomes an unclaimed
/// Principal holding exactly that identity — so whoever later signs in with
/// that forge account claims these roles, and a stranger who takes over the
/// login does not.
///
/// With no `subjects` filter this is a **re-sync**: an import row at this
/// Project for someone the forge no longer lists is deleted. Only on a complete
/// listing — any failed page fails the call and writes nothing. Native grants
/// and native revokes are never touched, and authorization keeps reading only
/// native storage: the forge is consulted here, not on the authz hot path.
#[utoipa::path(post, path = "/v1/repos/{org}/{repo}/bindings/import", summary = "Seed (and re-sync) bindings from forge collaborators (ADR-0049, ADR-0069)", responses((status = 200, body = [BindingDto]), (status = 409, description = "no login provider is associated with this repo's forge"), (status = 501, description = "the forge adapter cannot list collaborators")))]
async fn import_bindings(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
    Json(req): Json<ImportBindingsRequest>,
) -> Result<Json<Vec<BindingDto>>, ApiError> {
    let scope = scarab_identity::Scope::Project {
        org: org.clone(),
        name: repo.clone(),
    };
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let rbac = st.rbac.as_ref().ok_or(ApiError::NotFound)?;
    let identities = st.identities.as_ref().ok_or(ApiError::NotFound)?;
    let forge = st
        .forge
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no forge configured".into()))?;
    let connections = st
        .connections
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no forge connections configured".into()))?;
    let repo_ref = scarab_forge::RepoRef {
        owner: org.clone(),
        name: repo.clone(),
    };

    // Which forge serves this repo, and which login provider's identities are
    // accounts on it.
    let resolved = connections
        .resolve(&repo_ref)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or_else(|| {
            ApiError::BadRequest(format!("{org}/{repo} is not bound to a forge connection"))
        })?;
    let base_url = resolved.connection.base_url.trim_end_matches('/');
    let provider = st
        .oauth_providers
        .iter()
        .find(|p| p.forge_url() == Some(base_url))
        .ok_or_else(|| {
            ApiError::Conflict(format!(
                "no login provider is associated with the forge at {base_url}, so imported \
                 roles could never be signed into. Set SCARAB_OAUTH_FORGE_URL (or `forge_url` \
                 on a SCARAB_OAUTH_PROVIDERS entry) to {base_url} on the provider whose \
                 accounts are this forge's (ADR-0069)"
            ))
        })?;

    let listed = match forge.list_collaborators(&repo_ref).await {
        Ok(people) => people,
        Err(scarab_forge::ForgeError::Unsupported(what)) => {
            return Err(ApiError::NotImplemented(format!(
                "this forge adapter does not support {what}"
            )))
        }
        Err(e) => return Err(ApiError::BadRequest(format!("forge collaborators: {e}"))),
    };
    let filtered = !req.subjects.is_empty();
    let now = st.clock.now().await.0;
    let mut seeds: Vec<(String, scarab_identity::Role)> = Vec::new();
    for c in &listed {
        if filtered
            && !req
                .subjects
                .iter()
                .any(|s| s.eq_ignore_ascii_case(&c.login))
        {
            continue;
        }
        let role = if c.permissions.admin {
            scarab_identity::Role::Admin
        } else if c.permissions.write {
            scarab_identity::Role::Member
        } else if c.permissions.read {
            scarab_identity::Role::Viewer
        } else {
            continue; // listed, but no access — nothing to seed
        };
        let identity = scarab_identity::ProviderIdentity {
            provider: provider.provider_id().to_string(),
            external_id: c.external_id.clone(),
            handle: c.login.clone(),
            email: None,
            email_verified: false,
            display_name: None,
        };
        let principal = identities
            .pin(
                &identity,
                &provider.base_id_for_forge_account(&identity),
                now,
            )
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        match seeds.iter_mut().find(|(s, _)| *s == principal) {
            Some((_, r)) => *r = (*r).max(role),
            None => seeds.push((principal, role)),
        }
    }
    // Prune only on an unfiltered, non-empty listing: a filter is not a claim
    // about everyone else, and an empty list from a forge that always lists at
    // least the repo's owner is a broken answer, not a mass offboarding.
    let prune = !filtered && !listed.is_empty();
    rbac.import_seeds(&scope, provider.provider_id(), &seeds, prune)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(
        seeds
            .into_iter()
            .map(|(subject, role)| BindingDto {
                subject,
                role: role_name(role).to_string(),
                project: repo.clone(),
                display_name: None,
                claimed: None,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Issued API tokens (ADR-0049 amendment): the credential a machine can hold.
//
// Every client surface for one already existed — `scarab-cli` takes `--token` /
// `SCARAB_TOKEN` and sends a Bearer header, and `authenticate` already exempts
// Bearer from CSRF because such a caller presents its credential explicitly.
// What was missing was a credential that is not a 24h browser session id.
// ---------------------------------------------------------------------------

/// `POST /v1/orgs/{org}/tokens` body. Every field is required on purpose: a
/// credential with no verb and no expiry is the shape this repo already learned
/// not to mint twice.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MintTokenRequest {
    /// Human label — what makes the token revocable in practice. "demo
    /// keepalive", "amy's laptop".
    pub name: String,
    /// Project within the org to scope the token to. Absent = the whole org,
    /// inheriting down to its projects exactly as an org binding does.
    #[serde(default)]
    pub project: Option<String>,
    /// The **ceiling**: `viewer` | `member` | `admin` | `owner`. Never more than
    /// the minter holds in this scope, and capped again at each request by what
    /// the owner holds *then*.
    pub role: String,
    /// Lifetime in days, `1..=365`. No default and no "never".
    pub expires_in_days: u32,
}

/// The mint response — the **only** place the plaintext ever appears.
#[derive(Debug, Serialize, ToSchema)]
pub struct MintedTokenDto {
    /// The credential. Returned once, here. Scarab keeps only its SHA-256, so
    /// this exact string cannot be recovered from the database, from a backup,
    /// or by any later API call: a lost token is re-minted, never re-read.
    pub token: String,
    /// The token's record, for immediate display in the list it just joined.
    pub record: ApiTokenDto,
}

/// One issued token, as the API describes it — record only, never the secret.
#[derive(Debug, Serialize, ToSchema)]
pub struct ApiTokenDto {
    pub id: String,
    pub name: String,
    /// The principal whose live authority bounds this token.
    pub owner_subject: String,
    pub org: String,
    /// `None` for an org-scoped token.
    pub project: Option<String>,
    /// The granted ceiling, not the effective role — what the token can do
    /// depends on what its owner holds at the moment of each request.
    pub role: String,
    pub created_by: String,
    /// Unix-ms.
    pub created_at: i64,
    /// Unix-ms.
    pub expires_at: i64,
    /// Unix-ms of the last request that presented it, written back at most once
    /// a minute. `None` means it has never been used.
    pub last_used_at: Option<i64>,
    /// Unix-ms revocation, or `None` while it is live.
    pub revoked_at: Option<i64>,
}

impl From<scarab_identity::ApiToken> for ApiTokenDto {
    fn from(t: scarab_identity::ApiToken) -> Self {
        let (org, project) = match &t.scope {
            scarab_identity::Scope::Org(org) => (org.clone(), None),
            scarab_identity::Scope::Project { org, name } => (org.clone(), Some(name.clone())),
        };
        Self {
            id: t.id,
            name: t.name,
            owner_subject: t.owner_subject,
            org,
            project,
            role: role_name(t.role).to_string(),
            created_by: t.created_by,
            created_at: t.created_at,
            expires_at: t.expires_at,
            last_used_at: t.last_used_at,
            revoked_at: t.revoked_at,
        }
    }
}

const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

/// Mint an API token scoped to this org (or one of its projects). Administer on
/// the scope the token targets. The plaintext is returned **once**, here.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/tokens",
    params(("org" = String, Path, description = "org the token is scoped to")),
    request_body = MintTokenRequest,
    summary = "Mint an API token - the plaintext is returned ONCE (ADR-0049)",
    responses(
        (status = 201, body = MintedTokenDto),
        (status = 400, description = "bad role, name, or lifetime, or the role exceeds the minter's"),
        (status = 403, description = "not an admin of the target scope, or the caller is itself a token"),
        (status = 404, description = "API tokens not configured")
    )
)]
async fn mint_api_token(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(req): Json<MintTokenRequest>,
) -> Result<Response, ApiError> {
    let store = st.api_tokens.as_ref().ok_or(ApiError::NotFound)?.clone();
    let scope = match req.project.as_deref().filter(|p| !p.is_empty()) {
        Some(project) => scarab_identity::Scope::Project {
            org: org.clone(),
            name: project.to_string(),
        },
        None => scarab_identity::Scope::Org(org.clone()),
    };

    let caller = authenticate(&st, &headers, Action::Administer).await?;
    // A token may not mint tokens. Without this, "every token expires" is a
    // story rather than a property: a token holding Administer could mint its
    // own successor forever, and the expiry that makes this credential safe to
    // hand to a machine would bound nothing at all. Issuance stays an act a
    // human performs.
    if caller.grant.is_some() {
        return Err(ApiError::Forbidden);
    }
    if !caller.may_globally(Action::Administer)
        && !caller.may(&st, Action::Administer, &scope).await?
    {
        return Err(ApiError::Forbidden);
    }

    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest(
            "token `name` must not be empty".into(),
        ));
    }
    if req.expires_in_days == 0 || req.expires_in_days > scarab_identity::MAX_API_TOKEN_DAYS {
        return Err(ApiError::BadRequest(format!(
            "`expires_in_days` must be 1..={} — a token with no expiry is the one shape this credential must never take",
            scarab_identity::MAX_API_TOKEN_DAYS
        )));
    }
    let role = parse_role(&req.role)?;

    // Least privilege, enforced at mint: the grant may not exceed what the
    // minter holds in this scope. The easy implementation — clone the minter's
    // principal into the row, exactly as `sessions` denormalises it — is
    // precisely the one that yields permanent Owner credentials nobody can
    // scope down afterwards.
    //
    // Bounded by what the TOKEN will be able to re-derive per request — see
    // `token_owner_roles` — not by what the minter's session happens to carry.
    // The two differ, and using the session's view here would mint credentials
    // that pass this check and then hold nothing at all: a minter whose only
    // authority is a login-time snapshot is told so HERE, with the fix in the
    // message, instead of walking away with a dead token.
    let owner_view = Principal {
        subject: caller.principal.subject.clone(),
        display_name: None,
        roles: token_owner_roles(&st, &caller.principal.subject).await,
    };
    match live_role_in(&st, &owner_view, &scope).await? {
        Some(held) if held >= role => {}
        Some(held) => {
            return Err(ApiError::BadRequest(format!(
                "cannot mint a `{}` token: you hold `{}` in this scope, and a token may not exceed its minter",
                role_name(role),
                role_name(held)
            )))
        }
        None => {
            return Err(ApiError::BadRequest(
                "cannot mint a token here: you hold no role binding in this scope — grant yourself one on the org or project first (PUT /v1/orgs/{org}/bindings)".into(),
            ))
        }
    }

    let now = st.clock.now().await.0;
    // The random half of minting lives HERE, at the composition root: a CSPRNG
    // is I/O, so `scarab-identity` — which owns the hash and the authority math
    // — cannot mint a token it is able to verify (ADR-0031).
    let plaintext = format!(
        "{}{}",
        scarab_identity::API_TOKEN_PREFIX,
        random_token_secret()
    );
    let token = scarab_identity::ApiToken {
        id: Uuid::new_v4().to_string(),
        name: name.to_string(),
        owner_subject: caller.principal.subject.clone(),
        scope,
        role,
        expires_at: now + i64::from(req.expires_in_days) * MS_PER_DAY,
        created_by: caller.principal.subject.clone(),
        created_at: now,
        last_used_at: None,
        revoked_at: None,
    };
    store
        .put(&token, &scarab_identity::api_token_hash(&plaintext))
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    Ok((
        StatusCode::CREATED,
        Json(MintedTokenDto {
            token: plaintext,
            record: ApiTokenDto::from(token),
        }),
    )
        .into_response())
}

/// 32 bytes of CSPRNG, base64url without padding — a high-entropy bearer
/// secret, not a password, which is why the store hashes it with a plain digest
/// rather than stretching it.
fn random_token_secret() -> String {
    use base64::Engine as _;
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// List the tokens issued within this org, org- and project-scoped alike.
/// Administer on the Org. Records only — there is no plaintext to return.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/tokens",
    params(("org" = String, Path, description = "org")),
    summary = "List the API tokens issued in an org (ADR-0049)",
    responses(
        (status = 200, body = [ApiTokenDto]),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "API tokens not configured")
    )
)]
async fn list_api_tokens(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Vec<ApiTokenDto>>, ApiError> {
    let store = st.api_tokens.as_ref().ok_or(ApiError::NotFound)?;
    let scope = scarab_identity::Scope::Org(org.clone());
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let tokens = store
        .list(&org)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(tokens.into_iter().map(ApiTokenDto::from).collect()))
}

/// Revoke an issued token — permanent, and effective on the very next request
/// that presents it. Administer on the Org.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/tokens/{id}",
    params(
        ("org" = String, Path, description = "org"),
        ("id" = String, Path, description = "token id - NOT the token itself")
    ),
    summary = "Revoke an issued API token (ADR-0049)",
    responses(
        (status = 204, description = "revoked"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such live token in this org")
    )
)]
async fn revoke_api_token(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let store = st.api_tokens.as_ref().ok_or(ApiError::NotFound)?;
    let scope = scarab_identity::Scope::Org(org.clone());
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    // Scoped by org BEFORE revoking: `id` alone is a global handle, and an
    // admin of one org must not be able to reach into another's by guessing.
    let tokens = store
        .list(&org)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if !tokens.iter().any(|t| t.id == id) {
        return Err(ApiError::NotFound);
    }
    let now = st.clock.now().await.0;
    match store
        .revoke(&id, now)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
    {
        true => Ok(StatusCode::NO_CONTENT),
        // Already revoked. Reported rather than smoothed over, so the stamp of
        // when the credential actually died is never quietly rewritten.
        false => Err(ApiError::NotFound),
    }
}

/// How coarsely a token's `last_used_at` is written back. Observability, not
/// accounting: the question it answers is "is anyone still using this?", and one
/// write per token per minute answers it without putting an UPDATE on the hot
/// path of every request a busy CI robot makes.
const API_TOKEN_TOUCH_INTERVAL_MS: i64 = 60_000;

/// The authenticated caller of a request: who they are, plus — when the
/// credential was an issued API token — the narrowing that token carries.
///
/// This type exists because a token is **not** a principal. Building one and
/// handing it onward as a bare [`Principal`] would silently give the token
/// everything its owner has: every scoped decision in this file would resolve
/// the OWNER's bindings and never learn the caller was restricted to one repo.
/// Keeping the grant attached, and asking [`Caller::role_in`] rather than
/// `rbac.role_of` directly, is what makes the restriction real instead of
/// decorative.
#[derive(Debug, Clone)]
struct Caller {
    principal: Principal,
    /// `Some` when an issued API token authenticated this request.
    grant: Option<scarab_identity::ApiToken>,
    /// How the credential was presented. `None` only for the dev-insecure
    /// synthetic Owner (no session store wired), which presented nothing.
    via: Option<AuthVia>,
    /// The session behind the request, when it was one (cookie or Bearer) —
    /// what a StepUpConfirmation is bound to (ADR-0073 §4, §7).
    session: Option<SessionFacts>,
}

/// What ADR-0073 needs to know about the session a request rode on.
#[derive(Debug, Clone)]
struct SessionFacts {
    /// The session id itself — the bearer credential. Kept only so a
    /// StepUpConfirmationRequest raised over a Bearer session can seal it and
    /// re-check, at replay, that the session is still live. Never logged.
    id: step_up::requests::SessionSecret,
    /// Hex SHA-256 of the session id: confirmations are keyed by it, never
    /// by the id itself.
    id_hash: String,
    /// Until when it counts as a fresh browser login (first-passkey rule).
    fresh_login_until: Option<i64>,
}

impl Caller {
    /// A caller with no token narrowing and no session behind it — the
    /// dev-insecure synthetic Owner.
    fn dev_insecure_owner(principal: Principal) -> Self {
        Self {
            principal,
            grant: None,
            via: None,
            session: None,
        }
    }

    /// May a window / action StepUpConfirmation stand for this caller? Only a
    /// human's own browser: a cookie session, never a token or a session
    /// presented as Bearer (ADR-0073 §7).
    fn is_browser_session(&self) -> bool {
        self.grant.is_none() && self.via == Some(AuthVia::Cookie) && self.session.is_some()
    }

    /// The `caller` column of an OrgSecurityEventLog row.
    fn event_label(&self) -> String {
        match (&self.grant, self.via) {
            (Some(token), _) => format!("api_token:{}", token.id),
            (None, Some(AuthVia::Cookie)) => "session:cookie".to_string(),
            (None, Some(AuthVia::Bearer)) => "session:bearer".to_string(),
            (None, None) => "dev:insecure".to_string(),
        }
    }

    /// May this caller act on an **untenanted** resource — one belonging to no
    /// Org or Project (an inline dev run, the catch-all default)?
    ///
    /// Never, for a token. A token's authority is a scope it covers, and an
    /// untenanted resource sits in no scope to be covered; answering anything
    /// else here would be a hole straight through the scoping the rest of this
    /// type enforces.
    fn may_globally(&self, action: Action) -> bool {
        self.grant.is_none() && self.principal.can(action)
    }

    /// The role this caller effectively holds in `scope`, token ceiling applied.
    async fn role_in(
        &self,
        st: &AppState,
        scope: &scarab_identity::Scope,
    ) -> Result<Option<scarab_identity::Role>, ApiError> {
        let live = live_role_in(st, &self.principal, scope).await?;
        match &self.grant {
            None => Ok(live),
            Some(token) if token.covers(scope) => Ok(token.effective_role(live)),
            Some(_) => Ok(None),
        }
    }

    /// May this caller perform `action` in `scope`?
    async fn may(
        &self,
        st: &AppState,
        action: Action,
        scope: &scarab_identity::Scope,
    ) -> Result<bool, ApiError> {
        Ok(self
            .role_in(st, scope)
            .await?
            .is_some_and(|role| role.allows(action)))
    }
}

/// The role `principal` holds in `scope` **right now**, across the two live
/// sources ADR-0049 recognises: the C1 global bootstrap carried on the
/// principal, and the C2 native bindings in the store. The higher wins, exactly
/// as [`authorize_scoped`] has always treated them.
///
/// "Right now" is the load-bearing word for tokens. This is re-read on every
/// request rather than frozen into the token row at mint, which is why a
/// demotion reaches a token the instant it lands.
async fn live_role_in(
    st: &AppState,
    principal: &Principal,
    scope: &scarab_identity::Scope,
) -> Result<Option<scarab_identity::Role>, ApiError> {
    let global = principal.roles.iter().copied().max();
    let bound = match st.rbac.as_ref() {
        Some(rbac) => rbac
            .role_of(&principal.subject, scope)
            .await
            .map_err(|_| ApiError::Forbidden)?,
        None => None,
    };
    Ok(global.into_iter().chain(bound).max())
}

/// The global roles an issued token can re-derive for `subject` on **every**
/// request: the C1 bootstrap owners list, and nothing else.
///
/// Deliberately not `Principal::roles` off a session. Those are a login-time
/// snapshot — including the blanket `Viewer` login hands every human — and a
/// token capped by them would be capped by authority nothing can re-check
/// later, which is the frozen-authority trap this design exists to avoid. The
/// mint endpoint bounds a grant with exactly this view too, so what a minter is
/// allowed to issue and what the token will actually do cannot drift.
async fn token_owner_roles(st: &AppState, subject: &str) -> Vec<scarab_identity::Role> {
    // An owners entry naming an id since merged into this Principal still
    // means them (ADR-0069 §6). A store failure only ever costs authority.
    let mut ids = vec![subject.to_string()];
    if let Some(store) = st.identities.as_ref() {
        ids.extend(store.aliases_of(subject).await.unwrap_or_default());
    }
    // The owners list is install-wide (ADR-0069), so any provider's view of it
    // answers; `max` keeps this correct should they ever differ.
    st.oauth_providers
        .iter()
        .flat_map(|o| ids.iter().filter_map(|id| o.bootstrap_owner_role(id)))
        .max()
        .into_iter()
        .collect()
}

/// Authenticate the request: resolve the credential (Bearer or cookie), enforce
/// expiry + the CSRF double-submit on cookie mutations, and return the
/// [`Caller`] — **no role decision** (that's [`authorize`]/[`authorize_scoped`]).
/// With no session store configured, authn is **disabled** (dev/test) and every
/// caller is a synthetic Owner (the loud SCARAB_DEV_INSECURE posture).
async fn authenticate(
    st: &AppState,
    headers: &HeaderMap,
    action: Action,
) -> Result<Caller, ApiError> {
    // A StepUpConfirmationRequest being replayed (ADR-0073 §6): the request
    // carries no credential at all. The caller is rebuilt from what raised
    // it — the token by id, or the Bearer session — re-checked as live NOW
    // and narrowed to the human's live authority; headers are ignored, and
    // CSRF has nothing to defend (no browser sent this request). The
    // task-local is set only by the confirm endpoint around its replay.
    if let Some(authority) = step_up::requests::replay_authority() {
        return step_up::requests::replay_caller(st, &authority).await;
    }
    let Some(sessions) = st.sessions.as_ref() else {
        return Ok(Caller::dev_insecure_owner(Principal {
            subject: "anonymous".into(),
            display_name: None,
            roles: vec![scarab_identity::Role::Owner],
        }));
    };
    let (sid, via) = session_id(headers).ok_or(ApiError::Unauthorized)?;
    // Route on the prefix rather than probing both stores (ADR-0049
    // amendment): a mistyped token must fail as a token, with no chance of
    // being taken for a session id.
    if sid.starts_with(scarab_identity::API_TOKEN_PREFIX) {
        return authenticate_api_token(st, &sid, via).await;
    }
    let session = sessions
        .get(&sid)
        .await
        .map_err(|_| ApiError::Unauthorized)?
        .ok_or(ApiError::Unauthorized)?;
    if !session.is_valid(st.clock.now().await.0) {
        return Err(ApiError::Unauthorized);
    }
    // CSRF (ADR-0049): a cookie-authenticated MUTATION must double-submit the
    // session's token in `x-csrf-token` — a cross-site form can make the
    // browser send the cookie, but it can never read the token. Bearer
    // requests (API/CLI) carry the credential explicitly; no CSRF surface.
    if action != Action::Read && via == AuthVia::Cookie {
        let presented = headers.get("x-csrf-token").and_then(|v| v.to_str().ok());
        if !session.csrf_matches(presented) {
            return Err(ApiError::Forbidden);
        }
    }
    let facts = SessionFacts {
        id: step_up::requests::SessionSecret(session.id.clone()),
        id_hash: session_hash(&session.id),
        fresh_login_until: session.fresh_login_until(),
    };
    Ok(Caller {
        principal: session.principal,
        grant: None,
        via: Some(via),
        session: Some(facts),
    })
}

/// Authenticate a `scarab_pat_…` bearer (ADR-0049 amendment): hash it, load the
/// row, check it is live, and build the [`Caller`] that its owner's identity
/// plus its own grant describe.
///
/// Every failure answers the same 401 on the wire — unknown, expired and
/// revoked are indistinguishable neighbours to a caller holding a bad string,
/// and telling them apart would turn this into an oracle for which tokens exist.
async fn authenticate_api_token(
    st: &AppState,
    plaintext: &str,
    via: AuthVia,
) -> Result<Caller, ApiError> {
    // A token is a Bearer credential, full stop. Honouring one from the cookie
    // jar would put it back on the ambient path CSRF exists to defend — and the
    // Bearer branch above is exempt from CSRF *precisely because* a Bearer
    // caller presents its credential deliberately.
    if via != AuthVia::Bearer {
        return Err(ApiError::Unauthorized);
    }
    let Some(store) = st.api_tokens.as_ref() else {
        return Err(ApiError::Unauthorized);
    };
    let token = store
        .by_hash(&scarab_identity::api_token_hash(plaintext))
        .await
        .map_err(|_| ApiError::Unauthorized)?
        .ok_or(ApiError::Unauthorized)?;
    let now = st.clock.now().await.0;
    if !token.is_live(now) {
        return Err(ApiError::Unauthorized);
    }
    // Observability, coarse and best-effort: a failure to record the use must
    // never fail the request that was being observed.
    if token
        .last_used_at
        .is_none_or(|prev| now - prev >= API_TOKEN_TOUCH_INTERVAL_MS)
    {
        let _ = store.touch(&token.id, now).await;
    }
    let roles = token_owner_roles(st, &token.owner_subject).await;
    Ok(Caller {
        principal: Principal {
            subject: token.owner_subject.clone(),
            display_name: Some(format!("API token \u{201c}{}\u{201d}", token.name)),
            roles,
        },
        grant: Some(token),
        via: Some(via),
        session: None,
    })
}

/// Authorize `action` with **global** (flat) roles only — for resources that
/// belong to no tenant (inline dev runs, the catch-all default).
async fn authorize(
    st: &AppState,
    headers: &HeaderMap,
    action: Action,
) -> Result<Principal, ApiError> {
    authorize_scoped(st, headers, action, None).await
}

/// Authorize `action` in `scope` (ADR-0049 C2). The decision is scope-aware:
/// a flat **global** role on the principal (the C1/owners bootstrap) allows
/// everywhere; otherwise the native role bindings decide **for the request's
/// Org/Project** — never a live forge call. `None` scope (an untenanted
/// resource) is global-only.
async fn authorize_scoped(
    st: &AppState,
    headers: &HeaderMap,
    action: Action,
    scope: Option<&scarab_identity::Scope>,
) -> Result<Principal, ApiError> {
    let caller = authenticate(st, headers, action).await?;
    if caller.may_globally(action) {
        return Ok(caller.principal);
    }
    if let Some(scope) = scope {
        if caller.may(st, action, scope).await? {
            return Ok(caller.principal);
        }
    }
    Err(ApiError::Forbidden)
}

/// The tenant scope of a run (ADR-0049), if it was stamped at creation.
async fn run_scope(st: &AppState, run: &RunId) -> Option<scarab_identity::Scope> {
    st.db
        .run_tenant(run)
        .await
        .ok()
        .flatten()
        .map(|(org, name)| scarab_identity::Scope::Project { org, name })
}

/// How a request presented its session — Bearer carries the credential
/// explicitly (API/CLI); Cookie rides ambiently on browser requests and
/// therefore needs CSRF proof on mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthVia {
    Bearer,
    Cookie,
}

/// Extract a session id from `Authorization: Bearer <id>` or a
/// `scarab_session=<id>` cookie.
fn session_id(headers: &HeaderMap) -> Option<(String, AuthVia)> {
    if let Some(tok) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Some((tok.to_string(), AuthVia::Bearer));
    }
    cookie_value(headers, "scarab_session").map(|v| (v, AuthVia::Cookie))
}

/// The value of cookie `name`, if present.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie = headers.get("cookie").and_then(|v| v.to_str().ok())?;
    cookie
        .split(';')
        .filter_map(|p| p.trim().strip_prefix(&format!("{name}=")))
        .map(str::to_string)
        .next()
}

/// The distinct principals who have approved a `manual` gate `step` in its
/// CURRENT wait, in approval order (ADR-0037) — approvals given before the
/// gate's latest re-arm (an expired wait that was retried) do not count. One
/// definition, shared with the engine's dedup: [`scarab_engine::gate_approvers`].
fn gate_approvers(events: &[EventKind], step: &StepId) -> Vec<String> {
    scarab_engine::gate_approvers(events, step)
}

/// Approve a `manual` gate (ADR-0008, 0037). The authenticated principal's
/// approval is recorded on the event log (append-only, idempotent). For a
/// **deploy** run (one with a target environment), the gate is released only
/// once the accumulated approvers satisfy the Environment's effective rules for
/// the run's Project (ADR-0071 §3, `admits`); on release the deployment is
/// written to the legacy history. If the Environment no longer exists or no
/// longer admits the Project, the gate does not release (409). A plain gate (no
/// environment) releases on the first approval. Authz'd as a write.
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/gates/{step}/approve",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "gate step id")
    ),
    responses(
        (status = 202, description = "approval recorded (released or awaiting more approvals)"),
        (status = 404, description = "no such run or gate"),
        (status = 409, description = "not an approvable gate (not a manual gate, not pending, or not reached yet), or the run's Environment no longer exists or no longer admits its project")
    )
)]
async fn approve_gate(
    State(st): State<AppState>,
    Extension(guard): Extension<crate::step_up::GuardCtx>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    let principal = authorize_scoped(&st, &headers, Action::Write, scope.as_ref()).await?;
    let step = StepId(step);

    // 0. The governing Environment's effective rules for this Project
    //    (ADR-0071 §3), resolved NOW — before the approval is recorded,
    //    because they decide whether recording it needs a step-up (ADR-0073
    //    §3): an approval on a gate of an Environment with approvers is a
    //    crown jewel. So is one whose Environment cannot be resolved (gone,
    //    no longer admitting the Project, or a store error) while governance
    //    is wired — fail-closed: the approval would count if it came back.
    //    A plain gate, or an Environment with no approvers, is exempt. The
    //    same snapshot then governs the release below, so the approval is
    //    judged and released under one set of rules.
    let ctx = st.db.run_deploy_context(&run).await.map_err(ApiError::Db)?;
    // The approval stays recorded when the gate is held; it counts if the
    // Environment is restored.
    let governance = gate_release_governance(
        &st,
        ctx.as_ref(),
        "approval recorded, but the gate cannot release",
    )
    .await;
    match (&ctx, &governance) {
        (Some(c), Ok(Some(p))) if !p.approvers.is_empty() => {
            guard
                .require(
                    &st,
                    vec![format!(
                        "approves a deploy of `{}` into `{}`, which requires approvers",
                        c.project, c.environment
                    )],
                )
                .await?
        }
        (Some(c), Err(_)) => {
            guard
                .require(
                    &st,
                    vec![format!(
                        "approves a deploy of `{}` into `{}`, whose rules cannot be resolved",
                        c.project, c.environment
                    )],
                )
                .await?
        }
        _ => guard.exempt("no approvers required"),
    }

    // ADR-0076 §6: a gate inlined from a yanked library version neither
    // takes an approval nor releases. Where `gate.resolve` (ADR-0075 §2)
    // will be consulted.
    refuse_yanked_step(&st, &run, &step, "the gate cannot release").await?;

    // 1. Record this principal's approval — append-only, no resume, idempotent
    //    per (step, subject).
    match scarab_engine::record_gate_approval(
        st.db.as_ref(),
        st.clock.as_ref(),
        &run,
        &step,
        &principal.subject,
    )
    .await
    {
        Ok(()) => {}
        Err(RerunError::StepNotFound(_)) => return Err(ApiError::NotFound),
        // The step exists but can't take an approval right now — not a manual
        // gate, or a gate that is no longer awaiting approval (skipped/terminal/
        // already released). 409, and NO phantom `GateApproved` was appended.
        Err(
            e @ (RerunError::DependencyNotSatisfied { .. }
            | RerunError::NotFailed { .. }
            | RerunError::NotAManualGate(_)
            | RerunError::GateNotPending { .. }
            | RerunError::GateNotReached { .. }
            | RerunError::NotARejectableGate(_)
            | RerunError::CreationUnfinished),
        ) => return Err(ApiError::Conflict(e.to_string())),
        Err(RerunError::Db(e)) => return Err(ApiError::Db(e)),
    }

    // If the gate is already released, this is a late/duplicate approval — the
    // deploy already happened; nothing more to do (avoids double history).
    let released_already = st
        .db
        .steps_of_run(&run)
        .await
        .map_err(ApiError::Db)?
        .iter()
        .any(|s| s.step == step && s.status == StepStatus::Succeeded);
    if released_already {
        return Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "released": true })),
        )
            .into_response());
    }

    // 2. Gather the accumulated approvers; the governing Environment's rules
    //    were resolved at 0., NOW rather than at Run creation: an admin's
    //    change since — tightened rules, a withdrawn eligibility, a deleted
    //    Environment — governs the release.
    let approvers = gate_approvers(&st.db.events(&run).await.map_err(ApiError::Db)?, &step);
    let protection = governance?;

    // 3. A protected deploy releases only when admits() passes over the
    //    accumulated approvers (every required approver, and the ref still
    //    allowed); an unprotected/plain gate releases now.
    let admitted = match (&ctx, &protection) {
        (Some(c), Some(p)) => p.admits(&c.git_ref, &approvers).is_ok(),
        _ => true,
    };
    if !admitted {
        return Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "released": false, "approvals": approvers })),
        )
            .into_response());
    }

    // 4. Finalize the gate and resume the run (exactly-once).
    match scarab_engine::release_gate(st.db.as_ref(), st.clock.as_ref(), &run, &step).await {
        Ok(()) => {}
        Err(RerunError::StepNotFound(_)) => return Err(ApiError::NotFound),
        Err(
            e @ (RerunError::DependencyNotSatisfied { .. }
            | RerunError::NotFailed { .. }
            | RerunError::NotAManualGate(_)
            | RerunError::GateNotPending { .. }
            | RerunError::GateNotReached { .. }
            | RerunError::NotARejectableGate(_)
            | RerunError::CreationUnfinished),
        ) => return Err(ApiError::Conflict(e.to_string())),
        Err(RerunError::Db(e)) => return Err(ApiError::Db(e)),
    }

    // Derive `released` from the real post-release state rather than assuming:
    // `release_gate` swallows a non-Pending CAS as an exactly-once no-op, so a
    // bare `Ok(())` does not by itself prove a transition. The gate is released
    // iff its step is now `Succeeded`.
    let released = st
        .db
        .steps_of_run(&run)
        .await
        .map_err(ApiError::Db)?
        .iter()
        .any(|s| s.step == step && s.status == StepStatus::Succeeded);

    // 5. Record the deployment in the legacy approval history (ADR-0024, 0037,
    //    ADR-0070 §1) — only on a real release (a no-op re-release writes no
    //    duplicate history record). Kept through ADR-0071: it is the record
    //    `GET …/environments/{name}/deployments` still serves.
    if released {
        if let (Some(c), Some(store)) = (&ctx, st.environments.as_ref()) {
            let now = st.clock.now().await.0;
            store
                .record_deployment(&scarab_project::Deployment {
                    org: c.org.clone(),
                    project: c.project.clone(),
                    environment: c.environment.clone(),
                    git_ref: c.git_ref.clone(),
                    run: run.0.clone(),
                    approved_by: approvers.clone(),
                    at: now,
                })
                .await
                .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        }
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "released": released, "approvals": approvers })),
    )
        .into_response())
}

/// `POST /v1/runs/{id}/gates/{step}/reject` body: why the gate is failed.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RejectGateRequest {
    /// The human reason, journalled on the gate's verdict and its Attempt.
    /// Required: 1 to 1024 bytes once trimmed.
    pub reason: String,
}

/// The longest gate-reject reason accepted, in bytes (after trimming).
const GATE_REJECT_REASON_MAX_BYTES: usize = 1024;

/// A reject reason, trimmed and bounded (1..=1024 bytes), or a 400. A NUL
/// is refused too: Postgres cannot store it, so it would fail the write.
fn gate_reject_reason(reason: &str) -> Result<&str, ApiError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.len() > GATE_REJECT_REASON_MAX_BYTES {
        return Err(ApiError::BadRequest(format!(
            "reason is required: 1 to {GATE_REJECT_REASON_MAX_BYTES} bytes"
        )));
    }
    if reason.contains('\0') {
        return Err(ApiError::BadRequest(
            "reason contains a NUL character".into(),
        ));
    }
    Ok(reason)
}

/// Reject a waiting gate: fail it with a reason (ADR-0070 §9).
///
/// For any gate — `manual`, `timer` or `external` — that is pending and
/// reached (every step it needs has succeeded; slice b, criteria 2 and 3).
/// The gate's Attempt fails with
/// the reason and the rejecting principal, its dependents are skipped, and the
/// run ends `failed`; retrying the gate opens a new wait that asks again.
///
/// Authz'd as a run write, the same as approve. A reject only ever stops a
/// deploy — any writer can already cancel the run — so it does NOT consult the
/// Environment's approver rules, and it works when the Environment is gone or
/// no longer admits the project: the clean way out of a held gate.
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/gates/{step}/reject",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "gate step id")
    ),
    request_body = RejectGateRequest,
    responses(
        (status = 202, description = "this call rejected the gate: `{\"rejected\": true}`"),
        (status = 400, description = "missing, empty or over-long reason"),
        (status = 404, description = "no such run or step"),
        (status = 409, description = "not a rejectable gate, not pending, or not reached yet")
    )
)]
async fn reject_gate(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    let principal = authorize_scoped(&st, &headers, Action::Write, scope.as_ref()).await?;
    let req: RejectGateRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("bad reject body: {e}")))?;
    let reason = gate_reject_reason(&req.reason)?;
    gate_reject_outcome(
        scarab_engine::reject_gate(
            st.db.as_ref(),
            st.clock.as_ref(),
            &run,
            &StepId(step),
            &principal.subject,
            reason,
        )
        .await,
    )?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "rejected": true })),
    )
        .into_response())
}

/// Map a [`scarab_engine::reject_gate`] outcome: `Ok(false)` (a racing
/// resolver got there first) is a 409 like any other not-pending gate.
fn gate_reject_outcome(res: Result<bool, RerunError>) -> Result<(), ApiError> {
    match rerun_outcome(res)? {
        true => Ok(()),
        false => Err(ApiError::Conflict(
            "the gate is no longer awaiting a verdict".into(),
        )),
    }
}

/// The governing Environment's effective rules for releasing a Deploy Run's
/// gate, resolved NOW (ADR-0071 §2-3) — shared by every path that releases a
/// gate by request (`approve_gate`, `release_gate_external`) so neither can
/// drift into "no rules, release".
///
/// Fail-closed: a gate is the last point before the deploy proceeds, so an
/// Environment that no longer exists, or no longer admits this Project, holds
/// it with a 409 whose message starts with `held`. `Ok(None)` = a plain gate
/// (no deploy context) or a hand-built state with no governance store, which
/// releases as it always did.
///
/// Timer gates release inside the engine with no request to hold; they are
/// covered by the launch re-check instead (`SecretInjectingExecutor`): the
/// first step after the gate is refused at launch and fails `Config`.
async fn gate_release_governance(
    st: &AppState,
    ctx: Option<&scarab_engine::DeployContext>,
    held: &str,
) -> Result<Option<scarab_project::EffectiveProtection>, ApiError> {
    let (Some(c), Some(store)) = (ctx, st.environment_governance.as_ref()) else {
        return Ok(None);
    };
    let governance = store
        .deploy_governance(&c.org, &c.project, &c.environment)
        .await
        .map_err(org_environments::org_environment_api_error)?;
    match governance {
        None => Err(ApiError::Conflict(format!(
            "{held}: {}",
            unknown_environment_message(&c.org, &c.project, &c.environment)
        ))),
        Some(g) if !g.eligible => Err(ApiError::Conflict(format!(
            "{held}: {}",
            not_eligible_message(&c.org, &c.project, &c.environment)
        ))),
        Some(g) => Ok(Some(g.effective_protection())),
    }
}

/// HTTP header carrying an external-gate release token (`sha256=<hex>`).
const GATE_TOKEN_HEADER: &str = "x-scarab-gate-token";

/// Optional `POST /v1/runs/{id}/gates/{step}/release` body: an external gate's
/// verdict. Absent (or `passed: true`) releases the gate; `passed: false`
/// fails it with `reason`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ExternalGateVerdict {
    /// `false` fails the gate; `true` (the default) releases it.
    #[serde(default = "default_true")]
    pub passed: bool,
    /// Why the gate failed — required (1 to 1024 bytes, trimmed) with
    /// `passed: false`; ignored on a release.
    #[serde(default)]
    pub reason: Option<String>,
}

impl Default for ExternalGateVerdict {
    fn default() -> Self {
        Self {
            passed: true,
            reason: None,
        }
    }
}

/// Release an **external** gate by presenting its token (ADR-0034) — the path an
/// outside system (a deploy webhook, a change-management tool) uses instead of an
/// interactive approval. The token is `HMAC-SHA256(secret, "{run}:{step}")`
/// (`sha256=<hex>`), verified in constant time; no per-gate storage. The endpoint
/// is 404 when no token secret is configured, and only releases gates of kind
/// `external` (manual gates stay approval-only). In a Deploy Run, the gate is
/// held (409) when the run's Environment no longer exists or no longer admits
/// its Project (ADR-0071 §2) — the same check `approve_gate` makes. The token
/// proves who is releasing, not that the deploy is still allowed.
///
/// The same token can instead **fail** the gate with a reason: a body of
/// `{"passed": false, "reason": "…"}` rejects it (journalled `by
/// external:{step}`). A fail is never held — it only stops the deploy — so it
/// skips the Environment check. No body, or `"passed": true`, is a release.
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/gates/{step}/release",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "gate step id")
    ),
    request_body(content = Option<ExternalGateVerdict>, description = "external gate (`x-scarab-gate-token`): optional, `{\"passed\": false, \"reason\": \"…\"}` fails the gate instead of releasing it. Plugin gate (a plugin's `scarab-api` bearer token, ADR-0076 §7): a `PluginGateRelease`, `{\"attempt\", \"passed\": true, \"outputs\": {…}}` or `{\"attempt\", \"passed\": false, \"reason\": \"…\"}`"),
    responses(
        (status = 202, description = "gate released (or, with a fail body, failed)"),
        (status = 400, description = "malformed body, or a fail without a 1..1024-byte reason"),
        (status = 401, description = "bad or missing token"),
        (status = 404, description = "no such external gate, or token release disabled"),
        (status = 409, description = "the gate is not pending or not reached yet, or (a release) the run's Environment no longer exists or no longer admits its project")
    )
)]
async fn release_gate_external(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    // A plugin answering one of its own gates (ADR-0076 §7) presents its
    // `scarab-api` token, never a gate token.
    if plugin_gates::is_plugin_call(&headers) {
        return plugin_gates::release(&st, &headers, &id, &step, &body).await;
    }
    // Token release is opt-in: without a configured secret the endpoint 404s.
    let Some(secret) = st.gate_token_secret.as_ref() else {
        return Err(ApiError::NotFound);
    };
    let run = RunId(id);
    let step = StepId(step);

    // Only external gates are token-releasable; manual/timer are not.
    let is_external = st
        .db
        .steps_of_run(&run)
        .await?
        .iter()
        .any(|s| s.step == step && s.gate_kind.as_deref() == Some("external"));
    if !is_external {
        return Err(ApiError::NotFound);
    }

    // Verify the token = HMAC(secret, "{run}:{step}"), constant-time.
    let message = format!("{}:{}", run.0, step.0);
    let token = headers.get(GATE_TOKEN_HEADER).and_then(|v| v.to_str().ok());
    scarab_forge_github::verify_signature(secret, message.as_bytes(), token)
        .map_err(|_| ApiError::Unauthorized)?;

    // A fail verdict (only read once the token verifies): reject with the
    // reason. Never held by the Environment — it only stops the deploy.
    let verdict: ExternalGateVerdict = if body.iter().all(u8::is_ascii_whitespace) {
        ExternalGateVerdict::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::BadRequest(format!("bad gate verdict body: {e}")))?
    };
    if !verdict.passed {
        let reason = gate_reject_reason(verdict.reason.as_deref().unwrap_or_default())?;
        gate_reject_outcome(
            scarab_engine::reject_gate(
                st.db.as_ref(),
                st.clock.as_ref(),
                &run,
                &step,
                &format!("external:{}", step.0),
                reason,
            )
            .await,
        )?;
        return Ok(StatusCode::ACCEPTED);
    }

    // Only after the token verifies: an unauthenticated caller learns nothing
    // about the Environment. The rules themselves are not evaluated — the
    // token IS this gate's approval (ADR-0034) — only whether the deploy may
    // still proceed at all.
    let ctx = st.db.run_deploy_context(&run).await?;
    gate_release_governance(&st, ctx.as_ref(), "the gate cannot release").await?;
    // ADR-0076 §6: nor does one inlined from a yanked library version.
    refuse_yanked_step(&st, &run, &step, "the gate cannot release").await?;

    match scarab_engine::release_gate(st.db.as_ref(), st.clock.as_ref(), &run, &step).await {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(RerunError::StepNotFound(_)) => Err(ApiError::NotFound),
        Err(
            e @ (RerunError::DependencyNotSatisfied { .. }
            | RerunError::NotFailed { .. }
            | RerunError::NotAManualGate(_)
            | RerunError::GateNotPending { .. }
            | RerunError::GateNotReached { .. }
            | RerunError::NotARejectableGate(_)
            | RerunError::CreationUnfinished),
        ) => Err(ApiError::Conflict(e.to_string())),
        Err(RerunError::Db(e)) => Err(ApiError::Db(e)),
    }
}

/// 409 when `step` of `run` was inlined from a library version yanked in
/// the run's org (ADR-0076 §6), naming the version and the reason after
/// `refused` (what does not happen: "the gate cannot release").
async fn refuse_yanked_step(
    st: &AppState,
    run: &RunId,
    step: &StepId,
    refused: &str,
) -> Result<(), ApiError> {
    match libraries::stored_step_yank_refusal(st.db.as_ref(), st.libraries.as_deref(), run, &step.0)
        .await
    {
        Ok(None) => Ok(()),
        Ok(Some(refusal)) => Err(ApiError::Conflict(format!("{refused}: {refusal}"))),
        Err(e) => Err(ApiError::Db(DbError::Other(e))),
    }
}

/// HTTP header carrying the fence-scoped step-results ingest token (`sha256=<hex>`).
const RESULTS_TOKEN_HEADER: &str = "x-scarab-results-token";
/// HTTP header carrying the attempt id the token is scoped to.
const RESULTS_ATTEMPT_HEADER: &str = "x-scarab-attempt";
/// Cap on an ingested results body — results are small consumable values
/// (ADR-0041), not blobs.
const RESULTS_MAX_BYTES: usize = 64 * 1024;

/// The message an ADR-0042 results token signs: the `{run}:{step}:{attempt}`
/// fence. The k8s executor mints `HMAC-SHA256(secret, this)`; this endpoint
/// verifies it. Kept as one function so both sides format the fence identically.
pub fn results_token_message(run: &str, step: &str, attempt: &str) -> String {
    format!("{run}:{step}:{attempt}")
}

/// Ingest a step's named results (ADR-0040/0042): the trusted per-Pod egress
/// sidecar POSTs `{ name: value, … }` here, authenticated by a fence-scoped
/// token, and the control plane persists them to `step_runs.results`. The
/// untrusted step never calls this — only the sidecar holds the token.
///
/// 404 when no token secret is configured; 401 on a bad/missing token; 404 for
/// an unknown step; 413 for an over-large body; 409 when the attempt is no
/// longer the step's running frontier. The write is idempotent on the fence
/// (a re-drive overwrites deterministically, ADR-0021).
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/steps/{step}/results",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id")
    ),
    responses(
        (status = 202, description = "results recorded"),
        (status = 401, description = "bad or missing token"),
        (status = 404, description = "no such step, or results ingest disabled"),
        (status = 409, description = "the attempt is not the step's running frontier")
    )
)]
async fn ingest_step_results(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    Json(results): Json<std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<StatusCode, ApiError> {
    // Ingest is opt-in: without a configured secret the endpoint 404s.
    let Some(secret) = st.results_token_secret.as_ref() else {
        return Err(ApiError::NotFound);
    };
    let run = RunId(id);
    let step = StepId(step);

    // Verify the fence-scoped token = HMAC(secret, "{run}:{step}:{attempt}").
    let attempt = headers
        .get(RESULTS_ATTEMPT_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0");
    let message = results_token_message(&run.0, &step.0, attempt);
    let token = headers
        .get(RESULTS_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok());
    scarab_forge_github::verify_signature(secret, message.as_bytes(), token)
        .map_err(|_| ApiError::Unauthorized)?;

    // Bound the payload — results are small values, not blobs.
    let encoded = serde_json::to_vec(&results).unwrap_or_default();
    if encoded.len() > RESULTS_MAX_BYTES {
        return Err(ApiError::BadRequest(format!(
            "results exceed {RESULTS_MAX_BYTES} bytes"
        )));
    }

    // The token authenticates the fence; still require the step to exist so a
    // stray token can't create phantom rows.
    let exists = st
        .db
        .steps_of_run(&run)
        .await?
        .iter()
        .any(|s| s.step == step);
    if !exists {
        return Err(ApiError::NotFound);
    }

    // The fence the token authenticated names the attempt — the ingested
    // results land on that attempt's immutable evidence row as well as the
    // step's latest-evidence denormalization (ADR-0056) — but only while that
    // attempt is the step's frontier and the step still runs (review safety
    // S1). A late POST after the verdict was released (the bounded drain,
    // git-bug f02588d), or from a superseded attempt, would overwrite results
    // dependents already consumed: 409, nothing written.
    match st
        .db
        .ingest_attempt_results(&run, &step, &AttemptId(attempt.to_string()), &results)
        .await
    {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(DbError::Conflict) => Err(ApiError::Conflict(format!(
            "attempt `{attempt}` of step `{}` is no longer running: its results were \
             already settled",
            step.0
        ))),
        Err(e) => Err(e.into()),
    }
}

/// Build a [`SecretScope`] from the flat org/repo/environment selector,
/// enforcing that an environment scope names a repo (ADR-0014, 0024).
/// The RBAC scope of an (org, optional repo) coordinate (ADR-0049): a repo
/// pins the Project; bare org is Org scope. Environment adds no RBAC scope —
/// deploy authorization is the protection rules (ADR-0037).
fn rbac_scope(org: &str, repo: Option<&str>) -> scarab_identity::Scope {
    match repo {
        Some(name) => scarab_identity::Scope::Project {
            org: org.to_string(),
            name: name.to_string(),
        },
        None => scarab_identity::Scope::Org(org.to_string()),
    }
}

fn secret_scope(
    org: String,
    repo: Option<String>,
    environment: Option<String>,
) -> Result<scarab_secrets::SecretScope, ApiError> {
    if org.is_empty() {
        return Err(ApiError::BadRequest("org is required".into()));
    }
    match (repo, environment) {
        (None, None) => Ok(scarab_secrets::SecretScope::Org { org }),
        (Some(repo), None) => Ok(scarab_secrets::SecretScope::Repo { org, repo }),
        (Some(repo), Some(environment)) => Ok(scarab_secrets::SecretScope::EnvironmentProject {
            org,
            project: repo,
            environment,
        }),
        (None, Some(_)) => Err(ApiError::BadRequest(
            "environment-scoped secret requires a repo".into(),
        )),
    }
}

fn secret_err(e: scarab_secrets::SecretError) -> ApiError {
    use scarab_secrets::SecretError;
    match e {
        SecretError::NotFound => ApiError::NotFound,
        SecretError::Denied => ApiError::Forbidden,
        SecretError::Backend(m) => ApiError::Db(DbError::Other(m)),
    }
}

/// Define (or overwrite) a secret at a scope (ADR-0014). The value is stored
/// envelope-encrypted and is **never** returned by any endpoint. Administering
/// secrets requires the Administer capability.
#[utoipa::path(
    post,
    path = "/v1/secrets",
    request_body = PutSecretRequest,
    responses(
        (status = 204, description = "secret stored"),
        (status = 404, description = "secrets not configured")
    )
)]
async fn put_secret(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PutSecretRequest>,
) -> Result<StatusCode, ApiError> {
    let scope = rbac_scope(&req.org, req.repo.as_deref());
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    refuse_reserved_secret_org(&req.org)?;
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;
    if req.name.is_empty() {
        return Err(ApiError::BadRequest("name is required".into()));
    }
    let scope = secret_scope(req.org, req.repo, req.environment)?;
    secrets
        .put(
            &scope,
            scarab_secrets::Secret {
                key: req.name,
                value: req.value.into_bytes(),
            },
        )
        .await
        .map_err(secret_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// List the secret **names** at a scope (ADR-0014) — never the values.
#[utoipa::path(
    get,
    path = "/v1/secrets",
    params(
        ("org" = String, Query, description = "owning org"),
        ("repo" = Option<String>, Query, description = "repo (repo/env scope)"),
        ("environment" = Option<String>, Query, description = "environment (env scope)")
    ),
    responses(
        (status = 200, body = SecretListResponse),
        (status = 404, description = "secrets not configured")
    )
)]
async fn list_secrets(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SecretScopeQuery>,
) -> Result<Json<SecretListResponse>, ApiError> {
    let rscope = rbac_scope(&q.org, q.repo.as_deref());
    authorize_scoped(&st, &headers, Action::Administer, Some(&rscope)).await?;
    refuse_reserved_secret_org(&q.org)?;
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;
    let scope = secret_scope(q.org, q.repo, q.environment)?;
    let names = secrets.list_scoped(&scope).await.map_err(secret_err)?;
    Ok(Json(SecretListResponse { names }))
}

/// Delete a secret at a scope (ADR-0014). Idempotent.
#[utoipa::path(
    delete,
    path = "/v1/secrets",
    params(
        ("org" = String, Query, description = "owning org"),
        ("repo" = Option<String>, Query, description = "repo (repo/env scope)"),
        ("environment" = Option<String>, Query, description = "environment (env scope)"),
        ("name" = String, Query, description = "secret name to delete")
    ),
    responses(
        (status = 204, description = "deleted (idempotent)"),
        (status = 404, description = "secrets not configured")
    )
)]
async fn delete_secret(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SecretScopeQuery>,
) -> Result<StatusCode, ApiError> {
    let rscope = rbac_scope(&q.org, q.repo.as_deref());
    authorize_scoped(&st, &headers, Action::Administer, Some(&rscope)).await?;
    refuse_reserved_secret_org(&q.org)?;
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;
    let name = q
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("name is required".into()))?;
    let scope = secret_scope(q.org, q.repo, q.environment)?;
    secrets.delete(&scope, &name).await.map_err(secret_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Where the retired per-repo environment writes went (ADR-0071 §10). Said in
/// the 410 body, so a script still calling them learns the replacement from
/// the failure itself.
fn per_repo_environment_writes_gone(org: &str, repo: &str, name: &str) -> ApiError {
    ApiError::Gone(format!(
        "per-repo environments were replaced by org-level Environments (ADR-0071): \
         `{name}` is now the org Environment `/v1/orgs/{org}/environments/{name}`, \
         administered by an org admin (who also decides, via \
         `/v1/orgs/{org}/environments/{name}/eligibility`, whether `{repo}` may deploy there). \
         `{repo}`'s own stricter rules are its project overlay, written at \
         `PUT /v1/orgs/{org}/environments/{name}/projects/{repo}/overlay`; its \
         privileges are an org admin's grant at \
         `/v1/orgs/{org}/environments/{name}/projects/{repo}/privilege-grant`. \
         Read all of it at `GET /v1/repos/{org}/{repo}/environments/{name}`"
    ))
}

/// **Retired** (ADR-0071 §10): per-repo environments were migrated to org-level
/// Environments — each repo's old rules became its tighten-only overlay — and
/// this write would edit a table nothing reads any more. Always 410 with a
/// pointer to the replacement.
#[utoipa::path(put, path = "/v1/repos/{org}/{repo}/environments/{name}", params(("org" = String, Path, description = "repo owner"), ("repo" = String, Path, description = "repo name"), ("name" = String, Path, description = "Environment name")), summary = "Retired: per-repo environments are org-level Environments now (ADR-0071)", responses((status = 410, description = "gone; the body says where it went")))]
async fn put_environment(
    Path((org, repo, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    Err(per_repo_environment_writes_gone(&org, &repo, &name))
}

/// One Environment as this repo's Project sees it (ADR-0071): its baseline,
/// the Project's overlay, the effective rules admission enforces, and the
/// Project's privilege grant. 404 for an Environment that does not exist or
/// that the Project has nothing to do with (not eligible, no overlay, no
/// grant). Read capability.
#[utoipa::path(get, path = "/v1/repos/{org}/{repo}/environments/{name}", params(("org" = String, Path, description = "repo owner"), ("repo" = String, Path, description = "repo name"), ("name" = String, Path, description = "Environment name")), summary = "One environment as this project sees it: baseline, overlay, effective (ADR-0071)", responses((status = 200, body = org_environments::ProjectEnvironmentDto), (status = 404, description = "unknown, or not this project's")))]
async fn get_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, name)): Path<(String, String, String)>,
) -> Result<Json<org_environments::ProjectEnvironmentDto>, ApiError> {
    let scope = rbac_scope(&org, Some(&repo));
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let store = st
        .environment_governance
        .as_ref()
        .ok_or(ApiError::NotFound)?;
    let governance = store
        .deploy_governance(&org, &repo, &name)
        .await
        .map_err(org_environments::org_environment_api_error)?
        .filter(|g| g.eligible || g.overlay.is_some() || g.privilege_grant.is_some())
        .ok_or(ApiError::NotFound)?;
    Ok(Json((&governance).into()))
}

/// The Environments this repo's Project may deploy into or holds an overlay
/// or privilege grant in (ADR-0071), by name, each with its baseline,
/// overlay and effective rules computed server-side. Read capability.
#[utoipa::path(get, path = "/v1/repos/{org}/{repo}/environments", params(("org" = String, Path, description = "repo owner"), ("repo" = String, Path, description = "repo name")), summary = "The environments this project may deploy into, with baseline/overlay/effective rules (ADR-0071)", responses((status = 200, body = [org_environments::ProjectEnvironmentDto])))]
async fn list_environments(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
) -> Result<Json<Vec<org_environments::ProjectEnvironmentDto>>, ApiError> {
    let scope = rbac_scope(&org, Some(&repo));
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let store = st
        .environment_governance
        .as_ref()
        .ok_or(ApiError::NotFound)?;
    let governance = store
        .project_environment_governance(&org, &repo)
        .await
        .map_err(org_environments::org_environment_api_error)?;
    Ok(Json(governance.iter().map(Into::into).collect()))
}

/// **Retired** (ADR-0071 §10), like [`put_environment`]: always 410 with a
/// pointer. An org Environment is deleted by an org admin; a Project's
/// overlay by its own endpoint.
#[utoipa::path(delete, path = "/v1/repos/{org}/{repo}/environments/{name}", params(("org" = String, Path, description = "repo owner"), ("repo" = String, Path, description = "repo name"), ("name" = String, Path, description = "Environment name")), summary = "Retired: per-repo environments are org-level Environments now (ADR-0071)", responses((status = 410, description = "gone; the body says where it went")))]
async fn delete_environment(
    Path((org, repo, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    Err(per_repo_environment_writes_gone(&org, &repo, &name))
}

/// Legacy approval history (ADR-0070 §1): who approved which run into this
/// environment, most recent first (ADR-0037). Despite the path, nothing here is
/// a Deployment — a row is written when a manual gate is approved and records no
/// target, version or outcome. Kept readable, read-only;
/// admission happens in the run's gate-approval path. Read cap.
#[utoipa::path(get, path = "/v1/repos/{org}/{repo}/environments/{name}/deployments", params(("org" = String, Path, description = "repo owner"), ("repo" = String, Path, description = "repo name"), ("name" = String, Path, description = "Environment name")), summary = "Legacy approval history (ADR-0070 §1)", responses((status = 200, description = "most recent first")))]
async fn list_deployments(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, name)): Path<(String, String, String)>,
) -> Result<Json<Vec<scarab_project::Deployment>>, ApiError> {
    let scope = rbac_scope(&org, Some(&repo));
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let store = st.environments.as_ref().ok_or(ApiError::NotFound)?;
    let history = store
        .deployments(&org, &repo, &name)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(history))
}

/// The secret coverage matrix for a repo (ADR-0037 D, editable since ADR-0060):
/// each key's effective status in the **repo default** column and in each
/// environment column — `set` (a value at exactly that scope), `inherited`
/// (resolves from a broader scope, with `inherited_from` naming which),
/// `silenced` (unset on purpose), or `unset`. Post-inheritance, so a key defined
/// once at repo scope never reads as missing anywhere.
///
/// Names + status only, never values — the same `Administer` capability as
/// listing secrets. This is the read model behind the Project Secrets editor,
/// which writes through the scoped `/v1/secrets` endpoints.
#[utoipa::path(get, path = "/v1/repos/{org}/{repo}/secrets/matrix", summary = "The secret coverage matrix (ADR-0037) - names + status, never values", responses((status = 200, body = SecretMatrix)))]
async fn secret_matrix(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
) -> Result<Json<SecretMatrix>, ApiError> {
    let scope = rbac_scope(&org, Some(&repo));
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    // The matrix is a read (and annotation) surface over the same scopes
    // as `/v1/secrets`, so it is held to the same rule (redteam auth-4).
    refuse_reserved_secret_org(&org)?;
    let governance = st
        .environment_governance
        .as_ref()
        .ok_or(ApiError::NotFound)?;
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;

    // The environment columns are the Environments this Project may deploy
    // into, plus any it still holds an overlay or grant in (ADR-0071): every
    // EnvironmentProjectSecretScope its Deploy Runs could resolve, and none
    // of another Project's.
    let env_names: Vec<String> = governance
        .project_environment_governance(&org, &repo)
        .await
        .map_err(org_environments::org_environment_api_error)?
        .into_iter()
        .map(|g| g.environment.name)
        .collect();

    // The two broader scopes, kept apart (not merged as "inherited") so a cell
    // can name where it falls through FROM — the difference between overriding
    // the repo default and overriding an org-wide value.
    let keys_at = |scope: scarab_secrets::SecretScope| async move {
        secrets
            .list_scoped(&scope)
            .await
            .map(|v| v.into_iter().collect::<std::collections::BTreeSet<_>>())
            .map_err(secret_err)
    };
    let org_keys = keys_at(scarab_secrets::SecretScope::Org { org: org.clone() }).await?;
    let repo_keys = keys_at(scarab_secrets::SecretScope::Repo {
        org: org.clone(),
        repo: repo.clone(),
    })
    .await?;

    // Keys defined directly at each environment's scope.
    let mut env_keys: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut all_keys: std::collections::BTreeSet<String> =
        org_keys.union(&repo_keys).cloned().collect();
    for name in &env_names {
        let keys = keys_at(scarab_secrets::SecretScope::EnvironmentProject {
            org: org.clone(),
            project: repo.clone(),
            environment: name.clone(),
        })
        .await?;
        all_keys.extend(keys.iter().cloned());
        env_keys.insert(name.clone(), keys);
    }

    // Advisory annotations. A deployment with no coverage store still gets a
    // matrix — it just has nothing marked (the markers are not correctness).
    let mut silenced: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    if let Some(store) = st.secret_coverage.as_ref() {
        for (column, key) in store
            .silenced(&org, &repo)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?
        {
            silenced.insert((column.unwrap_or_default(), key));
        }
    }
    // A marker on a key that no longer exists anywhere would otherwise be
    // invisible; surface those rows so the annotation can be cleaned up.
    all_keys.extend(silenced.iter().map(|(_, key)| key.clone()));

    let mut columns = vec![REPO_DEFAULT_COLUMN.to_string()];
    columns.extend(env_names.iter().cloned());

    let keys = all_keys
        .into_iter()
        .map(|key| {
            let mut status = std::collections::BTreeMap::new();
            let mut inherited_from = std::collections::BTreeMap::new();
            for column in &columns {
                // Where a value sits at *exactly* this column's scope.
                let set_here = if column == REPO_DEFAULT_COLUMN {
                    repo_keys.contains(&key)
                } else {
                    env_keys.get(column).is_some_and(|k| k.contains(&key))
                };
                // What it falls through to, nearest broader scope first.
                let from = if set_here {
                    None
                } else if column != REPO_DEFAULT_COLUMN && repo_keys.contains(&key) {
                    Some("repo")
                } else if org_keys.contains(&key) {
                    Some("org")
                } else {
                    None
                };
                let s = match (set_here, from) {
                    (true, _) => "set",
                    (false, Some(origin)) => {
                        inherited_from.insert(column.clone(), origin.to_string());
                        "inherited"
                    }
                    // Only a genuinely empty cell can be silenced.
                    (false, None) if silenced.contains(&(column.clone(), key.clone())) => {
                        "silenced"
                    }
                    (false, None) => "unset",
                };
                status.insert(column.clone(), s.to_string());
            }
            SecretMatrixRow {
                key,
                status,
                inherited_from,
            }
        })
        .collect();

    Ok(Json(SecretMatrix {
        columns,
        environments: env_names,
        keys,
    }))
}

/// Mark a coverage cell as **intentionally unset** (ADR-0037 D). Purely
/// advisory: it silences one cell of the matrix and has no effect on secret
/// resolution, admission, or any run. Idempotent. `Administer`, like the rest of
/// the secret surface.
#[utoipa::path(
    put,
    path = "/v1/repos/{org}/{repo}/secrets/matrix/silenced",
    summary = "Mark a coverage cell intentionally unset (ADR-0037, advisory)",
    request_body = SilenceCellRequest,
    responses(
        (status = 204, description = "marked (idempotent)"),
        (status = 404, description = "coverage annotations not configured")
    )
)]
async fn silence_secret_cell(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
    Json(req): Json<SilenceCellRequest>,
) -> Result<StatusCode, ApiError> {
    let scope = rbac_scope(&org, Some(&repo));
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    // The matrix is a read (and annotation) surface over the same scopes
    // as `/v1/secrets`, so it is held to the same rule (redteam auth-4).
    refuse_reserved_secret_org(&org)?;
    let store = st.secret_coverage.as_ref().ok_or(ApiError::NotFound)?;
    if req.key.is_empty() {
        return Err(ApiError::BadRequest("key is required".into()));
    }
    store
        .silence(&org, &repo, coverage_column(&req.environment), &req.key)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Drop an "intentionally unset" marker (ADR-0037 D). Idempotent.
#[utoipa::path(
    delete,
    path = "/v1/repos/{org}/{repo}/secrets/matrix/silenced",
    summary = "Drop an intentionally-unset marker (ADR-0037, advisory)",
    params(
        ("key" = String, Query, description = "secret key"),
        ("environment" = Option<String>, Query, description = "environment column; omitted = the repo default")
    ),
    responses(
        (status = 204, description = "cleared (idempotent)"),
        (status = 404, description = "coverage annotations not configured")
    )
)]
async fn unsilence_secret_cell(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
    Query(q): Query<SilenceCellRequest>,
) -> Result<StatusCode, ApiError> {
    let scope = rbac_scope(&org, Some(&repo));
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    // The matrix is a read (and annotation) surface over the same scopes
    // as `/v1/secrets`, so it is held to the same rule (redteam auth-4).
    refuse_reserved_secret_org(&org)?;
    let store = st.secret_coverage.as_ref().ok_or(ApiError::NotFound)?;
    if q.key.is_empty() {
        return Err(ApiError::BadRequest("key is required".into()));
    }
    store
        .unsilence(&org, &repo, coverage_column(&q.environment), &q.key)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// An optional environment name as a coverage column: `None` (or an empty
/// string, which no environment can be named) is the repo-default column.
fn coverage_column(environment: &Option<String>) -> scarab_project::CoverageColumn<'_> {
    environment.as_deref().filter(|e| !e.is_empty())
}

/// One registered forge connection, as global Settings renders it (ADR-0060).
///
/// Carries the credential's **handle and whether it resolves** — never the
/// material. A connection is the unit an operator reasons about ("is my GitHub
/// App still wired up?"), so the DTO answers that without becoming a way to read
/// a secret back.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectionDto {
    pub id: String,
    /// `github` | `forgejo`.
    pub kind: String,
    /// The API base URL the adapter talks to (GHES / a self-hosted Forgejo).
    pub base_url: String,
    /// The forge's own web host, for deep links out of the UI.
    pub web_url: String,
    /// The `_forge`-scoped handle the credential lives under. An opaque name,
    /// not the secret.
    pub credential_ref: String,
    /// Does `credential_ref` actually resolve to material right now? The single
    /// most common breakage (a DB restored without its secrets, a reseed that
    /// never happened) is invisible until a run fails — this surfaces it.
    pub credential_present: bool,
    /// Unix-ms of the most recent accepted webhook delivery from this **forge
    /// kind**, if any. Deliveries are recorded per kind (the ADR-0046 replay
    /// guard is keyed that way), so with two connections of one kind this is a
    /// per-kind liveness signal, not a per-connection one.
    pub last_delivery_at: Option<i64>,
    /// The Projects this connection serves, from its repo bindings — a Project
    /// *is* a binding (ADR-0046), so this is the connection's whole footprint.
    pub projects: Vec<ConnectionProjectDto>,
    /// Can the forge enumerate what this credential reaches? Gates the re-sync
    /// affordance: GitHub can, so a drifted registry is healable; an adapter that
    /// cannot should not offer a button that always errors.
    pub supports_resync: bool,
    /// Is this connection managed declaratively (config-owned) and therefore
    /// read-only here (ADR-0060 part D)? A connection has exactly one owner —
    /// the `connections:` config or the database — and this says which, so the
    /// UI never offers an edit the next boot would silently revert.
    pub managed_by_config: bool,
    /// Can this connection's app configuration be checked against what Scarab
    /// needs (`GET …/preflight`)? Decided from the kind, like `supports_resync`
    /// and for the same reason: the check is a live forge round-trip, so the
    /// list must not perform one per row just to discover the button is
    /// pointless. The endpoint itself remains the authority.
    pub supports_preflight: bool,
}

/// A Project a connection serves, plus the forge coordinate it came from.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectionProjectDto {
    pub org: String,
    pub project: String,
    pub owner: String,
    pub name: String,
}

/// `POST /v1/connections` body (ADR-0060 part D, manual path): the forge to
/// connect and the credential to reach it with.
///
/// `credential` is **write-only** — it is written through to `SecretProvider`
/// under a server-generated handle and never appears in any response. There is
/// deliberately no "update the credential" field on the read DTO: a secret you
/// can read back is a secret you have leaked.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateConnectionRequest {
    /// `forgejo`. GitHub is not creatable here — installing the App *is* its
    /// registration (ADR-0060 part C), so a create form for it could not work.
    pub kind: String,
    /// The instance root Scarab talks to (e.g. `https://codeberg.org`). A
    /// trailing slash is normalized away.
    pub base_url: String,
    /// The forge access token. Write-only (see the struct docs).
    pub credential: String,
}

/// `POST /v1/connections` response: the created connection's id and the
/// generated handle its credential now lives under. Not the credential.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedConnectionDto {
    pub id: String,
    /// The server-generated `_forge`-scoped handle. Echoed so an operator can
    /// correlate the row with its secret; it is a name, not a value.
    pub credential_ref: String,
}

/// `POST /v1/connections/{id}/resync` body: what reconciliation changed.
#[derive(Debug, Serialize, ToSchema)]
pub struct ResyncResultDto {
    /// Repos the forge reports that Scarab did not have bound — now bound.
    pub bound: Vec<String>,
    /// How many bindings the forge confirms. Reported rather than acted on:
    /// see the handler's note on why re-sync never unbinds.
    pub confirmed: usize,
}

/// The reserved secret-scope org under which forge-connection credentials live
/// (ADR-0046). A `ForgeConnection` row carries only `credential_ref` — the key
/// within this scope; the material (GitHub App PEM / Forgejo token) is fetched
/// here at use-time and never persisted on the connection.
pub const FORGE_CREDENTIALS_ORG: &str = "_forge";

/// Org names Scarab keeps for itself: every `_`-prefixed name. Its own
/// credential scopes live there — [`FORGE_CREDENTIALS_ORG`] and
/// [`plugins::PLUGIN_CONNECTION_CREDENTIALS_ORG`] — so no Project is homed in
/// one (it would inherit those secrets; the registry's `bind_repo` refuses it
/// on every path), no plugin is installed into one, and `/v1/secrets`
/// reaches only `_forge` among them (git-bug 97223c7).
pub use scarab_forge::is_reserved_org;

/// `400` when `org` is reserved ([`is_reserved_org`]); `use_` names what it
/// was refused for.
pub(crate) fn refuse_reserved_org(org: &str, use_: &str) -> Result<(), ApiError> {
    if is_reserved_org(org) {
        return Err(ApiError::BadRequest(format!(
            "`{org}` is a reserved org name (`_`-prefixed names are Scarab's own): \
             it cannot {use_}"
        )));
    }
    Ok(())
}

/// The orgs `/v1/secrets` may address: every real one and, of the reserved
/// ones, only [`FORGE_CREDENTIALS_ORG`] — the forge App key is documented to
/// be written there through this endpoint. Plugin connection credentials
/// have their own API.
fn refuse_reserved_secret_org(org: &str) -> Result<(), ApiError> {
    if org == FORGE_CREDENTIALS_ORG {
        return Ok(());
    }
    refuse_reserved_org(org, "hold secrets written through /v1/secrets")
}

/// The gate every `/v1/connections*` endpoint shares: `Administer` on the Org.
///
/// A connection spans every Project it serves, so administering it is an
/// org-level act, not a per-repo one — and there is no org in the path (one
/// implicit Org, ADR-0060), so the check is "may this caller administer *an*
/// org?". A globally-`Administer` role passes outright; anyone else needs an
/// `Admin`+ binding on some `Scope::Org`. On a virgin install no org exists yet
/// (an org is the coordinate of a bound Project), so bootstrapping the first
/// connection necessarily needs the global role — the same asymmetry `/v1/me`
/// already reports via `can_administer` + an empty `admin_orgs`.
async fn authorize_org_administer(st: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let caller = authenticate(st, headers, Action::Administer).await?;
    if !caller.may_globally(Action::Administer) && administrable_orgs(st, &caller).await?.is_empty()
    {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// The forge adapter for **one connection** (ADR-0060) — the port the onboarding
/// endpoints act through.
///
/// Not `st.forge`: that port routes each call by resolving its repo through the
/// registry, and every question here is asked of a connection that may have
/// nothing bound yet ("what can this credential reach?", "put a hook on this repo
/// I am about to bind"). Falls back to `st.forge` when no factory is wired, which
/// is exactly what a test with one fake adapter means.
async fn connection_adapter(
    st: &AppState,
    conn: &scarab_forge::ForgeConnection,
) -> Result<Arc<dyn scarab_forge::ForgePort>, ApiError> {
    match st.forge_adapters.as_ref() {
        Some(factory) => factory
            .adapter_for_connection(conn)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string())),
        None => st.forge.clone().ok_or(ApiError::NotFound),
    }
}

/// Where a forge's deliveries land — the callback URL a registered hook posts to
/// (ADR-0046: separate endpoints per forge, each bound to its adapter and
/// verification secret, so there is no payload-sniffing on a shared path).
fn forge_webhook_url(public_url: &str, kind: scarab_forge::ForgeKind) -> String {
    format!(
        "{}/webhooks/{}",
        public_url.trim_end_matches('/'),
        kind.as_str()
    )
}

/// Can only a forge running on *this very host* deliver to `url`?
///
/// The dev default for `SCARAB_PUBLIC_URL` is loopback, and a forge will happily
/// register a hook pointing at it — so the deployment looks healthy and the
/// deliveries go to the forge's own localhost forever. Worth a warning at the
/// moment the hook is created, which is the last point anyone is watching.
fn is_loopback_callback(url: &str) -> bool {
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    // Strip userinfo, then the port — but not the colons inside a bracketed v6
    // literal (`[::1]:8080`).
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("ip6-localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The registered forge connections and their health (ADR-0060 part C) — what
/// the global Settings **Connections** section renders.
///
/// Read-only, and `Administer` on the Org: a connection spans every Project it
/// serves, so seeing the fleet is an org-level act, not a per-repo one. No
/// credential material is ever returned — only whether the handle resolves.
#[utoipa::path(
    get,
    path = "/v1/connections",
    summary = "Registered forge connections + their bound Projects and health (ADR-0060)",
    responses(
        (status = 200, body = [ConnectionDto]),
        (status = 403, description = "requires Administer on the org"),
        (status = 404, description = "no connection registry wired")
    )
)]
async fn list_connections(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ConnectionDto>>, ApiError> {
    authorize_org_administer(&st, &headers).await?;
    let connections = st.connections.as_ref().ok_or(ApiError::NotFound)?;

    let conns = connections
        .list_connections()
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    // Ownership (ADR-0060 part D), read once for the whole page: the durable
    // marker boot provisioning writes, not a guess.
    let config_owned: std::collections::BTreeSet<String> = connections
        .config_owned_connection_ids()
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .into_iter()
        .collect();
    let mut out = Vec::with_capacity(conns.len());
    for conn in conns {
        let mut projects = Vec::new();
        for repo in connections
            .repos_of(&conn.id)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?
        {
            if let Some(resolved) = connections
                .resolve(&repo)
                .await
                .map_err(|e| ApiError::BadRequest(e.to_string()))?
            {
                projects.push(ConnectionProjectDto {
                    org: resolved.org,
                    project: resolved.project,
                    owner: repo.owner,
                    name: repo.name,
                });
            }
        }
        projects.sort_by(|a, b| (&a.org, &a.project).cmp(&(&b.org, &b.project)));
        let credential_present = credential_present(&st, &conn).await;
        let last_delivery_at = connections
            .last_delivery_at(conn.kind)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        // Which adapters implement `list_accessible_repos`. Decided from the kind
        // rather than probed: probing means a live forge round-trip per connection
        // on every render of the Settings page. This only decides whether a button
        // is offered — `POST …/resync` is the authority and answers 501 if an
        // adapter really cannot. Both shipped adapters can enumerate as of
        // ADR-0060 slice 5 (`/user/repos` on Forgejo), which is also what lets the
        // bind pick-list exist.
        // Listed per kind rather than as "anything wired", so a future adapter
        // that cannot enumerate has to be added here deliberately instead of
        // inheriting a button that always errors.
        //
        // A config-owned connection offers it to nobody regardless (ADR-0060
        // part D): re-sync WRITES bindings, and config declares the repos it
        // owns, so an extra binding from here would be a change with no home in
        // the source that is authoritative for it — drift by another name.
        let managed_by_config = config_owned.contains(&conn.id);
        let supports_resync = (st.forge.is_some() || st.forge_adapters.is_some())
            && matches!(
                conn.kind,
                scarab_forge::ForgeKind::GitHub | scarab_forge::ForgeKind::Forgejo
            )
            && !managed_by_config;
        // Only GitHub can be asked what it granted (`GET /app`), and only in App
        // mode — which is also the only forge with the failure this checks for:
        // an App whose event subscription or `statuses` grant is empty looks
        // perfectly healthy from every other angle.
        let supports_preflight = (st.forge.is_some() || st.forge_adapters.is_some())
            && conn.kind == scarab_forge::ForgeKind::GitHub;
        out.push(ConnectionDto {
            web_url: forge_web_host(conn.kind, &conn.base_url),
            id: conn.id,
            kind: conn.kind.as_str().to_string(),
            base_url: conn.base_url,
            credential_ref: conn.credential_ref,
            credential_present,
            last_delivery_at,
            projects,
            supports_resync,
            supports_preflight,
            managed_by_config,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(out))
}

/// Does this connection's credential handle resolve to material right now?
///
/// Presence, not the value. A missing provider means we cannot tell, which reads
/// the same as absent here — either way the adapter cannot authenticate.
/// Resolution goes through the ONE path (ADR-0060 part D): a credential the
/// deployment supplies (`credential.env`/`file`, or the App PEM) is present even
/// though `SecretProvider` has never heard of it, and reporting it as MISSING
/// would be a lie that sends operators hunting a healthy connection.
async fn credential_present(st: &AppState, conn: &scarab_forge::ForgeConnection) -> bool {
    if st.credential_overrides.covers(conn) {
        return true;
    }
    match st.secrets.as_ref() {
        Some(secrets) => connection_credential(secrets.as_ref(), conn).await.is_ok(),
        None => false,
    }
}

/// One capability Scarab needs of a forge app, as the preflight reports it
/// (`scarab_forge::preflight::ForgeRequirement` on the wire).
#[derive(Debug, Serialize, ToSchema)]
pub struct CapabilityRequirementDto {
    /// `permission` | `event`.
    pub kind: String,
    /// The forge's own name for it (`statuses`, `push`) — the label on the
    /// setting an operator has to go and change.
    pub name: String,
    /// Minimum level for a permission (`read`/`write`/`admin`); absent for an
    /// event, which is subscribed or not.
    pub level: Option<String>,
    /// `required` | `recommended`.
    pub severity: String,
    /// What silently breaks without it.
    pub why: String,
}

impl From<&scarab_forge::preflight::ForgeRequirement> for CapabilityRequirementDto {
    fn from(r: &scarab_forge::preflight::ForgeRequirement) -> Self {
        Self {
            kind: r.kind.as_str().to_string(),
            name: r.name.to_string(),
            level: r.level.map(str::to_string),
            severity: r.severity.as_str().to_string(),
            why: r.why.to_string(),
        }
    }
}

/// A permission the forge reports as granted.
#[derive(Debug, Serialize, ToSchema)]
pub struct GrantedPermissionDto {
    pub name: String,
    pub level: String,
}

/// The result of checking a connection's app configuration against what Scarab
/// needs (ADR-0060 preflight) — the deeper cut of the same question
/// `credential_present` answers on the list.
///
/// Carries **no credential material**: granted permissions are names and levels,
/// events are names. Nothing here can authenticate to anything.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectionPreflightDto {
    pub id: String,
    pub kind: String,
    /// `ok` — every required capability is granted; `degraded` — at least one is
    /// missing (runs will silently not trigger, or checks will silently not
    /// post); `unknown` — the forge could not be asked. Three values, not two:
    /// "I could not look" must never render as "you are fine".
    pub status: String,
    /// Did the forge actually answer? `false` for every `unknown`.
    pub checked: bool,
    /// Why the check could not run, when it could not — an adapter that cannot
    /// introspect, a credential that does not resolve, a forge that errored.
    pub unavailable_reason: Option<String>,
    /// Everything Scarab needs from this forge, so the answer is legible even
    /// when nothing could be checked.
    pub required: Vec<CapabilityRequirementDto>,
    /// The subset of `required` the forge does not currently grant. Empty on
    /// `ok`, and (necessarily) empty on `unknown` — read `status`, not this.
    pub missing: Vec<CapabilityRequirementDto>,
    /// What the forge says the app *is* granted, including grants Scarab does
    /// not need — an operator comparing against the App settings page wants the
    /// whole picture, and an over-broad grant is worth seeing.
    pub granted_permissions: Vec<GrantedPermissionDto>,
    /// The webhook events the forge will deliver, as it reports them.
    pub subscribed_events: Vec<String>,
}

/// The "could not check" answer, in one place so every reason produces the same
/// shape: no gaps claimed, the requirement list still rendered, and a sentence
/// saying why. Kept honest by construction — a caller cannot accidentally report
/// `ok` for a connection nobody managed to ask.
fn preflight_unknown(
    conn: &scarab_forge::ForgeConnection,
    required: Vec<CapabilityRequirementDto>,
    reason: String,
) -> ConnectionPreflightDto {
    ConnectionPreflightDto {
        id: conn.id.clone(),
        kind: conn.kind.as_str().to_string(),
        status: "unknown".into(),
        checked: false,
        unavailable_reason: Some(reason),
        required,
        missing: Vec::new(),
        granted_permissions: Vec::new(),
        subscribed_events: Vec::new(),
    }
}

/// **Preflight a connection** (git-bug 90644c6): diff the forge app's *actual*
/// granted permissions and subscribed events against what Scarab needs, and
/// report the gap.
///
/// This exists because both halves of a misconfigured GitHub App fail
/// *silently*:
///
///  - **No events subscribed.** GitHub delivers `installation` /
///    `installation_repositories` regardless, so the connection registers
///    itself and `GET /v1/repos` looks healthy — while no push ever starts a
///    run. The operator's only signal is that nothing happens.
///  - **No `statuses:write`.** Every status post 403s while the run itself goes
///    green, so the forge simply never shows a check.
///
/// Until now the only defence was documentation. `GET /v1/connections` already
/// answers "does the credential resolve"; this is the same question one level
/// deeper — *and is it allowed to do the things Scarab will ask of it*.
///
/// A live forge round-trip, so it is its **own endpoint** rather than fields on
/// the list: rendering Settings must not fan out a call per connection
/// unasked-for (the same reasoning that made `supports_resync` kind-derived
/// rather than probed).
///
/// It answers **200 with `status: "unknown"`**, not 501, when the adapter cannot
/// introspect. "Unknown" is a real health state the UI must render next to the
/// credential line, and the requirement list is still worth showing; a 501 would
/// force every caller to invent that state itself.
///
/// Never returns credential material — only names, levels and event ids.
#[utoipa::path(
    get,
    path = "/v1/connections/{id}/preflight",
    summary = "Diff a connection's granted permissions and subscribed events against what Scarab needs (ADR-0060)",
    params(("id" = String, Path, description = "connection id")),
    responses(
        (status = 200, body = ConnectionPreflightDto),
        (status = 403, description = "requires Administer on the org"),
        (status = 404, description = "no such connection, or no registry wired")
    )
)]
async fn connection_preflight(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ConnectionPreflightDto>, ApiError> {
    authorize_org_administer(&st, &headers).await?;
    let connections = st.connections.as_ref().ok_or(ApiError::NotFound)?;
    let conn = connections
        .get_connection(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or(ApiError::NotFound)?;

    let required: Vec<CapabilityRequirementDto> = scarab_forge::preflight::required(conn.kind)
        .iter()
        .map(CapabilityRequirementDto::from)
        .collect();

    // No credential, no question to ask. Short-circuit BEFORE building an
    // adapter: the answer ("this connection cannot authenticate") is already
    // known, and the list's credential line is the place that says so.
    if !credential_present(&st, &conn).await {
        return Ok(Json(preflight_unknown(
            &conn,
            required,
            format!(
                "the credential `{}` does not resolve, so the forge cannot be asked",
                conn.credential_ref
            ),
        )));
    }

    // THIS connection's adapter, not the repo-routed port — the app's grants are
    // a connection-scoped fact, and a connection with nothing bound yet has no
    // repo to route through.
    let forge = match connection_adapter(&st, &conn).await {
        Ok(forge) => forge,
        Err(_) => {
            return Ok(Json(preflight_unknown(
                &conn,
                required,
                "no forge adapter is wired for this connection".into(),
            )))
        }
    };

    let granted = match forge.describe_capabilities().await {
        Ok(caps) => caps,
        // The adapter cannot look at all (Forgejo, or GitHub on a fixed token).
        Err(scarab_forge::ForgeError::Unsupported(what)) => {
            return Ok(Json(preflight_unknown(
                &conn,
                required,
                format!(
                    "{} cannot report its granted permissions or subscribed events ({what})",
                    conn.kind.as_str()
                ),
            )))
        }
        // It could have worked and did not — a forge outage, a revoked key. Also
        // unknown, but for a reason worth repeating verbatim.
        Err(e) => return Ok(Json(preflight_unknown(&conn, required, e.to_string()))),
    };

    let missing: Vec<CapabilityRequirementDto> =
        scarab_forge::preflight::missing(conn.kind, &granted)
            .into_iter()
            .map(CapabilityRequirementDto::from)
            .collect();
    let degraded = scarab_forge::preflight::is_degraded(conn.kind, &granted);
    Ok(Json(ConnectionPreflightDto {
        id: conn.id.clone(),
        kind: conn.kind.as_str().to_string(),
        status: if degraded { "degraded" } else { "ok" }.into(),
        checked: true,
        unavailable_reason: None,
        required,
        missing,
        granted_permissions: granted
            .permissions
            .iter()
            .map(|(name, level)| GrantedPermissionDto {
                name: name.clone(),
                level: level.clone(),
            })
            .collect(),
        subscribed_events: granted.events.iter().cloned().collect(),
    }))
}

/// Re-sync a connection against the forge (ADR-0060 part C): ask the forge which
/// repos this credential reaches and bind any Scarab does not have yet.
///
/// This is the **healing** path for GitHub, where installing the App *is*
/// registration and the registry is therefore only as current as the last
/// `installation_repositories` delivery. A dropped delivery leaves a repo the App
/// covers with no Project; re-sync notices.
///
/// It deliberately **only binds**. Unbinding on a forge's say-so would let one
/// failed API page delete governance — Environments, secrets and RBAC hang off a
/// Project — so removal stays an explicit human act. `confirmed` reports the
/// overlap so a stale binding is still visible.
#[utoipa::path(
    post,
    path = "/v1/connections/{id}/resync",
    summary = "Re-bind repos the forge reports for this connection (ADR-0060)",
    params(("id" = String, Path, description = "connection id")),
    responses(
        (status = 200, body = ResyncResultDto),
        (status = 404, description = "no such connection, or no registry/forge wired"),
        (status = 409, description = "the connection is managed by configuration (read-only)"),
        (status = 501, description = "this forge adapter cannot enumerate repos")
    )
)]
async fn resync_connection(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ResyncResultDto>, ApiError> {
    authorize_org_administer(&st, &headers).await?;
    let connections = st.connections.as_ref().ok_or(ApiError::NotFound)?;
    let conn = connections
        .get_connection(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    // Config-owned connections are read-only here (ADR-0060 part D): their repo
    // bindings are declared in the `connections:` block, so a binding written by
    // re-sync would live outside the source that is authoritative for them —
    // drift by another name. Refuse BEFORE talking to the forge, and say where
    // the change belongs.
    if connections_config::is_config_owned(connections.as_ref(), &id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
    {
        return Err(ApiError::Conflict(format!(
            "connection `{id}` is managed by configuration (ADR-0060 part D) — its repos are \
             declared in the `connections:` block. Add the repo there and redeploy."
        )));
    }
    // THIS connection's adapter, not the repo-routed port: enumeration is a
    // connection-scoped question, and a connection whose registry drifted to
    // empty has no repo left to route through.
    let forge = connection_adapter(&st, &conn).await?;

    let reported = match forge.list_accessible_repos().await {
        Ok(repos) => repos,
        Err(scarab_forge::ForgeError::Unsupported(what)) => {
            return Err(ApiError::NotImplemented(format!(
                "{} cannot enumerate repos ({what})",
                conn.kind.as_str()
            )))
        }
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };
    let known: std::collections::BTreeSet<scarab_forge::RepoRef> = connections
        .repos_of(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .into_iter()
        .collect();

    let mut bound = Vec::new();
    let mut confirmed = 0usize;
    for repo in reported {
        if known.contains(&repo) {
            confirmed += 1;
            continue;
        }
        // A forge account named like one of Scarab's own Orgs is never homed
        // there (the registry refuses it too); skipping it keeps one such
        // repo from failing the whole re-sync.
        if is_reserved_org(&repo.owner) {
            tracing::warn!(
                connection = %id,
                repo = %format!("{}/{}", repo.owner, repo.name),
                "re-sync skipped a repo whose owner is a reserved org name"
            );
            continue;
        }
        // Project name = repo name, org = repo owner — the same 1:1 mapping
        // `apply_installation_sync` uses, so a re-synced Project is
        // indistinguishable from a webhook-registered one.
        connections
            .bind_repo(&id, &repo, &repo.owner, &repo.name)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        // A new org gets the first-party libraries (ADR-0076 §5).
        libraries::offer_defaults_on_bind(&st, &repo.owner).await;
        bound.push(format!("{}/{}", repo.owner, repo.name));
    }
    Ok(Json(ResyncResultDto { bound, confirmed }))
}

/// Mint the ids for a manually-created connection: `(connection_id,
/// credential_ref)`.
///
/// Both are **server-generated** and correlated by construction, so a
/// connection can never be pointed at another connection's credential by an
/// operator typo — the handle is not an input. The random suffix keeps two
/// connections to the same host distinct (and their credentials separate),
/// which a host-derived name could not.
fn mint_connection_ids(kind: scarab_forge::ForgeKind) -> (String, String) {
    let suffix = Uuid::new_v4().simple().to_string();
    let id = format!("{}-{}", kind.as_str(), &suffix[..8]);
    let credential_ref = format!("{id}-credential");
    (id, credential_ref)
}

/// Create a forge connection with a **credential write-through** (ADR-0060 part
/// D): the token in the request body is stored in `SecretProvider` under a
/// server-generated handle in the reserved [`FORGE_CREDENTIALS_ORG`] scope, and
/// the connection row records only that handle.
///
/// This is the manual/UI half of part D and the reason Forgejo can be onboarded
/// at all: GitHub registers itself when the App is installed, but a Forgejo
/// instance has no such event, so without this endpoint its only route into the
/// registry was a hand-written database row.
///
/// The credential is **write-only** in the strong sense — it is written before
/// the connection row exists, never read back by any endpoint, and the response
/// carries only the generated handle. Order matters: writing the secret first
/// means a failure leaves an orphan secret (harmless, overwritten on retry)
/// rather than a connection whose credential never landed (a live row that
/// silently cannot authenticate).
#[utoipa::path(
    post,
    path = "/v1/connections",
    summary = "Create a forge connection, writing its credential through to the secret store (ADR-0060)",
    request_body = CreateConnectionRequest,
    responses(
        (status = 201, body = CreatedConnectionDto),
        (status = 400, description = "unknown kind, non-creatable kind, or a malformed base URL"),
        (status = 403, description = "requires Administer on the org"),
        (status = 404, description = "no connection registry or secret store wired"),
        (status = 409, description = "a connection to that forge and base URL already exists")
    )
)]
async fn create_connection(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateConnectionRequest>,
) -> Result<(StatusCode, Json<CreatedConnectionDto>), ApiError> {
    authorize_org_administer(&st, &headers).await?;
    let connections = st.connections.as_ref().ok_or(ApiError::NotFound)?;
    // No secret store means no write-through target. Refusing is the only honest
    // answer: a connection row with an unresolvable credential is a row that
    // cannot serve a single call.
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;

    let kind = scarab_forge::ForgeKind::from_str_token(req.kind.trim())
        .ok_or_else(|| ApiError::BadRequest(format!("unknown forge kind `{}`", req.kind)))?;
    // GitHub's registration IS the App installation (ADR-0060 part C) — Scarab
    // cannot install an App, so a row created here would be a connection to an
    // installation that may not exist. Say so instead of accepting a lie.
    if kind == scarab_forge::ForgeKind::GitHub {
        return Err(ApiError::BadRequest(
            "GitHub connections register themselves when the Scarab App is installed — \
             install it on the account instead of creating a connection here"
                .into(),
        ));
    }
    let base_url = req.base_url.trim().trim_end_matches('/').to_string();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(ApiError::BadRequest(
            "base_url must be an http(s) URL, e.g. https://codeberg.org".into(),
        ));
    }
    if req.credential.trim().is_empty() {
        return Err(ApiError::BadRequest("credential is required".into()));
    }

    // One connection per (kind, base URL). Two rows for one host would each
    // carry their own credential and each claim to serve it — an ambiguity with
    // no upside, and the shape an accidental double-submit produces.
    let existing = connections
        .list_connections()
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if let Some(dupe) = existing
        .iter()
        .find(|c| c.kind == kind && c.base_url == base_url)
    {
        return Err(ApiError::Conflict(format!(
            "connection `{}` already serves {base_url}",
            dupe.id
        )));
    }

    let (id, credential_ref) = mint_connection_ids(kind);
    secrets
        .put(
            &scarab_secrets::SecretScope::Org {
                org: FORGE_CREDENTIALS_ORG.to_string(),
            },
            scarab_secrets::Secret {
                key: credential_ref.clone(),
                value: req.credential.trim().as_bytes().to_vec(),
            },
        )
        .await
        .map_err(secret_err)?;
    connections
        .put_connection(&scarab_forge::ForgeConnection {
            id: id.clone(),
            kind,
            base_url,
            credential_ref: credential_ref.clone(),
        })
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedConnectionDto { id, credential_ref }),
    ))
}

/// Query for [`delete_connection`]: the acknowledgement that removing a
/// connection removes the Projects it serves.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeleteConnectionQuery {
    /// Confirm that the connection's repo bindings — i.e. its **Projects**, and
    /// the Environments, secrets and RBAC hanging off them — go with it.
    /// Without it, a connection that still has bindings is refused.
    #[serde(default)]
    pub unbind_repos: bool,
}

/// Delete a forge connection (ADR-0060 part D) and, when it is no longer
/// referenced, its write-through credential.
///
/// Two deliberate safeties:
///
///  1. **Bound repos block the delete** unless `unbind_repos=true`. A Project
///     *is* a repo binding (ADR-0046), so deleting a connection deletes
///     governance — the same reasoning that stops `resync` from ever unbinding.
///     A one-word query parameter is cheap; a silently deleted Environment is
///     not recoverable from the UI.
///  2. **A shared credential survives.** Every GitHub App installation points at
///     the one `github-app` handle, so deleting one installation must not pull
///     the material out from under the others. The secret is removed only when
///     no remaining connection references that handle.
#[utoipa::path(
    delete,
    path = "/v1/connections/{id}",
    summary = "Delete a forge connection and its unreferenced credential (ADR-0060)",
    params(
        ("id" = String, Path, description = "connection id"),
        ("unbind_repos" = Option<bool>, Query, description = "acknowledge that the connection's Projects go with it")
    ),
    responses(
        (status = 204, description = "connection deleted"),
        (status = 403, description = "requires Administer on the org"),
        (status = 404, description = "no such connection, or no registry wired"),
        (status = 409, description = "the connection still has bound repos (Projects)")
    )
)]
async fn delete_connection(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<DeleteConnectionQuery>,
) -> Result<StatusCode, ApiError> {
    authorize_org_administer(&st, &headers).await?;
    let connections = st.connections.as_ref().ok_or(ApiError::NotFound)?;
    deny_if_config_owned(connections, &id, "the connection itself").await?;
    let conn = connections
        .get_connection(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or(ApiError::NotFound)?;

    let bound = connections
        .repos_of(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if !bound.is_empty() && !q.unbind_repos {
        let names: Vec<String> = bound
            .iter()
            .map(|r| format!("{}/{}", r.owner, r.name))
            .collect();
        return Err(ApiError::Conflict(format!(
            "connection `{id}` still serves {} project(s): {} — pass unbind_repos=true to remove them with it",
            names.len(),
            names.join(", ")
        )));
    }

    connections
        .delete_connection(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // The credential outlives the row only if something else still points at it.
    if let Some(secrets) = st.secrets.as_ref() {
        let still_referenced = connections
            .list_connections()
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?
            .iter()
            .any(|c| c.credential_ref == conn.credential_ref);
        if !still_referenced {
            secrets
                .delete(
                    &scarab_secrets::SecretScope::Org {
                        org: FORGE_CREDENTIALS_ORG.to_string(),
                    },
                    &conn.credential_ref,
                )
                .await
                .map_err(secret_err)?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/connections/{id}/available-repos`: what the connection's credential
/// reaches, and which of those Scarab already governs.
#[derive(Debug, Serialize, ToSchema)]
pub struct AvailableRepoDto {
    pub owner: String,
    pub name: String,
    /// Already a Project on this connection — the bind form renders it as done
    /// rather than offering a no-op that silently re-homes a live binding.
    pub bound: bool,
}

/// `POST /v1/connections/{id}/repos` body: the repo to bring under governance.
#[derive(Debug, Deserialize, ToSchema)]
pub struct BindRepoRequest {
    pub owner: String,
    pub name: String,
    /// Also create the forge-side webhook, so a push actually reaches Scarab.
    /// Defaults to **true**: a bound repo with no hook is a Project that silently
    /// never builds, which is not a state anyone asks for on purpose.
    #[serde(default = "default_true")]
    pub register_webhook: bool,
}

fn default_true() -> bool {
    true
}

/// The outcome of binding a repo: the Project it created, and what happened to
/// the webhook.
#[derive(Debug, Serialize, ToSchema)]
pub struct BindRepoResultDto {
    /// The governed Project's natural key — `(owner, name)` in v1 (1 Project : 1
    /// RepoRef), the same mapping installation auto-registration uses.
    pub org: String,
    pub project: String,
    /// Did a forge-side webhook get registered (or already exist)?
    pub webhook_registered: bool,
    /// Why it did not, when it did not. The binding still stands — see the
    /// handler's note on why a hook failure is reported rather than rolled back.
    pub webhook_error: Option<String>,
}

/// The repos a connection's credential can reach (ADR-0060) — the **pick-list**
/// the bind form offers instead of asking an admin to type `owner/name` and get
/// it right.
///
/// The forge is the authority on what a connection covers, so this is a live
/// call, not a cached view. An adapter that cannot enumerate answers 501 rather
/// than an empty list: "I cannot look" and "there is nothing there" must not read
/// the same, or an admin concludes their token is scoped wrong.
#[utoipa::path(
    get,
    path = "/v1/connections/{id}/available-repos",
    summary = "Repos this connection's credential can reach, for the bind pick-list (ADR-0060)",
    params(("id" = String, Path, description = "connection id")),
    responses(
        (status = 200, body = [AvailableRepoDto]),
        (status = 403, description = "requires Administer on the org"),
        (status = 404, description = "no such connection, or no registry/forge wired"),
        (status = 501, description = "this forge adapter cannot enumerate repos")
    )
)]
async fn available_repos(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<AvailableRepoDto>>, ApiError> {
    authorize_org_administer(&st, &headers).await?;
    let connections = st.connections.as_ref().ok_or(ApiError::NotFound)?;
    let conn = connections
        .get_connection(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    let forge = connection_adapter(&st, &conn).await?;

    let reported = match forge.list_accessible_repos().await {
        Ok(repos) => repos,
        Err(scarab_forge::ForgeError::Unsupported(what)) => {
            return Err(ApiError::NotImplemented(format!(
                "{} cannot enumerate repos ({what})",
                conn.kind.as_str()
            )))
        }
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };
    let bound: std::collections::BTreeSet<scarab_forge::RepoRef> = connections
        .repos_of(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .into_iter()
        .collect();
    let mut out: Vec<AvailableRepoDto> = reported
        .into_iter()
        .map(|repo| AvailableRepoDto {
            bound: bound.contains(&repo),
            owner: repo.owner,
            name: repo.name,
        })
        .collect();
    out.sort_by(|a, b| (&a.owner, &a.name).cmp(&(&b.owner, &b.name)));
    Ok(Json(out))
}

/// Bind a repo to this connection — **which is how a Project comes into being**
/// (ADR-0060 part C).
///
/// There is no `projects` table: a Project *is* a `forge_repos` binding
/// (ADR-0046), so this endpoint is the repo→Project onboarding flow for any forge
/// without installation-style auto-registration. After it, the repo appears on
/// `GET /v1/repos`, can hold Environments and secrets, and its pushes resolve to
/// a tenant. GitHub keeps binding itself from the `installation` webhook; this is
/// the Forgejo path.
///
/// Registration is attempted **after** the binding lands and its failure is
/// *reported, not rolled back*: the binding is the durable governance fact, a
/// hook is a remote side effect on a system that may be momentarily unreachable,
/// and unbinding on a failed hook call would delete a Project an admin just
/// asked for. `POST …/repos/{owner}/{name}/webhook` retries.
#[utoipa::path(
    post,
    path = "/v1/connections/{id}/repos",
    summary = "Bind a repo to a connection, creating its Project, and register its webhook (ADR-0060)",
    params(("id" = String, Path, description = "connection id")),
    request_body = BindRepoRequest,
    responses(
        (status = 200, body = BindRepoResultDto),
        (status = 400, description = "missing owner/name"),
        (status = 403, description = "requires Administer on the org"),
        (status = 404, description = "no such connection, or no registry wired"),
        (status = 409, description = "the repo is already bound to a different connection")
    )
)]
async fn bind_repo(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<BindRepoRequest>,
) -> Result<Json<BindRepoResultDto>, ApiError> {
    authorize_org_administer(&st, &headers).await?;
    let connections = st.connections.as_ref().ok_or(ApiError::NotFound)?;
    deny_if_config_owned(connections, &id, "its set of repositories").await?;
    let conn = connections
        .get_connection(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    let repo = scarab_forge::RepoRef {
        owner: req.owner.trim().to_string(),
        name: req.name.trim().to_string(),
    };
    if repo.owner.is_empty() || repo.name.is_empty() {
        return Err(ApiError::BadRequest("owner and name are required".into()));
    }
    refuse_reserved_org(&repo.owner, "own a bound repo")?;

    // `bind_repo` upserts, so re-binding a repo owned by ANOTHER connection would
    // silently re-home a live Project onto a different forge account. A v1
    // `RepoRef` is globally unique across connections (ADR-0046), so this is a
    // conflict, not a move.
    if let Some(existing) = connections
        .resolve(&repo)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
    {
        if existing.connection.id != id {
            return Err(ApiError::Conflict(format!(
                "{}/{} is already bound to connection `{}`",
                repo.owner, repo.name, existing.connection.id
            )));
        }
    }

    // Project name = repo name, org = repo owner — identical to the mapping
    // `apply_installation_sync` and re-sync use, so a manually-bound Project is
    // indistinguishable from an auto-registered one.
    connections
        .bind_repo(&id, &repo, &repo.owner, &repo.name)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    // A new org gets the first-party libraries (ADR-0076 §5): claimed now,
    // installed in the background, so the bind never waits on a forge.
    libraries::offer_defaults_on_bind(&st, &repo.owner).await;

    let (webhook_registered, webhook_error) = if req.register_webhook {
        match try_register_webhook(&st, &conn, &repo).await {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e)),
        }
    } else {
        (false, None)
    };
    Ok(Json(BindRepoResultDto {
        org: repo.owner,
        project: repo.name,
        webhook_registered,
        webhook_error,
    }))
}

/// Register the forge-side webhook for `repo` on `conn`, returning the failure as
/// a human string. Idempotent by adapter contract (Forgejo skips a hook that
/// already points at the same callback URL); a documented no-op on GitHub, where
/// the App receives every installation's events on one endpoint.
async fn try_register_webhook(
    st: &AppState,
    conn: &scarab_forge::ForgeConnection,
    repo: &scarab_forge::RepoRef,
) -> Result<(), String> {
    let forge = connection_adapter(st, conn)
        .await
        .map_err(|_| "no forge adapter is wired for this connection".to_string())?;
    let callback = forge_webhook_url(&st.public_url, conn.kind);
    if is_loopback_callback(&callback) {
        tracing::warn!(
            callback = %callback,
            repo = %format!("{}/{}", repo.owner, repo.name),
            "registering a webhook whose callback is loopback: only a forge on \
             this host can ever deliver to it. Set SCARAB_PUBLIC_URL to the URL \
             the forge reaches Scarab on."
        );
    }
    forge
        .register_webhook(repo, &callback)
        .await
        .map_err(|e| e.to_string())
}

/// Register (or re-register) a bound repo's webhook — the retry for the one step
/// of onboarding that depends on the forge being reachable *right now*.
///
/// Only for a repo this connection already governs: registering a hook that
/// points at Scarab for a repo Scarab has no Project for would produce deliveries
/// that resolve to nothing.
#[utoipa::path(
    post,
    path = "/v1/connections/{id}/repos/{owner}/{name}/webhook",
    summary = "Register the forge-side webhook for a bound repo (ADR-0046 register_webhook)",
    params(
        ("id" = String, Path, description = "connection id"),
        ("owner" = String, Path, description = "repo owner"),
        ("name" = String, Path, description = "repo name")
    ),
    responses(
        (status = 200, body = BindRepoResultDto),
        (status = 400, description = "the forge rejected the registration"),
        (status = 403, description = "requires Administer on the org"),
        (status = 404, description = "no such connection, or the repo is not bound to it")
    )
)]
async fn register_repo_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, owner, name)): Path<(String, String, String)>,
) -> Result<Json<BindRepoResultDto>, ApiError> {
    authorize_org_administer(&st, &headers).await?;
    let connections = st.connections.as_ref().ok_or(ApiError::NotFound)?;
    deny_if_config_owned(
        connections,
        &id,
        "webhook registration for its repositories",
    )
    .await?;
    let conn = connections
        .get_connection(&id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    let repo = scarab_forge::RepoRef { owner, name };
    let resolved = connections
        .resolve(&repo)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .filter(|r| r.connection.id == id)
        .ok_or(ApiError::NotFound)?;

    try_register_webhook(&st, &conn, &repo)
        .await
        .map_err(ApiError::BadRequest)?;
    Ok(Json(BindRepoResultDto {
        org: resolved.org,
        project: resolved.project,
        webhook_registered: true,
        webhook_error: None,
    }))
}

/// Unbind a repo — **which removes its Project** (ADR-0060 part C).
///
/// The inverse of the bind above, and destructive in the same measure: the
/// binding is the Project, so its Environments, scoped secrets and RBAC go with
/// it. That is why re-sync never does this on a forge's say-so and why this is an
/// explicit, human-addressed endpoint.
///
/// The forge-side webhook is deliberately **left in place**. Deleting hooks is
/// not in the port (ADR-0046 exposes registration only), and a stale hook is
/// harmless: an unbound repo's deliveries resolve to nothing and are dropped.
#[utoipa::path(
    delete,
    path = "/v1/connections/{id}/repos/{owner}/{name}",
    summary = "Unbind a repo from a connection, removing its Project (ADR-0060)",
    params(
        ("id" = String, Path, description = "connection id"),
        ("owner" = String, Path, description = "repo owner"),
        ("name" = String, Path, description = "repo name")
    ),
    responses(
        (status = 204, description = "repo unbound"),
        (status = 403, description = "requires Administer on the org"),
        (status = 404, description = "no such connection, or the repo is not bound to it")
    )
)]
async fn unbind_repo(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, owner, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize_org_administer(&st, &headers).await?;
    let connections = st.connections.as_ref().ok_or(ApiError::NotFound)?;
    deny_if_config_owned(connections, &id, "its set of repositories").await?;
    let repo = scarab_forge::RepoRef { owner, name };
    // 404 rather than a silent 204 for a repo this connection does not govern: an
    // unbind aimed at the wrong connection must not read as "already gone".
    connections
        .resolve(&repo)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .filter(|r| r.connection.id == id)
        .ok_or(ApiError::NotFound)?;
    connections
        .unbind_repo(&id, &repo)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resolve a [`scarab_forge::ForgeConnection`]'s credential material at
/// use-time (ADR-0046): the bytes an adapter authenticates with, fetched from
/// `SecretProvider` under the reserved [`FORGE_CREDENTIALS_ORG`] scope by the
/// connection's `credential_ref` handle.
/// Refuse a write to a connection that **configuration owns** (ADR-0060 part D).
///
/// A config-declared connection is authoritative and read-only in-product: its
/// `base_url`, credential and repo bindings are whatever the `connections:` block
/// says at boot. A UI write here would either be reverted on the next deploy or
/// silently diverge from the source of truth — the config-vs-DB dual-write drift
/// part D exists to prevent. So the answer is 409 plus *where the change belongs*,
/// not a quiet no-op.
///
/// Read paths (`GET`, `available-repos`) stay open: seeing a config-owned
/// connection is exactly how an admin confirms the deploy took effect.
async fn deny_if_config_owned(
    connections: &Arc<dyn scarab_forge::ForgeConnectionStore>,
    id: &str,
    what: &str,
) -> Result<(), ApiError> {
    if connections_config::is_config_owned(connections.as_ref(), id)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
    {
        return Err(ApiError::Conflict(format!(
            "connection `{id}` is managed by configuration (ADR-0060 part D) — {what} is \
             declared in the `connections:` block. Change it there and redeploy."
        )));
    }
    Ok(())
}

pub async fn connection_credential(
    secrets: &dyn scarab_secrets::SecretProvider,
    conn: &scarab_forge::ForgeConnection,
) -> Result<Vec<u8>, scarab_secrets::SecretError> {
    let scope = scarab_secrets::SecretScope::Org {
        org: FORGE_CREDENTIALS_ORG.to_string(),
    };
    Ok(secrets.get(&scope, &conn.credential_ref).await?.value)
}

/// Resolve a step's scoped secrets and prepare them for injection (ADR-0014,
/// 0013): fetch each `key` at `scope` from `provider`, **register its value with
/// the log redactor** so it can never appear in stored or streamed logs, and
/// return the values as `(key, value)` env pairs for the Pod. The launch path
/// merges these into the step's env (an executor detail; the live Pod wiring is
/// k8s/`build_pod`).
pub async fn resolve_step_secrets(
    provider: &dyn scarab_secrets::SecretProvider,
    logs: &LogService,
    scope: &scarab_secrets::SecretScope,
    keys: &[String],
    locked_out: bool,
) -> Result<Vec<(String, String)>, scarab_secrets::SecretError> {
    // Fork-PR lockout (ADR-0015): untrusted runs get NO secrets, so we never
    // even read them from the provider.
    if locked_out {
        return Ok(Vec::new());
    }
    let mut env = Vec::with_capacity(keys.len());
    for key in keys {
        // `resolve` (not `get`) so an env-scoped run inherits repo/org secrets
        // (ADR-0037); the exact scope is tried first, so exact hits are unchanged.
        let secret = provider.resolve(scope, key).await?;
        logs.register_secret(&secret.value);
        env.push((
            key.clone(),
            String::from_utf8_lossy(&secret.value).into_owned(),
        ));
    }
    Ok(env)
}

/// The JWKS a cloud fetches to verify Scarab-issued OIDC tokens (ADR-0015).
#[utoipa::path(get, path = "/.well-known/jwks.json", summary = "OIDC issuer JWKS (ADR-0015)", responses((status = 200, description = "the signing keys"), (status = 404, description = "issuer disabled")))]
async fn jwks(State(st): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let issuer = st.oidc.as_ref().ok_or(ApiError::NotFound)?;
    Ok(Json(issuer.jwks()))
}

/// The OIDC discovery document.
#[utoipa::path(get, path = "/.well-known/openid-configuration", summary = "OIDC discovery (ADR-0015)", responses((status = 200, description = "the discovery document"), (status = 404, description = "issuer disabled")))]
async fn openid_configuration(
    State(st): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let issuer = st.oidc.as_ref().ok_or(ApiError::NotFound)?;
    Ok(Json(issuer.discovery()))
}

/// The security posture for a run triggered by `event` (ADR-0015, 0005). An
/// untrusted fork PR is locked out of secrets and its OIDC subject environment
/// is downgraded to `none`, so its token can never assume a real environment's
/// cloud role; trusted events keep their `target_env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPolicy {
    pub secrets_locked_out: bool,
    pub oidc_env: String,
}

pub fn fork_policy(event: &scarab_forge::Event, target_env: &str) -> ForkPolicy {
    if event.is_fork_pr() {
        ForkPolicy {
            secrets_locked_out: true,
            oidc_env: "none".to_string(),
        }
    } else {
        ForkPolicy {
            secrets_locked_out: false,
            oidc_env: target_env.to_string(),
        }
    }
}

/// `GET /metrics` (ADR-0053): Prometheus text exposition of the key gauges —
/// runs by status and outbox backlog, read live from the durable store on
/// each scrape (pull-model, always consistent, no in-process drift) — plus the
/// process-lifetime counters from [`crate::metrics`] for failures the store
/// cannot distinguish after the fact.
#[utoipa::path(get, path = "/metrics", summary = "Prometheus gauges (ADR-0053)", responses((status = 200, description = "Prometheus text exposition")))]
async fn metrics(State(st): State<AppState>) -> Result<Response, ApiError> {
    let mut out = String::new();
    out.push_str(
        "# HELP scarab_runs Current run count by status.
# TYPE scarab_runs gauge
",
    );
    for (status, n) in st.db.run_status_counts().await? {
        out.push_str(&format!(
            "scarab_runs{{status=\"{status}\"}} {n}
"
        ));
    }
    out.push_str(
        "# HELP scarab_outbox_depth Undispatched outbox messages.
# TYPE scarab_outbox_depth gauge
",
    );
    out.push_str(&format!(
        "scarab_outbox_depth {}
",
        st.db.outbox_depth().await?
    ));
    // Process-lifetime counters for failures that leave no distinguishing state
    // in the store — a rejected commit-status post looks like an untried one
    // (ba921db). See `crate::metrics`.
    crate::metrics::render(&mut out);
    let mut resp = out.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    Ok(resp)
}

/// `GET /readyz` (ADR-0053): readiness = can this replica actually serve —
/// DB and object store reachable. Distinct from `/healthz` (liveness =
/// process up): a server with a dead DB must leave rotation, not restart.
#[utoipa::path(get, path = "/readyz", summary = "Readiness: DB + object store reachable (ADR-0053)", responses((status = 200, description = "ready"), (status = 503, description = "a dependency is unreachable")))]
async fn readyz(State(st): State<AppState>) -> Response {
    if let Err(e) = st.db.run_status_counts().await {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")).into_response();
    }
    if let Some(store) = &st.artifact_store {
        // NotFound = reachable; only a backend error is unready.
        if let Err(scarab_storage::StorageError::Backend(e)) = store.get("readyz/probe").await {
            return (StatusCode::SERVICE_UNAVAILABLE, format!("store: {e}")).into_response();
        }
    }
    "ready".into_response()
}

/// Request-id middleware (ADR-0053): every response carries `x-request-id`
/// (honoring an inbound one), and a tracing span correlates the logs.
pub async fn request_id_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let span = tracing::info_span!("request", request_id = %id, method = %req.method(), path = %req.uri().path());
    let mut resp = {
        let _enter = span.enter();
        next.run(req).await
    };
    if let Ok(v) = axum::http::HeaderValue::from_str(&id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

#[utoipa::path(get, path = "/healthz", summary = "Liveness: process up (ADR-0053)", responses((status = 200, description = "alive")))]
async fn healthz() -> &'static str {
    "ok"
}

/// Serve the generated OpenAPI document.
#[allow(clippy::unused_async)]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// The curated operation groups carried by the spec, in sidebar display order.
///
/// The doc renderer (`starlight-openapi`) builds a page per operation and groups
/// them by tag, so an untagged spec renders as a bare overview with no operation
/// pages at all. Tagging happens here rather than on 53 `#[utoipa::path]`
/// annotations so the grouping is one curated list a reviewer can read, and a
/// newly-added route inherits its group from its path instead of silently
/// landing untagged (`operation_tags_cover_every_operation` asserts that).
const TAG_GROUPS: &[(&str, &str)] = &[
    (
        "Runs",
        "Launch, read, cancel, rerun and retry Runs; their events, logs, \
         results, workspace, artifacts and gates.",
    ),
    (
        "Repositories",
        "The governed repos Scarab knows about, their pipeline catalog, refs, \
         launch interface and manual dispatch.",
    ),
    (
        "Environments",
        "Org-level Environments (ADR-0071) and their governance; per-repo \
         Environments and their protection rules, plus their legacy approval \
         history.",
    ),
    (
        "Plugins",
        "PluginInstallations (ADR-0072): the first-party catalog, install and \
         uninstall, the kill switch, and the PluginConnections the \
         CredentialBroker proxies to.",
    ),
    (
        "Libraries",
        "Org-installed libraries (ADR-0076): fetch a version to review, \
         install, upgrade and remove, yanks, and the first-party defaults.",
    ),
    (
        "Policies",
        "Policies (ADR-0075): org Policies and Project typed settings with \
         their version history, the PolicyDecision logs, and the policy test \
         API.",
    ),
    (
        "Secrets",
        "Scoped secrets (org / repo / environment) and the effective-status \
         matrix that resolves them.",
    ),
    (
        "Webhooks",
        "Forge delivery endpoints. Called by the forge, not by clients; each \
         verifies its own signature.",
    ),
    (
        "Forge Connections",
        "The ForgeConnections Scarab authenticates to a forge with, and the \
         reconciliation that refreshes what they can see (ADR-0046).",
    ),
    (
        "Auth & Identity",
        "Login/session, issued API tokens, the current principal, OIDC \
         discovery and JWKS, and RBAC role bindings.",
    ),
    (
        "System",
        "Liveness, readiness and Prometheus metrics (ADR-0053).",
    ),
];

/// Which [`TAG_GROUPS`] entry an operation belongs to, decided by its path.
///
/// First match wins, so the order is load-bearing: `/…/bindings/import` is
/// access control rather than a repo operation, and `/…/repos/…/runs` is a Run
/// listing rather than a repo one. Returns `None` for a path no rule claims —
/// a test turns that into a build failure rather than an untagged operation.
fn tag_for_path(path: &str) -> Option<&'static str> {
    let group = if path.starts_with("/webhooks/") {
        "Webhooks"
    } else if path.starts_with("/v1/connections") {
        "Forge Connections"
    } else if path.starts_with("/.well-known/")
        || path.starts_with("/v1/auth/")
        || path == "/v1/me"
        || path.starts_with("/v1/me/")
        || path.starts_with("/v1/step-up/")
        || path.starts_with("/v1/security/")
        || path.contains("/security/")
        || path.contains("/passkeys/")
        || path.contains("bindings")
        || path.contains("/principals")
        || path.contains("/tokens")
    {
        "Auth & Identity"
    } else if matches!(path, "/healthz" | "/readyz" | "/metrics") {
        "System"
    } else if path.contains("/policies") || path.contains("/policy-") {
        // Before Runs: `/v1/runs/{id}/policy-decisions` is a Policy read.
        "Policies"
    } else if path.contains("/plugins")
        || path.contains("/plugin-catalog")
        || path == "/v1/orgs/{org}/gates"
    {
        "Plugins"
    } else if path.contains("/libraries") || path.contains("/library-") {
        "Libraries"
    } else if path.contains("/environments") || path.contains("/environment-migration-report") {
        "Environments"
    } else if path.contains("/secrets") {
        "Secrets"
    } else if path.contains("/runs") {
        "Runs"
    } else if path.starts_with("/v1/repos") {
        "Repositories"
    } else {
        return None;
    };
    Some(group)
}

/// Every operation a `PathItem` actually declares. `utoipa` models the methods
/// as separate `Option` fields rather than a map, so walking them needs this.
fn path_item_operations(
    item: &mut utoipa::openapi::path::PathItem,
) -> impl Iterator<Item = &mut utoipa::openapi::path::Operation> {
    [
        &mut item.get,
        &mut item.put,
        &mut item.post,
        &mut item.delete,
        &mut item.options,
        &mut item.head,
        &mut item.patch,
        &mut item.trace,
    ]
    .into_iter()
    .flatten()
}

/// Stamps the curated group onto every operation and declares the groups
/// top-level, so the canonical `openapi.json` carries its own grouping.
struct TagGroups;

impl utoipa::Modify for TagGroups {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let mut used = std::collections::HashSet::new();
        for (path, item) in &mut openapi.paths.paths {
            let Some(group) = tag_for_path(path) else {
                continue;
            };
            for op in path_item_operations(item) {
                op.tags = Some(vec![group.to_owned()]);
            }
            used.insert(group);
        }
        // Declare only the groups actually in use, in TAG_GROUPS order — an
        // empty group would render as an empty sidebar section.
        openapi.tags = Some(
            TAG_GROUPS
                .iter()
                .filter(|(name, _)| used.contains(name))
                .map(|(name, description)| {
                    utoipa::openapi::tag::TagBuilder::new()
                        .name(*name)
                        .description(Some(*description))
                        .build()
                })
                .collect(),
        );
    }
}

/// Is this path reachable **without** a Scarab credential?
///
/// Liveness/readiness/metrics are probes; the auth endpoints are how a caller
/// gets a credential in the first place; a webhook is called by the forge and
/// authenticates itself by signature, not by anything of ours. Everything else
/// needs one, and [`SecuritySchemes`] says so per operation rather than only
/// once at the top — a generated client reads the operation.
fn is_public_path(path: &str) -> bool {
    path.starts_with("/webhooks/")
        || path.starts_with("/.well-known/")
        || path.starts_with("/v1/auth/")
        || matches!(path, "/healthz" | "/readyz" | "/metrics" | "/openapi.json")
}

/// Declares HOW to authenticate to this API — which `openapi.json` did not say
/// at all before issued API tokens existed, because there was nothing to say:
/// the only credential was a browser session id.
struct SecuritySchemes;

impl utoipa::Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{
            ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme,
        };
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "apiToken",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("scarab_pat")
                    .description(Some(
                        "An issued API token: `Authorization: Bearer scarab_pat_…`. \
                         Mint one at `POST /v1/orgs/{org}/tokens`; it is scoped to one \
                         Org or Project, carries a role ceiling, and expires. This is \
                         what `scarab --token` / `SCARAB_TOKEN` sends.",
                    ))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "sessionCookie",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                "scarab_session",
                "A browser login session (ADR-0049). Set by the OAuth flow, and — \
                 unlike a Bearer token — it rides ambiently, so every mutation must \
                 also double-submit the `scarab_csrf` cookie as `x-csrf-token`.",
            ))),
        );

        let required = vec![
            utoipa::openapi::security::SecurityRequirement::new::<&str, [&str; 0], &str>(
                "apiToken",
                [],
            ),
            utoipa::openapi::security::SecurityRequirement::new::<&str, [&str; 0], &str>(
                "sessionCookie",
                [],
            ),
        ];
        for (path, item) in &mut openapi.paths.paths {
            // An EMPTY list is not the same as saying nothing: it declares the
            // operation as deliberately unauthenticated, so a reader can tell a
            // public endpoint from one whose security someone forgot.
            let security = if is_public_path(path) {
                Vec::new()
            } else {
                required.clone()
            };
            for op in path_item_operations(item) {
                op.security = Some(security.clone());
            }
        }
    }
}

/// Documents the step-up guard (ADR-0073) on every operation it governs,
/// from the route table itself — so the spec cannot drift from what the
/// guard enforces: each crown jewel (and each conditional one) can answer
/// `403 step_up_required` with a [`step_up::StepUpRefusal`] body, a machine
/// caller `429` when it has too many requests waiting, and a conditional
/// Environment write `409` when the state changed under it.
struct StepUpResponses;

impl utoipa::Modify for StepUpResponses {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use axum::http::Method;
        use utoipa::openapi::{ContentBuilder, Ref, RefOr, ResponseBuilder};
        const STEP_UP: &str = "`step_up_required` (ADR-0073): a crown-jewel operation needs a \
             fresh StepUpConfirmation — a passkey in the caller's own browser (`mode` `window` \
             or `action`; `enroll` when there is no passkey yet). An API token or a Bearer \
             session gets `confirmation_url` instead: a one-time StepUpConfirmationRequest its \
             human confirms.";
        const TOO_MANY: &str = "an API token or Bearer session already has 10 step-up \
             confirmation requests waiting (ADR-0073 §6)";
        const CHANGED: &str = "the governed state changed between this request's read and its \
             write (reload and retry), or the request conflicts with the current state";
        let plain = |d: &str| RefOr::T(ResponseBuilder::new().description(d).build());
        for entry in route_table() {
            let conditional = matches!(entry.op.class, step_up::Class::Conditional { .. });
            if !(conditional || matches!(entry.op.class, step_up::Class::CrownJewel(_))) {
                continue;
            }
            let Some(item) = openapi.paths.paths.get_mut(entry.path) else {
                continue;
            };
            let op = match entry.method {
                Method::POST => item.post.as_mut(),
                Method::PUT => item.put.as_mut(),
                Method::DELETE => item.delete.as_mut(),
                _ => None,
            };
            let Some(op) = op else { continue };
            let refusal = ContentBuilder::new()
                .schema(Some(Ref::from_schema_name("StepUpRefusal")))
                .build();
            let responses = &mut op.responses.responses;
            match responses.get_mut("403") {
                Some(RefOr::T(resp)) => {
                    if !resp.description.contains("step_up_required") {
                        resp.description = format!("{} — or {STEP_UP}", resp.description);
                    }
                    resp.content
                        .entry("application/json".to_string())
                        .or_insert(refusal);
                }
                Some(RefOr::Ref(_)) => {}
                None => {
                    let resp = ResponseBuilder::new()
                        .description(format!("{STEP_UP} Or the caller's role is insufficient."))
                        .content("application/json", refusal)
                        .build();
                    responses.insert("403".to_string(), RefOr::T(resp));
                }
            }
            responses
                .entry("429".to_string())
                .or_insert_with(|| plain(TOO_MANY));
            if conditional {
                responses
                    .entry("409".to_string())
                    .or_insert_with(|| plain(CHANGED));
            }
        }
    }
}

/// The generated OpenAPI document. The pipeline request schema is the IR subset.
#[derive(OpenApi)]
#[openapi(
    modifiers(&TagGroups, &SecuritySchemes, &StepUpResponses),
    paths(
        healthz,
        readyz,
        metrics,
        jwks,
        openid_configuration,
        login,
        search_principals,
        oauth_login_redirect,
        oauth_callback,
        auth_providers,
        begin_link_identity,
        unlink_identity,
        logout,
        me,
        mint_api_token,
        list_api_tokens,
        revoke_api_token,
        list_bindings,
        put_binding,
        delete_binding,
        import_bindings,
        github_webhook,
        forgejo_webhook,
        put_environment,
        get_environment,
        list_environments,
        delete_environment,
        list_deployments,
        org_environments::list_org_environments,
        org_environments::get_org_environment,
        org_environments::put_org_environment,
        org_environments::delete_org_environment,
        org_environments::get_environment_eligibility,
        org_environments::put_environment_eligibility,
        org_environments::get_environment_protection,
        org_environments::put_environment_protection,
        org_environments::list_environment_projects,
        org_environments::get_environment_project_overlay,
        org_environments::put_environment_project_overlay,
        org_environments::delete_environment_project_overlay,
        org_environments::get_environment_project_privilege_grant,
        org_environments::put_environment_project_privilege_grant,
        org_environments::delete_environment_project_privilege_grant,
        org_environments::get_environment_migration_report,
        org_environments::acknowledge_environment_migration_report,
        libraries::list_libraries,
        libraries::fetch_library,
        libraries::install_library,
        libraries::remove_library,
        libraries::yank_library,
        libraries::unyank_library,
        libraries::retry_library_defaults,
        libraries::trust_library_source,
        libraries::untrust_library_source,
        libraries::get_yank_impact,
        run_origin::get_step_origin,
        refused_runs::list_refused_runs,
        policies::list_org_policies,
        policies::create_org_policy,
        policies::get_org_policy,
        policies::update_org_policy,
        policies::delete_org_policy,
        policies::list_org_policy_versions,
        policies::list_project_policies,
        policies::create_project_policy,
        policies::get_project_policy,
        policies::update_project_policy,
        policies::delete_project_policy,
        policies::list_project_policy_versions,
        policies::policy_test,
        policies::list_org_policy_decisions,
        policies::list_run_policy_decisions,
        plugins::plugin_catalog,
        plugins::list_plugins,
        plugins::install_plugin,
        plugins::get_plugin,
        plugins::uninstall_plugin,
        plugins::kill_plugin,
        plugins::enable_plugin,
        plugins::put_plugin_storage,
        plugins::delete_plugin_storage,
        plugins::list_plugin_connections,
        plugins::put_plugin_connection,
        plugins::delete_plugin_connection,
        secret_matrix,
        silence_secret_cell,
        unsilence_secret_cell,
        list_connections,
        create_connection,
        delete_connection,
        resync_connection,
        connection_preflight,
        available_repos,
        bind_repo,
        register_repo_webhook,
        unbind_repo,
        ingest_step_results,
        create_run,
        dispatch,
        list_refs,
        list_pipelines,
        pipeline_interface,
        list_runs,
        list_projects,
        list_repo_runs,
        get_run,
        get_events,
        get_logs,
        get_step_logs,
        get_services,
        get_service_logs,
        attach_step,
        debug_pod_step,
        rerun_step,
        retry_step,
        rerun_plan,
        pin_run_snapshots,
        unpin_run_snapshots,
        cancel_run,
        list_artifacts,
        download_artifact,
        get_step_results,
        get_attempt_consumed,
        list_workspace,
        get_workspace_file,
        approve_gate,
        reject_gate,
        release_gate_external,
        plugin_gates::list_pending_gates,
        put_secret,
        list_secrets,
        delete_secret,
        step_up::ceremony::me_security,
        step_up::ceremony::passkey_registration_options,
        step_up::ceremony::passkey_registration,
        step_up::ceremony::remove_passkey,
        step_up::ceremony::step_up_options,
        step_up::ceremony::step_up_confirm,
        step_up::requests::get_request,
        step_up::requests::request_options,
        step_up::requests::confirm_request,
        step_up::events::my_security_events,
        step_up::events::security_events,
        step_up::events::org_security_events,
        step_up::events::org_security_summary,
        step_up::admin::reset_member_passkeys
    ),
    components(schemas(
        CreateRunRequest,
        DispatchRequest,
        DispatchKind,
        PipelineCatalogResponse,
        CatalogEntry,
        RefsResponse,
        RefDto,
        PipelineInterfaceResponse,
        PipelineDto,
        StepDto,
        CreateRunResponse,
        RunListResponse,
        RunSummaryDto,
        WaitingGateDto,
        RunStatusResponse,
        SnapshotRetentionDto,
        RerunPlanResponse,
        PlannedStepDto,
        PlanReasonDto,
        ExpiredInputDto,
        StepStatusDto,
        GateResolutionDto,
        StepServiceDto,
        AttemptDto,
        ServiceStatusDto,
        StepResultDto,
        ConsumedDto,
        ArtifactDto,
        WorkspaceEntryDto,
        WorkspaceListing,
        PutSecretRequest,
        RejectGateRequest,
        ExternalGateVerdict,
        plugin_gates::PluginGateRelease,
        plugin_gates::PendingGateDto,
        plugin_gates::PendingGatesPage,
        SecretListResponse,
        SecretMatrix,
        SecretMatrixRow,
        SilenceCellRequest,
        ConnectionDto,
        ConnectionProjectDto,
        CreateConnectionRequest,
        CreatedConnectionDto,
        AvailableRepoDto,
        BindRepoRequest,
        BindRepoResultDto,
        ResyncResultDto,
        ConnectionPreflightDto,
        CapabilityRequirementDto,
        GrantedPermissionDto,
        MeResponse,
        AuthProviderDto,
        LinkIdentityRequest,
        LinkIdentityResponse,
        PrincipalDto,
        PrincipalIdentityDto,
        MintTokenRequest,
        MintedTokenDto,
        ApiTokenDto,
        org_environments::OrgEnvironmentDto,
        org_environments::PutOrgEnvironmentRequest,
        org_environments::EnvironmentEligibilityDto,
        org_environments::ProjectEnvironmentDto,
        org_environments::EnvironmentProtectionRulesDto,
        org_environments::EffectiveProtectionDto,
        org_environments::EnvironmentProjectPrivilegeGrantDto,
        org_environments::ImageGrantDto,
        org_environments::PutEnvironmentProtectionRequest,
        org_environments::PutEnvironmentProjectOverlayRequest,
        org_environments::EnvironmentProjectOverlayDto,
        org_environments::PutEnvironmentProjectPrivilegeGrantRequest,
        org_environments::EnvironmentProjectDto,
        org_environments::LegacyProtectionRulesDto,
        org_environments::EnvironmentMigrationReportProjectDto,
        org_environments::EnvironmentMigrationReportDto,
        libraries::OrgLibrariesDto,
        libraries::LibraryInstallationDto,
        libraries::LibraryYankDto,
        libraries::UpdateNoticeDto,
        libraries::FirstPartyLibraryDto,
        libraries::LibraryDefaultsDto,
        libraries::MissingDefaultDto,
        libraries::FetchLibraryRequest,
        libraries::FetchedLibraryDto,
        libraries::InstalledVersionDto,
        libraries::InstallLibraryRequest,
        libraries::YankLibraryRequest,
        libraries::YankImpactDto,
        CompileErrorDto,
        CompileErrorResponse,
        refused_runs::RefusedRunDto,
        refused_runs::RefusedRunListResponse,
        libraries::YankImpactRunDto,
        run_origin::OriginLinkDto,
        run_origin::RunYankDto,
        run_origin::GateOutputDto,
        run_origin::PluginGateViewDto,
        run_origin::BoundInputDto,
        run_origin::RenderedSpecDto,
        run_origin::StepOriginDto,
        libraries::RetryDefaultsRequest,
        libraries::TrustedSourceDto,
        libraries::TrustSourceRequest,
        policies::PolicyDto,
        policies::PolicyVersionDto,
        policies::CreatePolicyRequest,
        policies::UpdatePolicyRequest,
        policies::PolicyDecisionRecordDto,
        policies::PolicyDecisionPageDto,
        policies::PolicyTestRequest,
        policies::PolicyTestPipeline,
        policies::PolicyTestTrigger,
        policies::PolicyTestResponse,
        policies::PolicyReadsDto,
        policies::RuleReadsDto,
        plugins::PluginCapabilityDto,
        plugins::RequestRuleDto,
        plugins::ConnectionSlotDto,
        plugins::PluginManifestDto,
        plugins::GateDefinitionDto,
        plugins::GateFieldDto,
        plugins::PluginCatalogEntryDto,
        plugins::InstallationStateDto,
        plugins::RuntimeStatusDto,
        plugins::PluginInstallationDto,
        plugins::PluginStorageDto,
        plugins::PluginStorageRequest,
        plugins::InstallPluginRequest,
        plugins::UninstalledPluginDto,
        plugins::PluginConnectionDto,
        plugins::PutPluginConnectionRequest,
        step_up::OpSummary,
        step_up::StepUpRefusal,
        step_up::WebauthnChallengeDto,
        step_up::ceremony::PasskeyDto,
        step_up::ceremony::MeSecurityResponse,
        step_up::ceremony::PasskeyRegistrationOptionsRequest,
        step_up::ceremony::PasskeyRegistrationRequest,
        step_up::ceremony::StepUpConfirmRequest,
        step_up::ceremony::StepUpConfirmResponse,
        step_up::requests::StepUpRequestDto,
        step_up::requests::StepUpRequestResultDto,
        step_up::events::SecurityEventDto,
        step_up::events::SecurityEventsResponse,
        step_up::events::OrgSecuritySummary,
        step_up::admin::PasskeyResetDto
    ))
)]
pub struct ApiDoc;

/// The generated OpenAPI document as pretty JSON — the stable artifact clients
/// generate from and CI diffs against (ADR-0012, 0028). This is the exact
/// document served at `/openapi.json`.
pub fn openapi_json() -> String {
    ApiDoc::openapi()
        .to_pretty_json()
        .expect("OpenAPI document serializes")
}

/// Build the HTTP router bound to `state`.
pub fn router(state: AppState) -> Router {
    router_inner(state)
}

fn router_inner(state: AppState) -> Router {
    // The single route list lives in `routes.rs`, each route registered with
    // its ADR-0073 classification.
    routes::governed_routes()
        .router
        // ADR-0073: the step-up guard runs on every matched route, deciding
        // from the route's registered classification.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            step_up::guard::guard,
        ))
        // The embedded web UI (ADR-0054): everything no API route claimed
        // falls through here — a real file from dist/, else index.html (SPA
        // client routing). Same-origin by construction: no CORS anywhere.
        .fallback(serve_ui)
        // Request-id correlation on every route (ADR-0053).
        .layer(axum::middleware::from_fn(request_id_middleware))
        .with_state(state)
}

/// Serve the SPA (ADR-0054): a request for a real file under the dist dir
/// gets it; anything else (a client-side route like `/acme/web/runs/…`) gets
/// index.html. Path traversal is rejected by segment sanitization. With no
/// UI dir configured (dev API-only), non-API paths are plain 404s.
///
/// Every answer forbids framing (`frame-ancestors 'none'`, `X-Frame-Options:
/// DENY`): the step-up confirm page is a human's last look at what a token
/// asked to run, and a framed one could be click-jacked (ADR-0073 §6).
async fn serve_ui(State(st): State<AppState>, uri: axum::http::Uri) -> Response {
    let mut resp = serve_ui_file(&st, &uri).await;
    let headers = resp.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(
        axum::http::header::X_FRAME_OPTIONS,
        axum::http::HeaderValue::from_static("DENY"),
    );
    resp
}

async fn serve_ui_file(st: &AppState, uri: &axum::http::Uri) -> Response {
    let Some(dir) = &st.ui_dir else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rel = uri.path().trim_start_matches('/');
    let mut path = dir.clone();
    // Segment-wise join: never let `..` (or an absolute segment) escape dist.
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') {
            continue;
        }
        path.push(seg);
    }
    let file = if path.is_file() {
        path
    } else {
        dir.join("index.html")
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            let mime = match file
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
            {
                "html" => "text/html; charset=utf-8",
                "js" => "application/javascript",
                "css" => "text/css",
                "svg" => "image/svg+xml",
                "png" => "image/png",
                "ico" => "image/x-icon",
                "json" => "application/json",
                "woff2" => "font/woff2",
                _ => "application/octet-stream",
            };
            let mut resp = bytes.into_response();
            resp.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static(mime),
            );
            resp
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Wire vocabulary (matches the durable store's on-disk vocabulary).
// ---------------------------------------------------------------------------

fn run_status_name(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Suspended => "suspended",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::DeadLettered => "dead_lettered",
    }
}

fn step_status_name(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Pending => "pending",
        StepStatus::Ready => "ready",
        StepStatus::Running => "running",
        StepStatus::Succeeded => "succeeded",
        StepStatus::Failed => "failed",
        StepStatus::Skipped => "skipped",
        StepStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod grant_admission_tests {
    use super::{admit_k8s_overlay, admit_step_grants};
    use scarab_pipeline::StepSecurity;
    use scarab_project::{
        migrate_legacy_environment, Environment, EnvironmentProjectPrivilegeGrant, ImageGrant,
        ProtectionRules,
    };
    use scarab_secrets::SecretScope;

    /// What ADR-0071 §10 step 5 makes of a legacy environment's privileges:
    /// the Project's grant (or none). Admission decides against it now, so
    /// these cases are ported by migrating their legacy rules — the
    /// assertions are the pre-ADR-0071 ones, unchanged.
    fn grant(p: &ProtectionRules) -> Option<EnvironmentProjectPrivilegeGrant> {
        let legacy = Environment {
            name: "prod".into(),
            protection: p.clone(),
        };
        migrate_legacy_environment("acme", "web", &legacy).1
    }

    const IMG: &str = "ghcr.io/acme/deployer@sha256:aaaa";

    fn rules(images: Vec<ImageGrant>) -> ProtectionRules {
        ProtectionRules {
            approvers: vec![],
            wait_timer: 0,
            allowed_refs: vec![],
            concurrency: 1,
            secret_scope: SecretScope::Org { org: "acme".into() },
            oidc_subject: String::new(),
            privileged_images: images,
            permit_k8s_overlay: false,
            require_reason: false,
        }
    }

    #[test]
    fn k8s_overlay_none_is_always_admitted_as_none() {
        assert_eq!(admit_k8s_overlay(None, None).unwrap(), None);
    }

    #[test]
    fn k8s_overlay_rejected_without_environment() {
        let overlay = serde_json::json!({"spec": {"schedulerName": "x"}});
        let err = admit_k8s_overlay(None, Some(&overlay)).unwrap_err();
        assert!(err
            .join("; ")
            .contains("requires a target Environment that permits"));
    }

    #[test]
    fn k8s_overlay_rejected_when_environment_does_not_permit() {
        let overlay = serde_json::json!({"spec": {"schedulerName": "x"}});
        let g = grant(&rules(vec![]));
        let err = admit_k8s_overlay(Some(g.as_ref()), Some(&overlay)).unwrap_err();
        assert!(err.join("; ").contains("permit_k8s_overlay"));
    }

    #[test]
    fn k8s_overlay_admitted_when_environment_permits() {
        let overlay = serde_json::json!({"spec": {"schedulerName": "x"}});
        let mut p = rules(vec![]);
        p.permit_k8s_overlay = true;
        assert_eq!(
            admit_k8s_overlay(Some(grant(&p).as_ref()), Some(&overlay)).unwrap(),
            Some(overlay)
        );
    }

    #[test]
    fn no_request_is_baseline() {
        let g = admit_step_grants(None, None, IMG, false).unwrap();
        assert!(!g.run_as_root && !g.privileged && g.add_capabilities.is_empty());
    }

    #[test]
    fn run_as_root_is_self_service_without_environment() {
        let sec = StepSecurity {
            run_as_root: true,
            ..Default::default()
        };
        let g = admit_step_grants(None, Some(&sec), IMG, false).unwrap();
        assert!(g.run_as_root);
    }

    #[test]
    fn governed_grant_without_environment_is_rejected() {
        let sec = StepSecurity {
            privileged: true,
            ..Default::default()
        };
        let err = admit_step_grants(None, Some(&sec), IMG, false).unwrap_err();
        assert!(err
            .iter()
            .any(|v| v.contains("require a target Environment")));
    }

    #[test]
    fn governed_grant_admitted_for_whitelisted_digest() {
        let sec = StepSecurity {
            privileged: true,
            ..Default::default()
        };
        let p = rules(vec![ImageGrant {
            image_digest: "sha256:aaaa".into(),
            privileged: true,
            capabilities: vec![],
        }]);
        let g = admit_step_grants(Some(grant(&p).as_ref()), Some(&sec), IMG, false).unwrap();
        assert!(g.privileged);
    }
}

#[cfg(test)]
mod canonicalize_ref_tests {
    use super::canonicalize_ref;

    #[test]
    fn bare_branch_becomes_a_heads_ref() {
        assert_eq!(canonicalize_ref("main"), "refs/heads/main");
        // A slashed branch name is still a bare branch (not a `refs/` ref).
        assert_eq!(canonicalize_ref("release/1.2"), "refs/heads/release/1.2");
    }

    #[test]
    fn fully_qualified_ref_is_verbatim() {
        assert_eq!(canonicalize_ref("refs/heads/main"), "refs/heads/main");
        assert_eq!(canonicalize_ref("refs/tags/v1"), "refs/tags/v1");
        assert_eq!(canonicalize_ref("refs/pull/7/head"), "refs/pull/7/head");
    }

    #[test]
    fn raw_sha_stays_verbatim_so_it_wont_match_a_branch_glob() {
        let sha = "0123456789abcdef0123456789abcdef01234567"; // 40 hex chars
        assert_eq!(canonicalize_ref(sha), sha);
    }

    #[test]
    fn uppercase_or_wrong_length_hex_is_treated_as_a_branch() {
        // 40 chars but uppercase → not a canonical SHA → a branch name.
        let upper = "0123456789ABCDEF0123456789ABCDEF01234567";
        assert_eq!(canonicalize_ref(upper), format!("refs/heads/{upper}"));
        // A short hex-ish name is a branch, not a SHA.
        assert_eq!(canonicalize_ref("abc123"), "refs/heads/abc123");
    }
}

#[cfg(test)]
mod debug_pod_tests {
    use super::{debug_pod_plan, ApiError};
    use scarab_engine::{Db, RunId, StepId, StepSpec, Timestamp};
    use scarab_testkit::InMemoryDb;

    /// The debug Pod starts the image the Step launched with: its stored
    /// image is a template, rendered with the Run's parameters and the
    /// Step's coordinate. Before, it started the template text as an image.
    #[tokio::test]
    async fn the_debug_pod_runs_the_rendered_image() {
        let db = InMemoryDb::new();
        let run = RunId("r".into());
        let step = StepId("s".into());
        db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
        db.set_run_params(
            &run,
            &std::collections::BTreeMap::from([("tag".to_string(), serde_json::json!("1.36"))]),
        )
        .await
        .unwrap();
        let spec: StepSpec = serde_json::from_value(serde_json::json!({
            "image": "busybox:${{ inputs.tag }}-${{ matrix.os }}",
            "command": [],
            "env": [],
            "matrix_values": { "os": "linux" }
        }))
        .unwrap();
        db.create_step_run(&run, &step, Some(&spec), &[], Timestamp(0))
            .await
            .unwrap();
        let Ok((_, image, _)) = debug_pod_plan(&db, &run, &step).await else {
            panic!("the debug Pod plan failed");
        };
        assert_eq!(image, "busybox:1.36-linux");

        // An image that does not render is refused, not started as text.
        let bad = StepId("bad".into());
        let spec: StepSpec = serde_json::from_value(serde_json::json!({
            "image": "busybox:${{ inputs.missing }}",
            "command": [],
            "env": []
        }))
        .unwrap();
        db.create_step_run(&run, &bad, Some(&spec), &[], Timestamp(0))
            .await
            .unwrap();
        match debug_pod_plan(&db, &run, &bad).await {
            Err(ApiError::BadRequest(m)) => assert!(m.contains("does not render"), "{m}"),
            Err(_) => panic!("expected a bad request"),
            Ok((_, image, _)) => panic!("expected a refusal, got image {image}"),
        }
    }
}

#[cfg(test)]
mod compile_tests {
    use super::{compile_bounded, off_runtime};
    use std::time::Duration;

    /// A compile runs off the async workers: one that takes too long, or
    /// panics, is an error with the reason, and the server keeps running.
    #[tokio::test]
    async fn work_off_the_runtime_is_bounded_in_time_and_contains_a_panic() {
        let slow = off_runtime(Duration::from_millis(10), || {
            std::thread::sleep(Duration::from_millis(100))
        })
        .await
        .unwrap_err();
        assert_eq!(slow, "did not finish within 0.01 seconds");
        let panicked = off_runtime(Duration::from_secs(5), || -> u8 { panic!("boom") })
            .await
            .unwrap_err();
        assert!(panicked.starts_with("stopped:"), "{panicked}");
        assert_eq!(off_runtime(Duration::from_secs(5), || 7).await, Ok(7));

        let (ir, _) = compile_bounded(
            "steps: [{ id: a, image: busybox }]".into(),
            Default::default(),
        )
        .await
        .unwrap();
        assert_eq!(ir.steps.len(), 1);
        let e = compile_bounded("steps: [{ id: a }]".into(), Default::default())
            .await
            .unwrap_err();
        assert!(e.contains("step `a`"), "{e}");
    }
}
