//! The CredentialBroker, as a Plugin reaches it (ADR-0072 §5, §11): the one
//! address its network allows. Identity lives under `/_scarab/plugin/v1`;
//! Argo CD is read through the broker's proxy at
//! `/_scarab/plugin/v1/connections/argocd/…`, which adds the Argo token —
//! this process never holds it.
//!
//! The broker token is a projected ServiceAccount token that the kubelet
//! rotates in place (audience `scarab-broker`, ten-minute TTL), so it is
//! re-read from its file before **every** request; a cached copy would
//! expire under a long-lived plugin.

use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;

/// Where every broker route lives.
pub const BROKER_PREFIX: &str = "/_scarab/plugin/v1";
/// The manifest's connection slot for Argo CD (`manifest.json`).
pub const ARGOCD_SLOT: &str = "argocd";
/// The largest body read from the broker's own routes (`/whoami`).
const MAX_BROKER_BODY_BYTES: usize = 16 * 1024 * 1024;
/// The largest Argo CD response read through the proxy. Managed resources
/// carry every resource's target AND live state as JSON strings, so a big
/// Application is legitimately tens of MiB; past this one it is refused
/// rather than let one response grow the plugin without bound under its
/// memory limit.
const MAX_ARGOCD_BODY_BYTES: usize = 64 * 1024 * 1024;
/// How much of an error body is kept: it is only logged.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

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
        // Only logged: a bounded prefix, never the whole of a hostile body.
        let body = match read_capped(resp, MAX_ERROR_BODY_BYTES, true).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => String::new(),
        };
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
        let bytes = read_capped(resp, MAX_BROKER_BODY_BYTES, false).await?;
        serde_json::from_slice(&bytes).map_err(|e| BrokerError::Decode(e.to_string()))
    }

    /// Who the broker says this installation is.
    pub async fn whoami(&self) -> Result<WhoAmI, BrokerError> {
        self.get_json("/whoami").await
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
        let bytes = read_capped(resp, MAX_ARGOCD_BODY_BYTES, false).await?;
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

/// Read a body of at most `limit` bytes. A declared `Content-Length` over
/// the limit is refused before reading; a chunked body is cut as soon as it
/// passes it. `truncate` keeps the first `limit` bytes instead of failing
/// (for error bodies, which are only logged).
async fn read_capped(
    mut resp: reqwest::Response,
    limit: usize,
    truncate: bool,
) -> Result<Vec<u8>, BrokerError> {
    let too_large = || BrokerError::Decode(format!("response body over {limit} bytes"));
    if !truncate && resp.content_length().is_some_and(|n| n > limit as u64) {
        return Err(too_large());
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| BrokerError::Transport(e.to_string()))?
    {
        if body.len() + chunk.len() > limit {
            if truncate {
                body.extend_from_slice(&chunk[..limit - body.len()]);
                return Ok(body);
            }
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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

    /// One raw HTTP/1.1 response on a loopback socket: `length` = declare a
    /// `Content-Length`, else close-delimited (no length known up front).
    async fn serve_once(body: Vec<u8>, length: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 4096];
            let _ = sock.read(&mut req).await;
            let head = if length {
                format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len())
            } else {
                "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_string()
            };
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn a_body_over_its_cap_is_refused_or_truncated_never_read_whole() {
        for length in [true, false] {
            let url = serve_once(vec![b'x'; 2048], length).await;
            let resp = reqwest::get(&url).await.unwrap();
            assert!(
                matches!(
                    read_capped(resp, 1024, false).await,
                    Err(BrokerError::Decode(_))
                ),
                "length declared: {length}"
            );
            let url = serve_once(vec![b'x'; 2048], length).await;
            let resp = reqwest::get(&url).await.unwrap();
            assert_eq!(read_capped(resp, 1024, true).await.unwrap().len(), 1024);
            let url = serve_once(vec![b'x'; 1024], length).await;
            let resp = reqwest::get(&url).await.unwrap();
            assert_eq!(read_capped(resp, 1024, false).await.unwrap().len(), 1024);
        }
    }
}
