//! Validated boot configuration — the ONE documented place for every
//! `SCARAB_*` knob the server process reads (ADR-0048).
//!
//! The composition root parses the CLI (clap merges flags with their env
//! counterparts), then [`Config::resolve`] reads the remaining env-only knobs
//! and validates the whole picture **before anything binds or connects**.
//! Motto: fail fast, fail early — an unsafe or pointless configuration stops
//! the process at boot with a clear message, never a silent facade.
//!
//! ## Knob inventory
//!
//! | Knob | Where | Meaning |
//! |------|-------|---------|
//! | `SCARAB_ROLE` | CLI `--role` | which slice(s) this process runs |
//! | `SCARAB_ADDR` | CLI `--addr` | bind address |
//! | `SCARAB_DATABASE_URL` | CLI `--database-url` | Postgres URL — **mandatory for every role**, the `workspace` data plane included (ADR-0067 part 2: it connects for derived rows, never migrates) |
//! | `SCARAB_OBJECT_DIR` | CLI `--object-dir` | local object-store directory — **explicit only** (tests/dev); no silent default since ADR-0067 part 1 |
//! | `SCARAB_NAMESPACE` | CLI `--namespace` | k8s namespace for step Pods |
//! | `SCARAB_EXECUTOR` | CLI `--executor` | `k8s` (prod) or `local` (dev/CLI) |
//! | `SCARAB_S3_BUCKET` | env | selects S3/MinIO object store when set |
//! | `SCARAB_S3_ENDPOINT` | env | S3 endpoint (empty = AWS) |
//! | `SCARAB_S3_REGION` | env | S3 region (default `us-east-1`) |
//! | `SCARAB_S3_ACCESS_KEY` | env | S3 access key |
//! | `SCARAB_S3_SECRET_KEY` | env | S3 secret key |
//! | `SCARAB_CAS_CONCURRENCY` | env | in-flight object-store round-trips per workspace-CAS leg (ADR-0061 s2); default 32. The legs are latency-bound, so this is a *floor* for remote storage — raise it when the store is far away, lower it when blobs are large, because peak memory is roughly `concurrency × largest blob` |
//! | `SCARAB_RESULTS_TOKEN_SECRET` | env | enables results-egress sidecar + ingest (ADR-0042) |
//! | `SCARAB_RESULTS_API_URL` | env | base URL the sidecar posts results to |
//! | `SCARAB_SIDECAR_IMAGE` | env | results-egress sidecar image |
//! | `SCARAB_WORKSPACE_TOKEN_SECRET` | env | enables the workspace service + the token Step Pods present to it (ADR-0061). **Required** under `--role workspace`; deliberately a DIFFERENT secret from `SCARAB_RESULTS_TOKEN_SECRET` |
//! | `SCARAB_WORKSPACE_URL` | env | base URL of the workspace service; default `http://scarab-workspace` |
//! | `SCARAB_WORKSPACE_DATA_DIR` | env | the workspace service's **warm tier** directory (its persistent volume); default `./.scarab/workspace-cas`. Only read under `--role workspace` |
//! | `SCARAB_WSFETCH_IMAGE` | env | the workspace **helper** image every workspace Step Pod carries (ADR-0061): the fetch init container (s3-feed) AND the egress hold/drain sidecar (`scarab-wsfetch hold` / the in-Pod `drain` the control plane execs); default `ghcr.io/thulasi-ram/scarab-wsfetch:edge`. ADR-0062 stage 2 replaces only the **eager-fetch** role; the egress role survives it — the old "dies with the node driver" note (git-bug 0628369) applied to the fetch role alone and is closed as superseded |
//! | `SCARAB_GITHUB_WEBHOOK_SECRET` | env | HMAC secret for `/webhooks/github` |
//! | `SCARAB_FORGEJO_WEBHOOK_SECRET` | env | HMAC secret for `/webhooks/forgejo` (ADR-0046 — each forge endpoint binds its own secret) |
//! | `SCARAB_GATE_TOKEN_SECRET` | env | enables external-gate release tokens (ADR-0034) |
//! | `SCARAB_OIDC_ISSUER` | env | enables the OIDC issuer (keyless federation, ADR-0015) |
//! | `SCARAB_OIDC_SIGNING_KEY_FILE` | env | PKCS#8 RSA PEM the issuer signs with — **required** when the issuer is enabled (persistent across restarts/replicas) |
//! | `SCARAB_OIDC_AUDIENCE` | env | `aud` of minted per-run tokens (ADR-0015); default `scarab` |
//! | `SCARAB_MASTER_KEY` | env | base64 32-byte KEK for envelope encryption (ADR-0014) — **required** unless `SCARAB_DEV_INSECURE=1` |
//! | `SCARAB_DEV_INSECURE` | env | `1`/`true`: downgrade the **security** hard-fails (KEK, auth) to loud boot warnings — dev only, never relaxes the Postgres requirement |
//! | `SCARAB_STEP_TIMEOUT_SECS` | env | global default step deadline (ADR-0047); default 3600 (1h), per-step overridable via `timeout:` |
//! | `SCARAB_PUBLIC_URL` | env | Scarab's public base URL — the run deep-link every forge status carries (ADR-0046); default `http://localhost:8080` (dev) |
//! | `SCARAB_CLONE_IMAGE` | env | the canonical scarab-clone image a `clone` step runs (ADR-0045); default `ghcr.io/thulasi-ram/scarab-clone:edge` — digest-pin in production |
//! | `SCARAB_PLACEMENT_CONFIG_FILE` | env | path to the operator placement config (ADR-0055): cluster baseline + PlacementProfile registry (YAML/JSON, gitops-managed); a bad path/parse is a boot failure |
//! | `SCARAB_GITHUB_APP_ID` | env | GitHub App id (ADR-0046): when set, GitHub connections authenticate in **App mode** (their credential secret is the App PEM); absent = token mode (dev) |
//! | `SCARAB_OAUTH_CLIENT_ID` … `_CLIENT_SECRET`, `_AUTHORIZE_URL`, `_TOKEN_URL`, `_USERINFO_URL` | env | OAuth/OIDC login provider (ADR-0049): all five together enable real authn (GitHub, Forgejo, or any OIDC issuer); a partial set refuses boot |
//! | `SCARAB_OAUTH_SCOPES` | env | space-separated scopes for the authorize redirect (optional; e.g. `read:user` for GitHub, `openid profile email` for OIDC) |
//! | `SCARAB_OAUTH_ISSUER` | env | **optional** — the provider's OIDC issuer (`iss`), e.g. `https://dex.example`. Set it for a real OIDC issuer (Dex/Keycloak/Google): the `id_token` is then verified (JWKS via `{issuer}/.well-known/openid-configuration`, `iss`/`aud`/`exp`/`nonce`) and its claims are the identity; an invalid one fails the login. Unset = plain OAuth2 (GitHub/Forgejo): no `id_token` is trusted, userinfo is the identity. Setting it *without* the five above is partial config and refuses boot |
//! | `SCARAB_RETENTION_LOG_DAYS` | env | terminal runs' log TTL in days (ADR-0050); default 30 — metadata is retained regardless |
//! | `SCARAB_RETENTION_ARTIFACT_DAYS` | env | terminal runs' artifact TTL in days (ADR-0052); default 90 |
//! | `SCARAB_RETENTION_WORKSPACE_DAYS` | env | terminal runs' workspace-CAS reachability TTL in days (ADR-0050 mark-sweep); default 14 — non-terminal runs always reachable |
//! | `SCARAB_OAUTH_OWNERS` | env | comma-separated entries granted `Owner` at login (bootstrap until scoped RBAC, ADR-0049 C2); everyone else logs in as `Viewer`. An entry matches the Principal subject (`sub`/`login`/`id`) **or** a provider-VERIFIED `email` claim (`email_verified`) — unverified/absent verification never grants `Owner` |
//! | `SCARAB_CONNECTIONS` | env | the declarative `connections:` block inline (YAML/JSON, ADR-0060 part D): config-owned forge connections, provisioned at boot and read-only in the UI. Wins over `_FILE` |
//! | `SCARAB_CONNECTIONS_FILE` | env | path to a file holding the same `connections:` block (the GitOps shape: a ConfigMap mount). A bad path/parse/validation is a boot failure |
//!
//! Step-runtime env is injected *into* step containers (or their init/sidecar
//! containers) by the executors and is **not** boot configuration — this table
//! does not cover it. Today that is `SCARAB_RUN`/`SCARAB_STEP`/`SCARAB_ATTEMPT`,
//! `SCARAB_RESULTS*`, `SCARAB_PARAM_*`, and the ADR-0061 workspace-fetch set
//! (`SCARAB_WORKSPACE_TOKEN_FILE`, `SCARAB_WORKSPACE_URL`,
//! `SCARAB_SNAPSHOT_ROOTS`, `SCARAB_WORKSPACE_TARGET`) — note that
//! `SCARAB_WORKSPACE_URL` appears in BOTH lists: the same value is boot config
//! for this process and injected env for a Step Pod's fetcher.
//! `SCARAB_SERVER`/`SCARAB_TOKEN` belong to `scarab` (the CLI client).

use base64::Engine;
use clap::{Parser, ValueEnum};
use serde::Deserialize;

/// Which slice(s) of Scarab this process should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Role {
    /// Run every role in one process (default; ideal for dev).
    Converged,
    /// Serve the public/control-plane HTTP API only.
    Api,
    /// Run the durable scheduler / reconciler loop only.
    Scheduler,
    /// Run the step executor / worker only.
    Executor,
    /// Ingest and normalize inbound forge webhooks only.
    Webhook,
    /// Serve the **workspace service** (ADR-0061): a warm content-addressed
    /// store on a persistent volume, in front of the cold object-storage
    /// archive. A **data-plane** role — it shares this binary (one image, so
    /// server↔service version skew is structurally impossible under one Helm
    /// release) but not the durable core.
    Workspace,
}

impl Role {
    /// Roles that drive the scheduler + executor background loop.
    pub fn runs_driver(self) -> bool {
        matches!(self, Role::Converged | Role::Scheduler | Role::Executor)
    }

    /// Roles that **own** the durable core: the migration path, the secrets
    /// store, the KEK.
    ///
    /// The workspace service (ADR-0061) is a **data-plane** component: it
    /// decrypts nothing, and running `migrate()` from N per-failure-domain
    /// replicas would be actively dangerous. Since ADR-0067 part 2 it DOES
    /// connect to the same Postgres — for derived, rebuildable rows only
    /// (drain records, write ledgers), which is why `SCARAB_DATABASE_URL` is
    /// now mandatory for **every** role (see [`Config::resolve`]) and this
    /// predicate no longer gates it. What it still scopes is exactly what the
    /// workspace role must not have: the KEK, and the migration path.
    ///
    /// This is a **narrow carve-out, not a weakening**: every other role still
    /// refuses to boot without a KEK, and there is still no API-only mode.
    pub fn needs_durable_core(self) -> bool {
        !matches!(self, Role::Workspace)
    }
}

