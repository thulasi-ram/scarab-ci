//! A Step's own secrets leave the Pod spec (ADR-0014, git-bug bcf2c11), and
//! only a Pod made by a server that keeps a value ledger gets them (git-bug
//! 2d78464, 85b29e0).
//!
//! `K8sExecutor::launch` against a fake API server: the Pod it POSTs holds
//! no secret value (a file mount and, for `env: true`, a `secretKeyRef`),
//! carries the `scarab.io/value-ledger` label, and the values go to the
//! per-Pod `<pod>-v-secrets` Secret, owner-referenced to the Pod. A Pod
//! without the label (an earlier version made it) gets no Secret, and its
//! log is not read. No code path reads a Secret. Hermetic: no cluster.

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
    /// Secrets POSTed (created).
    secrets: Vec<Value>,
    /// Secrets PUT (replaced), by name.
    replaced: Vec<(String, Value)>,
    /// Secrets DELETEd, by name.
    deleted: Vec<String>,
    /// Secrets read (GET), by name: none may be.
    secret_gets: Vec<String>,
    /// Pods DELETEd, by name.
    pods_deleted: Vec<String>,
    /// Pod log reads, by name.
    log_reads: Vec<String>,
}

/// What the fake API server holds before the launch.
#[derive(Clone, Default)]
struct Cluster {
    /// The Pod a GET returns; `None` answers 404.
    pod: Option<Value>,
    /// Every Secret POST answers 409 (it exists).
    secrets_exist: bool,
}

struct Fake {
    seen: Mutex<Seen>,
    cluster: Cluster,
}

fn not_found(kind: &str, name: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "kind": "Status", "apiVersion": "v1", "metadata": {},
            "status": "Failure", "reason": "NotFound", "code": 404,
            "message": format!("{kind} \"{name}\" not found"),
        })),
    )
}

async fn fake_api(cluster: Cluster) -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake {
        seen: Mutex::default(),
        cluster,
    });
    let app = Router::new()
        .route(
            "/api/v1/namespaces/{ns}/pods/{name}",
            get(
                |State(fake): State<Arc<Fake>>, Path((_ns, name)): Path<(String, String)>| async move {
                    match &fake.cluster.pod {
                        Some(pod) => (StatusCode::OK, Json(pod.clone())),
                        None => not_found("pods", &name),
                    }
                },
            )
            .delete(
                |State(fake): State<Arc<Fake>>, Path((_ns, name)): Path<(String, String)>| async move {
                    fake.seen.lock().unwrap().pods_deleted.push(name.clone());
                    match &fake.cluster.pod {
                        Some(pod) => (StatusCode::OK, Json(pod.clone())),
                        None => not_found("pods", &name),
                    }
                },
            ),
        )
        .route(
            "/api/v1/namespaces/{ns}/pods/{name}/log",
            get(
                |State(fake): State<Arc<Fake>>, Path((_ns, name)): Path<(String, String)>| async move {
                    fake.seen.lock().unwrap().log_reads.push(name);
                    "a line\n"
                },
            ),
        )
        .route(
            "/api/v1/namespaces/{ns}/pods",
            post(
                |State(fake): State<Arc<Fake>>, Json(mut pod): Json<Value>| async move {
                    fake.seen.lock().unwrap().pods.push(pod.clone());
                    pod["metadata"]["uid"] = json!("pod-uid-1");
                    (StatusCode::CREATED, Json(pod))
                },
            ),
        )
        .route(
            "/api/v1/namespaces/{ns}/secrets",
            post(
                |State(fake): State<Arc<Fake>>, Json(secret): Json<Value>| async move {
                    if fake.cluster.secrets_exist {
                        let name = secret["metadata"]["name"].as_str().unwrap_or_default();
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "kind": "Status", "apiVersion": "v1", "metadata": {},
                                "status": "Failure", "reason": "AlreadyExists", "code": 409,
                                "message": format!("secrets \"{name}\" already exists"),
                            })),
                        );
                    }
                    fake.seen.lock().unwrap().secrets.push(secret.clone());
                    (StatusCode::CREATED, Json(secret))
                },
            ),
        )
        .route(
            "/api/v1/namespaces/{ns}/secrets/{name}",
            get(
                |State(fake): State<Arc<Fake>>, Path((_ns, name)): Path<(String, String)>| async move {
                    fake.seen.lock().unwrap().secret_gets.push(name.clone());
                    not_found("secrets", &name)
                },
            )
            .put(
                |State(fake): State<Arc<Fake>>,
                 Path((_ns, name)): Path<(String, String)>,
                 Json(secret): Json<Value>| async move {
                    fake.seen
                        .lock()
                        .unwrap()
                        .replaced
                        .push((name, secret.clone()));
                    (StatusCode::OK, Json(secret))
                },
            )
            .delete(
                |State(fake): State<Arc<Fake>>, Path((_ns, name)): Path<(String, String)>| async move {
                    fake.seen.lock().unwrap().deleted.push(name.clone());
                    (
                        StatusCode::OK,
                        Json(json!({
                            "kind": "Secret", "apiVersion": "v1",
                            "metadata": { "name": name, "namespace": NS },
                        })),
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
        attempts: vec![],
        needs: vec![],
        gate_kind: None,
    }
}

/// The per-Pod Secret carries the values in `data` (base64 of the bytes as
/// stored), never `stringData`, which must be text.
fn assert_secret_data(secret: &Value) {
    use base64::Engine as _;
    let b64 = |v: &[u8]| base64::engine::general_purpose::STANDARD.encode(v);
    assert_eq!(secret.get("stringData"), None, "{secret:#}");
    assert_eq!(
        secret["data"],
        json!({
            "KUBECONFIG_DATA": b64(b"sekret-kubeconfig\xff\x00"),
            "ARGOCD_AUTH_TOKEN": b64(b"sekret-argo-token"),
        })
    );
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
            // Not UTF-8: the Secret must carry the bytes (git-bug c1f61c9).
            (
                "KUBECONFIG_DATA".into(),
                b"sekret-kubeconfig\xff\x00".to_vec(),
            ),
            ("ARGOCD_AUTH_TOKEN".into(), "sekret-argo-token".into()),
        ]),
        run_as_root: false,
        run_as_user: None,
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
        sandbox: None,
        registry_auth: None,
    }
}

