//! The reporter's configuration, from its environment.
//!
//! Two kinds of variable reach a Plugin Pod (ADR-0072 §9, slice d2's
//! `plugin_env`): core's own, `SCARAB_BROKER_URL` and
//! `SCARAB_BROKER_TOKEN_FILE`, set last so no config value can re-aim them;
//! and the installation's approved `config_keys` (`manifest.json`):
//! `ARGOCD_UI_URL` and `OWNER_ANNOTATION`.

use std::path::PathBuf;
use std::time::Duration;

/// Where the kubelet projects the broker token (slice d2's
/// `BROKER_TOKEN_DIR`/`BROKER_TOKEN_FILE`), if the env does not say.
pub const DEFAULT_TOKEN_FILE: &str = "/var/run/secrets/scarab/broker/token";
/// The Application annotation that lists its owner Projects (owner default
/// 11), as `org/repo, org/repo`.
pub const DEFAULT_OWNER_ANNOTATION: &str = "scarab.io/owner-projects";

#[derive(Debug, Clone)]
pub struct Config {
    /// The broker, by IP (a Plugin has no DNS).
    pub broker_url: String,
    pub token_file: PathBuf,
    /// Argo CD's UI, for each report's link; unset = no link.
    pub argocd_ui_url: Option<String>,
    pub owner_annotation: String,
    /// How long changes to one Application are coalesced before it is
    /// assessed: Argo rewrites an Application several times a second during
    /// a sync, and each write is a stream event.
    pub debounce: Duration,
    /// How often `GET /targets?resend=true` is polled — at least once a
    /// minute (ADR-0070 §11).
    pub resend_poll: Duration,
    /// Reconnect backoff: doubles from `backoff_min` to `backoff_max`.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// A watch stream silent this long is assumed half-open and re-opened
    /// (with a fresh list, so nothing is missed).
    pub stream_idle_reconnect: Duration,
    /// Every request but the watch stream.
    pub request_timeout: Duration,
    /// On SIGTERM: how long a last flush of pending reports may take.
    pub shutdown_flush: Duration,
}

impl Config {
    /// The production defaults around a broker URL and token file.
    pub fn new(broker_url: impl Into<String>, token_file: impl Into<PathBuf>) -> Self {
        Self {
            broker_url: broker_url.into(),
            token_file: token_file.into(),
            argocd_ui_url: None,
            owner_annotation: DEFAULT_OWNER_ANNOTATION.to_string(),
            debounce: Duration::from_secs(2),
            resend_poll: Duration::from_secs(30),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            stream_idle_reconnect: Duration::from_secs(600),
            request_timeout: Duration::from_secs(30),
            shutdown_flush: Duration::from_secs(5),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let set = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let broker_url = set("SCARAB_BROKER_URL").ok_or("SCARAB_BROKER_URL is not set")?;
        let lower = broker_url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return Err(format!(
                "SCARAB_BROKER_URL must be an http(s) URL, got {broker_url:?}"
            ));
        }
        let token_file =
            set("SCARAB_BROKER_TOKEN_FILE").unwrap_or_else(|| DEFAULT_TOKEN_FILE.into());
        let mut cfg = Self::new(broker_url, token_file);
        cfg.argocd_ui_url = set("ARGOCD_UI_URL");
        if let Some(a) = set("OWNER_ANNOTATION") {
            cfg.owner_annotation = a;
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_names_the_broker_and_the_defaults_fill_the_rest() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(Config::from_lookup(env(&[])).is_err());
        assert!(Config::from_lookup(env(&[("SCARAB_BROKER_URL", "10.0.0.1:8090")])).is_err());
        let cfg = Config::from_lookup(env(&[
            ("SCARAB_BROKER_URL", "http://10.0.0.1:8090"),
            ("ARGOCD_UI_URL", "https://argo.example"),
        ]))
        .unwrap();
        assert_eq!(cfg.token_file, PathBuf::from(DEFAULT_TOKEN_FILE));
        assert_eq!(cfg.owner_annotation, DEFAULT_OWNER_ANNOTATION);
        assert_eq!(cfg.argocd_ui_url.as_deref(), Some("https://argo.example"));
        assert!(cfg.resend_poll <= Duration::from_secs(60), "ADR-0070 §11");
    }
}