/// Which execution backend the driver uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExecutorKind {
    /// Kubernetes: one Pod per step (production; ADR-0005).
    K8s,
    /// Local host processes — a dev/CLI backend for laptop runs without a
    /// cluster (ADR-0036). Never a production deployment mode.
    Local,
}

/// The full CLI/env surface of the server binary. clap merges each flag with
/// its `SCARAB_*` env counterpart; env-only knobs are read in
/// [`Config::resolve`].
#[derive(Debug, Parser)]
#[command(name = "scarab-server", about = "Scarab durable CI — server process")]
pub struct Cli {
    /// The role this process runs as.
    #[arg(long, value_enum, env = "SCARAB_ROLE", default_value_t = Role::Converged)]
    pub role: Role,

    /// Validate configuration, print the startup report, and exit WITHOUT
    /// connecting or binding — a smoke check for images/CI that must not hang.
    /// Fails (non-zero) on invalid configuration, like a real boot would.
    #[arg(long)]
    pub dry_run: bool,

    /// Deprecated no-op: serving is the default. Kept so existing scripts and
    /// muscle memory (`--serve`) keep working.
    #[arg(long, hide = true)]
    pub serve: bool,

    /// Address to bind. Override per-environment via `SCARAB_ADDR` (e.g. a dev
    /// `.env.local` sets `127.0.0.1:8899`).
    #[arg(long, env = "SCARAB_ADDR", default_value = "0.0.0.0:8080")]
    pub addr: String,

    /// Postgres connection URL. Mandatory for every serving role (ADR-0048):
    /// Postgres is the durable core and the coordination bus — there is no
    /// API-only mode. `--emit-openapi` is the only DB-free path.
    #[arg(long, env = "SCARAB_DATABASE_URL")]
    pub database_url: Option<String>,

    /// Local directory backing the object store (logs/artifacts) — a bucket
    /// stand-in for tests and dev loops. EXPLICIT ONLY (ADR-0067 part 1): the
    /// object store is a hard requirement, so there is no silent `./.scarab/
    /// objects` fallback any more — set `SCARAB_S3_BUCKET` or pass this.
    #[arg(long, env = "SCARAB_OBJECT_DIR")]
    pub object_dir: Option<String>,

    /// Kubernetes namespace the executor launches step Pods into.
    #[arg(long, env = "SCARAB_NAMESPACE", default_value = "scarab")]
    pub namespace: String,

    /// Execution backend for the driver. `k8s` (default, production) or `local`
    /// (host processes — a cluster-free dev/CLI loop, ADR-0036).
    #[arg(long, value_enum, env = "SCARAB_EXECUTOR", default_value_t = ExecutorKind::K8s)]
    pub executor: ExecutorKind,

    /// Write the generated OpenAPI document to this path and exit (client
    /// codegen / CI spec check). Never connects to Postgres or serves.
    #[arg(long, value_name = "PATH")]
    pub emit_openapi: Option<String>,
}

/// S3/MinIO object-store settings (selected by `SCARAB_S3_BUCKET`).
#[derive(Debug, Clone)]
pub struct S3Config {
    pub bucket: String,
    /// Empty = real AWS (the SDK default endpoint).
    pub endpoint: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

/// Which object store backs logs/artifacts.
#[derive(Debug, Clone)]
pub enum StoreConfig {
    /// S3/MinIO — the dev harness and production.
    S3(S3Config),
    /// A local directory — zero-dependency dev.
    LocalDir(String),
}

/// Results-egress sidecar wiring (ADR-0042); present only when
/// `SCARAB_RESULTS_TOKEN_SECRET` is set.
#[derive(Debug, Clone)]
pub struct ResultsEgressConfig {
    /// Shared HMAC secret: mints the sidecar's fence token and verifies it at
    /// the ingest endpoint.
    pub token_secret: Vec<u8>,
    /// Base URL the sidecar posts results to.
    pub api_url: String,
    /// The sidecar container image.
    pub sidecar_image: String,
}

/// Workspace service wiring (ADR-0061), present when
/// `SCARAB_WORKSPACE_TOKEN_SECRET` is set. Mirrors [`ResultsEgressConfig`]:
/// one shared HMAC secret both mints the token a Step Pod carries and verifies
/// it at the service.
///
/// **Vocabulary**: this configures access to **Workspace Snapshots** — the
/// immutable content-addressed trees that flow along DAG edges — never to a
/// **Workspace**, which is the mutable pod-local filesystem a Step executes in
/// (CONTEXT.md §4.2). The knob names keep the service's name; the data they
/// point at is snapshots.
#[derive(Debug, Clone)]
pub struct WorkspaceServiceConfig {
    /// Shared HMAC secret for the workspace token (ADR-0061). Deliberately NOT
    /// the results-egress secret: that one carries no verb and never expires,
    /// and sharing it would let the workspace service forge step results.
    pub token_secret: Vec<u8>,
    /// Base URL of the workspace service — what the control plane and (once the
    /// fetcher lands) a Step Pod's helper dial.
    pub url: String,
    /// The **warm tier's** directory: the persistent volume the service holds
    /// its content-addressed store on. Only meaningful for `--role workspace`.
    pub data_dir: String,
    /// The **fetcher** image a Step Pod's init container runs to pull its input
    /// snapshots from the service (ADR-0061 s3-feed). Digest-pin in production,
    /// exactly as with `clone_image`.
    ///
    /// ⚠ A temporally-ordered stepping stone, not the design: it is *eager*, and
    /// the CSI/FUSE node driver replaces it — at which point this knob, the image
    /// and `WorkspaceFetch` all disappear together (git-bug 0628369).
    pub fetcher_image: String,
}

/// OIDC issuer settings (selected by `SCARAB_OIDC_ISSUER`). The signing key is
/// mandatory: without a persistent key the JWKS changes every boot and cloud
/// federation silently breaks on restart/replica (ADR-0048).
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// The public base URL clouds are configured to trust.
    pub issuer_url: String,
    /// Path to the PKCS#8 RSA private-key PEM the issuer signs with.
    pub signing_key_file: String,
    /// The `aud` claim of minted per-run tokens (ADR-0015) — what the cloud's
    /// trust policy is configured to expect. Default `scarab`.
    pub audience: String,
}

/// A string whose value must never reach a log line. `Debug` prints `***`, so
/// putting one inside a `#[derive(Debug)]` struct (like [`Config`]) cannot leak
/// it into the startup report, a panic message, or a `tracing` field.
#[derive(Clone, PartialEq, Eq)]
pub struct Redacted(String);

impl Redacted {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The material itself. Named so every read site is grep-able.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// One declaratively-provisioned forge connection (ADR-0060 part D): the
/// `connections:` block is **authoritative** for the connections it declares,
/// which are therefore read-only in the UI.
///
/// A spec is fully resolved: `credential.env` / `credential.file` material is
/// read and checked at boot, so if a `ConnectionSpec` exists its credential is
/// either present in hand or is a `SecretProvider` handle to look up at use-time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionSpec {
    /// Stable identity — the `forge_connections` primary key. Changing it in
    /// config provisions a *new* connection; it is not a rename.
    pub id: String,
    pub kind: scarab_forge::ForgeKind,
    /// The API base URL the adapter talks to.
    pub base_url: String,
    /// The `_forge`-scoped handle recorded on the connection row. For an
    /// env/file credential there is nothing to look up, so the id doubles as
    /// the handle — the readout then names the *config* as the source.
    pub credential_ref: String,
    /// Deployment-supplied credential material (`credential.env` /
    /// `credential.file`) — the **override** half of the one resolution path
    /// (ADR-0060 part D), generalizing `SCARAB_GITHUB_APP_PEM[_FILE]` from "just
    /// the PEM" to any connection. `None` = resolve `credential_ref` from
    /// `SecretProvider` instead.
    pub credential_material: Option<Redacted>,
    /// Where the credential comes from, for the startup report: `env VAR`,
    /// `file PATH`, or `secret VAR`. Never the value.
    pub credential_source: String,
    /// Repos this connection owns — each binding *is* a Project (ADR-0046), so
    /// declaring repos here is the declarative Project-onboarding path.
    pub repos: Vec<scarab_forge::RepoRef>,
}

/// The `connections:` document, as written in `SCARAB_CONNECTIONS` /
/// `SCARAB_CONNECTIONS_FILE` (or a Helm `scarab.connections` value, which is
/// passed through verbatim).
///
/// `deny_unknown_fields` throughout: a typo in a declarative block that is
/// silently ignored is exactly the "silent facade" ADR-0048 refuses. A misspelled
/// key fails the boot with the field name in the message.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConnectionsDoc {
    #[serde(default)]
    connections: Vec<RawConnection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConnection {
    id: String,
    kind: String,
    base_url: String,
    credential: RawCredential,
    /// `owner/name` coordinates to bind. Org = owner, Project = name — the same
    /// 1:1 mapping the installation webhook and re-sync use, so a config-bound
    /// Project is indistinguishable from a webhook-registered one.
    #[serde(default)]
    repos: Vec<String>,
}

/// Exactly one of the three sources. Two would make "which one wins?" a config
/// question, which is the ambiguity part D exists to remove.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCredential {
    /// Name of an env var holding the material (an override).
    #[serde(default)]
    env: Option<String>,
    /// Path to a mounted file holding the material (an override).
    #[serde(default)]
    file: Option<String>,
    /// A `SecretProvider` handle under the `_forge` org — no override; the
    /// material is fetched at use-time as it is for a DB-owned connection.
    #[serde(default)]
    secret_ref: Option<String>,
}

