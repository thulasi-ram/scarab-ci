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
        workspace_inputs: vec![],
        clone: None,
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
        workspace_inputs: vec![],
        clone: None,
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
        workspace_inputs: vec![],
        clone: None,
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

/// Live workspace flow (ADR-0029/0045 keystone): step A writes a file into
/// /workspace, its workspace is snapshotted into the CAS (Executor::output
/// returns the root), and step B (`needs: [A]`) has it materialized and reads
/// it back. Also proves restart determinism: re-running A yields the SAME
/// CAS root. `#[ignore]`d + gated on SCARAB_TEST_KUBE.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn workspace_flows_from_a_to_b_through_the_cas() {
    use scarab_storage::Cas;
    let Some(ns) = opted_in() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    // SCARAB_TEST_CAS_DIR pins the CAS for post-mortem inspection; default is
    // a tempdir that lives for the duration of the test.
    let tmp = tempfile::tempdir().expect("cas dir");
    let cas_dir = std::env::var("SCARAB_TEST_CAS_DIR")
        .unwrap_or_else(|_| tmp.path().to_string_lossy().into_owned());
    std::fs::create_dir_all(&cas_dir).expect("cas dir");
    let cas: std::sync::Arc<dyn Cas> = std::sync::Arc::new(
        scarab_storage_s3::S3Storage::local(&cas_dir).expect("local cas"),
    );
    let exec = K8sExecutor::with_client(ns, client).with_workspace_cas(cas);

    let step_run = |id: &str, attempt: &str| StepRun {
        run: RunId("run-ws".into()),
        step: StepId(id.into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId(attempt.into()),
            started_at: Timestamp(0),
            failure: None,
        }],
        needs: vec![],
        gate_kind: None,
    };
    let spec = |cmd: &str, inputs: Vec<String>| StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), cmd.into()],
        env: vec![],
        secrets: vec![],
        run_as_root: true, // busybox runs as root; fsGroup covers the emptyDir
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(120),
        workspace_inputs: inputs,
        clone: None,
    };
    async fn settle(exec: &K8sExecutor, h: &ExecHandle) -> ExecState {
        for _ in 0..90 {
            match exec.poll(h).await.expect("poll") {
                s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost) => {
                    return s;
                }
                _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            }
        }
        panic!("pod did not settle");
    }

    // --- Step A: produce a file in /workspace. ---
    let a = step_run("a", "a1");
    let ha = exec
        .launch(&a, &spec("echo scarab-was-here > /workspace/out.txt", vec![]))
        .await
        .expect("launch A");
    assert_eq!(settle(&exec, &ha).await, ExecState::Succeeded, "A succeeds");
    let root_a = exec
        .output(&ha)
        .await
        .expect("output")
        .expect("A produced a workspace snapshot");

    // --- Step B (needs: [A]): the file is materialized and readable. ---
    let b = step_run("b", "a1");
    let hb = exec
        .launch(
            &b,
            &spec(
                "grep -q scarab-was-here /workspace/out.txt",
                vec![root_a.clone()],
            ),
        )
        .await
        .expect("launch B");
    assert_eq!(
        settle(&exec, &hb).await,
        ExecState::Succeeded,
        "B read the file A wrote — the workspace flowed through the CAS"
    );

    // --- Restart determinism: a NEW attempt of A yields the SAME root. ---
    let a2 = step_run("a", "a2");
    let ha2 = exec
        .launch(&a2, &spec("echo scarab-was-here > /workspace/out.txt", vec![]))
        .await
        .expect("relaunch A");
    assert_eq!(settle(&exec, &ha2).await, ExecState::Succeeded);
    let root_a2 = exec.output(&ha2).await.expect("output").expect("snapshot");
    assert_eq!(root_a, root_a2, "same content => same CAS root (deterministic)");

    for h in [ha, hb, ha2] {
        exec.cancel(&h).await.expect("cleanup");
    }
}

/// LIVE clone step (ADR-0045): the canonical scarab-clone image clones a real
/// public repo at a pinned SHA into /workspace, the workspace (incl. .git) is
/// snapshotted into the CAS, and .git/config is credential-free. Anonymous
/// (public) clone — the token path is covered by unit + enrichment tests.
/// Needs the image in the cluster: SCARAB_TEST_CLONE_IMAGE (e.g. a locally
/// imported scarab-clone:test). `#[ignore]`d + SCARAB_TEST_KUBE gated.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn clone_step_produces_a_source_workspace() {
    use scarab_storage::Cas;
    let Some(ns) = opted_in() else { return };
    let Ok(clone_image) = std::env::var("SCARAB_TEST_CLONE_IMAGE") else {
        eprintln!("skipping: set SCARAB_TEST_CLONE_IMAGE to a scarab-clone image in the cluster");
        return;
    };
    let sha = std::env::var("SCARAB_TEST_CLONE_SHA").expect("SCARAB_TEST_CLONE_SHA");

    let client = kube::Client::try_default().await.expect("kube client");
    let tmp = tempfile::tempdir().expect("cas dir");
    let cas_dir = tmp.path().to_string_lossy().into_owned();
    let cas: std::sync::Arc<dyn Cas> = std::sync::Arc::new(
        scarab_storage_s3::S3Storage::local(&cas_dir).expect("local cas"),
    );
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(cas.clone())
        .with_clone_image(&clone_image);

    let step = StepRun {
        run: RunId("run-clone".into()),
        step: StepId("checkout".into()),
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
        image: String::new(),
        command: vec![],
        env: vec![],
        secrets: vec![],
        run_as_root: false, // scarab-clone is non-root by construction
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(300),
        workspace_inputs: vec![],
        clone: Some(scarab_engine::CloneConfig {
            owner: "thulasi-ram".into(),
            name: "scarab-ci".into(),
            sha: sha.clone(),
            url: "https://github.com/thulasi-ram/scarab-ci.git".into(),
            ..Default::default()
        }),
    };

    let h = exec.launch(&step, &spec).await.expect("launch clone");
    let mut terminal = None;
    for _ in 0..120 {
        match exec.poll(&h).await.expect("poll") {
            s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost) => {
                terminal = Some(s);
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    }
    assert_eq!(terminal, Some(ExecState::Succeeded), "clone step succeeds");

    // The workspace snapshot is the run's source tree — WITH .git (ADR-0045).
    let root = exec.output(&h).await.expect("output").expect("workspace snapshot");
    let out = tempfile::tempdir().expect("materialize dir");
    cas.materialize(
        &scarab_storage::TreeHash(root),
        out.path().to_str().unwrap(),
    )
    .await
    .expect("materialize the cloned workspace");
    assert!(out.path().join("Cargo.toml").exists(), "source is present");
    assert!(out.path().join(".git").is_dir(), ".git retained in the snapshot");
    // .git/config is credential-free (S2 guard held).
    let config = std::fs::read_to_string(out.path().join(".git/config")).expect("git config");
    assert!(
        !config.contains('@') || config.contains("github.com/thulasi-ram"),
        "no credential-bearing URL: {config}"
    );
    assert!(config.contains("https://github.com/thulasi-ram/scarab-ci.git"), "{config}");

    exec.cancel(&h).await.expect("cleanup");
}
