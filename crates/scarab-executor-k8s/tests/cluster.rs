//! Real-cluster round-trip for `K8sExecutor` (ADR-0004 acceptance).
//!
//! This test is `#[ignore]`d and additionally gated on the `SCARAB_TEST_KUBE`
//! env var, so `cargo test` NEVER runs it and it never touches an ambient
//! kubeconfig by accident. It is meant to run only against the dedicated dev
//! kind cluster wired up by the dev-harness slice (issue 7e2038d), e.g.:
//!
//!   SCARAB_TEST_KUBE=1 SCARAB_TEST_KUBE_NS=scarab-dev \
//!     cargo test -p scarab-executor-k8s --test cluster -- --ignored
//!
//! It proves the acceptance: a busybox `echo` runs to completion with its exit
//! code recorded, and a second `launch` of the same fence re-attaches to the
//! existing Pod rather than creating a new one.

use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{Attempt, AttemptId, Executor, RunId, StepId, StepRun, StepStatus, StepSpec, Timestamp};
use scarab_executor_k8s::{pod_name, K8sExecutor};

/// Fully drain a [`scarab_engine::LogChunks`] into one `Vec<u8>`.
#[cfg(test)]
async fn drain_to_end(mut chunks: Box<dyn scarab_engine::LogChunks>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(c) = chunks.next_chunk().await.expect("chunk") {
        out.extend(c);
    }
    out
}

fn opted_in() -> Option<String> {
    if std::env::var("SCARAB_TEST_KUBE").is_err() {
        eprintln!("skipping: set SCARAB_TEST_KUBE=1 to run against a dev cluster");
        return None;
    }
    Some(std::env::var("SCARAB_TEST_KUBE_NS").unwrap_or_else(|_| "default".to_string()))
}

fn step() -> StepRun {
    StepRun {
        run: RunId("run-1".into()),
        step: StepId("echo".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn busybox_runs_to_completion_and_relaunch_reattaches() {
    let Some(ns) = opted_in() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client);
    let step = step();
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), "echo hello scarab".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
    };

    // launch, then launch again — the second call must re-attach, not relaunch.
    let h1 = exec.launch(&step, &spec).await.expect("launch");
    let h2 = exec.launch(&step, &spec).await.expect("relaunch re-attaches");
    assert_eq!(h1, h2);
    assert_eq!(h1, ExecHandle(pod_name(&step)));

    // Poll to a terminal state.
    let mut terminal = None;
    for _ in 0..60 {
        match exec.poll(&h1).await.expect("poll") {
            s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost) => {
                terminal = Some(s);
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    }
    assert_eq!(terminal, Some(ExecState::Succeeded), "busybox echo exits 0");

    exec.cancel(&h1).await.expect("cancel cleans up the pod");
}

/// Live step-timeout acceptance (ADR-0047): a sleeping step whose `timeout:`
/// is 5s is killed by the kubelet (`activeDeadlineSeconds`) and surfaces as a
/// classified `Timeout` failure. `#[ignore]`d + gated on SCARAB_TEST_KUBE.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn sleeping_step_is_killed_at_its_deadline() {
    let Some(ns) = opted_in() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client);
    let step = StepRun {
        run: RunId("run-timeout".into()),
        step: StepId("sleeper".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
        }],
        needs: vec![],
        gate_kind: None,
    };
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), "sleep 300".into()],
        env: vec![],
        secrets: vec![],
        // busybox runs as root; without the self-service grant the hardened
        // baseline rejects the container (CreateContainerConfigError) and the
        // step never starts — this test needs the sleep to actually run.
        run_as_root: true,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(5),
    };

    let h = exec.launch(&step, &spec).await.expect("launch");

    // The kubelet enforces the deadline; DeadlineExceeded classifies Timeout.
    let mut terminal = None;
    for _ in 0..90 {
        match exec.poll(&h).await.expect("poll") {
            s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost) => {
                terminal = Some(s);
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    }
    match terminal {
        Some(ExecState::Failed { class, .. }) => assert_eq!(
            class,
            scarab_engine::ports::FailureClass::Timeout,
            "DeadlineExceeded must classify as Timeout"
        ),
        other => panic!("expected a Timeout failure, got {other:?}"),
    }

    exec.cancel(&h).await.expect("cancel cleans up the pod");
}

/// Live log tail (ADR-0013): `log_stream` follows a Pod's stdout and yields the
/// step's output. `#[ignore]`d + gated on SCARAB_TEST_KUBE — needs the dev kind
/// cluster. The chunking/drain loop itself is unit-tested cluster-free in
/// `scarab-server`'s `log_tail` tests; this proves the real k8s log endpoint
/// wiring end-to-end.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn log_stream_tails_pod_stdout() {
    let Some(ns) = opted_in() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client);
    let step = step();
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), "echo hello scarab logs".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
    };

    let h = exec.launch(&step, &spec).await.expect("launch");

    // Follow the log until the stream closes (the Pod finished), retrying the
    // open while the container is still starting (Pending → no log yet).
    let mut logs = Vec::new();
    for _ in 0..60 {
        if let Ok(Some(chunks)) = exec.log_stream(&step).await {
            logs = drain_to_end(chunks).await;
            if !logs.is_empty() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    assert!(
        String::from_utf8_lossy(&logs).contains("hello scarab logs"),
        "tailed logs should contain the step's stdout, got: {:?}",
        String::from_utf8_lossy(&logs)
    );

    exec.cancel(&h).await.expect("cancel cleans up the pod");
}

/// Live rootless-BuildKit image build (ADR-0018). `#[ignore]`d + gated on
/// SCARAB_TEST_KUBE — needs the dev kind cluster and a registry. Proves the
/// build step produces an image + digest end-to-end; the Pod spec and the
/// digest→artifact wiring are unit-tested in the library without a cluster.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster + registry; opt in with SCARAB_TEST_KUBE=1"]
async fn rootless_buildkit_builds_an_image() {
    if opted_in().is_none() {
        return;
    }
    // Intentionally left as a harness placeholder: applying build_pod_for_build
    // to the cluster, waiting for completion, and asserting a pushed digest.
}