/// A validated boot configuration: if a `Config` exists, the process may
/// legitimately start. Construction is the startup gate (ADR-0048).
#[derive(Debug, Clone)]
pub struct Config {
    pub role: Role,
    pub addr: String,
    /// Present for **every** role — construction fails without it, and there
    /// is still no API-only mode. The ADR-0061 workspace carve-out is gone:
    /// ADR-0067 part 2 has the Depot connect to the same Postgres for derived,
    /// rebuildable rows (drain records, write ledgers) — connecting yes,
    /// migrating never. The type carries the guarantee, so no dispatch site
    /// needs an `expect` or a `""` sentinel.
    pub database_url: String,
    pub namespace: String,
    pub executor: ExecutorKind,
    pub store: StoreConfig,
    pub results_egress: Option<ResultsEgressConfig>,
    /// Workspace service wiring (ADR-0061). `None` = no workspace service in
    /// this deployment; `--role workspace` refuses to boot without it.
    pub workspace: Option<WorkspaceServiceConfig>,
    pub github_webhook_secret: Option<Vec<u8>>,
    pub forgejo_webhook_secret: Option<Vec<u8>>,
    pub gate_token_secret: Option<Vec<u8>>,
    pub oidc: Option<OidcConfig>,
    /// The envelope-encryption KEK (`SCARAB_MASTER_KEY`). `None` only under
    /// `SCARAB_DEV_INSECURE=1` — the composition root generates a loud
    /// ephemeral key.
    pub master_key: Option<[u8; 32]>,
    /// `SCARAB_DEV_INSECURE=1`: the one loud escape hatch, for *security*
    /// hard-fails only (ADR-0048). Never the silent default.
    pub dev_insecure: bool,
    /// Global default step deadline in seconds (ADR-0047): applied when a step
    /// declares no `timeout:`. A default is mandatory — it closes the
    /// "hung Pod wedges the run forever" hole.
    pub step_timeout_secs: u32,
    /// Scarab's public base URL (ADR-0046): the run deep-link every forge
    /// status carries (`{public_url}/runs/{id}`). Dev default
    /// `http://localhost:8080`.
    pub public_url: String,
    /// GitHub App id (ADR-0046). `Some` = App mode (connection credentials
    /// are the App private-key PEM); `None` = token mode (dev).
    pub github_app_id: Option<String>,
    /// GitHub App private-key PEM supplied inline (`SCARAB_GITHUB_APP_PEM`) — a
    /// bootstrap-free / GitOps OVERRIDE of the DB-stored `_forge` credential, so
    /// a fresh DB needs no `reseed.sh` PUT. App mode only; inline wins over file.
    pub github_app_pem: Option<String>,
    /// Path to a mounted file holding the App PEM (`SCARAB_GITHUB_APP_PEM_FILE`).
    /// Read at boot (a bad path is a boot failure, ADR-0048), mirroring the OIDC
    /// signing key. Overridden by `github_app_pem` if both are set.
    pub github_app_pem_file: Option<String>,
    /// The canonical scarab-clone image (ADR-0045) — the image every `clone`
    /// step runs, digest-pinned in production.
    pub clone_image: String,
    /// Path to the operator **placement config** file (ADR-0055): the cluster
    /// baseline (tolerations/nodeSelector/default resources) + the named
    /// PlacementProfile registry, YAML/JSON, gitops-managed. `None` = no baseline
    /// and an empty registry (pre-0055 behavior). Read at boot; a bad path/parse
    /// is a boot failure (ADR-0048), mirroring the OIDC signing key.
    pub placement_config_file: Option<String>,
    /// OAuth/OIDC login (ADR-0049 C1). `Some` = real authn wired; `None` is
    /// only bootable under `SCARAB_DEV_INSECURE=1`.
    pub oauth: Option<OAuthConfig>,
    /// How long a terminal run's logs are retained, in days (ADR-0050;
    /// ADR-0030 default 30). Run metadata is retained regardless.
    pub retention_log_days: u32,
    /// How long a terminal run's artifacts are retained, in days (ADR-0052;
    /// ADR-0030 default 90).
    pub retention_artifact_days: u32,
    /// How long a TERMINAL run's workspace CAS stays reachable, in days
    /// (ADR-0050 mark-sweep; default 14). Non-terminal runs are always
    /// reachable regardless of age.
    pub retention_workspace_days: u32,
    /// In-flight object-store round-trips per workspace-CAS leg (ADR-0061 s2);
    /// default [`scarab_storage_s3::DEFAULT_CAS_CONCURRENCY`].
    ///
    /// Resolved *here* rather than by an ambient `std::env::var` in the adapter,
    /// so it obeys the same three rules as every other knob (ADR-0048): one
    /// documented place, a junk value fails the boot, and the live value appears
    /// in [`Config::startup_report`] where an operator can actually read it.
    pub cas_concurrency: usize,
    /// Config-owned forge connections (ADR-0060 part D), parsed and fully
    /// resolved at boot from `SCARAB_CONNECTIONS[_FILE]`. Empty = every
    /// connection is DB-owned (the pre-0060 world). Each of these is
    /// authoritative over its DB row and read-only in the UI.
    pub connections: Vec<ConnectionSpec>,
}

/// The forge-agnostic OAuth/OIDC login provider (ADR-0049): explicit
/// endpoints work identically for GitHub, Forgejo, and any OIDC issuer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    /// The provider's authorization endpoint the browser is redirected to.
    pub authorize_url: String,
    /// The token endpoint the code is exchanged at.
    pub token_url: String,
    /// The userinfo endpoint the access token is presented to; its `sub` (or
    /// `login`/`id`) becomes the Principal subject.
    pub userinfo_url: String,
    /// The OIDC issuer (`iss`) this provider mints `id_token`s as, if it is one
    /// (`SCARAB_OAUTH_ISSUER`). `Some` = **OIDC mode**: a returned `id_token` is
    /// verified against the issuer's JWKS (found via `{issuer}/.well-known/
    /// openid-configuration`) plus `iss`/`aud`/`exp`/`nonce`, and its claims win
    /// over userinfo; an invalid one fails the login. `None` = plain OAuth2
    /// (GitHub/Forgejo token mode): `id_token` is neither expected nor trusted,
    /// userinfo is the identity.
    pub oidc_issuer: Option<String>,
    /// Space-separated scopes for the authorize redirect (may be empty).
    pub scopes: String,
    /// Entries granted `Owner` at login (bootstrap until scoped RBAC, C2);
    /// everyone else authenticates as `Viewer`. An entry matches the Principal
    /// subject **or** a provider-VERIFIED `email` claim — with a real OIDC
    /// issuer `sub` is an opaque per-client id, so subject-only bootstrap would
    /// mean pasting UUIDs before anyone can administer anything.
    pub owners: Vec<String>,
}

/// A configuration the process must refuse to start under, with a message
/// telling the operator exactly what to fix.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error(
        "SCARAB_DATABASE_URL is not set. Postgres is mandatory for every serving role \
         (ADR-0048) — there is no API-only mode, and the workspace role needs it too \
         (ADR-0067 part 2: it reads/writes drain records and write ledgers; it never \
         migrates). Start a Postgres (dev: `just up`) and set SCARAB_DATABASE_URL; \
         only `--emit-openapi` works without a database. SCARAB_DEV_INSECURE does \
         NOT relax this."
    )]
    MissingDatabaseUrl,

    #[error(
        "SCARAB_MASTER_KEY is not set but the secrets store is enabled (ADR-0048). \
         Without it, secrets would be sealed under a random ephemeral key and become \
         undecryptable after a restart. Set a base64 32-byte key \
         (`head -c 32 /dev/urandom | base64`), or set SCARAB_DEV_INSECURE=1 (dev only) \
         to boot with a loud ephemeral key."
    )]
    MissingMasterKey,

    #[error(
        "SCARAB_MASTER_KEY is set but invalid — want base64 of exactly 32 bytes \
         (`head -c 32 /dev/urandom | base64`). A malformed key is a misconfiguration, \
         not an opt-out, so this fails even under SCARAB_DEV_INSECURE=1."
    )]
    InvalidMasterKey,

    #[error(
        "SCARAB_S3_BUCKET is set but SCARAB_S3_ACCESS_KEY / SCARAB_S3_SECRET_KEY are \
         empty (ADR-0048). Empty credentials would fail at first use, not at boot — \
         set both, or unset SCARAB_S3_BUCKET and pass an explicit --object-dir /
         SCARAB_OBJECT_DIR (a local-dir store, tests/dev)."
    )]
    MissingS3Credentials,

    #[error(
        "no object store configured (ADR-0067 part 1: Postgres AND an object \
         store are hard requirements — warm-only is not a deployment mode). Set \
         SCARAB_S3_BUCKET (+ endpoint/credentials) for S3/MinIO, or pass an \
         explicit --object-dir / SCARAB_OBJECT_DIR for a local-directory store \
         (tests/dev). There is no silent ./.scarab/objects fallback any more: a \
         store nobody chose is a store nobody is watching."
    )]
    MissingObjectStore,

    #[error(
        "SCARAB_OIDC_ISSUER is set but SCARAB_OIDC_SIGNING_KEY_FILE is not (ADR-0048). \
         Without a persistent signing key the JWKS changes every boot and cloud OIDC \
         federation silently breaks on restart/replica. Generate one \
         (`openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048`) and point \
         SCARAB_OIDC_SIGNING_KEY_FILE at it, or unset SCARAB_OIDC_ISSUER."
    )]
    MissingOidcSigningKey,

    #[error(
        "no authenticator configured (ADR-0048 auth default-deny). A server that can \
         authenticate no one must not pretend to be up. Configure OAuth/OIDC login \
         (SCARAB_OAUTH_CLIENT_ID/CLIENT_SECRET/AUTHORIZE_URL/TOKEN_URL/USERINFO_URL, \
         ADR-0049) or set SCARAB_DEV_INSECURE=1 (dev only — every caller is Owner)."
    )]
    NoAuthenticator,

    #[error(
        "OAuth login is partially configured (ADR-0049) — missing: {0}. All five \
         SCARAB_OAUTH_* endpoints/credentials must be set together (a partial \
         provider would fail at first login, not at boot)."
    )]
    PartialOAuth(String),

    #[error(
        "SCARAB_RETENTION_LOG_DAYS is set but invalid — want a positive integer number \
         of days (terminal runs' log TTL, ADR-0050)."
    )]
    InvalidRetention,

    #[error(
        "SCARAB_STEP_TIMEOUT_SECS is set but invalid — want a positive integer number \
         of seconds (the global default step deadline, ADR-0047)."
    )]
    InvalidStepTimeout,

    #[error(
        "SCARAB_CAS_CONCURRENCY is set but invalid — want a positive integer number of \
         in-flight object-store round-trips per workspace-CAS leg (ADR-0061 s2; default \
         32). Falling back to the default would silently serve a throughput the \
         operator did not ask for, which is exactly the kind of quiet substitution \
         SCARAB_STEP_TIMEOUT_SECS already refuses."
    )]
    InvalidCasConcurrency,

    #[error(
        "the declarative `connections:` block is invalid (ADR-0060 part D): {0}\n\
         A connection Scarab cannot construct would fail at first webhook, not at boot \
         (ADR-0048), so this refuses to start. Fix SCARAB_CONNECTIONS[_FILE] \
         (Helm: scarab.connections) and redeploy."
    )]
    InvalidConnections(String),

    #[error(
        "--role workspace is set but SCARAB_WORKSPACE_TOKEN_SECRET is not (ADR-0048/0061). \
         The workspace service serves Workspace Snapshots — every byte of every step's \
         inputs — so without a token secret it would serve them to any unauthenticated \
         caller. Set the same secret the control plane mints tokens with, or drop \
         --role workspace. SCARAB_DEV_INSECURE does NOT relax this: it covers missing \
         authenticators, not an open data plane."
    )]
    MissingWorkspaceTokenSecret,
}