#[tokio::test]
async fn launch_puts_secret_values_in_a_per_pod_secret_and_none_in_the_pod() {
    let (url, fake) = fake_api(Cluster::default()).await;
    let exec = executor(&url);

    exec.launch(&step(), &spec()).await.expect("launch");

    let seen = fake.seen.lock().unwrap();
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
        json!({ "name": format!("{pod_name}-v-secrets"), "key": "ARGOCD_AUTH_TOKEN" })
    );
    assert!(container["volumeMounts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["mountPath"] == "/scarab/secrets" && m["readOnly"] == true));

    let secret = seen
        .secrets
        .iter()
        .find(|s| s["metadata"]["name"] == format!("{pod_name}-v-secrets").as_str())
        .unwrap_or_else(|| panic!("the per-Pod Secret was created: {:#?}", seen.secrets));
    assert_secret_data(secret);
    let owner = &secret["metadata"]["ownerReferences"][0];
    assert_eq!(
        (owner["kind"].as_str(), owner["uid"].as_str()),
        (Some("Pod"), Some("pod-uid-1"))
    );
    assert_eq!(
        seen.pods[0]["metadata"]["labels"]["scarab.io/value-ledger"],
        json!("1")
    );
}

/// The Attempt has ended: the values leave etcd although the Pod is kept for
/// its logs. The Secret is not left to the owner reference alone.
#[tokio::test]
async fn the_per_pod_secret_is_deleted_when_the_attempt_ends() {
    let (url, fake) = fake_api(Cluster::default()).await;
    let exec = executor(&url);
    let handle = exec.launch(&step(), &spec()).await.expect("launch");
    assert!(fake.seen.lock().unwrap().deleted.is_empty());

    // The Attempt-end hook: the forge credential decorator calls it on every
    // terminal poll and on cancel.
    exec.delete_unit_secrets(&handle).await.expect("delete");

    let deleted = fake.seen.lock().unwrap().deleted.clone();
    for suffix in ["-v-secrets", "-v-token", "-secrets", "-token"] {
        assert!(
            deleted.contains(&format!("{}{suffix}", handle.0)),
            "{suffix}: {deleted:?}"
        );
    }
}

/// A Pod as the API returns it: with or without the value-ledger label, and
/// with its step container running or never started.
fn existing_pod(labelled: bool, started: bool) -> Value {
    let pod_name = scarab_executor_k8s::pod_name(&step());
    let labels = if labelled {
        json!({ "scarab.io/value-ledger": "1" })
    } else {
        json!({})
    };
    let statuses = if started {
        json!([{ "name": "step", "ready": true, "restartCount": 0, "image": "i",
                 "imageID": "", "state": { "running": {} } }])
    } else {
        json!([{ "name": "step", "ready": false, "restartCount": 0, "image": "i",
                 "imageID": "", "state": { "waiting": { "reason": "ContainerCreating" } } }])
    };
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": pod_name, "namespace": NS, "uid": "pod-uid-live",
                      "labels": labels },
        "spec": { "nodeName": "node-1", "containers": [{ "name": "step", "image": "i" }] },
        "status": { "phase": "Running", "containerStatuses": statuses },
    })
}

/// I1a (2d78464): a re-drive adopting a running Pod an earlier version made
/// writes no Secret: nothing this server mints reaches a Pod whose log it
/// does not store. Kills: a replace on an unlabelled Pod.
#[tokio::test]
async fn adopting_an_unlabelled_pod_writes_no_secret() {
    let (url, fake) = fake_api(Cluster {
        pod: Some(existing_pod(false, true)),
        secrets_exist: true,
    })
    .await;
    let handle = executor(&url)
        .launch(&step(), &spec())
        .await
        .expect("adopted");
    assert_eq!(handle.0, scarab_executor_k8s::pod_name(&step()));
    let seen = fake.seen.lock().unwrap();
    assert!(seen.secrets.is_empty(), "{:#?}", seen.secrets);
    assert!(seen.replaced.is_empty(), "{:#?}", seen.replaced);
    assert!(seen.pods.is_empty() && seen.pods_deleted.is_empty());
}

