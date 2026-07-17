//! # scarab-server — the converged API/worker process binary.
//!
//! One binary, selectable roles (ADR-0016). `converged` runs the API plus the
//! scheduler + executor background loop in a single process — ideal for dev and
//! small installs; the same code scales out by running roles as separate
//! replicas, since Postgres (the outbox) is the coordination bus. This binary is
//! a thin composition root over the [`scarab_server`] library.

use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use scarab_db_postgres::PostgresDb;
use scarab_engine::{Clock, Db, Executor};
use scarab_executor_k8s::{K8sExecutor, ResultsEgress};
use scarab_executor_local::LocalExecutor;
use scarab_server::{converged, router, AppState, LogService, SecretInjectingExecutor, SystemClock};
use scarab_storage::ObjectStore;
use scarab_storage_s3::S3Storage;

/// Which slice(s) of Scarab this process should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Role {
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
    fn runs_driver(self) -> bool {
        matches!(self, Role::Converged | Role::Scheduler | Role::Executor)
    }
}

/// Which execution backend the driver uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ExecutorKind {
    /// Kubernetes: one Pod per step (production; ADR-0005).
    K8s,
    /// Local host processes — a dev/CLI backend for laptop runs without a
    /// cluster (ADR-0036). Never a production deployment mode.
    Local,
}

#[derive(Debug, Parser)]
#[command(name = "scarab-server", about = "Scarab durable CI — server process")]
struct Cli {
    /// The role this process runs as.
    #[arg(long, value_enum, env = "SCARAB_ROLE", default_value_t = Role::Converged)]
    role: Role,

    /// Report the resolved role and exit WITHOUT binding a socket. Serving is the
    /// default now; this is the escape hatch for smoke checks / image
    /// healthchecks that must not hang.
    #[arg(long)]
    dry_run: bool,

    /// Deprecated no-op: serving is the default. Kept so existing scripts and
    /// muscle memory (`--serve`) keep working.
    #[arg(long, hide = true)]
    serve: bool,

    /// Address to bind. Override per-environment via `SCARAB_ADDR` (e.g. a dev
    /// `.env.local` sets `127.0.0.1:8899`).
    #[arg(long, env = "SCARAB_ADDR", default_value = "0.0.0.0:8080")]
    addr: String,

    /// Postgres connection URL. When set, the durable store is connected and the
    /// background driver runs; otherwise the process serves API-only (dev/smoke).
    #[arg(long, env = "SCARAB_DATABASE_URL")]
    database_url: Option<String>,

    /// Local directory backing the object store (logs/artifacts). Swapped for
    /// S3/MinIO in production; a plain directory needs no extra service for dev.
    #[arg(long, env = "SCARAB_OBJECT_DIR", default_value = "./.scarab/objects")]
    object_dir: String,

    /// Kubernetes namespace the executor launches step Pods into.
    #[arg(long, env = "SCARAB_NAMESPACE", default_value = "scarab")]
    namespace: String,

    /// Execution backend for the driver. `k8s` (default, production) or `local`
    /// (host processes — a cluster-free dev/CLI loop, ADR-0036).
    #[arg(long, value_enum, env = "SCARAB_EXECUTOR", default_value_t = ExecutorKind::K8s)]
    executor: ExecutorKind,

    /// Write the generated OpenAPI document to this path and exit (client
    /// codegen / CI spec check). Does not connect to Postgres or serve.
    #[arg(long, value_name = "PATH")]
    emit_openapi: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // rustls 0.23 refuses to pick a process-level default CryptoProvider when
    // more than one backend is linked — and this binary links both: `ring`
    // (sqlx's `tls-rustls-ring`) and `aws-lc-rs` (pulled by object_store). Left
    // unset, the first TLS handshake — the kube client dialing the API server —
    // panics. Install `ring` explicitly (matching sqlx) before any TLS. The
    // Result is Err only if a provider is already installed; ignore it.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    // Spec export: write openapi.json and exit (no DB, no serving).
    if let Some(path) = &cli.emit_openapi {
        std::fs::write(path, scarab_server::openapi_json())?;
        println!("wrote OpenAPI document to {path}");
        return Ok(());
    }

    tracing::info!(role = ?cli.role, "starting scarab-server");
    println!("scarab-server role = {:?}", cli.role);

    // Durable store.
    let connected = cli.database_url.is_some();
    let pg = match &cli.database_url {
        Some(url) => {
            let db = PostgresDb::connect(url).await?;
            db.migrate().await?;
            db
        }
        None => PostgresDb::new(),
    };
    // Keep a typed handle so the same Postgres adapter can back both the `Db`
    // port and the `EnvironmentStore` port (it implements both).
    let pg = Arc::new(pg);
    let db: Arc<dyn Db> = pg.clone();

