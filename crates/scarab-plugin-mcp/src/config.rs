//! The plugin's configuration, from the environment core sets on its Pod.
//!
//! - `SCARAB_BROKER_URL`: the broker, by IP. PluginApi is served there under
//!   the public API's own paths.
//! - `SCARAB_API_TOKEN_FILE`: the projected `scarab-api` token, which proves
//!   the installation. Core sets it for a plugin that registers an endpoint.
//! - `SCARAB_ENDPOINT_PORT`: the port the endpoint is served on.

use std::path::PathBuf;
use std::time::Duration;

/// The endpoint port when the environment does not say.
pub const DEFAULT_ENDPOINT_PORT: u16 = 8080;

#[derive(Debug, Clone)]
pub struct Config {
    pub broker_url: String,
    pub api_token_file: PathBuf,
    pub port: u16,
    /// Every PluginApi call.
    pub request_timeout: Duration,
}

impl Config {
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
        let api_token_file = set("SCARAB_API_TOKEN_FILE").ok_or(
            "SCARAB_API_TOKEN_FILE is not set: core sets it for a plugin that registers an \
             endpoint, and PluginApi takes no call without it",
        )?;
        let port = match set("SCARAB_ENDPOINT_PORT") {
            None => DEFAULT_ENDPOINT_PORT,
            Some(p) => p
                .parse::<u16>()
                .map_err(|_| format!("SCARAB_ENDPOINT_PORT must be a port, got {p:?}"))?,
        };
        Ok(Self {
            broker_url: broker_url.trim_end_matches('/').to_string(),
            api_token_file: PathBuf::from(api_token_file),
            port,
            request_timeout: Duration::from_secs(20),
        })
    }
}