impl Config {
    /// Resolve and validate the boot configuration from the parsed CLI plus the
    /// process environment. The only place the server reads `SCARAB_*` env.
    pub fn resolve(cli: &Cli) -> Result<Self, ConfigError> {
        Self::resolve_from(cli, |key| std::env::var(key).ok())
    }

    /// [`resolve`](Self::resolve) with an injectable environment (tests).
    fn resolve_from(cli: &Cli, env: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        // Postgres first: mandatory for EVERY role, and deliberately NOT
        // relaxed by the dev escape hatch (ADR-0048). The ADR-0061 workspace
        // carve-out ended with ADR-0067 part 2 — the Depot now connects to the
        // same database for its derived rows (drain records, write ledgers),
        // so a workspace replica without a URL is misconfigured, not minimal.
        // It still never migrates; that half of the boundary lives in
        // `workspaced::run`, not here.
        let database_url = match cli.database_url.clone().filter(|u| !u.is_empty()) {
            Some(url) => url,
            None => return Err(ConfigError::MissingDatabaseUrl),
        };

        let dev_insecure = matches!(
            env("SCARAB_DEV_INSECURE").as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("True")
        );

        // KEK: a *missing* key is downgradable to a loud ephemeral (dev); a
        // *malformed* key is a misconfiguration and always refuses.
        let master_key = match env("SCARAB_MASTER_KEY").filter(|v| !v.is_empty()) {
            Some(b64) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|_| ConfigError::InvalidMasterKey)?;
                let key: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| ConfigError::InvalidMasterKey)?;
                Some(key)
            }
            None if dev_insecure => None,
            // Same carve-out, same reason: the workspace service decrypts
            // nothing. It never constructs a `SecretProvider`, so a KEK there
            // would be a key nothing uses.
            None if !cli.role.needs_durable_core() => None,
            None => return Err(ConfigError::MissingMasterKey),
        };

        let store = match env("SCARAB_S3_BUCKET") {
            Some(bucket) => {
                let s3 = S3Config {
                    bucket,
                    endpoint: env("SCARAB_S3_ENDPOINT").unwrap_or_default(),
                    region: env("SCARAB_S3_REGION").unwrap_or_else(|| "us-east-1".into()),
                    access_key: env("SCARAB_S3_ACCESS_KEY").unwrap_or_default(),
                    secret_key: env("SCARAB_S3_SECRET_KEY").unwrap_or_default(),
                };
                // Enabled-but-unsafe, and not a security downgrade the dev
                // hatch covers: refuse regardless of SCARAB_DEV_INSECURE.
                if s3.access_key.is_empty() || s3.secret_key.is_empty() {
                    return Err(ConfigError::MissingS3Credentials);
                }
                StoreConfig::S3(s3)
            }
            // No bucket: a local-dir store is still a valid bucket stand-in
            // (tests, dev loops) but it must be CHOSEN, never defaulted
            // (ADR-0067 part 1). An empty value is "unset", matching how the
            // chart renders absent keys.
            None => match cli.object_dir.as_deref().filter(|d| !d.is_empty()) {
                Some(dir) => StoreConfig::LocalDir(dir.to_string()),
                None => return Err(ConfigError::MissingObjectStore),
            },
        };

        let oidc = match env("SCARAB_OIDC_ISSUER").filter(|v| !v.is_empty()) {
            Some(issuer_url) => Some(OidcConfig {
                issuer_url,
                signing_key_file: env("SCARAB_OIDC_SIGNING_KEY_FILE")
                    .filter(|v| !v.is_empty())
                    .ok_or(ConfigError::MissingOidcSigningKey)?,
                audience: env("SCARAB_OIDC_AUDIENCE")
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "scarab".into()),
            }),
            None => None,
        };

        // OAuth/OIDC login (ADR-0049 C1). All five knobs or none: a partial
        // provider is a misconfiguration, never silently ignored. The optional
        // `SCARAB_OAUTH_ISSUER` (ADR-0049 amendment) is a *mode* on top of those
        // five, so setting it alone is partial config too — never a silent no-op.
        let oauth_keys = [
            "SCARAB_OAUTH_CLIENT_ID",
            "SCARAB_OAUTH_CLIENT_SECRET",
            "SCARAB_OAUTH_AUTHORIZE_URL",
            "SCARAB_OAUTH_TOKEN_URL",
            "SCARAB_OAUTH_USERINFO_URL",
        ];
        let oauth_vals: Vec<Option<String>> = oauth_keys
            .iter()
            .map(|k| env(k).filter(|v| !v.is_empty()))
            .collect();
        let oauth = if oauth_vals.iter().all(Option::is_some) {
            Some(OAuthConfig {
                client_id: oauth_vals[0].clone().unwrap(),
                client_secret: oauth_vals[1].clone().unwrap(),
                authorize_url: oauth_vals[2].clone().unwrap(),
                token_url: oauth_vals[3].clone().unwrap(),
                userinfo_url: oauth_vals[4].clone().unwrap(),
                // Optional sixth knob, deliberately OUTSIDE the all-five-or-none
                // set: absent = plain OAuth2 (GitHub), present = OIDC mode with
                // id_token verification (ADR-0049 amendment).
                oidc_issuer: env("SCARAB_OAUTH_ISSUER").filter(|v| !v.is_empty()),
                scopes: env("SCARAB_OAUTH_SCOPES").unwrap_or_default(),
                owners: env("SCARAB_OAUTH_OWNERS")
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect(),
            })
        } else if oauth_vals.iter().any(Option::is_some)
            || env("SCARAB_OAUTH_ISSUER")
                .filter(|v| !v.is_empty())
                .is_some()
        {
            let missing: Vec<&str> = oauth_keys
                .iter()
                .zip(&oauth_vals)
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| *k)
                .collect();
            return Err(ConfigError::PartialOAuth(missing.join(", ")));
        } else {
            None
        };

        // Auth default-deny (ADR-0048): a boot that can authenticate no one
        // must not pretend to be up — OAuth login (ADR-0049) or the loud
        // dev escape hatch. Scoped to the roles that serve the human-facing
        // surface: the workspace role's authenticator is the fence-scoped
        // workspace token (SCARAB_WORKSPACE_TOKEN_SECRET — its own refusal
        // below, so this is not "authenticates no one"), it mounts no OAuth
        // login route, and an OAuth config there would configure nothing.
        if oauth.is_none() && !dev_insecure && !matches!(cli.role, Role::Workspace) {
            return Err(ConfigError::NoAuthenticator);
        }

        let step_timeout_secs = match env("SCARAB_STEP_TIMEOUT_SECS").filter(|v| !v.is_empty()) {
            Some(v) => v
                .parse::<u32>()
                .ok()
                .filter(|s| *s > 0)
                .ok_or(ConfigError::InvalidStepTimeout)?,
            None => 3_600,
        };

        // ADR-0061 s2's CAS-leg parallelism. Read here, not in the adapter: an
        // ambient env read there could neither fail the boot nor be reported.
        let cas_concurrency = match env(scarab_storage_s3::CAS_CONCURRENCY_ENV)
            .filter(|v| !v.trim().is_empty())
        {
            Some(v) => v
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or(ConfigError::InvalidCasConcurrency)?,
            None => scarab_storage_s3::DEFAULT_CAS_CONCURRENCY,
        };

        // Declarative connections (ADR-0060 part D). Parsed, validated AND
        // credential-resolved here: the whole point of the block is that a boot
        // either has working config-owned connections or refuses.
        let connections = connections_from_env(&env)?;

        // Workspace service (ADR-0061). Selected by its token secret, mirroring
        // the results-egress knob. `--role workspace` without one would serve
        // Workspace Snapshots to any unauthenticated caller, so that
        // combination refuses the boot rather than starting an open service.
        let workspace = match env("SCARAB_WORKSPACE_TOKEN_SECRET").filter(|v| !v.is_empty()) {
            Some(secret) => Some(WorkspaceServiceConfig {
                token_secret: secret.into_bytes(),
                url: env("SCARAB_WORKSPACE_URL")
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "http://scarab-workspace".into()),
                data_dir: env("SCARAB_WORKSPACE_DATA_DIR")
                    .filter(|v| !v.is_empty())
                    // Dev default beside the object dir; the chart sets
                    // /var/lib/scarab/cas on the PV.
                    .unwrap_or_else(|| "./.scarab/workspace-cas".into()),
                fetcher_image: env("SCARAB_WSFETCH_IMAGE")
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| {
                        scarab_executor_k8s::DEFAULT_WSFETCH_IMAGE.to_string()
                    }),
            }),
            None if matches!(cli.role, Role::Workspace) => {
                return Err(ConfigError::MissingWorkspaceTokenSecret)
            }
            None => None,
        };

        let results_egress = env("SCARAB_RESULTS_TOKEN_SECRET").map(|secret| ResultsEgressConfig {
            token_secret: secret.into_bytes(),
            api_url: env("SCARAB_RESULTS_API_URL").unwrap_or_else(|| "http://scarab-server".into()),
            sidecar_image: env("SCARAB_SIDECAR_IMAGE")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "ghcr.io/thulasi-ram/scarab-results-sidecar:edge".into()),
        });

        Ok(Config {
            role: cli.role,
            addr: cli.addr.clone(),
            database_url,
            namespace: cli.namespace.clone(),
            executor: cli.executor,
            store,
            results_egress,
            workspace,
            github_webhook_secret: env("SCARAB_GITHUB_WEBHOOK_SECRET").map(String::into_bytes),
            forgejo_webhook_secret: env("SCARAB_FORGEJO_WEBHOOK_SECRET").map(String::into_bytes),
            gate_token_secret: env("SCARAB_GATE_TOKEN_SECRET").map(String::into_bytes),
            oidc,
            master_key,
            dev_insecure,
            step_timeout_secs,
            public_url: env("SCARAB_PUBLIC_URL")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "http://localhost:8080".into()),
            github_app_id: env("SCARAB_GITHUB_APP_ID").filter(|v| !v.is_empty()),
            github_app_pem: env("SCARAB_GITHUB_APP_PEM").filter(|v| !v.is_empty()),
            github_app_pem_file: env("SCARAB_GITHUB_APP_PEM_FILE").filter(|v| !v.is_empty()),
            clone_image: env("SCARAB_CLONE_IMAGE")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "ghcr.io/thulasi-ram/scarab-clone:edge".into()),
            placement_config_file: env("SCARAB_PLACEMENT_CONFIG_FILE").filter(|v| !v.is_empty()),
            oauth,
            retention_log_days: match env("SCARAB_RETENTION_LOG_DAYS").filter(|v| !v.is_empty()) {
                Some(v) => v
                    .parse::<u32>()
                    .ok()
                    .filter(|d| *d > 0)
                    .ok_or(ConfigError::InvalidRetention)?,
                None => 30,
            },
            retention_artifact_days: match env("SCARAB_RETENTION_ARTIFACT_DAYS")
                .filter(|v| !v.is_empty())
            {
                Some(v) => v
                    .parse::<u32>()
                    .ok()
                    .filter(|d| *d > 0)
                    .ok_or(ConfigError::InvalidRetention)?,
                None => 90,
            },
            retention_workspace_days: match env("SCARAB_RETENTION_WORKSPACE_DAYS")
                .filter(|v| !v.is_empty())
            {
                Some(v) => v
                    .parse::<u32>()
                    .ok()
                    .filter(|d| *d > 0)
                    .ok_or(ConfigError::InvalidRetention)?,
                None => 14,
            },
            cas_concurrency,
            connections,
        })
    }

    /// The loud boot warnings the dev escape hatch trades hard-fails for
    /// (ADR-0048): insecure is opt-in and screaming, never silent.
    pub fn boot_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.dev_insecure {
            warnings.push(
                "⚠ AUTH DISABLED — no authenticator; ALL callers are treated as Owner \
                 (SCARAB_DEV_INSECURE=1, dev only)"
                    .to_string(),
            );
            if self.master_key.is_none() {
                warnings.push(
                    "⚠ EPHEMERAL SECRET KEY — SCARAB_MASTER_KEY unset; secrets written this \
                     boot CANNOT be decrypted after a restart (SCARAB_DEV_INSECURE=1, dev only)"
                        .to_string(),
                );
            }
        }
        warnings
    }

    /// The startup report (ADR-0048): one line per subsystem, enabled/disabled,
    /// logged on boot so the operational posture is legible on line one.
    /// Degraded states discovered while wiring (e.g. no cluster reachable) are
    /// logged by the composition root as they occur.
    pub fn startup_report(&self) -> Vec<String> {
        let store = match &self.store {
            StoreConfig::S3(s3) if s3.endpoint.is_empty() => {
                format!("s3 bucket={} region={}", s3.bucket, s3.region)
            }
            StoreConfig::S3(s3) => {
                format!("s3 bucket={} endpoint={}", s3.bucket, s3.endpoint)
            }
            StoreConfig::LocalDir(dir) => format!("local dir={dir}"),
        };
        let on_off = |enabled: bool| if enabled { "enabled" } else { "disabled" };
        vec![
            format!("role: {:?}", self.role),
            format!("addr: {}", self.addr),
            match self.role {
                Role::Workspace => format!(
                    "database: {} (connects, never migrates — ADR-0067 part 2)",
                    redact_url(&self.database_url)
                ),
                _ => format!(
                    "database: {} (mandatory, ADR-0048)",
                    redact_url(&self.database_url)
                ),
            },
            format!("object store: {store}"),
            format!(
                "cas leg concurrency: {} in-flight round-trips{} (ADR-0061 s2; \
                 peak memory ≈ concurrency × largest blob)",
                self.cas_concurrency,
                if self.cas_concurrency == scarab_storage_s3::DEFAULT_CAS_CONCURRENCY {
                    " (default)"
                } else {
                    " (SCARAB_CAS_CONCURRENCY)"
                },
            ),
            format!(
                "executor: {:?} (namespace={}, driver {})",
                self.executor,
                self.namespace,
                if self.role.runs_driver() {
                    "on"
                } else {
                    "off — role does not drive"
                },
            ),
            format!(
                "step timeout default: {}s (ADR-0047; per-step `timeout:` overrides)",
                self.step_timeout_secs,
            ),
            format!("public url: {} (run deep-links, ADR-0046)", self.public_url),
            format!("clone image: {} (ADR-0045)", self.clone_image),
            format!(
                "forge: registry-routed; github auth {} (ADR-0046)",
                match &self.github_app_id {
                    Some(id) => format!("App mode (app id {id})"),
                    None => "token mode (dev; set SCARAB_GITHUB_APP_ID for App auth)".into(),
                },
            ),
            format!(
                "secrets store: enabled (envelope encryption, ADR-0014; KEK {})",
                if self.master_key.is_some() {
                    "persistent"
                } else {
                    "EPHEMERAL"
                },
            ),
            format!(
                "auth: {}",
                match (&self.oauth, self.dev_insecure) {
                    (Some(o), _) => format!(
                        "OAuth/OIDC login enabled (authorize={}, {} owner(s), ADR-0049)",
                        o.authorize_url,
                        o.owners.len(),
                    ),
                    (None, true) =>
                        "DISABLED — SCARAB_DEV_INSECURE (all callers are Owner)".to_string(),
                    (None, false) => "enabled".to_string(),
                },
            ),
            format!(
                "results egress: {} (ADR-0042)",
                on_off(self.results_egress.is_some())
            ),
            match (&self.workspace, self.role) {
                (Some(ws), Role::Workspace) => format!(
                    "workspace service: SERVING (ADR-0061; warm tier {}, cold = object store above)",
                    ws.data_dir,
                ),
                (Some(ws), _) => format!("workspace service: client at {} (ADR-0061)", ws.url),
                (None, _) => "workspace service: disabled (ADR-0061)".to_string(),
            },
            format!(
                "github webhook: {}",
                on_off(self.github_webhook_secret.is_some())
            ),
            format!(
                "forgejo webhook: {}",
                on_off(self.forgejo_webhook_secret.is_some())
            ),
            format!(
                "gate release tokens: {}",
                on_off(self.gate_token_secret.is_some())
            ),
            match &self.oidc {
                Some(o) => format!(
                    "oidc issuer: {} (signing key: {})",
                    o.issuer_url, o.signing_key_file,
                ),
                None => "oidc issuer: disabled".to_string(),
            },
        ]
        .into_iter()
        // Declarative connections (ADR-0060 part D): the report names each one
        // and where its credential comes from — never the material — so
        // "which connections does config own?" is answerable from line one.
        .chain(std::iter::once(format!(
            "config-owned connections: {} (ADR-0060 part D; authoritative, read-only in the UI)",
            if self.connections.is_empty() {
                "none".to_string()
            } else {
                self.connections.len().to_string()
            },
        )))
        .chain(self.connections.iter().map(|c| {
            format!(
                "  connection {}: {} {} (credential: {}) → {} repo(s)",
                c.id,
                c.kind.as_str(),
                c.base_url,
                c.credential_source,
                c.repos.len(),
            )
        }))
        .collect()
    }
}

