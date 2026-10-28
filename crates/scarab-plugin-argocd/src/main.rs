//! `scarab-plugin-argocd`: the plugin process ([`scarab_plugin_argocd::standby`]).
//! Configuration is the Pod's environment
//! ([`scarab_plugin_argocd::Config::from_env`]); logs are JSON lines on
//! stdout (`RUST_LOG` filters, default `info`).
//!
//! SIGTERM (a kill switch scaling the Deployment to zero, an upgrade, a node
//! drain) stops the loop and exits 0. The process keeps no state.

use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = match scarab_plugin_argocd::Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(error = %e, "argocd: bad configuration");
            std::process::exit(2);
        }
    };
    tracing::info!(
        broker = %cfg.broker_url,
        token_file = %cfg.token_file.display(),
        "argocd: starting"
    );

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::info!("argocd: signal received; stopping");
        let _ = stop_tx.send(true);
    });

    if let Err(e) = scarab_plugin_argocd::run(cfg, stop_rx).await {
        tracing::error!(error = %e, "argocd: cannot run");
        std::process::exit(1);
    }
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
