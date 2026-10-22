//! The CredentialBroker, as a Plugin reaches it (ADR-0072 §5, §11): the one
//! address its network allows. Identity, cursors and report ingest live
//! under `/_scarab/plugin/v1`; Argo CD is read through the broker's proxy at
//! `/_scarab/plugin/v1/connections/argocd/…`, which adds the Argo token —
//! this process never holds it.
//!
//! The broker token is a projected ServiceAccount token that the kubelet
//! rotates in place (audience `scarab-broker`, ten-minute TTL), so it is
//! re-read from its file before **every** request; a cached copy would
//! expire under a long-lived reporter.

use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::report::ReportItem;

/// Where every broker route lives.
pub const BROKER_PREFIX: &str = "/_scarab/plugin/v1";
/// The manifest's connection slot for Argo CD (`manifest.json`).
pub const ARGOCD_SLOT: &str = "argocd";
/// `/targets` page size: the contract's maximum.
const TARGETS_PAGE_LIMIT: usize = 1000;

#[derive(Debug)]
pub enum BrokerError {
    /// The token file could not be read.
    Token(String),
    /// No response: connect, timeout, or a broken body.
    Transport(String),
    /// A response other than 2xx.
    Status {
        status: u16,
        retry_after: Option<Duration>,
        body: String,
    },
    /// A 2xx whose body is not what the contract says.
    Decode(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrokerError::Token(e) => write!(f, "broker token unreadable: {e}"),
            BrokerError::Transport(e) => write!(f, "broker unreachable: {e}"),
            BrokerError::Status { status, body, .. } => {
                let body: String = body.chars().take(300).collect();
                write!(f, "broker answered {status}: {body}")
            }
            BrokerError::Decode(e) => write!(f, "broker response unreadable: {e}"),
        }
    }
}

impl std::error::Error for BrokerError {}

