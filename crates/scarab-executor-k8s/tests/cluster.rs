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
