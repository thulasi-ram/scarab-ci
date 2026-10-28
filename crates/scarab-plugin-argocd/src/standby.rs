//! What the plugin process does while Scarab has no work for it.
//!
//! ADR-0074 removed the Deployment record, and with it this plugin's reporter
//! loop. Until the plugin has actions to answer, its process only proves,
//! every [`Config::check_interval`], the two things it will need: the broker
//! knows who it is ([`Broker::whoami`]), and Argo CD answers through the
//! broker's read proxy (`GET /api/version`, a `manifest.json` read rule).
//! Each check is logged; a failure is logged and retried at the next check,
//! never fatal — a killed installation answers 403 until the runtime scales
//! it to zero.

use tokio::sync::watch;

use crate::broker::{Broker, BrokerError};
use crate::config::Config;

/// What one check saw.
#[derive(Debug)]
pub struct Check {
    /// The installation the broker mapped this process's token to.
    pub installation: String,
    /// Argo CD's `/api/version` body, as read through the proxy.
    pub argocd_version: serde_json::Value,
}

/// One check: `whoami`, then Argo CD's version through the read proxy.
pub async fn check(broker: &Broker) -> Result<Check, BrokerError> {
    let me = broker.whoami().await?;
    let argocd_version = broker.argocd_get("/api/version", &[]).await?;
    Ok(Check {
        installation: me.name,
        argocd_version,
    })
}

/// Check now and every `cfg.check_interval` until `stop` turns true.
pub async fn run(cfg: Config, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    let broker = Broker::new(&cfg.broker_url, cfg.token_file.clone(), cfg.request_timeout)?;
    loop {
        match check(&broker).await {
            Ok(c) => tracing::info!(
                installation = %c.installation,
                argocd_version = %c.argocd_version,
                "argocd: broker and Argo CD reachable"
            ),
            Err(e) => tracing::warn!(error = %e, status = ?e.status(), "argocd: check failed"),
        }
        tokio::select! {
            _ = tokio::time::sleep(cfg.check_interval) => {}
            _ = stop.changed() => {}
        }
        if *stop.borrow() {
            return Ok(());
        }
    }
}
