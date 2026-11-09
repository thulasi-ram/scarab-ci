//! The `wait-live` gate pass ([`resolve_pass`]) against a fake broker on a
//! 127.0.0.1 listener: its read proxy forwards to the in-process fake Argo CD
//! (`src/bin/fake-argocd/server.rs`) and adds the Argo token, as the real
//! broker does, and its gate API lists the open waits and records each
//! answer. A test can make one Argo path hang, answer late, or answer with an
//! error status.
//!
//! The real broker and gate API are covered by `contract.rs` and
//! `wait_live.rs` (Postgres). This file needs no database, so it can pin what
//! the pass does when Argo or the broker misbehave.

#[path = "../src/bin/fake-argocd/server.rs"]
mod fake_argocd;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

use fake_argocd::{FakeAppSpec, FakeArgo};
use scarab_plugin_argocd::broker::Broker;
use scarab_plugin_argocd::gates::{resolve_pass, GateApi, PassReport, GATE_READS_AT_ONCE};

const ORG: &str = "acme";
const RUN: &str = "run-1";
const IMAGE: &str = "ghcr.io/acme/web:1.4.2";
const REVISION: &str = "9f1c2e7b9f1c2e7b9f1c2e7b9f1c2e7b9f1c2e7b";
/// The Argo token only the broker holds.
const ARGO_TOKEN: &str = "argo-api-token-s3cr3t";
const PROXY_PREFIX: &str = "/_scarab/plugin/v1/connections/argocd";

/// What the fake broker does with one Argo path instead of forwarding it.
#[derive(Clone)]
enum Upstream {
    /// Never answers: the plugin's request timeout ends it.
    Hang,
    /// Answers after this long.
    Delay(Duration),
}

#[derive(Default)]
struct Inner {
    pending: Vec<Value>,
    upstream: HashMap<String, Upstream>,
    release_status: HashMap<String, u16>,
    /// `(step, body)` of every release the gate API accepted or refused.
    answers: Vec<(String, Value)>,
}

#[derive(Clone)]
struct FakeBroker {
    inner: Arc<Mutex<Inner>>,
    argo: Router,
    in_flight: Arc<AtomicUsize>,
    most_in_flight: Arc<AtomicUsize>,
}

