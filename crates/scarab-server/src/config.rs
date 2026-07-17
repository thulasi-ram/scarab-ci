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
//! | `SCARAB_DATABASE_URL` | CLI `--database-url` | Postgres URL — **mandatory** for every serving role |
//! | `SCARAB_OBJECT_DIR` | CLI `--object-dir` | local object-store directory (dev) |
//! | `SCARAB_NAMESPACE` | CLI `--namespace` | k8s namespace for step Pods |
//! | `SCARAB_EXECUTOR` | CLI `--executor` | `k8s` (prod) or `local` (dev/CLI) |
//! | `SCARAB_S3_BUCKET` | env | selects S3/MinIO object store when set |
//! | `SCARAB_S3_ENDPOINT` | env | S3 endpoint (empty = AWS) |
//! | `SCARAB_S3_REGION` | env | S3 region (default `us-east-1`) |
//! | `SCARAB_S3_ACCESS_KEY` | env | S3 access key |
//! | `SCARAB_S3_SECRET_KEY` | env | S3 secret key |
//! | `SCARAB_RESULTS_TOKEN_SECRET` | env | enables results-egress sidecar + ingest (ADR-0042) |
//! | `SCARAB_RESULTS_API_URL` | env | base URL the sidecar posts results to |
//! | `SCARAB_SIDECAR_IMAGE` | env | results-egress sidecar image |
//! | `SCARAB_GITHUB_WEBHOOK_SECRET` | env | HMAC secret for `/webhooks/github` |
//! | `SCARAB_FORGEJO_WEBHOOK_SECRET` | env | HMAC secret for `/webhooks/forgejo` (ADR-0046 — each forge endpoint binds its own secret) |
//! | `SCARAB_GATE_TOKEN_SECRET` | env | enables external-gate release tokens (ADR-0034) |
//! | `SCARAB_OIDC_ISSUER` | env | enables the OIDC issuer (keyless federation, ADR-0015) |
//! | `SCARAB_OIDC_SIGNING_KEY_FILE` | env | PKCS#8 RSA PEM the issuer signs with — **required** when the issuer is enabled (persistent across restarts/replicas) |
//! | `SCARAB_MASTER_KEY` | env | base64 32-byte KEK for envelope encryption (ADR-0014) — **required** unless `SCARAB_DEV_INSECURE=1` |
//! | `SCARAB_DEV_INSECURE` | env | `1`/`true`: downgrade the **security** hard-fails (KEK, auth) to loud boot warnings — dev only, never relaxes the Postgres requirement |
//! | `SCARAB_STEP_TIMEOUT_SECS` | env | global default step deadline (ADR-0047); default 3600 (1h), per-step overridable via `timeout:` |
//! | `SCARAB_PUBLIC_URL` | env | Scarab's public base URL — the run deep-link every forge status carries (ADR-0046); default `http://localhost:8080` (dev) |
//! | `SCARAB_CLONE_IMAGE` | env | the canonical scarab-clone image a `clone` step runs (ADR-0045); default `ghcr.io/thulasi-ram/scarab-clone:edge` — digest-pin in production |
//! | `SCARAB_GITHUB_APP_ID` | env | GitHub App id (ADR-0046): when set, GitHub connections authenticate in **App mode** (their credential secret is the App PEM); absent = token mode (dev) |
//! | `SCARAB_OAUTH_CLIENT_ID` … `_CLIENT_SECRET`, `_AUTHORIZE_URL`, `_TOKEN_URL`, `_USERINFO_URL` | env | OAuth/OIDC login provider (ADR-0049): all five together enable real authn (GitHub, Forgejo, or any OIDC issuer); a partial set refuses boot |
//! | `SCARAB_OAUTH_SCOPES` | env | space-separated scopes for the authorize redirect (optional; e.g. `read:user` for GitHub, `openid profile` for OIDC) |
//! | `SCARAB_OAUTH_OWNERS` | env | comma-separated subjects granted `Owner` at login (bootstrap until scoped RBAC, ADR-0049 C2); everyone else logs in as `Viewer` |
//!
//! Step-runtime env (`SCARAB_RUN`/`SCARAB_STEP`/`SCARAB_ATTEMPT`/
//! `SCARAB_RESULTS*`/`SCARAB_PARAM_*`) is injected *into* step containers by
//! the executors and is not boot configuration; `SCARAB_SERVER`/`SCARAB_TOKEN`
//! belong to `scarab` (the CLI client).