impl BrokerError {
    pub fn status(&self) -> Option<u16> {
        match self {
            BrokerError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// `GET …/whoami`.
#[derive(Debug, Clone, Deserialize)]
pub struct WhoAmI {
    pub installation_id: String,
    pub org: String,
    pub name: String,
    pub plugin: String,
    pub principal: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// One entry of `GET …/targets`.
#[derive(Debug, Clone, Deserialize)]
pub struct TargetCursor {
    pub target_id: String,
    pub vendor_grouping: String,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub resend: bool,
    pub last_sequence: u64,
    #[serde(default)]
    pub version_scheme: Option<String>,
    #[serde(default)]
    pub last_version: Option<String>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_label: Option<String>,
    #[serde(default)]
    pub owners: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TargetsPage {
    targets: Vec<TargetCursor>,
    #[serde(default)]
    next_after: Option<String>,
}

/// One item's result, in request order.
#[derive(Debug, Clone, Deserialize)]
pub struct ItemResult {
    /// `accepted`, `duplicate`, `stale`, `unmapped` or `rejected`.
    pub outcome: String,
    #[serde(default)]
    pub input_seq: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReportResults {
    results: Vec<ItemResult>,
}

#[derive(Serialize)]
struct ReportBody<'a> {
    items: &'a [ReportItem],
}

/// A client of the broker.
#[derive(Clone)]
pub struct Broker {
    http: reqwest::Client,
    base: String,
    token_file: PathBuf,
    request_timeout: Duration,
}

/// Percent-encode a query value or path segment: unreserved characters
/// (RFC 3986) pass, everything else is `%XX`.
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn query_string(query: &[(&str, &str)]) -> String {
    let pairs: Vec<String> = query
        .iter()
        .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
        .collect();
    if pairs.is_empty() {
        String::new()
    } else {
        format!("?{}", pairs.join("&"))
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

impl Broker {
    pub fn new(
        base_url: &str,
        token_file: PathBuf,
        request_timeout: Duration,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // A 3xx is the broker's or the upstream's answer, never a hop to
            // follow: the broker forwards only to the admin-bound address.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("scarab-plugin-argocd/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
            token_file,
            request_timeout,
        })
    }

    /// The current projected token. Read every time: it rotates.
    fn token(&self) -> Result<String, BrokerError> {
        let raw = std::fs::read_to_string(&self.token_file)
            .map_err(|e| BrokerError::Token(format!("{}: {e}", self.token_file.display())))?;
        let token = raw.trim().to_string();
        if token.is_empty() {
            return Err(BrokerError::Token(format!(
                "{} is empty",
                self.token_file.display()
            )));
        }
        Ok(token)
    }

    fn url(&self, path_and_query: &str) -> String {
        format!("{}{BROKER_PREFIX}{path_and_query}", self.base)
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, BrokerError> {
        let mut req = req.bearer_auth(self.token()?);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| BrokerError::Transport(e.to_string()))?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let retry_after = retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        Err(BrokerError::Status {
            status,
            retry_after,
            body,
        })
    }

    async fn get_json<T: DeserializeOwned>(&self, path_and_query: &str) -> Result<T, BrokerError> {
        let resp = self
            .send(
                self.http
                    .get(self.url(path_and_query))
                    .header(reqwest::header::ACCEPT, "application/json"),
                Some(self.request_timeout),
            )
            .await?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BrokerError::Transport(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| BrokerError::Decode(e.to_string()))
    }

    /// Who the broker says this installation is.
    pub async fn whoami(&self) -> Result<WhoAmI, BrokerError> {
        self.get_json("/whoami").await
    }

    /// Every cursor (or, `resend_only`, every target flagged `resend`), all
    /// pages (ADR-0070 §11: page by `next_after`).
    pub async fn targets(&self, resend_only: bool) -> Result<Vec<TargetCursor>, BrokerError> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let limit = TARGETS_PAGE_LIMIT.to_string();
            let mut query: Vec<(&str, &str)> = vec![("limit", &limit)];
            if resend_only {
                query.push(("resend", "true"));
            }
            if let Some(a) = after.as_deref() {
                query.push(("after", a));
            }
            let page: TargetsPage = self
                .get_json(&format!("/targets{}", query_string(&query)))
                .await?;
            let done = page.next_after.is_none() || page.targets.is_empty();
            all.extend(page.targets);
            if done {
                return Ok(all);
            }
            after = page.next_after;
        }
    }

    /// `POST …/reports`: one result per item, in order.
    pub async fn post_reports(&self, items: &[ReportItem]) -> Result<Vec<ItemResult>, BrokerError> {
        let resp = self
            .send(
                self.http
                    .post(self.url("/reports"))
                    .json(&ReportBody { items }),
                Some(self.request_timeout),
            )
            .await?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BrokerError::Transport(e.to_string()))?;
        let results: ReportResults =
            serde_json::from_slice(&bytes).map_err(|e| BrokerError::Decode(e.to_string()))?;
        if results.results.len() != items.len() {
            return Err(BrokerError::Decode(format!(
                "{} results for {} items",
                results.results.len(),
                items.len()
            )));
        }
        Ok(results.results)
    }

    fn argocd_url(&self, argo_path: &str, query: &[(&str, &str)]) -> String {
        self.url(&format!(
            "/connections/{ARGOCD_SLOT}{argo_path}{}",
            query_string(query)
        ))
    }

    /// A read of Argo CD's API through the broker. Only the paths and query
    /// keys `manifest.json` lists get through.
    pub async fn argocd_get<T: DeserializeOwned>(
        &self,
        argo_path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, BrokerError> {
        let resp = self
            .send(
                self.http
                    .get(self.argocd_url(argo_path, query))
                    .header(reqwest::header::ACCEPT, "application/json"),
                Some(self.request_timeout),
            )
            .await?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BrokerError::Transport(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| BrokerError::Decode(e.to_string()))
    }

    /// A long-lived read (Argo's watch stream): no total timeout — the
    /// caller bounds it by idleness, and the broker cuts it on a kill.
    pub async fn argocd_stream(
        &self,
        argo_path: &str,
        query: &[(&str, &str)],
    ) -> Result<reqwest::Response, BrokerError> {
        self.send(self.http.get(self.argocd_url(argo_path, query)), None)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(
            query_string(&[("after", "argocd/web app"), ("limit", "1000")]),
            "?after=argocd%2Fweb%20app&limit=1000"
        );
        assert_eq!(query_string(&[]), "");
    }
}