impl FakeBroker {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Decrements the in-flight count however the handler ends (a client that
/// times out drops it).
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn proxy(State(b): State<FakeBroker>, uri: Uri) -> Response {
    let now = b.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    b.most_in_flight.fetch_max(now, Ordering::SeqCst);
    let _guard = InFlight(b.in_flight.clone());
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
    let argo_pq = pq.strip_prefix(PROXY_PREFIX).unwrap_or(pq).to_string();
    let argo_path = argo_pq.split('?').next().unwrap_or("").to_string();
    let upstream = b.lock().upstream.get(&argo_path).cloned();
    match upstream {
        Some(Upstream::Hang) => return std::future::pending::<Response>().await,
        Some(Upstream::Delay(d)) => tokio::time::sleep(d).await,
        None => {}
    }
    // The broker's own credential, never the plugin's.
    let req = Request::builder()
        .uri(argo_pq)
        .header("authorization", format!("Bearer {ARGO_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    b.argo.clone().oneshot(req).await.unwrap().into_response()
}

async fn list_gates(State(b): State<FakeBroker>) -> Json<Value> {
    let inner = b.lock();
    Json(json!({ "gates": inner.pending.clone(), "next": null }))
}

async fn release(
    State(b): State<FakeBroker>,
    Path((run, step)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let mut inner = b.lock();
    inner.answers.push((step.clone(), body));
    if let Some(s) = inner.release_status.get(&step) {
        let status = StatusCode::from_u16(*s).unwrap();
        return (
            status,
            Json(json!({ "error": "the wait is no longer open" })),
        )
            .into_response();
    }
    inner
        .pending
        .retain(|g| !(g["run"] == run.as_str() && g["step"] == step.as_str()));
    Json(json!({})).into_response()
}

struct World {
    fake: FakeArgo,
    fb: FakeBroker,
    broker: Broker,
    api: GateApi,
}

fn token_file(name: &str, value: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "scarab-argocd-gate-pass-{}-{name}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("token");
    std::fs::write(&path, value).unwrap();
    path
}

fn rand_suffix() -> u64 {
    static N: AtomicUsize = AtomicUsize::new(0);
    N.fetch_add(1, Ordering::SeqCst) as u64
}

impl World {
    async fn new(request_timeout: Duration) -> Self {
        let fake = FakeArgo::new(Some(ARGO_TOKEN.into()));
        let fb = FakeBroker {
            inner: Arc::default(),
            argo: fake_argocd::router(fake.clone()),
            in_flight: Arc::default(),
            most_in_flight: Arc::default(),
        };
        let app = Router::new()
            .route(&format!("{PROXY_PREFIX}/{{*rest}}"), get(proxy))
            .route("/v1/orgs/{org}/gates", get(list_gates))
            .route("/v1/runs/{run}/gates/{step}/release", post(release))
            .with_state(fb.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let broker_token = token_file("broker", "broker-token-1");
        let api_token = token_file("api", "api-token-1");
        Self {
            broker: Broker::new(&base, broker_token.clone(), request_timeout).unwrap(),
            api: GateApi::new(&base, api_token, request_timeout).unwrap(),
            fake,
            fb,
        }
    }

    /// A live Application at [`IMAGE`] in Argo's default namespace.
    fn live(&self, name: &str) {
        self.fake.put_app("argocd", name, live_spec());
    }

    fn upstream(&self, argo_path: &str, u: Upstream) {
        self.fb.lock().upstream.insert(argo_path.into(), u);
    }

    /// One open `wait-live` wait per app, step `live-<app>`.
    fn waits(&self, apps: &[&str]) {
        self.fb.lock().pending = apps
            .iter()
            .map(|a| wait(a, json!({ "app": a, "image": IMAGE })))
            .collect();
    }

    fn answered_steps(&self) -> Vec<String> {
        self.fb
            .lock()
            .answers
            .iter()
            .map(|(s, _)| s.clone())
            .collect()
    }

    fn pending_steps(&self) -> Vec<String> {
        self.fb
            .lock()
            .pending
            .iter()
            .map(|g| g["step"].as_str().unwrap().to_string())
            .collect()
    }

    async fn pass(&self) -> PassReport {
        resolve_pass(&self.broker, &self.api, ORG).await.unwrap()
    }
}

fn live_spec() -> FakeAppSpec {
    FakeAppSpec {
        revision: REVISION.into(),
        images: vec![IMAGE.into()],
        sync: "Synced".into(),
        health: "Healthy".into(),
        ..Default::default()
    }
}

fn wait(app: &str, inputs: Value) -> Value {
    json!({
        "run": RUN,
        "step": format!("live-{}", app.replace('/', "-")),
        "gate": "wait-live",
        "attempt": format!("attempt-{app}"),
        "inputs": inputs,
    })
}

fn key(app: &str) -> String {
    format!("{RUN}/live-{app}")
}

#[tokio::test]
async fn a_hung_application_does_not_hold_up_the_other_gates() {
    let timeout = Duration::from_secs(5);
    let w = Arc::new(World::new(timeout).await);
    for app in ["stuck", "a", "b", "c"] {
        w.live(app);
    }
    // `stuck` is first in the list and never answers; `a` answers last of
    // the three that do.
    w.upstream("/api/v1/applications/stuck", Upstream::Hang);
    w.upstream(
        "/api/v1/applications/a",
        Upstream::Delay(Duration::from_millis(300)),
    );
    w.waits(&["stuck", "a", "b", "c"]);

    let started = Instant::now();
    let pass = tokio::spawn({
        let w = w.clone();
        async move { w.pass().await }
    });
    while w.answered_steps().len() < 3 && started.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let answered_after = started.elapsed();
    assert!(
        answered_after < Duration::from_secs(2),
        "the three readable gates were answered after {answered_after:?}, \
         behind the hung read (request timeout {timeout:?})"
    );
    let mut answered = w.answered_steps();
    answered.sort();
    assert_eq!(answered, ["live-a", "live-b", "live-c"]);

    let report = pass.await.unwrap();
    // The report lists waits in the order the gate API listed them,
    // whatever order the reads finished in.
    assert_eq!(report.released, [key("a"), key("b"), key("c")]);
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(
        report.errors[0].starts_with(&format!("{}: reading `stuck`", key("stuck"))),
        "{:?}",
        report.errors
    );
    assert_eq!(w.pending_steps(), ["live-stuck"]);
}

#[tokio::test]
async fn a_pass_reads_at_most_its_bound_of_applications_at_once() {
    let w = World::new(Duration::from_secs(5)).await;
    let apps: Vec<String> = (0..3 * GATE_READS_AT_ONCE)
        .map(|i| format!("app-{i}"))
        .collect();
    let names: Vec<&str> = apps.iter().map(String::as_str).collect();
    for app in &names {
        w.live(app);
        w.upstream(
            &format!("/api/v1/applications/{app}"),
            Upstream::Delay(Duration::from_millis(100)),
        );
    }
    w.waits(&names);
    let report = w.pass().await;
    assert_eq!(report.released.len(), names.len(), "{report:?}");
    assert_eq!(
        w.fb.most_in_flight.load(Ordering::SeqCst),
        GATE_READS_AT_ONCE,
        "proxied requests in flight at once"
    );
}
