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
use axum::http::{HeaderMap, Request, StatusCode, Uri};
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
    /// Answers this status, with a `Retry-After: 1` on a 429 or 503.
    Status(u16),
}

/// One request the plugin made, as the broker saw it.
#[derive(Debug, Clone)]
struct Seen {
    path_and_query: String,
    authorization: Option<String>,
}

#[derive(Default)]
struct Inner {
    pending: Vec<Value>,
    upstream: HashMap<String, Upstream>,
    release_status: HashMap<String, u16>,
    proxied: Vec<Seen>,
    api: Vec<Seen>,
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

fn authorization(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Decrements the in-flight count however the handler ends (a client that
/// times out drops it).
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn proxy(State(b): State<FakeBroker>, uri: Uri, headers: HeaderMap) -> Response {
    let now = b.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    b.most_in_flight.fetch_max(now, Ordering::SeqCst);
    let _guard = InFlight(b.in_flight.clone());
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
    let argo_pq = pq.strip_prefix(PROXY_PREFIX).unwrap_or(pq).to_string();
    let argo_path = argo_pq.split('?').next().unwrap_or("").to_string();
    let upstream = {
        let mut inner = b.lock();
        inner.proxied.push(Seen {
            path_and_query: argo_pq.clone(),
            authorization: authorization(&headers),
        });
        inner.upstream.get(&argo_path).cloned()
    };
    match upstream {
        Some(Upstream::Hang) => return std::future::pending::<Response>().await,
        Some(Upstream::Delay(d)) => tokio::time::sleep(d).await,
        Some(Upstream::Status(s)) => {
            let status = StatusCode::from_u16(s).unwrap();
            let mut resp =
                (status, Json(json!({ "error": format!("upstream {s}") }))).into_response();
            if s == 429 || s == 503 {
                resp.headers_mut()
                    .insert("retry-after", "1".parse().unwrap());
            }
            return resp;
        }
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

async fn list_gates(State(b): State<FakeBroker>, uri: Uri, headers: HeaderMap) -> Json<Value> {
    let mut inner = b.lock();
    inner.api.push(Seen {
        path_and_query: uri.to_string(),
        authorization: authorization(&headers),
    });
    Json(json!({ "gates": inner.pending.clone(), "next": null }))
}

async fn release(
    State(b): State<FakeBroker>,
    Path((run, step)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut inner = b.lock();
    inner.api.push(Seen {
        path_and_query: uri.to_string(),
        authorization: authorization(&headers),
    });
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
    broker_token: PathBuf,
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
            broker_token,
        }
    }

    /// A live Application at [`IMAGE`] in Argo's default namespace.
    fn live(&self, name: &str) {
        self.fake.put_app("argocd", name, live_spec());
    }

    fn upstream(&self, argo_path: &str, u: Upstream) {
        self.fb.lock().upstream.insert(argo_path.into(), u);
    }

    fn clear_upstream(&self) {
        self.fb.lock().upstream.clear();
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

fn proxied_paths(w: &World) -> Vec<String> {
    w.fb.lock()
        .proxied
        .iter()
        .map(|s| s.path_and_query.clone())
        .collect()
}

#[tokio::test]
async fn an_error_status_on_a_read_leaves_the_wait_open_for_the_next_pass() {
    // 409 is not something Argo answers to a GET; 429 and 503 are the
    // broker's own limits (per installation, and broker-wide), each with a
    // Retry-After. The pass treats all three alike: the wait is not
    // answered, the read is not retried within the pass, the other waits
    // go on, and the next pass reads it again. The Retry-After is parsed
    // (`BrokerError::Status::retry_after`) but the pass does not wait on
    // it: the next pass comes a gate interval (15s) later anyway.
    for status in [409u16, 429, 503] {
        let w = World::new(Duration::from_secs(5)).await;
        w.live("web");
        w.live("api");
        w.upstream("/api/v1/applications/web", Upstream::Status(status));
        w.waits(&["web", "api"]);

        let report = w.pass().await;
        assert_eq!(report.released, [key("api")], "{status}: {report:?}");
        assert_eq!(report.errors.len(), 1, "{status}: {report:?}");
        assert!(
            report.errors[0].starts_with(&format!(
                "{}: reading `web`: broker answered {status}",
                key("web")
            )),
            "{status}: {:?}",
            report.errors
        );
        assert_eq!(w.answered_steps(), ["live-api"], "{status}");
        assert_eq!(
            proxied_paths(&w)
                .iter()
                .filter(|p| p.starts_with("/api/v1/applications/web"))
                .count(),
            1,
            "{status}: one read of `web` in the pass, no retry"
        );
        assert_eq!(w.pending_steps(), ["live-web"], "{status}");

        w.clear_upstream();
        let report = w.pass().await;
        assert_eq!(report.released, [key("web")], "{status}: {report:?}");
        assert!(w.pending_steps().is_empty(), "{status}");
    }
}

#[tokio::test]
async fn a_409_on_the_answer_is_reported_and_the_pass_goes_on() {
    // The gate API answers 409 to a wait that passed its expiry (or whose
    // attempt moved on): the answer is reported once, not retried within
    // the pass, and the other waits are answered.
    let w = World::new(Duration::from_secs(5)).await;
    w.live("web");
    w.live("api");
    w.waits(&["web", "api"]);
    w.fb.lock().release_status.insert("live-web".into(), 409);

    let report = w.pass().await;
    assert_eq!(report.released, [key("api")], "{report:?}");
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert!(
        report.errors[0].starts_with(&format!("{}: answering: broker answered 409", key("web"))),
        "{:?}",
        report.errors
    );
    let mut answered = w.answered_steps();
    answered.sort();
    assert_eq!(answered, ["live-api", "live-web"], "one answer each");
}

#[tokio::test]
async fn the_plugin_sends_its_own_rotating_token_and_never_the_argo_token() {
    // Two credentials rotate. The broker token is a projected ServiceAccount
    // token the kubelet rewrites in place: the plugin re-reads the file for
    // every request, so the request after a rotation carries the new one.
    // The Argo token is bound in the broker, which adds it to each proxied
    // request: the plugin never holds it, so a re-bind needs nothing from
    // the plugin. What the plugin sends is its broker token, never Argo's.
    let w = World::new(Duration::from_secs(5)).await;
    w.fake.put_app(
        "argocd",
        "web",
        FakeAppSpec {
            sync: "OutOfSync".into(),
            ..live_spec()
        },
    );
    w.waits(&["web"]);
    assert_eq!(w.pass().await.waiting, [key("web")]);
    std::fs::write(&w.broker_token, "broker-token-2\n").unwrap();
    w.live("web");
    assert_eq!(w.pass().await.released, [key("web")]);

    let proxied = w.fb.lock().proxied.clone();
    let auth: Vec<&str> = proxied
        .iter()
        .map(|s| s.authorization.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        auth,
        [
            "Bearer broker-token-1",
            "Bearer broker-token-1",
            "Bearer broker-token-2",
            "Bearer broker-token-2",
        ],
        "two reads a pass, the second pass after the rotation"
    );
    let api = w.fb.lock().api.clone();
    assert!(
        api.iter()
            .all(|s| s.authorization.as_deref() == Some("Bearer api-token-1")),
        "{api:?}"
    );
    for s in proxied.iter().chain(api.iter()) {
        assert!(
            !s.authorization
                .as_deref()
                .unwrap_or("")
                .contains(ARGO_TOKEN),
            "the plugin sent the Argo token: {s:?}"
        );
    }
    // And the broker's credential is what Argo saw on every read.
    let argo = w.fake.requests();
    assert_eq!(argo.len(), 4);
    assert!(argo.iter().all(|r| r.authorized), "{argo:?}");
}

#[tokio::test]
async fn an_application_deleted_between_passes_leaves_the_wait_open() {
    // The plugin polls; it does not watch, so it sees no DELETED event. An
    // Application deleted between two passes reads as Argo's 404 on the
    // next one: the wait is not answered (an app-of-apps can create the
    // Application again) and stays open until it expires, or until the
    // Application is back and live.
    let w = World::new(Duration::from_secs(5)).await;
    w.fake.put_app(
        "argocd",
        "web",
        FakeAppSpec {
            sync: "OutOfSync".into(),
            ..live_spec()
        },
    );
    w.waits(&["web"]);
    assert_eq!(w.pass().await.waiting, [key("web")]);

    assert!(w.fake.delete_app("argocd", "web"));
    let report = w.pass().await;
    assert!(report.released.is_empty() && report.failed.is_empty() && report.waiting.is_empty());
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert!(
        report.errors[0].starts_with(&format!(
            "{}: reading `web`: broker answered 404",
            key("web")
        )),
        "{:?}",
        report.errors
    );
    assert!(w.answered_steps().is_empty());
    assert_eq!(w.pending_steps(), ["live-web"]);

    w.live("web");
    assert_eq!(w.pass().await.released, [key("web")]);
}

#[tokio::test]
async fn an_application_outside_argos_namespace_is_read_with_app_namespace() {
    // Apps in any namespace: `app: <namespace>/<name>`, as the argocd CLI
    // writes it, reads the Application with `?appNamespace=<namespace>`
    // (a query key manifest.json allows). A bare name sends none, and Argo
    // looks in its own namespace.
    let w = World::new(Duration::from_secs(5)).await;
    w.fake.put_app("team-a", "web", live_spec());
    w.live("api");
    w.fb.lock().pending = vec![
        wait("team-a/web", json!({ "app": "team-a/web", "image": IMAGE })),
        wait("api", json!({ "app": "api", "image": IMAGE })),
    ];
    let report = w.pass().await;
    assert_eq!(
        report.released,
        [key("team-a-web"), key("api")],
        "{report:?}"
    );
    let mut paths = proxied_paths(&w);
    paths.sort();
    assert_eq!(
        paths,
        [
            "/api/v1/applications/api",
            "/api/v1/applications/api/managed-resources",
            "/api/v1/applications/web/managed-resources?appNamespace=team-a",
            "/api/v1/applications/web?appNamespace=team-a",
        ]
    );
}

#[tokio::test]
async fn an_app_that_names_no_application_fails_without_a_read() {
    let bad = [
        "",
        ".",
        "..",
        "a/b/c",
        "/web",
        "team-a/",
        "./web",
        "team-a/..",
        "..",
    ];
    let w = World::new(Duration::from_secs(5)).await;
    w.fb.lock().pending = bad
        .iter()
        .enumerate()
        .map(|(i, app)| wait(&format!("bad{i}"), json!({ "app": app, "image": IMAGE })))
        .collect();
    let report = w.pass().await;
    assert_eq!(report.failed.len(), bad.len(), "{report:?}");
    assert!(proxied_paths(&w).is_empty(), "{:?}", proxied_paths(&w));
    for (_, body) in w.fb.lock().answers.iter() {
        assert_eq!(body["passed"], false);
        assert!(
            body["reason"]
                .as_str()
                .unwrap()
                .contains("is not an Argo CD Application name"),
            "{body}"
        );
    }
}