use base64::Engine;
use clap::{Parser, ValueEnum};

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
}

impl Role {
    /// Roles that drive the scheduler + executor background loop.
    pub fn runs_driver(self) -> bool {
        matches!(self, Role::Converged | Role::Scheduler | Role::Executor)
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

    /// Local directory backing the object store (logs/artifacts). Swapped for
    /// S3/MinIO in production; a plain directory needs no extra service for dev.
    #[arg(long, env = "SCARAB_OBJECT_DIR", default_value = "./.scarab/objects")]
    pub object_dir: String,

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

/// OIDC issuer settings (selected by `SCARAB_OIDC_ISSUER`). The signing key is
/// mandatory: without a persistent key the JWKS changes every boot and cloud
/// federation silently breaks on restart/replica (ADR-0048).
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// The public base URL clouds are configured to trust.
    pub issuer_url: String,
    /// Path to the PKCS#8 RSA private-key PEM the issuer signs with.
    pub signing_key_file: String,
}

/// A validated boot configuration: if a `Config` exists, the process may
/// legitimately start. Construction is the startup gate (ADR-0048).
#[derive(Debug, Clone)]
pub struct Config {
    pub role: Role,
    pub addr: String,
    /// Always present — construction fails without it (no API-only mode).
    pub database_url: String,
    pub namespace: String,
    pub executor: ExecutorKind,
    pub store: StoreConfig,
    pub results_egress: Option<ResultsEgressConfig>,
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
    /// The canonical scarab-clone image (ADR-0045) — the image every `clone`
    /// step runs, digest-pinned in production.
    pub clone_image: String,
    /// OAuth/OIDC login (ADR-0049 C1). `Some` = real authn wired; `None` is
    /// only bootable under `SCARAB_DEV_INSECURE=1`.
    pub oauth: Option<OAuthConfig>,
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
    /// Space-separated scopes for the authorize redirect (may be empty).
    pub scopes: String,
    /// Subjects granted `Owner` at login (bootstrap until scoped RBAC, C2);
    /// everyone else authenticates as `Viewer`.
    pub owners: Vec<String>,
}

/// A configuration the process must refuse to start under, with a message
/// telling the operator exactly what to fix.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error(
        "SCARAB_DATABASE_URL is not set. Postgres is mandatory for every serving role \
         (ADR-0048) — there is no API-only mode. Start a Postgres (dev: `just up`) and \
         set SCARAB_DATABASE_URL; only `--emit-openapi` works without a database. \
         SCARAB_DEV_INSECURE does NOT relax this."
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
         set both, or unset SCARAB_S3_BUCKET to use the local object dir."
    )]
    MissingS3Credentials,

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
        "SCARAB_STEP_TIMEOUT_SECS is set but invalid — want a positive integer number \
         of seconds (the global default step deadline, ADR-0047)."
    )]
    InvalidStepTimeout,
}

impl Config {
    /// Resolve and validate the boot configuration from the parsed CLI plus the
    /// process environment. The only place the server reads `SCARAB_*` env.
    pub fn resolve(cli: &Cli) -> Result<Self, ConfigError> {
        Self::resolve_from(cli, |key| std::env::var(key).ok())
    }

