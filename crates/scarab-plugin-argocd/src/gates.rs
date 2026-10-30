//! `wait-live` (ADR-0076 §7): the gate this plugin resolves.
//!
//! A Pipeline waits on it with `invoke: plugin:<installation>/wait-live`,
//! passing the Argo CD Application (`app`) and an image reference (`image`).
//! The plugin asks Scarab for the open waits that name it, reads each
//! Application through the broker's read proxy, and answers:
//!
//! - **pass**, with output `revision`, when [`assess`] says the Application
//!   is live (synced, healthy, no operation running) and its **target**
//!   manifests render `image` ([`target_images`]): the same checks as the
//!   documented wait Step (ADR-0074 §5);
//! - **fail**, with Argo's reason, when [`assess`] says Argo has given up
//!   (a failed sync of the current revision, a rollout past its deadline);
//! - nothing otherwise: the wait stays open, and its expiry ends it.
//!
//! What it knows of Scarab is the gate API's HTTP contract, spelled out here
//! in its own types. It calls that API at the broker's address, the one its
//! network reaches, with its `scarab-api` token, which the kubelet rotates in
//! place: the file is re-read for every request.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::argo::{Application, ManagedResources};
use crate::assess::{assess, Status};
use crate::broker::{encode_component, Broker, BrokerError};
use crate::version::target_images;

/// The gate's name in `manifest.json`.
pub const WAIT_LIVE: &str = "wait-live";
/// The most waits asked for per page.
const PAGE_LIMIT: u32 = 100;
/// The longest body read from the gate API.
const MAX_API_BODY_BYTES: usize = 8 * 1024 * 1024;

/// One open wait, as `GET /v1/orgs/{org}/gates?pending=true&releaser=me`
/// lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct PendingGate {
    pub run: String,
    pub step: String,
    pub gate: String,
    pub attempt: String,
    #[serde(default)]
    pub inputs: Value,
}

#[derive(Debug, Deserialize)]
struct Page {
    gates: Vec<PendingGate>,
    #[serde(default)]
    next: Option<String>,
}

/// What `wait-live` decides about one Application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    /// Live at the image: release with this revision.
    Pass { revision: String },
    /// Argo gave up: fail with this reason.
    Fail { reason: String },
    /// Not yet; why.
    Wait { why: String },
}

/// `wait-live` on one Application: live AND serving `image`, failed, or not
/// yet. A pure function of what Argo said, so every rule is a table test.
pub fn judge(app: &Application, managed: &ManagedResources, image: &str) -> Judgement {
    let a = assess(app);
    match a.status {
        Status::Failed => Judgement::Fail { reason: a.label },
        Status::InProgress => Judgement::Wait { why: a.label },
        Status::Live => {
            let images = target_images(managed);
            if images.contains(image) {
                Judgement::Pass {
                    revision: app.status.sync.revision.clone().unwrap_or_default(),
                }
            } else {
                Judgement::Wait {
                    why: format!(
                        "synced and healthy, but its manifests do not render `{image}` yet"
                    ),
                }
            }
        }
    }
}

/// A client of the gate API, at the broker's address.
#[derive(Clone)]
pub struct GateApi {
    http: reqwest::Client,
    base: String,
    token_file: PathBuf,
    request_timeout: Duration,
}

