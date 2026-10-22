//! A fake Argo CD API server: just enough of Argo's REST API for the
//! reporter — list, get, managed resources and the NDJSON watch stream —
//! plus a control API (`/_fake/…`) a test or `just plugin-verify` drives.
//!
//! Self-contained on purpose (axum, futures, serde, tokio only): the
//! `fake-argocd` binary and the contract test both `#[path]`-include this
//! file, so the server the local verification drives is the very one the
//! contract test asserts against.
//!
//! A control call describes an Application compactly ([`FakeAppSpec`]) and
//! the fake renders the shapes Argo serves: the `Application` (status, an
//! operation with hook results and retry counts, per-resource health) and
//! its managed resources (target AND live states as JSON strings, hooks
//! flagged). Every control write is a reconcile: `reconciledAt` and the
//! `resourceVersion` move, and a `MODIFIED` (or `ADDED`) event is streamed.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

/// One Application, as a control call describes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FakeAppSpec {
    /// The Argo CD project (the reporter's vendor grouping).
    pub project: String,
    /// The target revision (the gitops commit).
    pub revision: String,
    /// Container images of the app's Deployment in the TARGET manifests.
    pub images: Vec<String>,
    /// Its initContainers' images.
    pub init_images: Vec<String>,
    /// What runs (live state and `summary.images`); default = `images`.
    /// The reporter must never read it.
    pub live_images: Option<Vec<String>>,
    pub replicas: u32,
    /// A ConfigMap's data: changing only this is a config-only change.
    pub config: BTreeMap<String, String>,
    /// `Synced` | `OutOfSync` | `Unknown`.
    pub sync: String,
    /// `Healthy` | `Progressing` | `Degraded` | `Suspended` | `Missing`.
    pub health: String,
    /// The Deployment's health message (Argo puts it per resource).
    pub health_message: Option<String>,
    pub automated: bool,
    pub retry_limit: i64,
    pub annotations: BTreeMap<String, String>,
    /// Hook Jobs in git (excluded from the version).
    pub hooks: Vec<FakeHook>,
    pub operation: Option<FakeOperation>,
}