/// Read, parse, validate and credential-resolve the declarative `connections:`
/// block (ADR-0060 part D).
///
/// Inline `SCARAB_CONNECTIONS` wins over `SCARAB_CONNECTIONS_FILE`, mirroring
/// `SCARAB_GITHUB_APP_PEM` vs `..._FILE` — the precedent this generalizes.
/// Every failure mode is a boot refusal (ADR-0048): an unreadable file, a parse
/// error, an unknown field, an invalid entry, or credential material the
/// deployment promised (`env:`/`file:`) but did not deliver.
///
/// Public with an injectable environment so the block can be exercised end-to-end
/// (YAML → specs → registry → API) without mutating the process environment.
pub fn connections_from_env(
    env: &impl Fn(&str) -> Option<String>,
) -> Result<Vec<ConnectionSpec>, ConfigError> {
    let (raw, source) = match env("SCARAB_CONNECTIONS").filter(|v| !v.trim().is_empty()) {
        Some(inline) => (inline, "SCARAB_CONNECTIONS".to_string()),
        None => match env("SCARAB_CONNECTIONS_FILE").filter(|v| !v.is_empty()) {
            Some(path) => {
                let raw = std::fs::read_to_string(&path).map_err(|e| {
                    ConfigError::InvalidConnections(format!(
                        "cannot read SCARAB_CONNECTIONS_FILE {path}: {e}"
                    ))
                })?;
                (raw, format!("SCARAB_CONNECTIONS_FILE {path}"))
            }
            None => return Ok(Vec::new()),
        },
    };

    // YAML is a superset of JSON, so one parser accepts both shapes.
    let doc: RawConnectionsDoc = serde_yaml::from_str(&raw)
        .map_err(|e| ConfigError::InvalidConnections(format!("{source}: {e}")))?;

    let mut out: Vec<ConnectionSpec> = Vec::with_capacity(doc.connections.len());
    for raw_conn in doc.connections {
        let spec = resolve_connection(raw_conn, env)
            .map_err(|e| ConfigError::InvalidConnections(format!("{source}: {e}")))?;
        // Duplicate ids would race each other into the same row on every boot,
        // with the last one silently winning.
        if out.iter().any(|s| s.id == spec.id) {
            return Err(ConfigError::InvalidConnections(format!(
                "{source}: connection id `{}` is declared twice",
                spec.id
            )));
        }
        out.push(spec);
    }
    Ok(out)
}

