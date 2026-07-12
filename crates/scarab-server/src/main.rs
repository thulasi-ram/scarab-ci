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
use scarab_executor_k8s::K8sExecutor;
use scarab_server::{converged, router, AppState, LogService, SystemClock};
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

#[derive(Debug, Parser)]
#[command(name = "scarab-server", about = "Scarab durable CI — server process")]
struct Cli {
    /// The role this process runs as.
    #[arg(long, value_enum, default_value_t = Role::Converged)]
    role: Role,

    /// Actually bind and serve HTTP. Without this the process reports its role
    /// and exits (so `--help` and smoke checks never hang).
    #[arg(long)]
    serve: bool,

    /// Address to bind when `--serve` is set.
    #[arg(long, default_value = "0.0.0.0:8080")]
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
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
    let db: Arc<dyn Db> = Arc::new(pg);

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

    // Background scheduler + executor loop. Best-effort kube connect: without a
    // cluster the driver is skipped and the process serves API-only.
    if cli.role.runs_driver() && connected {
        match K8sExecutor::connect(cli.namespace.clone()).await {
            Ok(exec) => {
                let executor: Arc<dyn Executor> = Arc::new(exec);
                // Forge-status posting is wired once GitHub App auth lands; until
                // then the driver ticks without a forge.
                converged::spawn_driver(
                    db.clone(),
                    clock.clone(),
                    executor,
                    None,
                    "scarab-server".to_string(),
                    Duration::from_millis(500),
                );
                tracing::info!("converged driver started");
            }
            Err(e) => {
                tracing::warn!(error = %e, "no kubernetes cluster reachable; driver not started (API-only)");
            }
        }
    }

    let mut state = AppState::new(db, clock, logs);
    if let Ok(secret) = std::env::var("SCARAB_GITHUB_WEBHOOK_SECRET") {
        state = state.with_github_webhook_secret(secret.into_bytes());
    }
    let app = router(state);

    if cli.serve {
        let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
        println!("listening on {}", cli.addr);
        axum::serve(listener, app).await?;
    } else {
        println!("(dry run — pass --serve to bind {})", cli.addr);
    }

    Ok(())
}
