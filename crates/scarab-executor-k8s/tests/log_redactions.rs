//! `K8sExecutor::log_redactions` and the overlay dry run against a fake API
//! server. Hermetic: no cluster.
//!
//! `log_redactions` reads back the secret values of a step's Pod so a
//! control plane that did not launch the step redacts its log. A read that
//! fails must never count as "the step has no secret values": the log tail
//! would then store them in clear.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use scarab_engine::ports::LogRedactions;
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, ExecError, Executor, RunId, StepId, StepRun, StepSpec,
    StepStatus, Timestamp,
};
use scarab_executor_k8s::{pod_name, K8sExecutor};

const NS: &str = "scarab-test";

/// How the fake API server answers a GET of one Secret.
#[derive(Clone)]
enum SecretAnswer {
    Value(&'static [u8]),
    NotFound,
    Forbidden,
}

#[derive(Default)]
struct Fake {
    /// The Pod a GET returns; `None` answers 404.
    pod: Option<Value>,
    /// Secret answers by name; a name not here answers 404.
    secrets: HashMap<String, SecretAnswer>,
    /// Secret names read, in order.
    secret_gets: Mutex<Vec<String>>,
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
        .route(
            "/api/v1/namespaces/{ns}/secrets/{name}",
            get(
                |State(fake): State<Arc<Fake>>, Path((_ns, name)): Path<(String, String)>| async move {
                    fake.secret_gets.lock().unwrap().push(name.clone());
                    match fake.secrets.get(&name) {
                        Some(SecretAnswer::Value(v)) => {
                            use base64::Engine as _;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(v);
                            (
                                StatusCode::OK,
                                Json(json!({
                                    "kind": "Secret", "apiVersion": "v1",
                                    "metadata": { "name": name, "namespace": NS },
                                    "data": { "TOKEN": b64 },
                                })),
                            )
                        }
                        Some(SecretAnswer::Forbidden) => status(
                            StatusCode::FORBIDDEN,
                            "Forbidden",
                            format!("secrets \"{name}\" is forbidden: cannot get secrets"),
                        ),
                        Some(SecretAnswer::NotFound) | None => status(
                            StatusCode::NOT_FOUND,
                            "NotFound",
                            format!("secrets \"{name}\" not found"),
                        ),
                    }
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

/// The step's Pod as the API returns it: it mounts `<pod>-secrets`, or no
/// Secret at all.
fn pod(with_secrets: bool) -> Value {
    let name = pod_name(&step());
    let volumes = if with_secrets {
        json!([{ "name": "scarab-secrets", "projected": { "sources": [
            { "secret": { "name": format!("{name}-secrets") } }
        ] } }])
    } else {
        json!([])
    };
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": name, "namespace": NS, "uid": "pod-uid-1" },
        "spec": {
            "containers": [{ "name": "step", "image": "busybox" }],
            "volumes": volumes,
        },
    })
}

fn secrets(answer: SecretAnswer) -> HashMap<String, SecretAnswer> {
    HashMap::from([(format!("{}-secrets", pod_name(&step())), answer)])
}

/// The Pod's Secret exists: its values are read back.
#[tokio::test]
async fn the_values_of_a_referenced_secret_are_read_back() {
    let (url, _) = fake_api(Fake {
        pod: Some(pod(true)),
        secrets: secrets(SecretAnswer::Value(b"sekret-db")),
        ..Fake::default()
    })
    .await;
    let answer = executor(&url).log_redactions(&step()).await.unwrap();
    assert_eq!(answer, LogRedactions::Values(vec![b"sekret-db".to_vec()]));
}

/// RT-R3 mutation 3: a Secret the API refuses to read (RBAC) is an error.
/// Read as "no values", the tail would store the step's secrets in clear.
#[tokio::test]
async fn a_secret_the_api_refuses_to_read_is_an_error() {
    let (url, _) = fake_api(Fake {
        pod: Some(pod(true)),
        secrets: secrets(SecretAnswer::Forbidden),
        ..Fake::default()
    })
    .await;
    let err = executor(&url)
        .log_redactions(&step())
        .await
        .expect_err("a refused read is not \"no values\"");
    assert!(err.to_string().contains("forbidden"), "{err}");
}

/// R3-1 / RT-R3-1: the Pod references a Secret that is gone (deleted when
/// the attempt settled). The answer is `Gone`, not "no values".
#[tokio::test]
async fn a_referenced_secret_that_is_gone_answers_gone() {
    let (url, _) = fake_api(Fake {
        pod: Some(pod(true)),
        secrets: secrets(SecretAnswer::NotFound),
        ..Fake::default()
    })
    .await;
    let answer = executor(&url).log_redactions(&step()).await.unwrap();
    assert_eq!(answer, LogRedactions::Gone);
}

/// A Pod that references no credential Secret has no values, and no Secret
/// is read.
#[tokio::test]
async fn a_pod_with_no_credential_secret_has_no_values() {
    let (url, fake) = fake_api(Fake {
        pod: Some(pod(false)),
        ..Fake::default()
    })
    .await;
    let answer = executor(&url).log_redactions(&step()).await.unwrap();
    assert_eq!(answer, LogRedactions::Values(Vec::new()));
    assert!(fake.secret_gets.lock().unwrap().is_empty());
}

/// No Pod: nothing says which Secrets it would mount. An error, not "no
/// values" (a launch writes the Pod before its Secrets).
#[tokio::test]
async fn a_missing_pod_is_an_error() {
    let (url, _) = fake_api(Fake::default()).await;
    let err = executor(&url)
        .log_redactions(&step())
        .await
        .expect_err("no Pod");
    assert!(err.to_string().contains("NotFound"), "{err}");
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
