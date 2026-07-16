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
//! | `SCARAB_GATE_TOKEN_SECRET` | env | enables external-gate release tokens (ADR-0034) |
//! | `SCARAB_OIDC_ISSUER` | env | enables the OIDC issuer (keyless federation, ADR-0015) |
//! | `SCARAB_MASTER_KEY` | env (read by `scarab-secrets-postgres`) | base64 32-byte KEK for envelope encryption (ADR-0014) |
//!
//! Step-runtime env (`SCARAB_RUN`/`SCARAB_STEP`/`SCARAB_ATTEMPT`/
//! `SCARAB_RESULTS*`/`SCARAB_PARAM_*`) is injected *into* step containers by
//! the executors and is not boot configuration; `SCARAB_SERVER`/`SCARAB_TOKEN`
//! belong to `scarab` (the CLI client).

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
    pub gate_token_secret: Option<Vec<u8>>,
    pub oidc_issuer: Option<String>,
}

/// A configuration the process must refuse to start under, with a message
/// telling the operator exactly what to fix.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error(
        "SCARAB_DATABASE_URL is not set. Postgres is mandatory for every serving role \
         (ADR-0048) — there is no API-only mode. Start a Postgres (dev: `just up`) and \
         set SCARAB_DATABASE_URL; only `--emit-openapi` works without a database."
    )]
    MissingDatabaseUrl,
}

impl Config {
    /// Resolve and validate the boot configuration from the parsed CLI plus the
    /// process environment. The only place the server reads `SCARAB_*` env.
    pub fn resolve(cli: &Cli) -> Result<Self, ConfigError> {
        Self::resolve_from(cli, |key| std::env::var(key).ok())
    }

    /// [`resolve`](Self::resolve) with an injectable environment (tests).
    fn resolve_from(cli: &Cli, env: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let database_url = cli
            .database_url
            .clone()
            .filter(|u| !u.is_empty())
            .ok_or(ConfigError::MissingDatabaseUrl)?;

        let store = match env("SCARAB_S3_BUCKET") {
            Some(bucket) => StoreConfig::S3(S3Config {
                bucket,
                endpoint: env("SCARAB_S3_ENDPOINT").unwrap_or_default(),
                region: env("SCARAB_S3_REGION").unwrap_or_else(|| "us-east-1".into()),
                access_key: env("SCARAB_S3_ACCESS_KEY").unwrap_or_default(),
                secret_key: env("SCARAB_S3_SECRET_KEY").unwrap_or_default(),
            }),
            None => StoreConfig::LocalDir(cli.object_dir.clone()),
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
            gate_token_secret: env("SCARAB_GATE_TOKEN_SECRET").map(String::into_bytes),
            oidc_issuer: env("SCARAB_OIDC_ISSUER"),
        })
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
            format!("secrets store: enabled (envelope encryption, ADR-0014)"),
            format!("results egress: {} (ADR-0042)", on_off(self.results_egress.is_some())),
            format!("github webhook: {}", on_off(self.github_webhook_secret.is_some())),
            format!("gate release tokens: {}", on_off(self.gate_token_secret.is_some())),
            format!(
                "oidc issuer: {}",
                self.oidc_issuer.as_deref().unwrap_or("disabled"),
            ),
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
    fn defaults_resolve_to_local_store_and_no_optional_features() {
        let cfg = Config::resolve_from(&cli(Some("postgres://u:pw@localhost/scarab")), no_env)
            .unwrap();
        assert!(matches!(cfg.store, StoreConfig::LocalDir(ref d) if d == "./.scarab/objects"));
        assert!(cfg.results_egress.is_none());
        assert!(cfg.github_webhook_secret.is_none());
        assert!(cfg.gate_token_secret.is_none());
        assert!(cfg.oidc_issuer.is_none());
    }

    #[test]
    fn s3_bucket_selects_s3_store() {
        let env = |k: &str| match k {
            "SCARAB_S3_BUCKET" => Some("scarab-logs".to_string()),
            "SCARAB_S3_ENDPOINT" => Some("http://127.0.0.1:9000".to_string()),
            "SCARAB_S3_ACCESS_KEY" => Some("k".to_string()),
            "SCARAB_S3_SECRET_KEY" => Some("s".to_string()),
            _ => None,
        };
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
        let env = |k: &str| {
            (k == "SCARAB_RESULTS_TOKEN_SECRET").then(|| "hmac".to_string())
        };
        let cfg = Config::resolve_from(&cli(Some("postgres://l/scarab")), env).unwrap();
        let egress = cfg.results_egress.expect("egress enabled");
        assert_eq!(egress.token_secret, b"hmac");
        assert_eq!(egress.api_url, "http://scarab-server"); // default
    }

    #[test]
    fn startup_report_redacts_the_database_password() {
        let cfg = Config::resolve_from(&cli(Some("postgres://scarab:hunter2@db:5432/scarab")), no_env)
            .unwrap();
        let report = cfg.startup_report().join("\n");
        assert!(!report.contains("hunter2"), "{report}");
        assert!(report.contains("postgres://scarab:***@db:5432/scarab"), "{report}");
    }
}