/// Validate one raw entry into a fully-resolved [`ConnectionSpec`]. Errors are
/// plain messages; the caller prefixes the source.
fn resolve_connection(
    raw: RawConnection,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<ConnectionSpec, String> {
    let id = raw.id.trim().to_string();
    if id.is_empty() {
        return Err("a connection needs a non-empty `id`".into());
    }
    // The id is a durable primary key that also appears in URLs and log fields.
    if id.chars().any(|c| c.is_whitespace()) {
        return Err(format!("connection id `{id}` must not contain whitespace"));
    }
    let kind = scarab_forge::ForgeKind::from_str_token(raw.kind.trim()).ok_or_else(|| {
        format!(
            "connection `{id}`: unknown kind `{}` — want `github` or `forgejo` \
             (a kind selects the adapter crate, ADR-0046)",
            raw.kind
        )
    })?;
    let base_url = raw.base_url.trim().trim_end_matches('/').to_string();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(format!(
            "connection `{id}`: base_url `{base_url}` must be an absolute http(s) URL"
        ));
    }

    // Exactly one credential source. Zero means the connection can never
    // authenticate; more than one reintroduces a precedence question.
    let declared: Vec<&str> = [
        raw.credential.env.as_ref().map(|_| "env"),
        raw.credential.file.as_ref().map(|_| "file"),
        raw.credential.secret_ref.as_ref().map(|_| "secret_ref"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if declared.len() != 1 {
        return Err(format!(
            "connection `{id}`: credential must declare exactly one of \
             `env` / `file` / `secret_ref` (found {})",
            if declared.is_empty() {
                "none".to_string()
            } else {
                declared.join(" + ")
            }
        ));
    }

    let (credential_ref, credential_material, credential_source) = match (
        raw.credential.env,
        raw.credential.file,
        raw.credential.secret_ref,
    ) {
        // env-override: the deployment supplies the material directly. An empty
        // or absent var is a broken promise, so it refuses the boot rather than
        // authenticating with "" at first use.
        (Some(var), _, _) => {
            let var = var.trim().to_string();
            let material = env(&var).filter(|v| !v.trim().is_empty()).ok_or_else(|| {
                format!(
                    "connection `{id}`: credential env var `{var}` is not set (or empty). \
                     The config promises this connection's credential comes from the \
                     environment; booting without it would fail at the first webhook."
                )
            })?;
            (
                id.clone(),
                Some(Redacted::new(material)),
                format!("env {var}"),
            )
        }
        // file-override: same contract, mounted-file shape (external-secrets /
        // sealed-secrets / SOPS), mirroring SCARAB_GITHUB_APP_PEM_FILE.
        (None, Some(path), _) => {
            let path = path.trim().to_string();
            let material = std::fs::read_to_string(&path).map_err(|e| {
                format!("connection `{id}`: cannot read credential file {path}: {e}")
            })?;
            if material.trim().is_empty() {
                return Err(format!(
                    "connection `{id}`: credential file {path} is empty"
                ));
            }
            (
                id.clone(),
                Some(Redacted::new(material)),
                format!("file {path}"),
            )
        }
        // No override: a `SecretProvider` handle, resolved at use-time exactly
        // as a DB-owned connection's is. Deliberately NOT a boot failure when
        // absent — the running server is the only way to PUT it, so refusing
        // here would deadlock a fresh database. The startup audit reports it
        // DEGRADED instead.
        (None, None, Some(secret_ref)) => {
            let secret_ref = secret_ref.trim().to_string();
            if secret_ref.is_empty() {
                return Err(format!("connection `{id}`: credential secret_ref is empty"));
            }
            let source = format!("secret {secret_ref}");
            (secret_ref, None, source)
        }
        (None, None, None) => unreachable!("guarded by the exactly-one check above"),
    };

    let mut repos = Vec::with_capacity(raw.repos.len());
    for entry in &raw.repos {
        let entry = entry.trim();
        let (owner, name) = entry.split_once('/').ok_or_else(|| {
            format!("connection `{id}`: repo `{entry}` must be written `owner/name`")
        })?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return Err(format!(
                "connection `{id}`: repo `{entry}` must be written `owner/name`"
            ));
        }
        repos.push(scarab_forge::RepoRef {
            owner: owner.to_string(),
            name: name.to_string(),
        });
    }

    Ok(ConnectionSpec {
        id,
        kind,
        base_url,
        credential_ref,
        credential_material,
        credential_source,
        repos,
    })
}

/// Redact the password in a connection URL for logging
/// (`postgres://user:***@host/db`).
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((userinfo, host)) = rest.split_once('@') else {
        return url.to_string();
    };
    match userinfo.split_once(':') {
        Some((user, _pw)) => format!("{scheme}://{user}:***@{host}"),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(database_url: Option<&str>) -> Cli {
        Cli {
            role: Role::Converged,
            dry_run: false,
            serve: false,
            addr: "0.0.0.0:8080".into(),
            database_url: database_url.map(String::from),
            object_dir: Some("./.scarab/objects".into()),
            namespace: "scarab".into(),
            executor: ExecutorKind::K8s,
            emit_openapi: None,
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A valid base64 32-byte master key for tests.
    fn key_b64() -> String {
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    }

    /// Env with the security knobs satisfied (master key set, dev flag off →
    /// still refuses on auth until ADR-0049; use `dev_env` to boot).
    fn dev_env(extra: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            if k == "SCARAB_DEV_INSECURE" {
                return Some("1".to_string());
            }
            extra
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn missing_database_url_refuses_to_boot() {
        let err = Config::resolve_from(&cli(None), no_env).unwrap_err();
        assert_eq!(err, ConfigError::MissingDatabaseUrl);
        // The operator-facing message says what to fix and names the carve-out.
        let msg = err.to_string();
        assert!(msg.contains("SCARAB_DATABASE_URL"), "{msg}");
        assert!(msg.contains("--emit-openapi"), "{msg}");
    }

    #[test]
    fn empty_database_url_also_refuses() {
        let err = Config::resolve_from(&cli(Some("")), no_env).unwrap_err();
        assert_eq!(err, ConfigError::MissingDatabaseUrl);
    }

    #[test]
    fn dev_insecure_does_not_relax_the_database_requirement() {
        let err = Config::resolve_from(&cli(None), dev_env(&[])).unwrap_err();
        assert_eq!(err, ConfigError::MissingDatabaseUrl);
    }

    /// ADR-0067 part 2: the workspace role now REQUIRES Postgres (it reads and
    /// writes the drain-record and write-ledger rows), while the KEK carve-out
    /// survives — the role still decrypts nothing. The env here deliberately
    /// does NOT set `SCARAB_DEV_INSECURE`, so a missing master key passing is
    /// the carve-out itself, not the dev escape hatch.
    #[test]
    fn the_workspace_role_requires_postgres_but_not_a_kek() {
        let mut no_db = cli(None);
        no_db.role = Role::Workspace;
        let ws_env = |k: &str| {
            (k == "SCARAB_WORKSPACE_TOKEN_SECRET").then(|| "ws-secret".to_string())
        };
        assert_eq!(
            Config::resolve_from(&no_db, ws_env).unwrap_err(),
            ConfigError::MissingDatabaseUrl,
            "ADR-0067 part 2: a Depot without Postgres is misconfigured"
        );

        let mut cli = cli(Some("postgres://scarab:pw@db/scarab"));
        cli.role = Role::Workspace;
        let config = Config::resolve_from(&cli, ws_env)
            .expect("the data-plane role needs Postgres but no KEK");
        assert_eq!(config.database_url, "postgres://scarab:pw@db/scarab");
        assert!(config.master_key.is_none());
        assert_eq!(
            config.workspace.as_ref().map(|w| w.token_secret.clone()),
            Some(b"ws-secret".to_vec())
        );
        // And the report states the narrowed boundary out loud.
        assert!(config
            .startup_report()
            .iter()
            .any(|l| l.contains("never migrates")));
    }

    /// Every role requires Postgres — the workspace role included since
    /// ADR-0067 part 2. If `Api` ever passes without one, the ADR-0048
    /// "no API-only mode" rule has been quietly repealed.
    #[test]
    fn every_role_requires_postgres() {
        for role in [
            Role::Converged,
            Role::Api,
            Role::Scheduler,
            Role::Executor,
            Role::Webhook,
            Role::Workspace,
        ] {
            let mut cli = cli(None);
            cli.role = role;
            assert_eq!(
                Config::resolve_from(&cli, dev_env(&[])).unwrap_err(),
                ConfigError::MissingDatabaseUrl,
                "{role:?} must require Postgres"
            );
        }
    }

    /// A workspace service with no token secret would serve every step's inputs
    /// to anyone who can reach the port. That is not a dev convenience.
    #[test]
    fn the_workspace_role_refuses_to_boot_without_a_token_secret() {
        let mut cli = cli(Some("postgres://l/scarab"));
        cli.role = Role::Workspace;
        assert_eq!(
            Config::resolve_from(&cli, dev_env(&[])).unwrap_err(),
            ConfigError::MissingWorkspaceTokenSecret,
        );
    }

    /// The workspace token secret is NOT the results-egress secret: sharing one
    /// would turn a results-write credential into a content read+write
    /// credential and let the workspace service forge step results.
    #[test]
    fn the_workspace_and_results_secrets_are_separate_knobs() {
        let config = Config::resolve_from(
            &cli(Some("postgres://l/scarab")),
            dev_env(&[("SCARAB_RESULTS_TOKEN_SECRET", "results-secret")]),
        )
        .unwrap();
        assert!(config.results_egress.is_some());
        assert!(
            config.workspace.is_none(),
            "the results secret must not enable the workspace service"
        );
    }

    #[test]
    fn missing_master_key_refuses_without_the_dev_flag() {
        let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), no_env).unwrap_err();
        assert_eq!(err, ConfigError::MissingMasterKey);
    }

    #[test]
    fn missing_master_key_boots_ephemeral_under_dev_insecure_with_loud_warnings() {
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), dev_env(&[])).unwrap();
        assert!(cfg.master_key.is_none());
        let warnings = cfg.boot_warnings().join("\n");
        assert!(warnings.contains("AUTH DISABLED"), "{warnings}");
        assert!(warnings.contains("EPHEMERAL SECRET KEY"), "{warnings}");
    }

    #[test]
    fn github_app_pem_env_and_file_populate_for_bootstrap_free_auth() {
        // enh 245a99c: the App PEM can be supplied at boot (inline or via a
        // mounted file), so a fresh DB / GitOps deploy needs no reseed PUT.
        let cfg = Config::resolve_from(
            &cli(Some("postgres://l/scarab")),
            dev_env(&[
                ("SCARAB_GITHUB_APP_ID", "12345"),
                (
                    "SCARAB_GITHUB_APP_PEM",
                    "-----BEGIN RSA PRIVATE KEY-----\nk\n-----END RSA PRIVATE KEY-----",
                ),
                ("SCARAB_GITHUB_APP_PEM_FILE", "/etc/scarab/app.pem"),
            ]),
        )
        .unwrap();
        assert_eq!(cfg.github_app_id.as_deref(), Some("12345"));
        assert!(cfg
            .github_app_pem
            .as_deref()
            .unwrap()
            .contains("BEGIN RSA PRIVATE KEY"));
        assert_eq!(
            cfg.github_app_pem_file.as_deref(),
            Some("/etc/scarab/app.pem")
        );
    }

    #[test]
    fn github_app_pem_is_absent_by_default() {
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), dev_env(&[])).unwrap();
        assert!(cfg.github_app_pem.is_none());
        assert!(cfg.github_app_pem_file.is_none());
    }

    #[test]
    fn invalid_master_key_refuses_even_under_dev_insecure() {
        let env = |k: &str| match k {
            "SCARAB_DEV_INSECURE" => Some("1".to_string()),
            "SCARAB_MASTER_KEY" => Some("not-base64!!".to_string()),
            _ => None,
        };
        let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap_err();
        assert_eq!(err, ConfigError::InvalidMasterKey);

        // Right length prefix but wrong byte count also refuses.
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        let env = move |k: &str| match k {
            "SCARAB_DEV_INSECURE" => Some("1".to_string()),
            "SCARAB_MASTER_KEY" => Some(short.clone()),
            _ => None,
        };
        let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap_err();
        assert_eq!(err, ConfigError::InvalidMasterKey);
    }

    #[test]
    fn no_authenticator_and_no_dev_flag_refuses_to_boot() {
        let k = key_b64();
        let env = move |key: &str| (key == "SCARAB_MASTER_KEY").then(|| k.clone());
        let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap_err();
        assert_eq!(err, ConfigError::NoAuthenticator);
    }

    #[test]
    fn s3_bucket_with_empty_creds_refuses_even_under_dev_insecure() {
        let env = dev_env(&[("SCARAB_S3_BUCKET", "scarab-logs")]);
        let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap_err();
        assert_eq!(err, ConfigError::MissingS3Credentials);
    }

    #[test]
    fn oidc_issuer_without_signing_key_refuses() {
        let env = dev_env(&[("SCARAB_OIDC_ISSUER", "https://scarab.example")]);
        let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap_err();
        assert_eq!(err, ConfigError::MissingOidcSigningKey);

        let env = dev_env(&[
            ("SCARAB_OIDC_ISSUER", "https://scarab.example"),
            ("SCARAB_OIDC_SIGNING_KEY_FILE", "/etc/scarab/oidc.pem"),
        ]);
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap();
        let oidc = cfg.oidc.expect("oidc enabled");
        assert_eq!(oidc.signing_key_file, "/etc/scarab/oidc.pem");
    }

    #[test]
    fn an_explicit_object_dir_resolves_to_a_local_store_and_no_optional_features() {
        // `cli()` passes --object-dir explicitly; that is the ONLY way a
        // LocalDir store exists since ADR-0067 part 1.
        let cfg =
            Config::resolve_from(&cli(Some("postgres://u:pw@localhost/scarab")), dev_env(&[]))
                .unwrap();
        assert!(matches!(cfg.store, StoreConfig::LocalDir(ref d) if d == "./.scarab/objects"));
        assert!(cfg.results_egress.is_none());
        assert!(cfg.github_webhook_secret.is_none());
        assert!(cfg.gate_token_secret.is_none());
        assert!(cfg.oidc.is_none());
    }

    /// ADR-0067 part 1: the object store is a hard requirement. No bucket and
    /// no explicit local dir is a refusal at the gate — never a silent
    /// `./.scarab/objects` that nobody chose.
    #[test]
    fn no_bucket_and_no_object_dir_refuses_the_boot() {
        let mut c = cli(Some("postgres://u:pw@localhost/scarab"));
        c.object_dir = None;
        let err = Config::resolve_from(&c, dev_env(&[])).unwrap_err();
        assert_eq!(err, ConfigError::MissingObjectStore);

        // An empty value is "unset", matching how the chart renders absent
        // keys — not a LocalDir("") that fails at first use.
        c.object_dir = Some(String::new());
        let err = Config::resolve_from(&c, dev_env(&[])).unwrap_err();
        assert_eq!(err, ConfigError::MissingObjectStore);
    }

    /// `SCARAB_CAS_CONCURRENCY` (ADR-0061 s2) is a real knob, not an ambient env
    /// read in the adapter: it defaults, it is reported, and — the part that was
    /// wrong — a junk value FAILS THE BOOT instead of silently substituting 32.
    /// An operator who typos this knob is asking for a specific throughput; the
    /// old fallback answered "sure" and served a different one.
    #[test]
    fn cas_concurrency_defaults_is_reported_and_refuses_junk() {
        let base = cli(Some("postgres://l/scarab"));

        let cfg = Config::resolve_from(&base, dev_env(&[])).unwrap();
        assert_eq!(
            cfg.cas_concurrency,
            scarab_storage_s3::DEFAULT_CAS_CONCURRENCY
        );
        // Reported, so the live value is answerable from the boot log alone.
        let report = cfg.startup_report().join("\n");
        assert!(
            report.contains("cas leg concurrency: 32 in-flight round-trips (default)"),
            "the default must be in the startup report: {report}"
        );

        let cfg = Config::resolve_from(&base, dev_env(&[("SCARAB_CAS_CONCURRENCY", " 96 ")]))
            .expect("a padded integer is still an integer");
        assert_eq!(cfg.cas_concurrency, 96);
        assert!(cfg
            .startup_report()
            .join("\n")
            .contains("cas leg concurrency: 96 in-flight round-trips (SCARAB_CAS_CONCURRENCY)"));

        // Empty is "unset" (a Helm `casConcurrency: ""` renders no key, but an
        // explicit empty env var must not be a boot failure either).
        assert_eq!(
            Config::resolve_from(&base, dev_env(&[("SCARAB_CAS_CONCURRENCY", "")]))
                .unwrap()
                .cas_concurrency,
            scarab_storage_s3::DEFAULT_CAS_CONCURRENCY
        );

        // Junk and zero both refuse. Zero especially: `with_concurrency` clamps
        // it to 1, i.e. the serial behaviour ADR-0061 s2 exists to remove — a
        // 30× slowdown is not a reasonable reading of a typo.
        for vars in [
            &[("SCARAB_CAS_CONCURRENCY", "nonsense")] as &'static [(&str, &str)],
            &[("SCARAB_CAS_CONCURRENCY", "0")],
            &[("SCARAB_CAS_CONCURRENCY", "-4")],
            &[("SCARAB_CAS_CONCURRENCY", "32.5")],
        ] {
            let bad = vars[0].1;
            let err = Config::resolve_from(&base, dev_env(vars))
                .expect_err("junk CAS concurrency must refuse the boot");
            assert!(
                matches!(err, ConfigError::InvalidCasConcurrency),
                "{bad:?} gave {err:?}"
            );
        }
    }

    #[test]
    fn s3_bucket_selects_s3_store() {
        let env = dev_env(&[
            ("SCARAB_S3_BUCKET", "scarab-logs"),
            ("SCARAB_S3_ENDPOINT", "http://127.0.0.1:9000"),
            ("SCARAB_S3_ACCESS_KEY", "k"),
            ("SCARAB_S3_SECRET_KEY", "s"),
        ]);
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap();
        match cfg.store {
            StoreConfig::S3(s3) => {
                assert_eq!(s3.bucket, "scarab-logs");
                assert_eq!(s3.endpoint, "http://127.0.0.1:9000");
                assert_eq!(s3.region, "us-east-1"); // default
            }
            other => panic!("expected S3 store, got {other:?}"),
        }
    }

    #[test]
    fn results_egress_needs_only_the_token_secret() {
        let env = dev_env(&[("SCARAB_RESULTS_TOKEN_SECRET", "hmac")]);
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap();
        let egress = cfg.results_egress.expect("egress enabled");
        assert_eq!(egress.token_secret, b"hmac");
        assert_eq!(egress.api_url, "http://scarab-server"); // default
    }

    #[test]
    fn startup_report_redacts_the_database_password() {
        let cfg = Config::resolve_from(
            &cli(Some("postgres://scarab:hunter2@db:5432/scarab")),
            dev_env(&[]),
        )
        .unwrap();
        let report = cfg.startup_report().join("\n");
        assert!(!report.contains("hunter2"), "{report}");
        assert!(
            report.contains("postgres://scarab:***@db:5432/scarab"),
            "{report}"
        );
        // The insecure posture is visible in the report, not hidden.
        assert!(report.contains("auth: DISABLED"), "{report}");
        assert!(report.contains("KEK EPHEMERAL"), "{report}");
    }

    // --- declarative connections (ADR-0060 part D) --------------------------
    //
    // The block is the IaC half of "a connection has exactly one owner", so its
    // parsing is a *gate*, not a convenience: every one of these cases would
    // otherwise surface as a connection that silently cannot authenticate.

    const FORGEJO_BLOCK: &str = r#"