impl GateApi {
    pub fn new(
        base_url: &str,
        token_file: PathBuf,
        request_timeout: Duration,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
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

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Vec<u8>, BrokerError> {
        let resp = req
            .bearer_auth(self.token()?)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|e| BrokerError::Transport(e.to_string()))?;
        let status = resp.status();
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_API_BODY_BYTES as u64)
        {
            return Err(BrokerError::Decode("response body too large".into()));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BrokerError::Transport(e.to_string()))?;
        if bytes.len() > MAX_API_BODY_BYTES {
            return Err(BrokerError::Decode("response body too large".into()));
        }
        if !status.is_success() {
            return Err(BrokerError::Status {
                status: status.as_u16(),
                retry_after: None,
                body: String::from_utf8_lossy(&bytes[..bytes.len().min(64 * 1024)]).into_owned(),
            });
        }
        Ok(bytes.to_vec())
    }

    /// Every open wait naming this plugin in `org`, all pages.
    pub async fn pending(&self, org: &str) -> Result<Vec<PendingGate>, BrokerError> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut url = format!(
                "{}/v1/orgs/{}/gates?pending=true&releaser=me&limit={PAGE_LIMIT}",
                self.base,
                encode_component(org)
            );
            if let Some(a) = &after {
                url.push_str(&format!("&after={}", encode_component(a)));
            }
            let bytes = self.send(self.http.get(url)).await?;
            let page: Page =
                serde_json::from_slice(&bytes).map_err(|e| BrokerError::Decode(e.to_string()))?;
            out.extend(page.gates);
            match page.next {
                Some(n) => after = Some(n),
                None => return Ok(out),
            }
        }
    }

    /// Answer `gate`'s wait: release with `outputs`, or fail with `reason`.
    pub async fn answer(&self, gate: &PendingGate, verdict: &Judgement) -> Result<(), BrokerError> {
        let body = match verdict {
            Judgement::Pass { revision } => json!({
                "attempt": gate.attempt,
                "passed": true,
                "outputs": { "revision": revision },
            }),
            Judgement::Fail { reason } => json!({
                "attempt": gate.attempt,
                "passed": false,
                "reason": reason,
            }),
            Judgement::Wait { .. } => return Ok(()),
        };
        let url = format!(
            "{}/v1/runs/{}/gates/{}/release",
            self.base,
            encode_component(&gate.run),
            encode_component(&gate.step)
        );
        self.send(
            self.http
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string()),
        )
        .await
        .map(|_| ())
    }
}

/// What one pass did with each wait.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct PassReport {
    pub released: Vec<String>,
    pub failed: Vec<String>,
    pub waiting: Vec<String>,
    /// Waits not answered this pass: an input missing, Argo unreadable, or
    /// the answer refused. Logged and tried again next pass.
    pub errors: Vec<String>,
}

/// The `app` and `image` inputs of a `wait-live` wait.
fn inputs(gate: &PendingGate) -> Option<(&str, &str)> {
    Some((
        gate.inputs.get("app")?.as_str()?,
        gate.inputs.get("image")?.as_str()?,
    ))
}

/// One pass: answer every open `wait-live` wait in `org` that Argo has
/// decided. A wait for a gate this plugin does not know is left alone.
pub async fn resolve_pass(broker: &Broker, api: &GateApi, org: &str) -> Result<PassReport, BrokerError> {
    let mut report = PassReport::default();
    for gate in api.pending(org).await? {
        let key = format!("{}/{}", gate.run, gate.step);
        if gate.gate != WAIT_LIVE {
            continue;
        }
        let Some((app, image)) = inputs(&gate) else {
            report.errors.push(format!("{key}: no `app` or `image` input"));
            continue;
        };
        // `.` and `..` survive percent-encoding and a URL resolves them, so
        // the read would name another path; a `/` is not in an Application
        // name. Neither becomes valid later: fail the gate now.
        if app.is_empty() || app == "." || app == ".." || app.contains('/') {
            let verdict = Judgement::Fail {
                reason: format!("`app` {app:?} is not an Argo CD Application name"),
            };
            match api.answer(&gate, &verdict).await {
                Ok(()) => report.failed.push(key),
                Err(e) => report.errors.push(format!("{key}: answering: {e}")),
            }
            continue;
        }
        let path = format!("/api/v1/applications/{}", encode_component(app));
        let read = async {
            let application: Application = broker.argocd_get(&path, &[]).await?;
            let managed: ManagedResources = broker
                .argocd_get(&format!("{path}/managed-resources"), &[])
                .await?;
            Ok::<_, BrokerError>((application, managed))
        };
        let verdict = match read.await {
            Ok((application, managed)) => judge(&application, &managed, image),
            Err(e) => {
                report.errors.push(format!("{key}: reading `{app}`: {e}"));
                continue;
            }
        };
        if let Err(e) = api.answer(&gate, &verdict).await {
            report.errors.push(format!("{key}: answering: {e}"));
            continue;
        }
        match verdict {
            Judgement::Pass { .. } => report.released.push(key),
            Judgement::Fail { .. } => report.failed.push(key),
            Judgement::Wait { .. } => report.waiting.push(key),
        }
    }
    Ok(report)
}