/// Red team R4-1: an unlabelled Pod none of whose containers started (an
/// earlier leader stopped between the Pod create and its Secret writes, so
/// it waits on Secrets that never come) is deleted, and the launch fails to
/// be retried with a labelled Pod; no Secret is written for it. Kills:
/// adopting it as is (it sits in FailedMount until the provisioning
/// deadline).
#[tokio::test]
async fn an_unlabelled_pod_that_never_started_is_replaced() {
    let (url, fake) = fake_api(Cluster {
        pod: Some(existing_pod(false, false)),
        secrets_exist: false,
    })
    .await;
    let err = executor(&url)
        .launch(&step(), &spec())
        .await
        .expect_err("the launch is retried");
    assert!(
        matches!(err, scarab_engine::ExecError::Launch(_)),
        "{err:?}"
    );
    let seen = fake.seen.lock().unwrap();
    assert_eq!(
        seen.pods_deleted,
        vec![scarab_executor_k8s::pod_name(&step())]
    );
    assert!(seen.secrets.is_empty() && seen.replaced.is_empty());
}

/// I1 (85b29e0 iv): the log of a Pod without the label is not read: the
/// open answers `NotLedgerComplete` from the Pod it checked. A labelled
/// Pod's log opens. Kills: the label check removed.
#[tokio::test]
async fn the_log_of_an_unlabelled_pod_is_not_read() {
    let (url, fake) = fake_api(Cluster {
        pod: Some(existing_pod(false, true)),
        secrets_exist: false,
    })
    .await;
    let err = executor(&url)
        .log_stream(&step(), scarab_engine::LogFrom::Start)
        .await
        .err()
        .expect("not read");
    assert!(
        matches!(err, scarab_engine::ExecError::NotLedgerComplete(_)),
        "{err:?}"
    );
    assert!(fake.seen.lock().unwrap().log_reads.is_empty());

    let (url, fake) = fake_api(Cluster {
        pod: Some(existing_pod(true, true)),
        secrets_exist: false,
    })
    .await;
    let mut chunks = executor(&url)
        .log_stream(&step(), scarab_engine::LogFrom::Start)
        .await
        .expect("opens")
        .expect("a log source");
    assert_eq!(
        chunks.next_chunk().await.unwrap(),
        Some(b"a line\n".to_vec())
    );
    assert_eq!(fake.seen.lock().unwrap().log_reads.len(), 1);
}

/// Q3 = B: no code path reads a Secret's value: not a launch, a re-drive,
/// a log open, a source-state question, nor the Attempt-end delete. The
/// fake records every Secret GET. Kills: a read-back left in place.
#[tokio::test]
async fn no_code_path_reads_a_secret_value() {
    for cluster in [
        Cluster::default(),
        Cluster {
            pod: Some(existing_pod(true, true)),
            secrets_exist: true,
        },
        Cluster {
            pod: Some(existing_pod(false, true)),
            secrets_exist: true,
        },
    ] {
        let (url, fake) = fake_api(cluster).await;
        let exec = executor(&url);
        let handle = exec.launch(&step(), &spec()).await.expect("launch");
        let _ = exec
            .log_stream(&step(), scarab_engine::LogFrom::Start)
            .await;
        let _ = exec
            .log_source_state(&scarab_engine::LogSourceRef::Step(step()))
            .await;
        exec.delete_unit_secrets(&handle).await.unwrap();
        let gets = fake.seen.lock().unwrap().secret_gets.clone();
        assert!(gets.is_empty(), "Secrets read: {gets:?}");
    }
}

/// A re-drive after a crash between creating the Pod and its Secret, or after
/// both exist: the launch adopts the live Pod and replaces the Secret with
/// this launch's values, owned by the live Pod's uid.
#[tokio::test]
async fn a_redrive_replaces_the_per_pod_secret_owned_by_the_live_pod() {
    let pod_name = scarab_executor_k8s::pod_name(&step());
    let (url, fake) = fake_api(Cluster {
        pod: Some(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": pod_name, "namespace": NS, "uid": "pod-uid-live",
                          "labels": { "scarab.io/value-ledger": "1" } },
        })),
        secrets_exist: true,
    })
    .await;
    let exec = executor(&url);

    let handle = exec.launch(&step(), &spec()).await.expect("launch");

    let seen = fake.seen.lock().unwrap();
    assert_eq!(handle.0, pod_name);
    assert!(seen.pods.is_empty(), "no second Pod: {:#?}", seen.pods);
    let name = format!("{pod_name}-v-secrets");
    let (_, secret) = seen
        .replaced
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("{name} was replaced: {:#?}", seen.replaced));
    assert_secret_data(secret);
    let owner = &secret["metadata"]["ownerReferences"][0];
    assert_eq!(
        (owner["kind"].as_str(), owner["uid"].as_str()),
        (Some("Pod"), Some("pod-uid-live"))
    );
}
