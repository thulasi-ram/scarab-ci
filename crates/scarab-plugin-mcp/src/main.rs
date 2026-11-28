//! `scarab-plugin-mcp`: the plugin process ([`scarab_plugin_mcp::serve`]).
//! Configuration is the Pod's environment
//! ([`scarab_plugin_mcp::Config::from_env`]); logs are JSON lines on stdout
//! (`RUST_LOG` filters, default `info`).
//!
//! SIGTERM (a kill switch scaling the Deployment to zero, an upgrade, a node
//! drain) stops accepting requests, lets those in flight finish, and exits 0.
//! The process keeps no state.

use std::sync::Arc;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = match scarab_plugin_mcp::Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(error = %e, "mcp: bad configuration");
            std::process::exit(2);
        }
    };
    tracing::info!(
        broker = %cfg.broker_url,
        token_file = %cfg.api_token_file.display(),
        port = cfg.port,
        "mcp: starting"
    );
    let server = match scarab_plugin_mcp::McpServer::new(cfg) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!(error = %e, "mcp: cannot start");
            std::process::exit(1);
        }
    };
    if let Err(e) = scarab_plugin_mcp::serve(server, wait_for_signal()).await {
        tracing::error!(error = %e, "mcp: cannot serve");
        std::process::exit(1);
    }
    tracing::info!("mcp: stopped");
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
