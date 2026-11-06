//! A Step's own secrets leave the Pod spec (ADR-0014, git-bug bcf2c11).
//!
//! `K8sExecutor::launch` against a fake API server: the Pod it POSTs holds
//! no secret value (a file mount and, for `env: true`, a `secretKeyRef`),
//! and the values go to the per-Pod `<pod>-secrets` Secret, owner-referenced
//! to the Pod. Hermetic: no cluster.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use scarab_engine::{Executor, ResolvedSecrets, RunId, StepId, StepRun, StepSpec, StepStatus};
use scarab_executor_k8s::K8sExecutor;

const NS: &str = "scarab-test";

#[derive(Default)]
struct Seen {
    pods: Vec<Value>,
    secrets: Vec<Value>,
}

async fn fake_api() -> (String, Arc<Mutex<Seen>>) {
    let seen: Arc<Mutex<Seen>> = Arc::default();
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
                |State(seen): State<Arc<Mutex<Seen>>>, Json(mut pod): Json<Value>| async move {
                    seen.lock().unwrap().pods.push(pod.clone());
                    pod["metadata"]["uid"] = json!("pod-uid-1");
                    (StatusCode::CREATED, Json(pod))
                },
            ),
        )
        .route(
            "/api/v1/namespaces/{ns}/secrets",
            post(
                |State(seen): State<Arc<Mutex<Seen>>>, Json(secret): Json<Value>| async move {
                    seen.lock().unwrap().secrets.push(secret.clone());
                    (StatusCode::CREATED, Json(secret))
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn spec() -> StepSpec {
    StepSpec {
        image: "argoproj/argocd".into(),
        command: vec!["sh".into(), "-c".into(), "argocd app sync web".into()],
        env: vec![],
        secrets: vec![
            "KUBECONFIG_DATA".into(),
            scarab_pipeline::SecretRef::file_and_env("ARGOCD_AUTH_TOKEN"),
        ],
        resolved_secrets: ResolvedSecrets(vec![
            ("KUBECONFIG_DATA".into(), "sekret-kubeconfig".into()),
            ("ARGOCD_AUTH_TOKEN".into(), "sekret-argo-token".into()),
        ]),
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
        forge_token: None,
    }
}

#[tokio::test]
async fn launch_puts_secret_values_in_a_per_pod_secret_and_none_in_the_pod() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (url, seen) = fake_api().await;
    let config = kube::Config::new(url.parse().unwrap());
    let client = kube::Client::try_from(config).expect("kube client");
    let exec = K8sExecutor::with_client(NS, client);
    let step = StepRun {
        run: RunId("r1".into()),
        step: StepId("deploy".into()),
        status: StepStatus::Running,
        attempts: vec![],
        needs: vec![],
        gate_kind: None,
    };

    exec.launch(&step, &spec()).await.expect("launch");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.pods.len(), 1, "one Pod: {:#?}", seen.pods);
    let pod = serde_json::to_string(&seen.pods[0]).unwrap();
    for value in ["sekret-kubeconfig", "sekret-argo-token"] {
        assert!(!pod.contains(value), "{value} is in the Pod: {pod}");
    }
    let pod_name = seen.pods[0]["metadata"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let container = &seen.pods[0]["spec"]["containers"][0];
    let env = container["env"].as_array().unwrap();
    assert!(
        env.iter().all(|e| e["name"] != "KUBECONFIG_DATA"),
        "{env:?}"
    );
    let argo = env
        .iter()
        .find(|e| e["name"] == "ARGOCD_AUTH_TOKEN")
        .unwrap();
    assert_eq!(
        argo["valueFrom"]["secretKeyRef"],
        json!({ "name": format!("{pod_name}-secrets"), "key": "ARGOCD_AUTH_TOKEN" })
    );
    assert!(container["volumeMounts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["mountPath"] == "/scarab/secrets" && m["readOnly"] == true));

    let secret = seen
        .secrets
        .iter()
        .find(|s| s["metadata"]["name"] == format!("{pod_name}-secrets").as_str())
        .unwrap_or_else(|| panic!("the per-Pod Secret was created: {:#?}", seen.secrets));
    assert_eq!(
        secret["stringData"],
        json!({ "KUBECONFIG_DATA": "sekret-kubeconfig", "ARGOCD_AUTH_TOKEN": "sekret-argo-token" })
    );
    let owner = &secret["metadata"]["ownerReferences"][0];
    assert_eq!(
        (owner["kind"].as_str(), owner["uid"].as_str()),
        (Some("Pod"), Some("pod-uid-1"))
    );
}
