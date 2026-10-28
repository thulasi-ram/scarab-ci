//! The plugin's configuration, from its environment.
//!
//! Core sets two variables on a Plugin Pod (ADR-0072 §9, slice d2's
//! `plugin_env`), last, so no config value can re-aim them:
//! `SCARAB_BROKER_URL` and `SCARAB_BROKER_TOKEN_FILE`.

use std::path::PathBuf;
use std::time::Duration;

/// Where the kubelet projects the broker token (slice d2's
/// `BROKER_TOKEN_DIR`/`BROKER_TOKEN_FILE`), if the env does not say.
pub const DEFAULT_TOKEN_FILE: &str = "/var/run/secrets/scarab/broker/token";

#[derive(Debug, Clone)]
pub struct Config {
    /// The broker, by IP (a Plugin has no DNS).
    pub broker_url: String,
    pub token_file: PathBuf,
    /// How often the plugin checks it can still reach the broker and Argo CD.
    pub check_interval: Duration,
    /// Every request.
    pub request_timeout: Duration,
}

impl Config {
    /// The production defaults around a broker URL and token file.
    pub fn new(broker_url: impl Into<String>, token_file: impl Into<PathBuf>) -> Self {
        Self {
            broker_url: broker_url.into(),
            token_file: token_file.into(),
            check_interval: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
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
        Ok(Self::new(broker_url, token_file))
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
        let cfg =
            Config::from_lookup(env(&[("SCARAB_BROKER_URL", "http://10.0.0.1:8090")])).unwrap();
        assert_eq!(cfg.token_file, PathBuf::from(DEFAULT_TOKEN_FILE));
    }
}