impl Default for FakeAppSpec {
    fn default() -> Self {
        Self {
            project: "default".into(),
            revision: "0000000000000000000000000000000000000000".into(),
            images: Vec::new(),
            init_images: Vec::new(),
            live_images: None,
            replicas: 1,
            config: BTreeMap::new(),
            sync: "Synced".into(),
            health: "Healthy".into(),
            health_message: None,
            automated: true,
            retry_limit: 2,
            annotations: BTreeMap::new(),
            hooks: Vec::new(),
            operation: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FakeHook {
    pub name: String,
    pub hook_type: String,
    pub image: String,
}

impl Default for FakeHook {
    fn default() -> Self {
        Self {
            name: "migrate".into(),
            hook_type: "PreSync".into(),
            image: "ghcr.io/acme/migrate:1".into(),
        }
    }
}

/// The current (or last) sync operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FakeOperation {
    /// `Running` | `Terminating` | `Failed` | `Error` | `Succeeded`.
    pub phase: String,
    pub message: Option<String>,
    pub retry_count: i64,
    /// The revision it syncs; default = the spec's.
    pub revision: Option<String>,
    pub hook_results: Vec<FakeHookResult>,
}

impl Default for FakeOperation {
    fn default() -> Self {
        Self {
            phase: "Running".into(),
            message: None,
            retry_count: 0,
            revision: None,
            hook_results: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FakeHookResult {
    pub name: String,
    pub hook_type: String,
    /// `Running` | `Succeeded` | `Failed` | `Error`.
    pub phase: String,
    pub message: Option<String>,
}

impl Default for FakeHookResult {
    fn default() -> Self {
        Self {
            name: "migrate".into(),
            hook_type: "PreSync".into(),
            phase: "Running".into(),
            message: None,
        }
    }
}

/// One request an Argo route received.
#[derive(Debug, Clone, Serialize)]
pub struct SeenRequest {
    pub path_and_query: String,
    /// It carried the expected `Authorization` (the broker's credential).
    pub authorized: bool,
}

struct Stored {
    spec: FakeAppSpec,
    application: Value,
    managed: Value,
}

struct Inner {
    apps: BTreeMap<(String, String), Stored>,
    resource_version: u64,
    /// Every event ever streamed, by resourceVersion: a watch that starts
    /// at a list's version replays what came after it, as Argo's does.
    history: Vec<(u64, String)>,
    requests: Vec<SeenRequest>,
}

/// The fake. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct FakeArgo {
    inner: Arc<Mutex<Inner>>,
    events: broadcast::Sender<(u64, String)>,
    /// The bearer token Argo routes require; `None` = any.
    token: Option<String>,
}

impl FakeArgo {
    pub fn new(token: Option<String>) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                apps: BTreeMap::new(),
                resource_version: 1000,
                history: Vec::new(),
                requests: Vec::new(),
            })),
            events,
            token,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Create or replace an Application, as a reconcile would: new
    /// `reconciledAt` and `resourceVersion`, and a watch event.
    pub fn put_app(&self, namespace: &str, name: &str, spec: FakeAppSpec) {
        let mut inner = self.lock();
        inner.resource_version += 1;
        let rv = inner.resource_version;
        let application = render_application(namespace, name, &spec, rv);
        let managed = render_managed_resources(name, &spec);
        let key = (namespace.to_string(), name.to_string());
        let kind = if inner.apps.contains_key(&key) {
            "MODIFIED"
        } else {
            "ADDED"
        };
        let line = event_line(kind, &application);
        inner.apps.insert(
            key,
            Stored {
                spec,
                application,
                managed,
            },
        );
        inner.history.push((rv, line.clone()));
        let _ = self.events.send((rv, line));
    }

    pub fn delete_app(&self, namespace: &str, name: &str) -> bool {
        let mut inner = self.lock();
        let Some(stored) = inner
            .apps
            .remove(&(namespace.to_string(), name.to_string()))
        else {
            return false;
        };
        inner.resource_version += 1;
        let rv = inner.resource_version;
        let line = event_line("DELETED", &stored.application);
        inner.history.push((rv, line.clone()));
        let _ = self.events.send((rv, line));
        true
    }

    pub fn spec(&self, namespace: &str, name: &str) -> Option<FakeAppSpec> {
        self.lock()
            .apps
            .get(&(namespace.to_string(), name.to_string()))
            .map(|s| s.spec.clone())
    }

    pub fn requests(&self) -> Vec<SeenRequest> {
        self.lock().requests.clone()
    }
}

fn event_line(kind: &str, application: &Value) -> String {
    let mut line = json!({ "result": { "type": kind, "application": application } }).to_string();
    line.push('\n');
    line
}

/// `reconciledAt` for resourceVersion `rv`: a plausible RFC 3339 time that
/// moves with every write.
fn reconciled_at(rv: u64) -> String {
    let s = rv % 86_400;
    format!(
        "2026-10-22T{:02}:{:02}:{:02}Z",
        s / 3600,
        (s / 60) % 60,
        s % 60
    )
}

fn containers(prefix: &str, images: &[String]) -> Vec<Value> {
    images
        .iter()
        .enumerate()
        .map(|(i, image)| json!({ "name": format!("{prefix}{i}"), "image": image }))
        .collect()
}

fn deployment_manifest(name: &str, spec: &FakeAppSpec, images: &[String]) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": name,
            "namespace": name,
            "labels": { "app.kubernetes.io/instance": name }
        },
        "spec": {
            "replicas": spec.replicas,
            "selector": { "matchLabels": { "app": name } },
            "template": {
                "metadata": { "labels": { "app": name } },
                "spec": {
                    "initContainers": containers("init-", &spec.init_images),
                    "containers": containers("app-", images)
                }
            }
        }
    })
}

fn hook_manifest(app: &str, hook: &FakeHook) -> Value {
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": hook.name,
            "namespace": app,
            "annotations": {
                "argocd.argoproj.io/hook": hook.hook_type,
                "argocd.argoproj.io/hook-delete-policy": "BeforeHookCreation"
            }
        },
        "spec": {
            "template": {
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [{ "name": "hook", "image": hook.image }]
                }
            }
        }
    })
}

fn render_managed_resources(name: &str, spec: &FakeAppSpec) -> Value {
    let live_images = spec.live_images.as_ref().unwrap_or(&spec.images);
    let mut items = vec![
        json!({
            "group": "apps",
            "kind": "Deployment",
            "namespace": name,
            "name": name,
            "targetState": deployment_manifest(name, spec, &spec.images).to_string(),
            "liveState": deployment_manifest(name, spec, live_images).to_string(),
            "hook": false
        }),
        json!({
            "group": "",
            "kind": "ConfigMap",
            "namespace": name,
            "name": format!("{name}-config"),
            "targetState": json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": { "name": format!("{name}-config"), "namespace": name },
                "data": spec.config
            }).to_string(),
            "liveState": "null",
            "hook": false
        }),
    ];
    for hook in &spec.hooks {
        items.push(json!({
            "group": "batch",
            "kind": "Job",
            "namespace": name,
            "name": hook.name,
            "targetState": hook_manifest(name, hook).to_string(),
            "liveState": "null",
            "hook": true
        }));
    }
    json!({ "items": items })
}

