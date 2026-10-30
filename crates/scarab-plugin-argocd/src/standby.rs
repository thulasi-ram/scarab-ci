//! What the plugin process does while Scarab has no work for it.
//!
//! ADR-0074 removed the Deployment record, and with it this plugin's reporter
//! loop. Until the plugin has actions to answer, its process only proves,
//! every [`Config::check_interval`], the two things it will need: the broker
//! knows who it is ([`Broker::whoami`]), and Argo CD answers through the
//! broker's read proxy (`GET /api/version`, a `manifest.json` read rule).
//! Each check is logged; a failure is logged and retried at the next check,
//! never fatal — a killed installation answers 403 until the runtime scales
//! it to zero. An installation that resolves gates also answers its
//! `wait-live` waits ([`crate::gates`], ADR-0076 §7).

use tokio::sync::watch;

use crate::broker::{Broker, BrokerError};
use crate::config::Config;
use crate::gates::{resolve_pass, GateApi, PassReport};

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

/// Check now and every `cfg.check_interval` until `stop` turns true. With
/// an api token ([`Config::api_token_file`]), also answer the open
/// `wait-live` waits every `cfg.gate_interval` ([`crate::gates`]).
pub async fn run(cfg: Config, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    let broker = Broker::new(&cfg.broker_url, cfg.token_file.clone(), cfg.request_timeout)?;
    let api = cfg
        .api_token_file
        .clone()
        .map(|file| GateApi::new(&cfg.broker_url, file, cfg.request_timeout))
        .transpose()?;
    let tick = match &api {
        Some(_) => cfg.gate_interval.min(cfg.check_interval),
        None => cfg.check_interval,
    };
    let mut last_check: Option<std::time::Instant> = None;
    loop {
        if last_check.is_none_or(|t| t.elapsed() >= cfg.check_interval) {
            last_check = Some(std::time::Instant::now());
            match check(&broker).await {
                Ok(c) => tracing::info!(
                    installation = %c.installation,
                    argocd_version = %c.argocd_version,
                    "argocd: broker and Argo CD reachable"
                ),
                Err(e) => tracing::warn!(error = %e, status = ?e.status(), "argocd: check failed"),
            }
        }
        if let Some(api) = &api {
            match gates_once(&broker, api).await {
                Ok(r) => {
                    for e in &r.errors {
                        tracing::warn!(error = %e, "argocd: a gate wait was not answered");
                    }
                    if !(r.released.is_empty() && r.failed.is_empty()) {
                        tracing::info!(released = ?r.released, failed = ?r.failed, "argocd: gates answered");
                    }
                }
                Err(e) => tracing::warn!(error = %e, status = ?e.status(), "argocd: gate pass failed"),
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(tick) => {}
            _ = stop.changed() => {}
        }
        if *stop.borrow() {
            return Ok(());
        }
    }
}

/// One gate pass in the installation's own org, which the broker names.
pub async fn gates_once(broker: &Broker, api: &GateApi) -> Result<PassReport, BrokerError> {
    let me = broker.whoami().await?;
    resolve_pass(broker, api, &me.org).await
}