    // Object store: MinIO/S3 when SCARAB_S3_BUCKET is set (the dev harness /
    // prod), else a local directory (zero-dependency dev).
    let store: Arc<dyn ObjectStore> = match std::env::var("SCARAB_S3_BUCKET") {
        Ok(bucket) => {
            let endpoint = std::env::var("SCARAB_S3_ENDPOINT").unwrap_or_default();
            let region = std::env::var("SCARAB_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
            let key = std::env::var("SCARAB_S3_ACCESS_KEY").unwrap_or_default();
            let secret = std::env::var("SCARAB_S3_SECRET_KEY").unwrap_or_default();
            Arc::new(S3Storage::s3(bucket, &endpoint, &region, &key, &secret)?)
        }
        Err(_) => Arc::new(S3Storage::local(&cli.object_dir)?),
    };
    let logs = Arc::new(LogService::new(store, db.clone()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // Secrets store (envelope-encrypted, ADR-0014): built up-front — before the
    // driver — so the launch path can resolve and inject env-scoped secrets
    // (ADR-0037). Master key from SCARAB_MASTER_KEY. Only when connected.
    let secrets: Option<Arc<dyn scarab_secrets::SecretProvider>> =
        if let (true, Some(url)) = (connected, &cli.database_url) {
            let s = scarab_secrets_postgres::PostgresSecrets::connect(url).await?;
            s.migrate().await?;
            Some(Arc::new(s))
        } else {
            None
        };

    // Background scheduler + executor loop. The k8s backend best-effort connects
    // (without a cluster the driver is skipped, API-only); the local backend
    // needs nothing but a working host (dev/CLI, ADR-0036).
    // Results egress (ADR-0042): a shared HMAC secret both mints the k8s sidecar's
    // fence token and verifies it at the ingest endpoint. Absent = no capture.
    let results_token_secret = std::env::var("SCARAB_RESULTS_TOKEN_SECRET")
        .ok()
        .map(String::into_bytes);
    let results_egress = results_token_secret.as_ref().map(|secret| ResultsEgress {
        base_url: std::env::var("SCARAB_RESULTS_API_URL")
            .unwrap_or_else(|_| "http://scarab-server".to_string()),
        token_secret: secret.clone(),
        sidecar_image: std::env::var("SCARAB_SIDECAR_IMAGE")
            .unwrap_or_else(|_| "ghcr.io/scarab/sidecar:latest".to_string()),
    });

    if cli.role.runs_driver() && connected {
        let executor: Option<Arc<dyn Executor>> = match cli.executor {
            ExecutorKind::Local => {
                tracing::info!("using the local (host-process) executor — dev/CLI backend");
                Some(Arc::new(LocalExecutor::new()))
            }
            ExecutorKind::K8s => match K8sExecutor::connect(cli.namespace.clone()).await {
                Ok(mut exec) => {
                    if let Some(egress) = results_egress.clone() {
                        exec = exec.with_results_egress(egress);
                        tracing::info!("results egress sidecar enabled (ADR-0042)");
                    }
                    Some(Arc::new(exec))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "no kubernetes cluster reachable; driver not started (API-only)");
                    None
                }
            },
        };
        if let Some(executor) = executor {
            // Inject env-scoped secrets into deploy-run pods at launch (ADR-0037)
            // when a secrets store is wired; otherwise launch unchanged.
            let executor: Arc<dyn Executor> = match &secrets {
                Some(sec) => Arc::new(SecretInjectingExecutor::new(
                    executor,
                    db.clone(),
                    sec.clone(),
                    logs.clone(),
                )),
                None => executor,
            };
            // Local host processes finish instantly, so re-poll them quickly; k8s
            // Pods take time, so the default window avoids thundering re-claims.
            // A short window is safe either way — `launch` is idempotent (ADR-0021).
            let visibility_ms = match cli.executor {
                ExecutorKind::Local => 1_000,
                ExecutorKind::K8s => 30_000,
            };
            // Forge-status posting is wired once GitHub App auth lands; until then
            // the driver ticks without a forge.
            converged::spawn_driver(
                db.clone(),
                clock.clone(),
                executor,
                None,
                // Feed the log pipeline from the executor's live tail (ADR-0013).
                Some(logs.clone()),
                "scarab-server".to_string(),
                Duration::from_millis(500),
                visibility_ms,
            );
            tracing::info!("converged driver started");
        }
    }

    let mut state = AppState::new(db, clock, logs);
    if let Ok(secret) = std::env::var("SCARAB_GITHUB_WEBHOOK_SECRET") {
        state = state.with_github_webhook_secret(secret.into_bytes());
    }
    // Results ingest (ADR-0042): enables POST …/steps/:step/results for the
    // egress sidecar, verified with the same secret that minted its token.
    if let Some(secret) = results_token_secret {
        state = state.with_results_token_secret(secret);
    }
    // External-gate release tokens (ADR-0034): enables POST …/gates/:step/release.
    if let Ok(secret) = std::env::var("SCARAB_GATE_TOKEN_SECRET") {
        state = state.with_gate_token_secret(secret.into_bytes());
    }
    // Environments + deployment history: the Postgres adapter is the store. Only
    // wired when actually connected — an unconnected PG would fail at request
    // time. Enables /v1/environments/* and (with an EnvironmentStore present)
    // admission enforcement for env-targeting runs (ADR-0024).
    if connected {
        state = state.with_environments(pg.clone());
    }
    // Enable /v1/secrets management with the secrets store built above (ADR-0014).
    if let Some(sec) = &secrets {
        state = state.with_secrets(sec.clone());
    }
    // OIDC issuer for keyless federation (ADR-0014): serve JWKS + discovery so a
    // cloud provider can verify Scarab-minted tokens. The issuer URL is the
    // public base URL clouds are configured to trust; enabled only when set.
    if let Ok(issuer_url) = std::env::var("SCARAB_OIDC_ISSUER") {
        match scarab_server::oidc::Rs256Issuer::generate(issuer_url) {
            Ok(issuer) => {
                state = state.with_oidc(Arc::new(issuer));
                tracing::info!("OIDC issuer enabled");
            }
            Err(e) => tracing::warn!(error = %e, "failed to generate OIDC signing key; issuer disabled"),
        }
    }
    let app = router(state);

    // Serve by default; `--serve` still forces it, `--dry-run` opts out.
    let serve = cli.serve || !cli.dry_run;
    if serve {
        let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
        println!("listening on {}", cli.addr);
        axum::serve(listener, app).await?;
    } else {
        println!("(dry run — omit --dry-run to bind {})", cli.addr);
    }

    Ok(())
}