fn render_application(namespace: &str, name: &str, spec: &FakeAppSpec, rv: u64) -> Value {
    let mut sync_policy = json!({ "retry": { "limit": spec.retry_limit } });
    if spec.automated {
        sync_policy["automated"] = json!({ "prune": true, "selfHeal": true });
    }
    let mut health = json!({ "status": spec.health });
    let mut resource_health = json!({ "status": spec.health });
    if let Some(m) = &spec.health_message {
        resource_health["message"] = json!(m);
        // Older Argo also put it on the Application.
        health["message"] = json!(m);
    }
    let live_images = spec.live_images.as_ref().unwrap_or(&spec.images);
    let mut status = json!({
        "sync": {
            "status": spec.sync,
            "revision": spec.revision,
            "comparedTo": {
                "source": { "repoURL": "https://git.example/acme/gitops.git", "path": name },
                "destination": { "server": "https://kubernetes.default.svc", "namespace": name }
            }
        },
        "health": health,
        "reconciledAt": reconciled_at(rv),
        "resources": [{
            "group": "apps",
            "version": "v1",
            "kind": "Deployment",
            "namespace": name,
            "name": name,
            "status": spec.sync,
            "health": resource_health
        }],
        "summary": { "images": live_images }
    });
    if let Some(op) = &spec.operation {
        let finished = !matches!(op.phase.as_str(), "Running" | "Terminating");
        let revision = op.revision.clone().unwrap_or_else(|| spec.revision.clone());
        let mut resources: Vec<Value> = op
            .hook_results
            .iter()
            .map(|h| {
                let mut r = json!({
                    "group": "batch",
                    "version": "v1",
                    "kind": "Job",
                    "namespace": name,
                    "name": h.name,
                    "status": if matches!(h.phase.as_str(), "Failed" | "Error") { "SyncFailed" } else { "Synced" },
                    "hookType": h.hook_type,
                    "hookPhase": h.phase,
                    "syncPhase": h.hook_type
                });
                if let Some(m) = &h.message {
                    r["message"] = json!(m);
                }
                r
            })
            .collect();
        if op.phase == "Succeeded" {
            resources.push(json!({
                "group": "apps",
                "version": "v1",
                "kind": "Deployment",
                "namespace": name,
                "name": name,
                "status": "Synced",
                "message": format!("deployment.apps/{name} configured"),
                "syncPhase": "Sync"
            }));
        }
        let mut state = json!({
            "operation": {
                "sync": { "revision": revision },
                "retry": { "limit": spec.retry_limit },
                "initiatedBy": { "automated": spec.automated }
            },
            "phase": op.phase,
            "startedAt": reconciled_at(rv.saturating_sub(1)),
            "syncResult": { "revision": revision, "resources": resources }
        });
        if let Some(m) = &op.message {
            state["message"] = json!(m);
        }
        if op.retry_count > 0 {
            state["retryCount"] = json!(op.retry_count);
        }
        if finished {
            state["finishedAt"] = json!(reconciled_at(rv));
        }
        status["operationState"] = state;
    }
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Application",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "resourceVersion": rv.to_string(),
            "annotations": spec.annotations
        },
        "spec": {
            "project": spec.project,
            "source": {
                "repoURL": "https://git.example/acme/gitops.git",
                "path": name,
                "targetRevision": "HEAD"
            },
            "destination": { "server": "https://kubernetes.default.svc", "namespace": name },
            "syncPolicy": sync_policy
        },
        "status": status
    })
}

/// The fake as an axum router: Argo's routes (behind the bearer check) and
/// the control routes (open — bind the fake to loopback).
pub fn router(fake: FakeArgo) -> Router {
    let argo = Router::new()
        .route("/api/version", get(version))
        .route("/api/v1/applications", get(list_applications))
        .route("/api/v1/applications/{name}", get(get_application))
        .route(
            "/api/v1/applications/{name}/managed-resources",
            get(managed_resources),
        )
        .route("/api/v1/stream/applications", get(stream_applications))
        .route_layer(axum::middleware::from_fn_with_state(
            fake.clone(),
            record_and_authorize,
        ));
    let control = Router::new()
        .route("/_fake/healthz", get(|| async { "ok" }))
        .route(
            "/_fake/apps/{namespace}/{name}",
            put(control_put).delete(control_delete).get(control_get),
        )
        .route("/_fake/requests", get(control_requests));
    argo.merge(control).with_state(fake)
}