    /// [`resolve`](Self::resolve) with an injectable environment (tests).
    fn resolve_from(cli: &Cli, env: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        // Postgres first: mandatory for every serving role, and deliberately
        // NOT relaxed by the dev escape hatch (ADR-0048).
        let database_url = cli
            .database_url
            .clone()
            .filter(|u| !u.is_empty())
            .ok_or(ConfigError::MissingDatabaseUrl)?;

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
                let key: [u8; 32] = bytes.try_into().map_err(|_| ConfigError::InvalidMasterKey)?;
                Some(key)
            }
            None if dev_insecure => None,
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
            None => StoreConfig::LocalDir(cli.object_dir.clone()),
        };

        let oidc = match env("SCARAB_OIDC_ISSUER").filter(|v| !v.is_empty()) {
            Some(issuer_url) => Some(OidcConfig {
                issuer_url,
                signing_key_file: env("SCARAB_OIDC_SIGNING_KEY_FILE")
                    .filter(|v| !v.is_empty())
                    .ok_or(ConfigError::MissingOidcSigningKey)?,
            }),
            None => None,
        };

        // OAuth/OIDC login (ADR-0049 C1). All five knobs or none: a partial
        // provider is a misconfiguration, never silently ignored.
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
                scopes: env("SCARAB_OAUTH_SCOPES").unwrap_or_default(),
                owners: env("SCARAB_OAUTH_OWNERS")
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect(),
            })
        } else if oauth_vals.iter().any(Option::is_some) {
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
        // dev escape hatch.
        if oauth.is_none() && !dev_insecure {
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

        let results_egress = env("SCARAB_RESULTS_TOKEN_SECRET").map(|secret| ResultsEgressConfig {
            token_secret: secret.into_bytes(),
            api_url: env("SCARAB_RESULTS_API_URL").unwrap_or_else(|| "http://scarab-server".into()),
            sidecar_image: env("SCARAB_SIDECAR_IMAGE")
                .unwrap_or_else(|| "ghcr.io/scarab/sidecar:latest".into()),
        });

        Ok(Config {
            role: cli.role,
            addr: cli.addr.clone(),
            database_url,
            namespace: cli.namespace.clone(),
            executor: cli.executor,
            store,
            results_egress,
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
            clone_image: env("SCARAB_CLONE_IMAGE")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "ghcr.io/thulasi-ram/scarab-clone:edge".into()),
            oauth,
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
            format!("database: {} (mandatory, ADR-0048)", redact_url(&self.database_url)),
            format!("object store: {store}"),
            format!(
                "executor: {:?} (namespace={}, driver {})",
                self.executor,
                self.namespace,
                if self.role.runs_driver() { "on" } else { "off — role does not drive" },
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
                if self.master_key.is_some() { "persistent" } else { "EPHEMERAL" },
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
            format!("results egress: {} (ADR-0042)", on_off(self.results_egress.is_some())),
            format!("github webhook: {}", on_off(self.github_webhook_secret.is_some())),
            format!("forgejo webhook: {}", on_off(self.forgejo_webhook_secret.is_some())),
            format!("gate release tokens: {}", on_off(self.gate_token_secret.is_some())),
            match &self.oidc {
                Some(o) => format!(
                    "oidc issuer: {} (signing key: {})",
                    o.issuer_url, o.signing_key_file,
                ),
                None => "oidc issuer: disabled".to_string(),
            },
        ]
    }
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
            object_dir: "./.scarab/objects".into(),
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
    fn defaults_resolve_to_local_store_and_no_optional_features() {
        let cfg =
            Config::resolve_from(&cli(Some("postgres://u:pw@localhost/scarab")), dev_env(&[]))
                .unwrap();
        assert!(matches!(cfg.store, StoreConfig::LocalDir(ref d) if d == "./.scarab/objects"));
        assert!(cfg.results_egress.is_none());
        assert!(cfg.github_webhook_secret.is_none());
        assert!(cfg.gate_token_secret.is_none());
        assert!(cfg.oidc.is_none());
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
        assert!(report.contains("postgres://scarab:***@db:5432/scarab"), "{report}");
        // The insecure posture is visible in the report, not hidden.
        assert!(report.contains("auth: DISABLED"), "{report}");
        assert!(report.contains("KEK EPHEMERAL"), "{report}");
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
