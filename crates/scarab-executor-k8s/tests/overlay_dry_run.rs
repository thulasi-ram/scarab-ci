//! The overlay dry run of `K8sExecutor::launch` against a fake API server.
//! Hermetic: no cluster.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, ExecError, Executor, RunId, StepId, StepRun, StepSpec,
    StepStatus, Timestamp,
};
use scarab_executor_k8s::K8sExecutor;

const NS: &str = "scarab-test";

#[derive(Default)]
struct Fake {
    /// The Pod a GET returns; `None` answers 404.
    pod: Option<Value>,
    /// Every dry-run create fails, as behind a webhook that rejects dry runs.
    dry_run_fails: bool,
    /// Pod creates: `(dry run, body)`.
    creates: Mutex<Vec<(bool, Value)>>,
}

fn status(code: StatusCode, reason: &str, message: String) -> (StatusCode, Json<Value>) {
    (
        code,
        Json(json!({
            "kind": "Status", "apiVersion": "v1", "metadata": {},
            "status": "Failure", "reason": reason, "code": code.as_u16(),
            "message": message,
        })),
    )
}

async fn fake_api(fake: Fake) -> (String, Arc<Fake>) {
    let fake = Arc::new(fake);
    let app = Router::new()
        .route(
            "/api/v1/namespaces/{ns}/pods/{name}",
            get(
                |State(fake): State<Arc<Fake>>, Path((_ns, name)): Path<(String, String)>| async move {
                    match &fake.pod {
                        Some(pod) => (StatusCode::OK, Json(pod.clone())),
                        None => status(
                            StatusCode::NOT_FOUND,
                            "NotFound",
                            format!("pods \"{name}\" not found"),
                        ),
                    }
                },
            ),
        )
        .route(
            "/api/v1/namespaces/{ns}/pods",
            post(
                |State(fake): State<Arc<Fake>>,
                 RawQuery(query): RawQuery,
                 Json(pod): Json<Value>| async move {
                    let dry_run = query.is_some_and(|q| q.contains("dryRun=All"));
                    fake.creates.lock().unwrap().push((dry_run, pod.clone()));
                    let overlaid = pod["spec"]["nodeSelector"]["pool"].is_string();
                    if dry_run && !overlaid {
                        // Valid without the overlay, unless the test says
                        // the dry run itself fails.
                        if fake.dry_run_fails {
                            return status(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "InternalError",
                                "Internal error occurred: failed calling webhook".into(),
                            );
                        }
                        return (StatusCode::CREATED, Json(pod));
                    }
                    status(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Invalid",
                        "Pod \"x\" is invalid: spec.nodeSelector: Invalid value: \
                         \"not a label value!\""
                            .into(),
                    )
                },
            ),
        )
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), fake)
}

fn executor(url: &str) -> K8sExecutor {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = kube::Config::new(url.parse().unwrap());
    let client = kube::Client::try_from(config).expect("kube client");
    K8sExecutor::with_client(NS, client)
}

fn step() -> StepRun {
    StepRun {
        key: None,
        run: RunId("r1".into()),
        step: StepId("deploy".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

fn overlay_spec() -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), "true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        run_as_user: None,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(600),
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        forge_token: None,
        sandbox: None,
        registry_auth: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: Some(json!({
            "spec": { "nodeSelector": { "pool": "not a label value!" } }
        })),
        oidc_token: None,
        resolved_secrets: Default::default(),
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
    }
}

/// RT-R3 mutation 5: the API refuses the Pod and the Pod without the
/// overlay passes a dry run, so the overlay is the cause: `Refused`. The
/// dry run is of the Pod without the overlay (the fake refuses one that
/// carries it), and stores nothing.
#[tokio::test]
async fn an_overlay_the_api_refuses_fails_the_step() {
    let (url, fake) = fake_api(Fake::default()).await;
    let err = executor(&url)
        .launch(&step(), &overlay_spec())
        .await
        .expect_err("refused");
    assert!(
        matches!(&err, ExecError::Refused(m) if m.contains("k8s_overlay")),
        "{err:?}"
    );
    let creates = fake.creates.lock().unwrap();
    assert_eq!(creates.len(), 2, "one create, one dry run");
    assert!(!creates[0].0 && creates[1].0);
    assert!(!creates[1].1["spec"]["nodeSelector"]["pool"].is_string());
}

/// RT-R3 mutation 5: when the dry run itself fails (a webhook that rejects
/// dry runs, an API error), nothing shows the overlay is the cause: the
/// launch error stays a launch error, not the author's `Refused`.
#[tokio::test]
async fn a_failing_dry_run_does_not_blame_the_overlay() {
    let (url, fake) = fake_api(Fake {
        dry_run_fails: true,
        ..Fake::default()
    })
    .await;
    let err = executor(&url)
        .launch(&step(), &overlay_spec())
        .await
        .expect_err("the create fails");
    assert!(matches!(err, ExecError::Launch(_)), "{err:?}");
    assert_eq!(fake.creates.lock().unwrap().len(), 2, "the dry run ran");
}
