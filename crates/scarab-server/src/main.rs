//! # scarab-server — the composition root and API/worker process.
//!
//! One binary, selectable roles. The `converged` role runs everything in a
//! single process (great for dev / small installs); the other roles let an
//! operator scale each concern independently. This is a compiling skeleton:
//! it wires the (stub) adapters behind the domain ports and exposes a minimal
//! HTTP surface.

use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use clap::{Parser, ValueEnum};

use scarab_engine::{Db, Executor};
use scarab_forge::ForgePort;
use scarab_secrets::SecretProvider;
use scarab_storage::ObjectStore;

use scarab_db_postgres::PostgresDb;
use scarab_executor_k8s::K8sExecutor;
use scarab_executor_local::LocalExecutor;
use scarab_forge_github::GithubForge;
use scarab_secrets_postgres::PostgresSecrets;
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

#[derive(Debug, Parser)]
#[command(name = "scarab-server", about = "Scarab durable CI — server process")]
struct Cli {
    /// The role this process runs as.
    #[arg(long, value_enum, default_value_t = Role::Converged)]
    role: Role,

    /// Actually bind and serve HTTP. Without this the process wires the
    /// composition root, reports its role, and exits (so `--help` and smoke
    /// checks never hang).
    #[arg(long)]
    serve: bool,

    /// Address to bind when `--serve` is set.
    #[arg(long, default_value = "0.0.0.0:8080")]
    addr: String,
}

/// The wired-up composition root: adapters bound behind domain ports.
#[allow(dead_code)]
struct Composition {
    db: Box<dyn Db>,
    secrets: Box<dyn SecretProvider>,
    forge: Box<dyn ForgePort>,
    store: Box<dyn ObjectStore>,
    executor: Box<dyn Executor>,
}

impl Composition {
    /// Construct the stub adapters. Constructors perform no I/O, so this is
    /// safe to call at startup regardless of role.
    fn wire(role: Role) -> Self {
        let executor: Box<dyn Executor> = match role {
            // The dedicated executor role targets Kubernetes.
            Role::Executor => Box::new(K8sExecutor::new("scarab")),
            // Everything else defaults to the local-process executor.
            _ => Box::new(LocalExecutor::new()),
        };

        Composition {
            db: Box::new(PostgresDb::new()),
            secrets: Box::new(PostgresSecrets::new()),
            forge: Box::new(GithubForge::new("<token>")),
            store: Box::new(S3Storage::new("scarab-artifacts")),
            executor,
        }
    }
}

/// Build the HTTP router.
fn router() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/runs", post(create_run))
}

/// Liveness probe.
async fn healthz() -> &'static str {
    "ok"
}

/// Enqueue a new run (stub).
async fn create_run() -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "run creation not yet implemented")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    tracing::info!(role = ?cli.role, "starting scarab-server");
    println!("scarab-server role = {:?}", cli.role);

    // Wire the composition root (stub adapters behind the ports).
    let _composition = Composition::wire(cli.role);

    let app = router();

    if cli.serve {
        let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
        println!("listening on {}", cli.addr);
        axum::serve(listener, app).await?;
    } else {
        println!("(dry run — pass --serve to bind {})", cli.addr);
    }

    Ok(())
}
