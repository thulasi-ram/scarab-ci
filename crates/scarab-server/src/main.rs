//! # scarab-server — the API/worker process binary.
//!
//! One binary, selectable roles. The `converged` role runs everything in a
//! single process (great for dev / small installs); the other roles let an
//! operator scale each concern independently. This binary is a thin shell over
//! the [`scarab_server`] library, which owns the router and handlers. Connecting
//! Postgres/object-store and spawning the background scheduler loop land with
//! the converged-wiring slice.

use std::sync::Arc;

use clap::{Parser, ValueEnum};

use scarab_db_postgres::PostgresDb;
use scarab_server::{router, AppState, LogService, SystemClock};
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

    /// Actually bind and serve HTTP. Without this the process reports its role
    /// and exits (so `--help` and smoke checks never hang).
    #[arg(long)]
    serve: bool,

    /// Address to bind when `--serve` is set.
    #[arg(long, default_value = "0.0.0.0:8080")]
    addr: String,

    /// Postgres connection URL. When set, the durable store is connected;
    /// otherwise the process serves with an unconnected store (dev/smoke).
    #[arg(long, env = "SCARAB_DATABASE_URL")]
    database_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    tracing::info!(role = ?cli.role, "starting scarab-server");
    println!("scarab-server role = {:?}", cli.role);

    let pg = match &cli.database_url {
        Some(url) => {
            let db = PostgresDb::connect(url).await?;
            db.migrate().await?;
            db
        }
        None => PostgresDb::new(),
    };
    let db: Arc<dyn scarab_engine::Db> = Arc::new(pg);
    let store: Arc<dyn scarab_storage::ObjectStore> = Arc::new(S3Storage::new("scarab-logs"));
    let logs = Arc::new(LogService::new(store, db.clone()));
    let state = AppState::new(db, Arc::new(SystemClock), logs);
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
