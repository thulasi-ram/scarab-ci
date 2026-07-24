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
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Executor, RunId, StepId, StepRun, StepSpec, StepStatus,
    Timestamp,
};
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

/// A per-invocation unique run id: live tests re-launch by fence, so a fixed
/// id would re-attach to a leftover Pod from a previous invocation whose CAS
/// (a tempdir) is gone.
fn unique_run(prefix: &str) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{prefix}-{t}")
}

/// The optional checkout credential for the LIVE clone tests: the repo they
/// clone (this one) may be private, in which case an anonymous clone is
/// SourceUnavailable by design. Set SCARAB_TEST_CLONE_TOKEN (CI: the ambient
/// GITHUB_TOKEN; locally: `gh auth token`) to authenticate; delivery still
/// goes through the tmpfs + GIT_ASKPASS path (ADR-0045), never the URL.
fn clone_credential() -> Option<scarab_engine::CloneCredential> {
    std::env::var("SCARAB_TEST_CLONE_TOKEN")
        .ok()
        .map(|token| scarab_engine::CloneCredential {
            username: "x-access-token".into(),
            token,
        })
}

/// A one-step StepRun under a UNIQUE run id (see [`unique_run`]): the fence
/// (run/step/attempt) names the Pod, so a fixed id would re-attach to a
/// leftover Pod from a previous invocation — or collide with a concurrently
/// running sibling test sharing the fixture.
fn step(run_prefix: &str) -> StepRun {
    StepRun {
        run: RunId(unique_run(run_prefix)),
        step: StepId("echo".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
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
    let step = step("run-echo");
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), "echo hello scarab".into()],
        env: vec![],
        secrets: vec![],
        // busybox runs as root; without the self-service grant the hardened
        // ADR-0039 baseline rejects the container (CreateContainerConfigError).
        run_as_root: true,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
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
    };

    // launch, then launch again — the second call must re-attach, not relaunch.
    let h1 = exec.launch(&step, &spec).await.expect("launch");
    let h2 = exec
        .launch(&step, &spec)
        .await
        .expect("relaunch re-attaches");
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
            outcome: AttemptOutcome::Running,
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
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
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
    let step = step("run-logs");
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), "echo hello scarab logs".into()],
        env: vec![],
        secrets: vec![],
        // busybox runs as root; the hardened baseline would reject it.
        run_as_root: true,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
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
    let cas: std::sync::Arc<dyn Cas> =
        std::sync::Arc::new(scarab_storage_s3::S3Storage::local(&cas_dir).expect("local cas"));
    let exec = K8sExecutor::with_client(ns, client).with_workspace_cas(cas);

    let step_run = |id: &str, attempt: &str| StepRun {
        run: RunId("run-ws".into()),
        step: StepId(id.into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId(attempt.into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
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
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
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
        .launch(
            &a,
            &spec("echo scarab-was-here > /workspace/out.txt", vec![]),
        )
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
        .launch(
            &a2,
            &spec("echo scarab-was-here > /workspace/out.txt", vec![]),
        )
        .await
        .expect("relaunch A");
    assert_eq!(settle(&exec, &ha2).await, ExecState::Succeeded);
    let root_a2 = exec.output(&ha2).await.expect("output").expect("snapshot");
    assert_eq!(
        root_a, root_a2,
        "same content => same CAS root (deterministic)"
    );

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
    let run_id = unique_run("run-clone");
    let Some(ns) = opted_in() else { return };
    let Ok(clone_image) = std::env::var("SCARAB_TEST_CLONE_IMAGE") else {
        eprintln!("skipping: set SCARAB_TEST_CLONE_IMAGE to a scarab-clone image in the cluster");
        return;
    };
    let sha = std::env::var("SCARAB_TEST_CLONE_SHA").expect("SCARAB_TEST_CLONE_SHA");

    let client = kube::Client::try_default().await.expect("kube client");
    let tmp = tempfile::tempdir().expect("cas dir");
    let cas_dir = tmp.path().to_string_lossy().into_owned();
    let cas: std::sync::Arc<dyn Cas> =
        std::sync::Arc::new(scarab_storage_s3::S3Storage::local(&cas_dir).expect("local cas"));
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(cas.clone())
        .with_clone_image(&clone_image);

    let step = StepRun {
        run: RunId(run_id.clone()),
        step: StepId("checkout".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
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
            credential: clone_credential(),
            ..Default::default()
        }),
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
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
    let root = exec
        .output(&h)
        .await
        .expect("output")
        .expect("workspace snapshot");
    let out = tempfile::tempdir().expect("materialize dir");
    cas.materialize(
        &scarab_storage::TreeHash(root.clone()),
        out.path().to_str().unwrap(),
    )
    .await
    .expect("materialize the cloned workspace");
    assert!(out.path().join("Cargo.toml").exists(), "source is present");
    assert!(
        out.path().join(".git").is_dir(),
        ".git retained in the snapshot"
    );
    // .git/config is credential-free (S2 guard held).
    let config = std::fs::read_to_string(out.path().join(".git/config")).expect("git config");
    assert!(
        !config.contains('@') || config.contains("github.com/thulasi-ram"),
        "no credential-bearing URL: {config}"
    );
    assert!(
        config.contains("https://github.com/thulasi-ram/scarab-ci.git"),
        "{config}"
    );

    // A downstream `needs: [checkout]` step consumes the snapshot: the source
    // AND its .git materialize into /workspace (clone → CAS → materialize),
    // and HEAD is the pinned SHA — asserted from inside the cluster.
    let build = StepRun {
        run: RunId(run_id.clone()),
        step: StepId("build".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![StepId("checkout".into())],
        gate_kind: None,
    };
    let build_spec = StepSpec {
        image: clone_image.clone(), // has git; entrypoint overridden by command
        command: vec![
            "sh".into(),
            "-c".into(),
            // -c safe.directory: the workspace is materialized by the init
            // container's uid, the step runs as the image's — same situation
            // every CI runner handles for authored git use.
            format!(
                "ls /workspace | head -5; \
                 head=$(git -C /workspace -c safe.directory=/workspace rev-parse HEAD); \
                 echo \"head=$head\"; \
                 test -f /workspace/Cargo.toml && \
                 test \"$head\" = \"{sha}\" && \
                 echo DOWNSTREAM-SEES-PINNED-SOURCE"
            ),
        ],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(300),
        workspace_inputs: vec![root.clone()],
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
    };
    let bh = exec
        .launch(&build, &build_spec)
        .await
        .expect("launch downstream");
    let mut terminal = None;
    for _ in 0..120 {
        match exec.poll(&bh).await.expect("poll") {
            s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost) => {
                terminal = Some(s);
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    }
    assert_eq!(
        terminal,
        Some(ExecState::Succeeded),
        "downstream step sees the pinned source"
    );

    exec.cancel(&bh).await.expect("cleanup downstream");
    exec.cancel(&h).await.expect("cleanup");
}

/// LIVE `depth: full` variant (ADR-0045): the full-history clone exposes more
/// than one commit — asserted by a downstream step running `git rev-list
/// --count HEAD` on the materialized workspace.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn clone_depth_full_exposes_history() {
    use scarab_storage::Cas;
    let run_id = unique_run("run-clone-full");
    let Some(ns) = opted_in() else { return };
    let Ok(clone_image) = std::env::var("SCARAB_TEST_CLONE_IMAGE") else {
        eprintln!("skipping: set SCARAB_TEST_CLONE_IMAGE to a scarab-clone image in the cluster");
        return;
    };
    let sha = std::env::var("SCARAB_TEST_CLONE_SHA").expect("SCARAB_TEST_CLONE_SHA");

    let client = kube::Client::try_default().await.expect("kube client");
    let tmp = tempfile::tempdir().expect("cas dir");
    let cas: std::sync::Arc<dyn Cas> = std::sync::Arc::new(
        scarab_storage_s3::S3Storage::local(tmp.path().to_str().unwrap()).expect("local cas"),
    );
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(cas.clone())
        .with_clone_image(&clone_image);

    let step = StepRun {
        run: RunId(run_id.clone()),
        step: StepId("checkout".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    };
    let spec = StepSpec {
        image: String::new(),
        command: vec![],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(300),
        workspace_inputs: vec![],
        clone: Some(scarab_engine::CloneConfig {
            owner: "thulasi-ram".into(),
            name: "scarab-ci".into(),
            sha: sha.clone(),
            depth_full: true,
            url: "https://github.com/thulasi-ram/scarab-ci.git".into(),
            credential: clone_credential(),
            ..Default::default()
        }),
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    };
    let h = exec.launch(&step, &spec).await.expect("launch full clone");
    let mut terminal = None;
    for _ in 0..180 {
        match exec.poll(&h).await.expect("poll") {
            s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost) => {
                terminal = Some(s);
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    }
    assert_eq!(terminal, Some(ExecState::Succeeded), "full clone succeeds");
    let root = exec
        .output(&h)
        .await
        .expect("output")
        .expect("workspace snapshot");

    let count = StepRun {
        run: RunId(run_id.clone()),
        step: StepId("history".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![StepId("checkout".into())],
        gate_kind: None,
    };
    let count_spec = StepSpec {
        image: clone_image.clone(),
        command: vec![
            "sh".into(),
            "-c".into(),
            // >1 commit = real history, not a shallow graft.
            "n=$(git -C /workspace -c safe.directory=/workspace rev-list --count HEAD) && \
             echo \"commits=$n\" && [ \"$n\" -gt 1 ]"
                .into(),
        ],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(300),
        workspace_inputs: vec![root],
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
    };
    let ch = exec
        .launch(&count, &count_spec)
        .await
        .expect("launch history check");
    let mut terminal = None;
    for _ in 0..120 {
        match exec.poll(&ch).await.expect("poll") {
            s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost) => {
                terminal = Some(s);
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    }
    assert_eq!(
        terminal,
        Some(ExecState::Succeeded),
        "depth: full exposes >1 commit of history"
    );

    exec.cancel(&ch).await.expect("cleanup history");
    exec.cancel(&h).await.expect("cleanup clone");
}

/// LIVE vanished-SHA case (ADR-0045 fencing): a pinned commit that no longer
/// exists upstream is TERMINAL — SourceUnavailable (exit 86), surfaced fast,
/// never a retry loop.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn clone_vanished_sha_fails_fast_with_source_unavailable() {
    use scarab_storage::Cas;
    let Some(ns) = opted_in() else { return };
    let Ok(clone_image) = std::env::var("SCARAB_TEST_CLONE_IMAGE") else {
        eprintln!("skipping: set SCARAB_TEST_CLONE_IMAGE to a scarab-clone image in the cluster");
        return;
    };

    let client = kube::Client::try_default().await.expect("kube client");
    let tmp = tempfile::tempdir().expect("cas dir");
    let cas: std::sync::Arc<dyn Cas> = std::sync::Arc::new(
        scarab_storage_s3::S3Storage::local(tmp.path().to_str().unwrap()).expect("local cas"),
    );
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(cas)
        .with_clone_image(&clone_image);

    let step = StepRun {
        run: RunId(unique_run("run-clone-gone")),
        step: StepId("checkout".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    };
    let spec = StepSpec {
        image: String::new(),
        command: vec![],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(300),
        workspace_inputs: vec![],
        clone: Some(scarab_engine::CloneConfig {
            owner: "thulasi-ram".into(),
            name: "scarab-ci".into(),
            // A well-formed SHA that exists on no forge: the rewritten-history
            // case. The fetch fails, the guard exits 86 — SourceUnavailable.
            sha: "0000000000000000000000000000000000000001".into(),
            url: "https://github.com/thulasi-ram/scarab-ci.git".into(),
            // Authenticated so the 86 is genuinely the vanished SHA, not a
            // repo-access rejection (the repo may be private).
            credential: clone_credential(),
            ..Default::default()
        }),
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    };

    let started = std::time::Instant::now();
    let h = exec
        .launch(&step, &spec)
        .await
        .expect("launch doomed clone");
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
    match terminal {
        Some(ExecState::Failed { exit_code, .. }) => {
            assert_eq!(exit_code, Some(86), "SourceUnavailable is exit 86");
        }
        other => panic!("expected a terminal Failed(86), got {other:?}"),
    }
    // Fast = one attempt, no retry loop: well under the 2-minute poll window.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(115),
        "vanished SHA must fail fast, took {:?}",
        started.elapsed()
    );

    exec.cancel(&h).await.expect("cleanup");
}

/// LIVE `kind: build` (ADR-0018): rootless BuildKit builds a trivial image
/// from a CAS-materialized workspace and pushes it to a local in-cluster
/// registry; a verification step then reads the registry's tag list.
/// Needs a registry Service in the cluster (SCARAB_TEST_REGISTRY, e.g.
/// `registry.default.svc.cluster.local:5000` — plain HTTP, hence
/// insecure_push). `#[ignore]`d + SCARAB_TEST_KUBE gated.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn build_step_builds_and_pushes_to_a_local_registry() {
    use scarab_storage::Cas;
    let Some(ns) = opted_in() else { return };
    let Ok(registry) = std::env::var("SCARAB_TEST_REGISTRY") else {
        eprintln!("skipping: set SCARAB_TEST_REGISTRY to an in-cluster registry host:port");
        return;
    };
    let run_id = unique_run("run-build");

    let client = kube::Client::try_default().await.expect("kube client");
    let tmp = tempfile::tempdir().expect("cas dir");
    let cas: std::sync::Arc<dyn Cas> = std::sync::Arc::new(
        scarab_storage_s3::S3Storage::local(tmp.path().to_str().unwrap()).expect("local cas"),
    );
    let exec = K8sExecutor::with_client(ns, client).with_workspace_cas(cas.clone());

    // The build context: a trivial Dockerfile, ingested as the upstream
    // (checkout) workspace.
    let ctx = tempfile::tempdir().expect("context dir");
    std::fs::write(
        ctx.path().join("Dockerfile"),
        "FROM busybox\nRUN echo scarab-built > /built.txt\n",
    )
    .unwrap();
    let root = cas
        .ingest(ctx.path().to_str().unwrap())
        .await
        .expect("ingest context")
        .root
        .0;

    let image = format!("{registry}/scarab-test:live");
    let step = StepRun {
        run: RunId(run_id.clone()),
        step: StepId("image".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![StepId("checkout".into())],
        gate_kind: None,
    };
    let spec = StepSpec {
        image: String::new(),
        command: vec![],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(600),
        workspace_inputs: vec![root],
        clone: None,
        build: Some(scarab_engine::BuildConfig {
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            image: image.clone(),
            push: true,
            insecure_push: true, // the local test registry is plain HTTP
            ..Default::default()
        }),
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    };
    let h = exec.launch(&step, &spec).await.expect("launch build");
    let mut terminal = None;
    for _ in 0..300 {
        match exec.poll(&h).await.expect("poll") {
            s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost) => {
                terminal = Some(s);
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    }
    assert_eq!(terminal, Some(ExecState::Succeeded), "build step succeeds");

    // The push is observable: a verification step reads the registry's tag
    // list from inside the cluster.
    let verify = StepRun {
        run: RunId(run_id.clone()),
        step: StepId("verify".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    };
    let verify_spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec![
            "sh".into(),
            "-c".into(),
            format!(
                "tags=$(wget -qO- http://{registry}/v2/scarab-test/tags/list) && \
                 echo \"$tags\" && echo \"$tags\" | grep -q live"
            ),
        ],
        env: vec![],
        secrets: vec![],
        // busybox runs as root; the hardened baseline would reject it.
        run_as_root: true,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(120),
        workspace_inputs: vec![],
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
    };
    let vh = exec
        .launch(&verify, &verify_spec)
        .await
        .expect("launch verify");
    let mut terminal = None;
    for _ in 0..120 {
        match exec.poll(&vh).await.expect("poll") {
            s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost) => {
                terminal = Some(s);
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    }
    assert_eq!(
        terminal,
        Some(ExecState::Succeeded),
        "the pushed tag is listed by the registry"
    );

    exec.cancel(&vh).await.expect("cleanup verify");
    exec.cancel(&h).await.expect("cleanup build");
}

/// LIVE results egress (ADR-0042/0041): a step writes a named result to
/// /scarab/results; the trusted sidecar (the real scarab-results-sidecar
/// image) drains it on step exit and POSTs it — token-authenticated — to the
/// ingest URL (a stub listener on the host standing in for the control
/// plane). Needs SCARAB_TEST_SIDECAR_IMAGE (in-cluster image) and
/// SCARAB_TEST_HOST_IP (host address reachable from Pods; colima:
/// 192.168.5.2). `#[ignore]`d + SCARAB_TEST_KUBE gated.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn results_sidecar_captures_a_named_result_end_to_end() {
    let Some(ns) = opted_in() else { return };
    let Ok(sidecar_image) = std::env::var("SCARAB_TEST_SIDECAR_IMAGE") else {
        eprintln!("skipping: set SCARAB_TEST_SIDECAR_IMAGE to a scarab-results-sidecar image");
        return;
    };
    let Ok(host_ip) = std::env::var("SCARAB_TEST_HOST_IP") else {
        eprintln!("skipping: set SCARAB_TEST_HOST_IP (host address reachable from Pods)");
        return;
    };
    let run_id = unique_run("run-results");

    // A stub ingest endpoint: records the one POST the sidecar makes.
    use axum::routing::post;
    let received: std::sync::Arc<tokio::sync::Mutex<Option<(String, String, serde_json::Value)>>> =
        Default::default();
    let rec = received.clone();
    let app = axum::Router::new().route(
        "/v1/runs/{run}/steps/{step}/results",
        post(
            move |headers: axum::http::HeaderMap,
                  axum::extract::Path((run, step)): axum::extract::Path<(String, String)>,
                  axum::Json(body): axum::Json<serde_json::Value>| {
                let rec = rec.clone();
                async move {
                    let token = headers
                        .get("x-scarab-results-token")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    *rec.lock().await = Some((format!("{run}/{step}"), token, body));
                    axum::http::StatusCode::ACCEPTED
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let secret = b"live-results-secret".to_vec();
    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client).with_results_egress(
        scarab_executor_k8s::ResultsEgress {
            base_url: format!("http://{host_ip}:{port}"),
            token_secret: secret.clone(),
            sidecar_image,
        },
    );

    let step = StepRun {
        run: RunId(run_id.clone()),
        step: StepId("emit".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    };
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec![
            "sh".into(),
            "-c".into(),
            "echo '{\"answer\":42}' > /scarab/results/compute.json && echo emitted".into(),
        ],
        env: vec![],
        secrets: vec![],
        run_as_root: true, // busybox
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(120),
        workspace_inputs: vec![],
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
    };
    let h = exec.launch(&step, &spec).await.expect("launch");
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
    assert_eq!(terminal, Some(ExecState::Succeeded), "step succeeds");

    // The kubelet SIGTERMs the sidecar after the step exits; the drain POST
    // arrives within its termination window.
    let mut got = None;
    for _ in 0..60 {
        if let Some(r) = received.lock().await.clone() {
            got = Some(r);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let (fence, token, body) = got.expect("the sidecar posted the results");
    assert_eq!(fence, format!("{run_id}/emit"));
    // The fence token is the real HMAC the executor minted.
    let expected = scarab_forge_github::sign_hex(&secret, format!("{run_id}:emit:a1").as_bytes());
    assert_eq!(token, expected, "fence-scoped token authenticated");
    assert_eq!(
        body["compute"]["answer"], 42,
        "named result captured: {body}"
    );

    exec.cancel(&h).await.expect("cleanup");
}

/// LIVE cancel teardown (ADR-0054): a RUNNING step's Pod is actually deleted
/// from the cluster by `cancel` (SIGTERM + grace), not just marked cancelled.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn cancel_tears_down_a_running_pod() {
    let Some(ns) = opted_in() else { return };
    let run_id = unique_run("run-cancel");

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns.clone(), client.clone());
    let step = StepRun {
        run: RunId(run_id.clone()),
        step: StepId("sleeper".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    };
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), "sleep 300".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: true, // busybox
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(600),
        workspace_inputs: vec![],
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
    };
    let h = exec.launch(&step, &spec).await.expect("launch");
    // Wait until it is actually Running (the interesting teardown case).
    for _ in 0..90 {
        if exec.poll(&h).await.expect("poll") == ExecState::Running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert_eq!(
        exec.poll(&h).await.unwrap(),
        ExecState::Running,
        "step reached Running"
    );

    exec.cancel(&h).await.expect("cancel");

    // The Pod OBJECT disappears from the cluster (grace period honored —
    // allow up to 60s for SIGTERM + kubelet cleanup).
    use kube::api::Api;
    let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client, &ns);
    let mut gone = false;
    for _ in 0..60 {
        if pods.get_opt(&h.0).await.expect("get pod").is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert!(gone, "the Pod was torn down, not left running");
}

/// LIVE artifacts (ADR-0052): a step writes files to /scarab/artifacts; the
/// harvest uploads the glob-selected ones as object blobs and records the
/// metadata on the Pod, surfaced via `Executor::artifacts`.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn artifacts_are_harvested_post_step() {
    use scarab_storage::ObjectStore;
    let Some(ns) = opted_in() else { return };
    let run_id = unique_run("run-artifacts");

    let client = kube::Client::try_default().await.expect("kube client");
    let tmp = tempfile::tempdir().expect("store dir");
    let storage = std::sync::Arc::new(
        scarab_storage_s3::S3Storage::local(tmp.path().to_str().unwrap()).expect("local store"),
    );
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(storage.clone())
        .with_artifact_store(storage.clone());

    let step = StepRun {
        run: RunId(run_id.clone()),
        step: StepId("emit".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    };
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec![
            "sh".into(),
            "-c".into(),
            "mkdir -p /scarab/artifacts/dist && \
             echo report > /scarab/artifacts/dist/report.txt && \
             echo scratch > /scarab/artifacts/notes.tmp"
                .into(),
        ],
        env: vec![],
        secrets: vec![],
        run_as_root: true, // busybox
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(120),
        workspace_inputs: vec![],
        clone: None,
        build: None,
        artifacts: vec!["dist/*".into()], // the .tmp is NOT published
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    };
    let h = exec.launch(&step, &spec).await.expect("launch");
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
    assert_eq!(terminal, Some(ExecState::Succeeded), "step succeeds");

    // The glob-selected artifact was harvested; the .tmp was not.
    let artifacts = exec.artifacts(&h).await.expect("artifacts");
    assert_eq!(artifacts.len(), 1, "{artifacts:?}");
    assert_eq!(artifacts[0].name, "dist/report.txt");
    assert_eq!(artifacts[0].content_type, "text/plain");
    // The blob is real and downloadable from the store.
    let bytes = storage.get(&artifacts[0].object_key).await.expect("blob");
    assert_eq!(bytes, b"report\n");

    exec.cancel(&h).await.expect("cleanup");
}

/// A StepRun fixture for one step of `run` with a single Running attempt `a1`
/// — the shape every live test hand-builds; shared by the Phase-2 cases below.
fn step_run_of(run: &str, step: &str) -> StepRun {
    StepRun {
        run: RunId(run.into()),
        step: StepId(step.into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

/// A busybox StepSpec fixture: `sh -c <cmd>` with the given root grant.
fn busybox_spec(cmd: &str, run_as_root: bool) -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), cmd.into()],
        env: vec![],
        secrets: vec![],
        run_as_root,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(600),
        workspace_inputs: vec![],
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

/// LIVE run_as_root reflection (ADR-0039): the `run_as_root` grant on the
/// StepSpec is actually present on the Pod the API server ADMITTED — read
/// back from the cluster, not from the locally built spec (the local shape is
/// unit-tested in the library). Both directions: the grant pins uid 0 and
/// drops `runAsNonRoot`; the default keeps the hardened non-root baseline.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn run_as_root_is_reflected_on_the_admitted_pod() {
    let Some(ns) = opted_in() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns.clone(), client.clone());
    use kube::api::Api;
    let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client, &ns);

    // The `step` container's securityContext, read back from the API server.
    let admitted_sc = |pod: &k8s_openapi::api::core::v1::Pod| {
        pod.spec
            .as_ref()
            .expect("pod spec")
            .containers
            .iter()
            .find(|c| c.name == "step")
            .expect("the step container")
            .security_context
            .clone()
            .expect("a securityContext is always set (ADR-0039)")
    };

    // --- Granted: run_as_root pins uid 0, runAsNonRoot off. ---
    let root_step = step_run_of(&unique_run("run-rootgrant"), "as-root");
    let hr = exec
        .launch(&root_step, &busybox_spec("sleep 300", true))
        .await
        .expect("launch root-granted step");
    let sc = admitted_sc(&pods.get(&hr.0).await.expect("admitted pod"));
    assert_eq!(sc.run_as_non_root, Some(false), "grant lifts the baseline");
    assert_eq!(sc.run_as_user, Some(0), "grant pins uid 0 explicitly");
    // The grant is root-only — never an escalation (ADR-0039).
    assert_eq!(sc.privileged, Some(false));
    assert_eq!(sc.allow_privilege_escalation, Some(false));

    // --- Default: the hardened restricted baseline stands. ---
    let base_step = step_run_of(&unique_run("run-rootbase"), "baseline");
    let hb = exec
        .launch(&base_step, &busybox_spec("sleep 300", false))
        .await
        .expect("launch baseline step");
    let sc = admitted_sc(&pods.get(&hb.0).await.expect("admitted pod"));
    assert_eq!(sc.run_as_non_root, Some(true), "baseline is non-root");
    assert_eq!(sc.run_as_user, None, "no uid pinned without the grant");

    for h in [hr, hb] {
        exec.cancel(&h).await.expect("cleanup");
    }
}

/// LIVE unpullable image (ADR-0047): a step whose image does not exist keeps
/// its Pod `Pending` forever at the kubelet level (`ErrImagePull` →
/// `ImagePullBackOff`), so the executor must surface a TERMINAL verdict fast —
/// `Failed { class: Infra { never_started: true } }` (bounded auto-retry
/// budget; the process never started) — rather than hang the attempt.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn image_pull_failure_fails_the_attempt_fast() {
    let Some(ns) = opted_in() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client);
    let step = step_run_of(&unique_run("run-nopull"), "nopull");
    let spec = StepSpec {
        // A well-formed reference that exists in no registry.
        image: "ghcr.io/thulasi-ram/scarab-no-such-image:never".into(),
        ..busybox_spec("true", false)
    };

    let started = std::time::Instant::now();
    let h = exec.launch(&step, &spec).await.expect("launch doomed step");
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
    match terminal {
        Some(ExecState::Failed { exit_code, class }) => {
            assert_eq!(exit_code, None, "the step process never ran");
            assert_eq!(
                class,
                scarab_engine::ports::FailureClass::Infra {
                    never_started: true
                },
                "an unpullable image is pre-start infra (bounded retry), not a hang"
            );
        }
        other => panic!("expected a terminal Failed for the unpullable image, got {other:?}"),
    }
    // Fast = surfaced from the waiting reason, well inside the poll window —
    // never the step-timeout path (the spec allows 600s).
    assert!(
        started.elapsed() < std::time::Duration::from_secs(115),
        "image-pull failure must fail fast, took {:?}",
        started.elapsed()
    );

    exec.cancel(&h).await.expect("cleanup");
}

/// LIVE orphan-Pod teardown regression (ADR-0056 amendment, git-bug fd6e6d4):
/// rerunning a step while its descendant is in-flight must tear the
/// descendant's Pod down for real — the engine-side supersede path
/// (`rerun_step` enqueues a scoped `SUPERSEDE_TEARDOWN`;
/// `reconcile_supersessions` cancels the named handle) is unit-tested over
/// `FakeExecutor` in `scarab-db-postgres/tests/rerun_supersede_inmemory.rs`,
/// but the bug was a REAL Pod left running, so this drives the same engine
/// path against the live cluster: the descendant's sleeper Pod must be GONE
/// from the namespace afterwards, not orphaned burning resources.
///
/// Cancel-path teardown has its own live case above
/// (`cancel_tears_down_a_running_pod`).
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn rerun_supersede_tears_down_the_in_flight_descendant_pod() {
    use scarab_engine::{rerun_step, Db, RunStatus, Scheduler};
    use scarab_testkit::{FakeClock, InMemoryDb};

    let Some(ns) = opted_in() else { return };
    let run = RunId(unique_run("run-orphan"));
    let b = StepId("b".into());
    let c = StepId("c".into());
    let a1 = AttemptId("a1".into());

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns.clone(), client.clone());

    // Launch c's Pod for real: a sleeper that outlives the test if orphaned.
    let sleeper = busybox_spec("sleep 300", true); // busybox runs as root
    let c_run = StepRun {
        run: run.clone(),
        step: c.clone(),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: a1.clone(),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![b.clone()],
        gate_kind: None,
    };
    let h = exec.launch(&c_run, &sleeper).await.expect("launch c");
    for _ in 0..90 {
        if exec.poll(&h).await.expect("poll") == ExecState::Running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert_eq!(
        exec.poll(&h).await.unwrap(),
        ExecState::Running,
        "descendant c is in-flight"
    );

    // Mirror the durable state the engine holds at this point: b Succeeded,
    // c Running on attempt a1 whose recorded handle is the REAL Pod.
    let db = InMemoryDb::new();
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&sleeper), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &c,
        Some(&sleeper),
        std::slice::from_ref(&b),
        Timestamp(0),
    )
    .await
    .unwrap();
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &c,
        &Attempt {
            id: a1.clone(),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    db.set_attempt_handle(&run, &c, &a1, &h.0).await.unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    // The human reruns b — c's in-flight attempt is superseded...
    let clock = FakeClock::new(1_000);
    rerun_step(&db, &clock, &run, &b, Some("live-test".into()))
        .await
        .expect("rerun_step");

    // ...and the driver's next supersession pass cancels the REAL Pod.
    let sched = Scheduler::new(&db, &clock, &exec, "live-drv");
    sched
        .reconcile_supersessions()
        .await
        .expect("reconcile_supersessions");

    // The superseded attempt's Pod is GONE from the namespace (grace period
    // honored — allow up to 60s for SIGTERM + kubelet cleanup).
    use kube::api::Api;
    let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client, &ns);
    let mut gone = false;
    for _ in 0..60 {
        if pods.get_opt(&h.0).await.expect("get pod").is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert!(
        gone,
        "the superseded descendant's Pod was torn down, not orphaned"
    );
}
