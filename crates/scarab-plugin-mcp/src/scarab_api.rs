//! PluginApi: the public API's routes on the broker listener, called as the
//! person the request is for. Every call carries the installation's
//! `scarab-api` token and the request's delegated caller handle; core holds
//! the call to the operations the person granted and this plugin declared.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// The header the delegated caller handle travels in, both ways.
pub const DELEGATED_CALLER_HANDLE_HEADER: &str = "x-scarab-delegated-caller-handle";

/// The largest JSON answer read from PluginApi.
pub const MAX_API_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Why a PluginApi call did not give an answer.
#[derive(Debug)]
pub enum ApiCallError {
    /// No `scarab-api` token could be read.
    Token(String),
    /// The broker could not be reached, or the answer could not be read.
    Transport(String),
    /// PluginApi refused or failed: the status and the start of its body.
    Status { status: u16, body: String },
}

impl std::fmt::Display for ApiCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiCallError::Token(e) => write!(f, "the plugin's token: {e}"),
            ApiCallError::Transport(e) => write!(f, "Scarab could not be reached: {e}"),
            ApiCallError::Status { status, body } => {
                write!(f, "Scarab answered {status}: {body}")
            }
        }
    }
}

/// Percent-encode a path segment or query value: unreserved characters
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

/// `s` percent-encoded as one URL path segment, or `None` when it is empty,
/// `.` or `..`: those pass [`encode_component`] unchanged (`.` is
/// unreserved), and a URL parser resolves them as dot segments, so the
/// request would reach another path than the one the tool means.
pub fn path_segment(s: &str) -> Option<String> {
    match s {
        "" | "." | ".." => None,
        s => Some(encode_component(s)),
    }
}

/// A PluginApi client for one request: the delegated caller handle it was
/// given is the only authority it carries.
pub struct ScarabApi<'a> {
    pub http: &'a reqwest::Client,
    pub base: &'a str,
    pub api_token_file: &'a PathBuf,
    pub handle: &'a str,
    pub timeout: Duration,
}

impl ScarabApi<'_> {
    /// The current projected token; read every time, since it rotates.
    fn token(&self) -> Result<String, ApiCallError> {
        let raw = std::fs::read_to_string(self.api_token_file)
            .map_err(|e| ApiCallError::Token(format!("{}: {e}", self.api_token_file.display())))?;
        let token = raw.trim().to_string();
        if token.is_empty() {
            return Err(ApiCallError::Token("the token file is empty".into()));
        }
        Ok(token)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Vec<u8>, ApiCallError> {
        let mut resp = req
            .bearer_auth(self.token()?)
            .header(DELEGATED_CALLER_HANDLE_HEADER, self.handle)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ApiCallError::Transport(e.to_string()))?;
        let status = resp.status();
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| ApiCallError::Transport(e.to_string()))?
        {
            if body.len() + chunk.len() > MAX_API_BODY_BYTES {
                return Err(ApiCallError::Transport(
                    "the answer is larger than the plugin reads".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            return Err(ApiCallError::Status {
                status: status.as_u16(),
                body: text.chars().take(500).collect(),
            });
        }
        Ok(body)
    }

    fn json(body: &[u8]) -> Result<Value, ApiCallError> {
        if body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(body).map_err(|e| ApiCallError::Transport(e.to_string()))
    }

    /// GET `path_and_query` as JSON.
    pub async fn get_json(&self, path_and_query: &str) -> Result<Value, ApiCallError> {
        let body = self
            .send(
                self.http
                    .get(format!("{}{path_and_query}", self.base))
                    .header(reqwest::header::ACCEPT, "application/json"),
            )
            .await?;
        Self::json(&body)
    }

    /// GET `path_and_query` as text, keeping only its last `keep` bytes.
    pub async fn get_text_tail(
        &self,
        path_and_query: &str,
        keep: usize,
    ) -> Result<(String, bool), ApiCallError> {
        let mut resp = self
            .http
            .get(format!("{}{path_and_query}", self.base))
            .bearer_auth(self.token()?)
            .header(DELEGATED_CALLER_HANDLE_HEADER, self.handle)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ApiCallError::Transport(e.to_string()))?;
        let status = resp.status();
        let mut tail: Vec<u8> = Vec::new();
        let mut cut = false;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| ApiCallError::Transport(e.to_string()))?
        {
            tail.extend_from_slice(&chunk);
            if tail.len() > keep {
                let drop = tail.len() - keep;
                tail.drain(..drop);
                cut = true;
            }
        }
        let text = String::from_utf8_lossy(&tail).into_owned();
        if !status.is_success() {
            return Err(ApiCallError::Status {
                status: status.as_u16(),
                body: text.chars().take(500).collect(),
            });
        }
        Ok((text, cut))
    }

    /// POST to `path`, with `body` as JSON when there is one. An empty answer
    /// (a 202 with no body) reads as `null`.
    pub async fn post_json(&self, path: &str, body: Option<&Value>) -> Result<Value, ApiCallError> {
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            req = req.json(body);
        }
        let body = self.send(req).await?;
        Self::json(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dot_segment_or_an_empty_one_is_refused() {
        assert_eq!(path_segment(""), None);
        assert_eq!(path_segment("."), None);
        assert_eq!(path_segment(".."), None);
        assert_eq!(path_segment("..."), Some("...".into()));
        assert_eq!(path_segment("a/../b"), Some("a%2F..%2Fb".into()));
        assert_eq!(path_segment("%2e%2e"), Some("%252e%252e".into()));
    }
}