async fn record_and_authorize(State(fake): State<FakeArgo>, req: Request, next: Next) -> Response {
    let authorized = match &fake.token {
        None => true,
        Some(token) => bearer(req.headers()) == Some(token.as_str()),
    };
    fake.lock().requests.push(SeenRequest {
        path_and_query: req
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default(),
        authorized,
    });
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid session", "code": 16, "message": "invalid session" })),
        )
            .into_response();
    }
    next.run(req).await
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("{what} not found"), "code": 5, "message": format!("{what} not found") })),
    )
        .into_response()
}

async fn version() -> Json<Value> {
    Json(json!({ "Version": "v2.13.0+fake" }))
}

async fn list_applications(State(fake): State<FakeArgo>) -> Json<Value> {
    let inner = fake.lock();
    let items: Vec<Value> = inner.apps.values().map(|s| s.application.clone()).collect();
    Json(json!({
        "metadata": { "resourceVersion": inner.resource_version.to_string() },
        // Argo serves `null`, not `[]`, for none.
        "items": if items.is_empty() { Value::Null } else { Value::Array(items) }
    }))
}

#[derive(Debug, Deserialize)]
struct AppQuery {
    #[serde(rename = "appNamespace")]
    app_namespace: Option<String>,
}

/// Argo's default application namespace when `appNamespace` is absent.
const DEFAULT_APP_NAMESPACE: &str = "argocd";

fn lookup(fake: &FakeArgo, name: &str, q: &AppQuery) -> Option<(Value, Value)> {
    let ns = q
        .app_namespace
        .clone()
        .unwrap_or_else(|| DEFAULT_APP_NAMESPACE.to_string());
    fake.lock()
        .apps
        .get(&(ns, name.to_string()))
        .map(|s| (s.application.clone(), s.managed.clone()))
}

async fn get_application(
    State(fake): State<FakeArgo>,
    Path(name): Path<String>,
    Query(q): Query<AppQuery>,
) -> Response {
    match lookup(&fake, &name, &q) {
        Some((app, _)) => Json(app).into_response(),
        None => not_found(&format!("application {name}")),
    }
}

async fn managed_resources(
    State(fake): State<FakeArgo>,
    Path(name): Path<String>,
    Query(q): Query<AppQuery>,
) -> Response {
    match lookup(&fake, &name, &q) {
        Some((_, managed)) => Json(managed).into_response(),
        None => not_found(&format!("application {name}")),
    }
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    #[serde(rename = "resourceVersion")]
    resource_version: Option<String>,
}

async fn stream_applications(
    State(fake): State<FakeArgo>,
    Query(q): Query<StreamQuery>,
) -> Response {
    let from: u64 = q
        .resource_version
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // Subscribe and take the backlog under one lock, so no event falls
    // between them.
    let (backlog, rx) = {
        let inner = fake.lock();
        let rx = fake.events.subscribe();
        let backlog: Vec<(u64, String)> = inner
            .history
            .iter()
            .filter(|(rv, _)| *rv > from)
            .cloned()
            .collect();
        (backlog, rx)
    };
    let last = backlog.last().map(|(rv, _)| *rv).unwrap_or(from);
    let backlog = futures::stream::iter(
        backlog
            .into_iter()
            .map(|(_, line)| Ok::<Bytes, Infallible>(Bytes::from(line))),
    );
    let live = futures::stream::unfold((rx, last), |(mut rx, last)| async move {
        loop {
            match rx.recv().await {
                Ok((rv, line)) if rv > last => {
                    return Some((Ok::<Bytes, Infallible>(Bytes::from(line)), (rx, rv)))
                }
                Ok(_) => continue,
                // Lagged or closed: end the stream; the client re-lists.
                Err(_) => return None,
            }
        }
    });
    use futures::StreamExt;
    (
        [("content-type", "application/json")],
        Body::from_stream(backlog.chain(live)),
    )
        .into_response()
}

async fn control_put(
    State(fake): State<FakeArgo>,
    Path((namespace, name)): Path<(String, String)>,
    Json(spec): Json<FakeAppSpec>,
) -> StatusCode {
    fake.put_app(&namespace, &name, spec);
    StatusCode::NO_CONTENT
}

async fn control_delete(
    State(fake): State<FakeArgo>,
    Path((namespace, name)): Path<(String, String)>,
) -> StatusCode {
    if fake.delete_app(&namespace, &name) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn control_get(
    State(fake): State<FakeArgo>,
    Path((namespace, name)): Path<(String, String)>,
) -> Response {
    match fake.spec(&namespace, &name) {
        Some(spec) => Json(spec).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn control_requests(State(fake): State<FakeArgo>) -> Json<Vec<SeenRequest>> {
    Json(fake.requests())
}