connections:
  - id: forgejo-main
    kind: forgejo
    base_url: https://git.example.com/
    credential:
      env: FORGEJO_CI_TOKEN
    repos:
      - acme/widgets
      - acme/gadgets
"#;

    #[test]
    fn no_connections_declared_by_default() {
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), dev_env(&[])).unwrap();
        assert!(cfg.connections.is_empty());
    }

    #[test]
    fn config_declared_connection_resolves_its_credential_from_an_env_var() {
        let cfg = Config::resolve_from(
            &cli(Some("postgres://l/scarab")),
            dev_env(&[
                ("SCARAB_CONNECTIONS", FORGEJO_BLOCK),
                ("FORGEJO_CI_TOKEN", "tok-abc123"),
            ]),
        )
        .unwrap();
        let conn = &cfg.connections[0];
        assert_eq!(conn.id, "forgejo-main");
        assert_eq!(conn.kind, scarab_forge::ForgeKind::Forgejo);
        // The trailing slash is normalized away — the adapter joins paths onto it.
        assert_eq!(conn.base_url, "https://git.example.com");
        // The material is in hand: no SecretProvider round-trip will be needed.
        assert_eq!(
            conn.credential_material.as_ref().map(Redacted::expose),
            Some("tok-abc123")
        );
        assert_eq!(conn.credential_source, "env FORGEJO_CI_TOKEN");
        assert_eq!(
            conn.repos,
            vec![
                scarab_forge::RepoRef {
                    owner: "acme".into(),
                    name: "widgets".into()
                },
                scarab_forge::RepoRef {
                    owner: "acme".into(),
                    name: "gadgets".into()
                },
            ]
        );
    }

    #[test]
    fn credential_material_never_appears_in_debug_or_the_startup_report() {
        let cfg = Config::resolve_from(
            &cli(Some("postgres://l/scarab")),
            dev_env(&[
                ("SCARAB_CONNECTIONS", FORGEJO_BLOCK),
                ("FORGEJO_CI_TOKEN", "tok-abc123"),
            ]),
        )
        .unwrap();
        // `Config` is Debug and gets logged/panicked with; the token must not
        // ride along.
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("tok-abc123"), "{debug}");
        let report = cfg.startup_report().join("\n");
        assert!(!report.contains("tok-abc123"), "{report}");
        // What the operator DOES get: which connections config owns, and where
        // each credential comes from.
        assert!(report.contains("config-owned connections: 1"), "{report}");
        assert!(
            report.contains("connection forgejo-main: forgejo https://git.example.com"),
            "{report}"
        );
        assert!(
            report.contains("credential: env FORGEJO_CI_TOKEN"),
            "{report}"
        );
        assert!(report.contains("2 repo(s)"), "{report}");
    }

    #[test]
    fn a_promised_credential_env_var_that_is_unset_refuses_to_boot() {
        // The whole value of the declarative path is that a deploy either has
        // working connections or does not come up (ADR-0048). Booting with an
        // empty token would fail at the first webhook instead.
        let err = Config::resolve_from(
            &cli(Some("postgres://l/scarab")),
            dev_env(&[("SCARAB_CONNECTIONS", FORGEJO_BLOCK)]),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::InvalidConnections(_)), "{msg}");
        assert!(msg.contains("FORGEJO_CI_TOKEN"), "{msg}");
        assert!(msg.contains("forgejo-main"), "{msg}");
    }

    #[test]
    fn an_empty_credential_env_var_is_treated_as_unset() {
        let err = Config::resolve_from(
            &cli(Some("postgres://l/scarab")),
            dev_env(&[
                ("SCARAB_CONNECTIONS", FORGEJO_BLOCK),
                ("FORGEJO_CI_TOKEN", "   "),
            ]),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidConnections(_)), "{err}");
    }

    #[test]
    fn a_secret_ref_credential_carries_no_override_material() {
        // The other half of the one resolution path: no override, so the handle
        // is resolved from SecretProvider at use-time exactly as a DB-owned
        // connection's is. Absence there is DEGRADED, not a boot refusal — only
        // the running server can PUT it.
        let cfg = Config::resolve_from(
            &cli(Some("postgres://l/scarab")),
            dev_env(&[(
                "SCARAB_CONNECTIONS",
                r#"
connections:
  - id: forgejo-main
    kind: forgejo
    base_url: https://git.example.com
    credential:
      secret_ref: forgejo-token
"#,
            )]),
        )
        .unwrap();
        let conn = &cfg.connections[0];
        assert!(conn.credential_material.is_none());
        assert_eq!(conn.credential_ref, "forgejo-token");
        assert_eq!(conn.credential_source, "secret forgejo-token");
    }

    #[test]
    fn a_connection_needs_exactly_one_credential_source() {
        for (block, expect) in [
            (
                r#"
connections:
  - id: c
    kind: forgejo
    base_url: https://git.example.com
    credential: {}
"#,
                "none",
            ),
            (
                r#"
connections:
  - id: c
    kind: forgejo
    base_url: https://git.example.com
    credential:
      env: A
      secret_ref: b
"#,
                "env + secret_ref",
            ),
        ] {
            let env = move |k: &str| match k {
                "SCARAB_DEV_INSECURE" => Some("1".to_string()),
                "SCARAB_CONNECTIONS" => Some(block.to_string()),
                _ => None,
            };
            let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap_err();
            let msg = err.to_string();
            assert!(matches!(err, ConfigError::InvalidConnections(_)), "{msg}");
            assert!(msg.contains(expect), "{msg}");
        }
    }

    #[test]
    fn invalid_entries_refuse_to_boot_with_the_offending_value_named() {
        // Each case is a misconfiguration that would otherwise become a
        // connection that cannot work, or (for the typo) be silently ignored.
        let cases: [(&str, &str); 4] = [
            (
                r#"
connections:
  - id: c
    kind: gitlob
    base_url: https://git.example.com
    credential: { secret_ref: t }
"#,
                "gitlob",
            ),
            (
                r#"
connections:
  - id: c
    kind: forgejo
    base_url: git.example.com
    credential: { secret_ref: t }
"#,
                "absolute http(s) URL",
            ),
            (
                r#"
connections:
  - id: c
    kind: forgejo
    base_url: https://git.example.com
    credential: { secret_ref: t }
    repos: [ widgets ]
"#,
                "owner/name",
            ),
            (
                r#"
connections:
  - id: c
    kind: forgejo
    base_urls: https://git.example.com
    credential: { secret_ref: t }
"#,
                "base_urls",
            ),
        ];
        for (block, expect) in cases {
            let env = move |k: &str| match k {
                "SCARAB_DEV_INSECURE" => Some("1".to_string()),
                "SCARAB_CONNECTIONS" => Some(block.to_string()),
                _ => None,
            };
            let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap_err();
            let msg = err.to_string();
            assert!(matches!(err, ConfigError::InvalidConnections(_)), "{msg}");
            assert!(msg.contains(expect), "want {expect:?} in: {msg}");
        }
    }

    #[test]
    fn a_duplicate_connection_id_refuses_to_boot() {
        // Two entries with one id would race into the same row every boot, last
        // one silently winning — the drift the single-owner rule forbids.
        let err = Config::resolve_from(
            &cli(Some("postgres://l/scarab")),
            dev_env(&[(
                "SCARAB_CONNECTIONS",
                r#"
connections:
  - id: dup
    kind: forgejo
    base_url: https://a.example.com
    credential: { secret_ref: t }
  - id: dup
    kind: forgejo
    base_url: https://b.example.com
    credential: { secret_ref: t }
"#,
            )]),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("declared twice"), "{msg}");
    }

    #[test]
    fn the_block_can_come_from_a_file_and_inline_wins_over_it() {
        let dir = std::env::temp_dir().join(format!("scarab-conn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connections.yaml");
        std::fs::write(
            &path,
            r#"
connections:
  - id: from-file
    kind: forgejo
    base_url: https://file.example.com
    credential: { secret_ref: t }
"#,
        )
        .unwrap();
        let path_s = path.to_string_lossy().to_string();

        // File only: parsed (the GitOps shape — a mounted ConfigMap).
        let p = path_s.clone();
        let env = move |k: &str| match k {
            "SCARAB_DEV_INSECURE" => Some("1".to_string()),
            "SCARAB_CONNECTIONS_FILE" => Some(p.clone()),
            _ => None,
        };
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap();
        assert_eq!(cfg.connections[0].id, "from-file");

        // Inline wins, mirroring SCARAB_GITHUB_APP_PEM vs ..._FILE.
        let p = path_s.clone();
        let env = move |k: &str| match k {
            "SCARAB_DEV_INSECURE" => Some("1".to_string()),
            "SCARAB_CONNECTIONS_FILE" => Some(p.clone()),
            "SCARAB_CONNECTIONS" => Some(
                r#"
connections:
  - id: from-inline
    kind: forgejo
    base_url: https://inline.example.com
    credential: { secret_ref: t }
"#
                .to_string(),
            ),
            _ => None,
        };
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap();
        assert_eq!(cfg.connections.len(), 1);
        assert_eq!(cfg.connections[0].id, "from-inline");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_connections_file_refuses_to_boot() {
        // Mirrors SCARAB_GITHUB_APP_PEM_FILE / the OIDC signing key: a path that
        // does not exist is a misconfiguration, not "no connections".
        let env = dev_env(&[("SCARAB_CONNECTIONS_FILE", "/nope/connections.yaml")]);
        let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::InvalidConnections(_)), "{msg}");
        assert!(msg.contains("/nope/connections.yaml"), "{msg}");
    }

    #[test]
    fn a_file_credential_is_read_at_boot_and_a_bad_path_refuses() {
        let dir = std::env::temp_dir().join(format!("scarab-cred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "file-token\n").unwrap();
        let block = format!(
            r#"
connections:
  - id: forgejo-main
    kind: forgejo
    base_url: https://git.example.com
    credential:
      file: {}
"#,
            path.to_string_lossy()
        );
        let b = block.clone();
        let env = move |k: &str| match k {
            "SCARAB_DEV_INSECURE" => Some("1".to_string()),
            "SCARAB_CONNECTIONS" => Some(b.clone()),
            _ => None,
        };
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap();
        assert_eq!(
            cfg.connections[0]
                .credential_material
                .as_ref()
                .map(Redacted::expose),
            Some("file-token\n")
        );

        let env = dev_env(&[(
            "SCARAB_CONNECTIONS",
            r#"
connections:
  - id: forgejo-main
    kind: forgejo
    base_url: https://git.example.com
    credential:
      file: /nope/token
"#,
        )]);
        let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap_err();
        assert!(err.to_string().contains("/nope/token"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR-0049 amendment: `SCARAB_OAUTH_ISSUER` is an optional *mode* on top of
    /// the five-knob provider — with them it turns on id_token verification;
    /// alone it is partial config, never a silent no-op. Owners parse as
    /// subject-or-email entries.
    #[test]
    fn oauth_issuer_is_optional_but_never_silently_ignored() {
        const FIVE: &[(&str, &str)] = &[
            ("SCARAB_OAUTH_CLIENT_ID", "cid"),
            ("SCARAB_OAUTH_CLIENT_SECRET", "sekret"),
            ("SCARAB_OAUTH_AUTHORIZE_URL", "https://idp.example/authorize"),
            ("SCARAB_OAUTH_TOKEN_URL", "https://idp.example/token"),
            ("SCARAB_OAUTH_USERINFO_URL", "https://idp.example/userinfo"),
            ("SCARAB_OAUTH_OWNERS", " ada@example.com ,8f3c-opaque "),
        ];
        const WITH_ISSUER: &[(&str, &str)] = &[
            ("SCARAB_OAUTH_CLIENT_ID", "cid"),
            ("SCARAB_OAUTH_CLIENT_SECRET", "sekret"),
            ("SCARAB_OAUTH_AUTHORIZE_URL", "https://idp.example/authorize"),
            ("SCARAB_OAUTH_TOKEN_URL", "https://idp.example/token"),
            ("SCARAB_OAUTH_USERINFO_URL", "https://idp.example/userinfo"),
            ("SCARAB_OAUTH_ISSUER", "https://dex.example/"),
        ];
        const ISSUER_ONLY: &[(&str, &str)] = &[("SCARAB_OAUTH_ISSUER", "https://dex.example")];

        // Plain OAuth2 (the GitHub shape): no issuer, so no id_token is trusted.
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), dev_env(FIVE)).unwrap();
        let oauth = cfg.oauth.as_ref().expect("five knobs enable login");
        assert_eq!(oauth.oidc_issuer, None);
        assert_eq!(oauth.owners, vec!["ada@example.com", "8f3c-opaque"]);

        // OIDC mode.
        let cfg =
            Config::resolve_from(&cli(Some("postgres://l/scarab")), dev_env(WITH_ISSUER)).unwrap();
        assert_eq!(
            cfg.oauth.unwrap().oidc_issuer.as_deref(),
            Some("https://dex.example/")
        );

        // The issuer alone cannot quietly do nothing.
        let err = Config::resolve_from(&cli(Some("postgres://l/scarab")), dev_env(ISSUER_ONLY))
            .unwrap_err();
        let msg = err.to_string();
        for key in [
            "SCARAB_OAUTH_CLIENT_ID",
            "SCARAB_OAUTH_CLIENT_SECRET",
            "SCARAB_OAUTH_AUTHORIZE_URL",
            "SCARAB_OAUTH_TOKEN_URL",
            "SCARAB_OAUTH_USERINFO_URL",
        ] {
            assert!(msg.contains(key), "{msg}");
        }
    }

    #[test]
    fn no_warnings_without_the_dev_flag_path() {
        // A dev boot with a persistent key still warns about auth, but not KEK.
        let k = key_b64();
        let env = move |key: &str| match key {
            "SCARAB_DEV_INSECURE" => Some("1".to_string()),
            "SCARAB_MASTER_KEY" => Some(k.clone()),
            _ => None,
        };
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap();
        let warnings = cfg.boot_warnings().join("\n");
        assert!(warnings.contains("AUTH DISABLED"), "{warnings}");
        assert!(!warnings.contains("EPHEMERAL SECRET KEY"), "{warnings}");
    }
}
