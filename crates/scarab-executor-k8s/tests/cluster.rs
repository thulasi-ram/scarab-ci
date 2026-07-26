//! Real-cluster round-trip for `K8sExecutor` (ADR-0004 acceptance).
//!
//! This test is `#[ignore]`d and additionally gated on the `SCARAB_TEST_KUBE`
//! env var, so `cargo test` NEVER runs it and it never touches an ambient
//! kubeconfig by accident.
//!
//! # How to run it — one recipe, both audiences
//!
//! Every case here needs more than a cluster: a workspace service (ADR-0061), the
//! fetcher/clone/sidecar images present in the node, a registry, a host address a
//! Pod can reach. There are exactly two places that stand all of that up, and
//! they set the SAME `SCARAB_TEST_*` set:
//!
//! - CI: the `kind` workflow (`.github/workflows/kind.yml`);
//! - local: `just kube-tests`, which owns the proc-mode stack and drives this
//!   suite against it.
//!
//! Do not hand-roll the env. Once `SCARAB_TEST_KUBE` is set, a missing var is a
//! **panic** ([`tier_var`], [`workspace_fixture`]) and not a skip — because for
//! seven cases it silently was a skip, and `kind/cluster-tests` is
//! `required-if-run`, so the merge gate read that silence as proof.
//!
//! It proves the acceptance: a busybox `echo` runs to completion with its exit
//! code recorded, and a second `launch` of the same fence re-attaches to the
//! existing Pod rather than creating a new one.

use scarab_engine::ports::{ExecHandle, ExecState, FailureClass};
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

/// How many *consecutive* failed polls this harness will ride out before it
/// calls the error permanent. One blip is a retryable fault; a wall of them is
/// a broken executor and must still fail the test.
const MAX_TRANSIENT_POLL_ERRS: u32 = 10;

/// Poll `h` to a terminal state, tolerating transient poll errors **the way the
/// scheduler does**.
///
/// `K8sExecutor::poll` legitimately returns `Err` mid-flight: it drives the
/// workspace legs (ADR-0029/0045) as a side effect, and those can hit a
/// retryable apiserver/exec-stream fault — `workspace: broken pipe` was
/// observed 1-in-9 on the CI kind tier. The engine classifies that as
/// `DriveErr::Transient`, yields no verdict, and re-polls next tick; a test
/// that `.expect()`s the poll instead converts a retryable blip into a red
/// build. That mismatch was this tier's only observed flake, and the reason it
/// could not be promoted off advisory probation (git-bug a0b42ad).
///
/// Returns `None` if `ticks` seconds elapse with no terminal state.
async fn poll_to_terminal(exec: &K8sExecutor, h: &ExecHandle, ticks: u32) -> Option<ExecState> {
    let mut consecutive = 0;
    for _ in 0..ticks {
        match exec.poll(h).await {
            Ok(s @ (ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost)) => {
                return Some(s)
            }
            Ok(_) => consecutive = 0,
            Err(e) => {
                consecutive += 1;
                assert!(
                    consecutive <= MAX_TRANSIENT_POLL_ERRS,
                    "poll failed {consecutive}x consecutively — not a transient fault: {e}"
                );
                eprintln!("poll: riding out transient error #{consecutive}: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    None
}

/// Wait for `h` to reach `Running` with the same transient tolerance as
/// [`poll_to_terminal`], and return the last state observed — so the caller
/// asserts on one reading instead of re-polling (which could catch a *later*
/// state and fail for the wrong reason).
async fn poll_until_running(exec: &K8sExecutor, h: &ExecHandle, ticks: u32) -> ExecState {
    let mut consecutive = 0;
    let mut last = ExecState::Pending;
    for _ in 0..ticks {
        match exec.poll(h).await {
            Ok(ExecState::Running) => return ExecState::Running,
            Ok(s) => {
                consecutive = 0;
                last = s;
            }
            Err(e) => {
                consecutive += 1;
                assert!(
                    consecutive <= MAX_TRANSIENT_POLL_ERRS,
                    "poll failed {consecutive}x consecutively — not a transient fault: {e}"
                );
                eprintln!("poll: riding out transient error #{consecutive}: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    last
}

fn opted_in() -> Option<String> {
    if std::env::var("SCARAB_TEST_KUBE").is_err() {
        eprintln!("skipping: set SCARAB_TEST_KUBE=1 to run against a dev cluster");
        return None;
    }
    Some(std::env::var("SCARAB_TEST_KUBE_NS").unwrap_or_else(|_| "default".to_string()))
}

/// Read a `SCARAB_TEST_*` var the tier's runner is expected to provide.
///
/// **Only ever called after [`opted_in`]**, so reaching it means the tier is
/// running — and an absent var is therefore a wiring bug, not a reason to skip.
/// It panics.
///
/// That is not pedantry. Every one of these used to be an `else { return }`, and
/// with the ADR-0061 workspace vars nothing set them anywhere: seven cases
/// reported PASS while executing nothing, and `kind/cluster-tests` is
/// `required-if-run` in `.github/required-checks.txt`, so `just pr-gate` read that
/// silence as a green tier. A live case that cannot run must be RED.
///
/// Wiring lives in exactly two places, both of which set the full set:
/// `.github/workflows/kind.yml` and the `just kube-tests` recipe.
fn tier_var(name: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => panic!(
            "{name} is not set, but SCARAB_TEST_KUBE is — this live case would have \
             silently no-op'd while reporting PASS. Wire it in \
             .github/workflows/kind.yml (CI) or run `just kube-tests` (local, which \
             stands up everything this tier needs)."
        ),
    }
}

/// The ADR-0061 workspace service this tier drives, and the `Cas` that MUST be
/// the same store.
///
/// # Why a fixture, and why it is one object
///
/// Since s3-feed there is no control-plane feed: a Step with `needs:` is
/// provisioned by an init container that dials the workspace service. So a live
/// test of the workspace flow needs the two halves to agree about *where the
/// content is*, and the cheap way to guarantee that is to make them literally the
/// same endpoint:
///
/// - the executor's `Cas` (used by the **drain**, and by the test itself) is a
///   [`WorkspaceClient`], which implements `Cas` precisely so a control plane can
///   be pointed at the service with no call-site change;
/// - the Pod's fetcher dials the same service.
///
/// These tests used to hand the executor an `S3Storage::local` on a **tempdir of
/// the test host**, which the fetcher could never see. Keeping that would have
/// left a test that passes only because the feed never happened.
///
/// # Two URLs, deliberately
///
/// `SCARAB_TEST_WORKSPACE_URL` is host-facing (the test process is the control
/// plane); `SCARAB_TEST_WORKSPACE_POD_URL` is what a Pod dials, and in proc mode
/// they differ — the host reaches loopback, a Pod cannot (see the long note in
/// `deploy/local-proc/up.sh`). It defaults to the host URL, which is right in
/// Helm mode where there is one in-cluster Service.
///
/// # Missing configuration is FATAL, not a skip
///
/// This fixture shipped as a triple `else { return }`, and the effect was that
/// seven cases — five of which ran green the week before — reported PASS while
/// executing nothing, because no CI job, recipe or script set the three vars it
/// asks for. `kind/cluster-tests` is `required-if-run`, so the merge gate saw
/// green for a tier that had gone silent.
///
/// So: once the tier is opted in ([`opted_in`]), absent workspace configuration
/// **panics**. Mirrors `SCARAB_TEST_REQUIRE_PG` in
/// `crates/scarab-server/tests/common/mod.rs` — with one deliberate difference:
/// the panic is keyed on the SAME condition as `opted_in` rather than on a second
/// opt-out var, because a second var is how this failed the first time.
struct WorkspaceFixture {
    /// The service, behind `Cas`. What the executor drains into and what the test
    /// reads back.
    cas: std::sync::Arc<dyn scarab_storage::Cas>,
    /// What the Step Pod's fetcher is told.
    fetch: scarab_executor_k8s::WorkspaceFetch,
}

fn workspace_fixture() -> Option<WorkspaceFixture> {
    let vars = [
        "SCARAB_TEST_WORKSPACE_URL",
        "SCARAB_TEST_WORKSPACE_SECRET",
        "SCARAB_TEST_WSFETCH_IMAGE",
    ];
    let missing: Vec<&str> = vars
        .iter()
        .copied()
        .filter(|v| std::env::var(v).map(|s| s.is_empty()).unwrap_or(true))
        .collect();
    if !missing.is_empty() {
        // Deliberately the same predicate `opted_in` uses. One var opts the tier
        // in; nothing else may opt a case out of it.
        if std::env::var("SCARAB_TEST_KUBE").is_ok() {
            panic!(
                "live workspace test skipped but SCARAB_TEST_KUBE is set — missing: {}.\n\
                 Since ADR-0061 s3-feed a Step with `needs:` is provisioned by an init \
                 container that dials the workspace service, so a green run without these \
                 proves nothing. Wire them up:\n\
                 \x20 CI:    .github/workflows/kind.yml (workspace service + kind-loaded fetcher)\n\
                 \x20 local: `just up` writes deploy/local-proc/.env.generated; \
                 `just kube-tests` sources it",
                missing.join(", ")
            );
        }
        eprintln!(
            "SKIPPED (live workspace test): set {} — `just kube-tests` wires them up",
            vars.join(", ")
        );
        return None;
    }
    let url = std::env::var("SCARAB_TEST_WORKSPACE_URL").expect("checked above");
    let secret = std::env::var("SCARAB_TEST_WORKSPACE_SECRET").expect("checked above");
    let image = std::env::var("SCARAB_TEST_WSFETCH_IMAGE").expect("checked above");
    let pod_url = std::env::var("SCARAB_TEST_WORKSPACE_POD_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| url.clone());

    // The test process stands in for the control plane, so it mints itself a
    // BROWSE token — not root-limited, because it reads and writes arbitrary
    // snapshots. A Pod never gets one of these; the executor mints each Pod a
    // `read` token scoped to exactly that Step's inputs.
    use scarab_executor_k8s::workspace_token;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let token = workspace_token::mint(
        secret.as_bytes(),
        &workspace_token::browse_claims(now + 3_600),
    );
    Some(WorkspaceFixture {
        cas: std::sync::Arc::new(scarab_workspace_client::WorkspaceClient::new(&url, token)),
        fetch: scarab_executor_k8s::WorkspaceFetch {
            url: pod_url,
            token_secret: secret.into_bytes(),
            fetcher_image: image,
        },
    })
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &h1, 60).await;
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &h, 90).await;
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
        workspace_outputs: vec![],
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

/// Live workspace flow (ADR-0029/0045 keystone, now ADR-0061 s3-feed): step A
/// writes a file into /workspace, its workspace is snapshotted into the CAS
/// (Executor::output returns the root), and step B (`needs: [A]`) has it
/// materialized and reads it back. Also proves restart determinism: re-running A
/// yields the SAME CAS root. `#[ignore]`d + gated on SCARAB_TEST_KUBE.
///
/// **Step B is now the acceptance test for the fetcher**, not for a control-plane
/// tar tunnel: the only way B can see A's file is if the init container dialled
/// the workspace service and materialised the snapshot itself.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn workspace_flows_from_a_to_b_through_the_cas() {
    let Some(ns) = opted_in() else { return };
    let Some(ws) = workspace_fixture() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(ws.cas.clone())
        .with_workspace_service(ws.fetch.clone());

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
        workspace_outputs: vec![],
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
        poll_to_terminal(exec, h, 90)
            .await
            .expect("pod did not settle")
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

    // --- Restart determinism, on the digest that carries it (git-bug 945b1f4). ---
    //
    // This once asserted `root_a == root_a2` — same content, same CAS root — and
    // that assertion was **right about the requirement and wrong about the
    // digest**, which is why it started failing here. ADR-0061's s7 put `mtime_ms`
    // into the tree entry, deliberately, so cross-Step incremental compilation
    // stops being degraded; the entry is in the hash preimage, so a root moves with
    // the wall clock. Measured on this very cluster, the two roots differed in
    // exactly one field:
    //
    //   6ab25ad8… [{"name":"out.txt","target":{"Blob":"d112afe6…"},"mode":420,"mtime_ms":1785098063000}]
    //   93722441… [{"name":"out.txt","target":{"Blob":"d112afe6…"},"mode":420,"mtime_ms":1785098073000}]
    //
    // It was narrowed to "the blob addresses match" for one commit, as a holding
    // position with a pointer to the ticket. s8 answers it: the requirement lives on
    // the **content identity** — the same fold with mtimes dropped, which is what
    // ADR-0027's invalidation compares — and both halves are asserted here, because
    // the pair is the point. If the roots ever stop differing, s7's mtime fidelity
    // has silently regressed; if the identities ever start differing, ADR-0027 is
    // dead again and nothing downstream will ever be skipped.
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
    assert_ne!(
        root_a, root_a2,
        "two attempts write at different wall clocks, so their ROOTS must differ — \
         if they stop differing, the CAS stopped recording mtimes and s7's fidelity \
         work has regressed"
    );

    let identity_a = exec
        .output_identity(&ha)
        .await
        .expect("identity")
        .expect("A recorded a content identity");
    let identity_a2 = exec
        .output_identity(&ha2)
        .await
        .expect("identity")
        .expect("A2 recorded a content identity");
    assert_eq!(
        identity_a, identity_a2,
        "same content => same CONTENT IDENTITY across attempts. This is the \
         restart determinism ADR-0027 is built on; when it moves with the clock, a \
         producer's re-run always invalidates every dependent and skip-if-unchanged \
         is dead (git-bug 945b1f4)"
    );

    // Both roots still resolve independently: an identity is a label on evidence,
    // never a redirection, so attempt a1's workspace is still exactly a1's.
    let blobs_of = |entries: Vec<scarab_storage::TreeEntry>| {
        let mut v: Vec<String> = entries
            .into_iter()
            .map(|e| match e.target {
                scarab_storage::TreeTarget::Blob(b) => format!("{}={}", e.name, b.0),
                scarab_storage::TreeTarget::Tree(t) => format!("{}/{}", e.name, t.0),
            })
            .collect();
        v.sort();
        v
    };
    let first = ws
        .cas
        .tree_entries(&scarab_storage::TreeHash(root_a.clone()))
        .await
        .expect("read A's tree");
    let second = ws
        .cas
        .tree_entries(&scarab_storage::TreeHash(root_a2.clone()))
        .await
        .expect("read A2's tree");
    assert_eq!(
        blobs_of(first),
        blobs_of(second),
        "same content => same BLOB addresses, in both attempts' own trees"
    );

    for h in [ha, hb, ha2] {
        exec.cancel(&h).await.expect("cleanup");
    }
}

/// LIVE ADR-0061 part 4 (s4): at the **instant** `Succeeded` is first observed,
/// the Attempt's Workspace Snapshot must already be durable and readable.
///
/// The unit tests pin the *rule* (`settled_state` / `workspace_snapshot_lost`)
/// against Pod fixtures. This pins the *ordering* against the real thing, which is
/// what the rule is about: `drive_workspace` ingests, patches
/// `scarab.io/workspace-root`, and only then releases the egress barrier, so the
/// first `Succeeded` a caller can see is already backed by content the store
/// holds. A green verdict whose snapshot is not yet (or never) durable is a claim
/// the durable record cannot back (CONTEXT.md §2).
///
/// Deliberately asserted on the FIRST terminal observation, not after a settle
/// loop: polling until the root appears would test nothing at all.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn a_green_attempt_is_backed_by_a_durable_snapshot_at_the_instant_it_goes_green() {
    let Some(ns) = opted_in() else { return };
    let Some(ws) = workspace_fixture() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(ws.cas.clone())
        .with_workspace_service(ws.fetch.clone());

    let step = StepRun {
        run: RunId(unique_run("run-ws-durable")),
        step: StepId("produce".into()),
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
        image: "busybox:1.36".into(),
        command: vec![
            "sh".into(),
            "-c".into(),
            "mkdir -p /workspace/dist && echo durable > /workspace/dist/evidence.txt".into(),
        ],
        env: vec![],
        secrets: vec![],
        run_as_root: true,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(120),
        workspace_inputs: vec![],
        workspace_outputs: vec![],
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
    assert_eq!(
        poll_to_terminal(&exec, &h, 120).await,
        Some(ExecState::Succeeded)
    );

    // 1. The verdict carries a root. Without ADR-0061 part 4's rule a Pod whose
    //    barrier died could report Succeeded with `output() == None`, and the
    //    engine records the output only `if Some` and finalises the step anyway.
    let root = exec
        .output(&h)
        .await
        .expect("output")
        .expect("Succeeded must carry a workspace snapshot root");

    // 2. And the root is really THERE — in the store, not merely named. This is the
    //    difference between "we wrote an annotation" and "the evidence is safe".
    let entries = ws
        .cas
        .tree_entries(&scarab_storage::TreeHash(root.clone()))
        .await
        .expect("the snapshot named by a green Attempt must be readable");
    assert!(
        entries.iter().any(|e| e.name == "dist"),
        "the snapshot must contain what the step wrote: {entries:?}"
    );

    exec.cancel(&h).await.expect("cleanup");
}

/// LIVE: a Step whose input snapshot does not exist must get a **verdict**, not a
/// hang (ADR-0061 s3-feed).
///
/// This is only reachable now. The `busybox` doorstop the fetcher replaced could
/// not fail — it spun on a marker file — so a missing input was discovered by the
/// control plane and mapped to `DriveErr::InputMissing`. The discovery moved into
/// the Pod, and this test is what proves the replacement classification is
/// equivalent: `Infra { never_started: true }` (the step's process never ran, so
/// no side effect is possible; bounded auto-retry then dead-letters).
///
/// Kind-tier and not unit-testable: the *unit* test pins `pod_state`'s reading of
/// a Pod fixture, but that a real fetcher exits non-zero, and that the kubelet
/// then reports the Pod the way the fixture claims, is exactly the
/// "reports success but structurally cannot work" seam a cluster is needed for.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn a_missing_input_snapshot_fails_the_attempt_instead_of_hanging() {
    let Some(ns) = opted_in() else { return };
    let Some(ws) = workspace_fixture() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(ws.cas.clone())
        .with_workspace_service(ws.fetch.clone());

    let step = StepRun {
        run: RunId(unique_run("run-ws-missing")),
        step: StepId("consume".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![StepId("produce".into())],
        gate_kind: None,
    };
    // A well-formed but absent snapshot root: 64 hex chars, so it passes the
    // service's path guard and gets a genuine 404 rather than a 400.
    let spec = StepSpec {
        image: "busybox:1.36".into(),
        command: vec!["sh".into(), "-c".into(), "echo should-never-run".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: true,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(120),
        workspace_inputs: vec!["f".repeat(64)],
        workspace_outputs: vec![],
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
    assert_eq!(
        poll_to_terminal(&exec, &h, 120).await,
        Some(ExecState::Failed {
            exit_code: None,
            class: FailureClass::Infra {
                never_started: true
            },
        }),
        "a fetch that cannot find its input must fail the attempt fast — the old \
         doorstop would have waited for a feed that never came"
    );
    exec.cancel(&h).await.expect("cleanup");
}

/// LIVE: a Step that inherits a workspace when no workspace service is configured
/// is **refused at launch** (ADR-0061 s3-feed, fail-closed).
///
/// The tempting alternative is to let the Pod run with an empty `/workspace`. That
/// does not fail — it produces a *wrong answer*, and the Attempt then claims in
/// the durable record to have tested a tree that was never there, which is the one
/// thing this product may not do (CONTEXT.md §2). There is deliberately no
/// fallback to the deleted control-plane feed (ADR-0061 D2.3).
///
/// No cluster work happens here beyond connecting, but it lives in this tier
/// because it asserts on the real `Executor::launch`.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn a_step_with_inputs_is_refused_when_no_workspace_service_is_configured() {
    let Some(ns) = opted_in() else { return };
    let client = kube::Client::try_default().await.expect("kube client");
    let tmp = tempfile::tempdir().expect("cas dir");
    let cas: std::sync::Arc<dyn scarab_storage::Cas> = std::sync::Arc::new(
        scarab_storage_s3::S3Storage::local(tmp.path().to_str().unwrap()).expect("local cas"),
    );
    // Workspace CAS wired, NO workspace service.
    let exec = K8sExecutor::with_client(ns, client).with_workspace_cas(cas);

    let step = StepRun {
        run: RunId(unique_run("run-ws-unconfigured")),
        step: StepId("consume".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![StepId("produce".into())],
        gate_kind: None,
    };
    let mut spec = StepSpec {
        image: "busybox:1.36".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: true,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(60),
        workspace_inputs: vec!["a".repeat(64)],
        workspace_outputs: vec![],
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
    let err = exec
        .launch(&step, &spec)
        .await
        .expect_err("must refuse, not create a Pod with an empty workspace");
    assert!(
        format!("{err:?}").contains("no workspace service is configured"),
        "{err:?}"
    );
    // No Pod was created — the refusal is BEFORE any effect.
    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::namespaced(
        kube::Client::try_default().await.expect("kube client"),
        &std::env::var("SCARAB_TEST_KUBE_NS").unwrap_or_else(|_| "default".to_string()),
    );
    assert!(
        pods.get_opt(&pod_name(&step))
            .await
            .expect("get pod")
            .is_none(),
        "a refused launch must leave nothing behind"
    );

    // The same step with no inputs launches fine — every `clone` step and every
    // first step in a pipeline has this shape, and they must not need a service.
    spec.workspace_inputs = vec![];
    let h = exec.launch(&step, &spec).await.expect("launch without inputs");
    exec.cancel(&h).await.expect("cleanup");
}

/// LIVE per-path publishing (`outputs:`, ADR-0007): the egress leg prunes the
/// published snapshot to the declared paths, and a dependent materializes only
/// those. Kind-tier because the prune runs in `drive_workspace` off the Pod
/// annotation — `FakeExecutor` has no workspace at all, so this wiring is
/// k8s-observable-only (the feature-acceptance rule's kind trigger). The prune
/// *algebra* is proven in-process in `scarab-storage-s3/tests/workspace.rs`.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn declared_outputs_publish_only_those_paths_through_the_cas() {
    let Some(ns) = opted_in() else { return };
    let Some(ws) = workspace_fixture() else { return };
    let run_id = unique_run("run-ws-out");

    let client = kube::Client::try_default().await.expect("kube client");
    // The pruned root must be readable BY THE FETCHER, so the executor's `Cas` is
    // the workspace service — a host tempdir would leave step B unable to see
    // anything and the assertion below would pass for the wrong reason.
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(ws.cas.clone())
        .with_workspace_service(ws.fetch.clone());

    let step_run = |id: &str| StepRun {
        run: RunId(run_id.clone()),
        step: StepId(id.into()),
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
    let spec = |cmd: &str, inputs: Vec<String>, outputs: Vec<String>| StepSpec {
        image: "busybox:latest".into(),
        command: vec!["sh".into(), "-c".into(), cmd.into()],
        env: vec![],
        secrets: vec![],
        run_as_root: true,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(120),
        workspace_inputs: inputs,
        workspace_outputs: outputs,
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

    // Producer writes a publishable dir AND junk; it declares only `dist`.
    let a = step_run("a");
    let ha = exec
        .launch(
            &a,
            &spec(
                "mkdir -p /workspace/dist /workspace/target && \
                 echo shipped > /workspace/dist/app && \
                 echo cache > /workspace/target/huge && \
                 echo scratch > /workspace/scratch.tmp",
                vec![],
                vec!["dist".into()],
            ),
        )
        .await
        .expect("launch A");
    assert_eq!(
        poll_to_terminal(&exec, &ha, 90).await,
        Some(ExecState::Succeeded),
        "the producer succeeds"
    );
    let root_a = exec
        .output(&ha)
        .await
        .expect("output")
        .expect("A published a snapshot");

    // The consumer sees `dist` and NOTHING else.
    let b = step_run("b");
    let hb = exec
        .launch(
            &b,
            &spec(
                "grep -q shipped /workspace/dist/app && \
                 [ ! -e /workspace/target ] && [ ! -e /workspace/scratch.tmp ]",
                vec![root_a.clone()],
                vec![],
            ),
        )
        .await
        .expect("launch B");
    assert_eq!(
        poll_to_terminal(&exec, &hb, 90).await,
        Some(ExecState::Succeeded),
        "B must see the declared `dist` and none of the undeclared files"
    );

    // Fail-closed: declaring a path the step never produced fails the step with
    // a permanent, author-fixable verdict — not a narrower publish, not a retry.
    let c = step_run("c");
    let hc = exec
        .launch(
            &c,
            &spec(
                "mkdir -p /workspace/dist && echo shipped > /workspace/dist/app",
                vec![],
                vec!["dist".into(), "coverage".into()],
            ),
        )
        .await
        .expect("launch C");
    assert_eq!(
        poll_to_terminal(&exec, &hc, 90).await,
        Some(ExecState::Failed {
            exit_code: None,
            class: scarab_engine::ports::FailureClass::Config,
        }),
        "an undeclared-but-missing output is a developer verdict, not infra churn"
    );

    for h in [ha, hb, hc] {
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
    let run_id = unique_run("run-clone");
    let Some(ns) = opted_in() else { return };
    let clone_image = tier_var("SCARAB_TEST_CLONE_IMAGE");
    let sha = tier_var("SCARAB_TEST_CLONE_SHA");
    // The downstream `needs: [checkout]` step at the bottom of this test is fed by
    // the fetcher, so this test needs the service too.
    let Some(ws) = workspace_fixture() else { return };
    let cas = ws.cas.clone();

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(cas.clone())
        .with_workspace_service(ws.fetch.clone())
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &h, 120).await;
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
            // -c safe.directory: the workspace is materialized by the FETCHER's
            // uid (ADR-0061 s3-feed; the control plane's before that), the step
            // runs as the image's — same situation every CI runner handles for
            // authored git use.
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &bh, 120).await;
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
    let run_id = unique_run("run-clone-full");
    let Some(ns) = opted_in() else { return };
    let clone_image = tier_var("SCARAB_TEST_CLONE_IMAGE");
    let sha = tier_var("SCARAB_TEST_CLONE_SHA");
    // The history check downstream inherits the checkout, so it is fed by the
    // fetcher (ADR-0061 s3-feed).
    let Some(ws) = workspace_fixture() else { return };

    let client = kube::Client::try_default().await.expect("kube client");
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(ws.cas.clone())
        .with_workspace_service(ws.fetch.clone())
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &h, 180).await;
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &ch, 120).await;
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
    let clone_image = tier_var("SCARAB_TEST_CLONE_IMAGE");

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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &h, 120).await;
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
    let Some(ns) = opted_in() else { return };
    let registry = tier_var("SCARAB_TEST_REGISTRY");
    // The build step inherits its context as a workspace input, so the fetcher
    // provisions it (ADR-0061 s3-feed).
    let Some(ws) = workspace_fixture() else { return };
    let run_id = unique_run("run-build");

    let client = kube::Client::try_default().await.expect("kube client");
    let cas = ws.cas.clone();
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(cas.clone())
        .with_workspace_service(ws.fetch.clone());

    // The build context: a trivial Dockerfile, ingested as the upstream
    // (checkout) workspace — through the SERVICE, so the fetcher can read it back.
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &h, 300).await;
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &vh, 120).await;
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
    let sidecar_image = tier_var("SCARAB_TEST_SIDECAR_IMAGE");
    let host_ip = tier_var("SCARAB_TEST_HOST_IP");
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &h, 120).await;
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
        workspace_outputs: vec![],
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
    assert_eq!(
        poll_until_running(&exec, &h, 90).await,
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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &h, 120).await;
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

/// LIVE artifacts from a FAILING step (a28a173, ADR-0052 + ADR-0056). The
/// harvest used to be gated on `exit == 0`, so a red step's evidence — the JUnit
/// XML, the crash log, the screenshot — was uploaded nowhere and indexed nowhere,
/// which also made the scheduler's `ExecState::Failed` harvest branch dead code
/// on k8s. Kind-tier because the gate is k8s-observable-only: it lives in the
/// egress-sidecar settle, which `FakeExecutor` cannot model.
///
/// The step writes its artifact and then exits 1. The terminal verdict must be
/// the step's OWN failure (never masked as Timeout by the withhold, never
/// re-classified by the harvest), and the artifact must be harvested anyway.
#[tokio::test]
#[ignore = "requires a dev kubernetes cluster; opt in with SCARAB_TEST_KUBE=1"]
async fn artifacts_are_harvested_from_a_failed_step() {
    use scarab_storage::ObjectStore;
    let Some(ns) = opted_in() else { return };
    let run_id = unique_run("run-artifacts-failed");

    let client = kube::Client::try_default().await.expect("kube client");
    let tmp = tempfile::tempdir().expect("store dir");
    let storage = std::sync::Arc::new(
        scarab_storage_s3::S3Storage::local(tmp.path().to_str().unwrap()).expect("local store"),
    );
    let exec = K8sExecutor::with_client(ns, client)
        .with_workspace_cas(storage.clone())
        .with_artifact_store(storage.clone());

    let step = step_run_of(&run_id, "emit-and-fail");
    let spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec![
            "sh".into(),
            "-c".into(),
            // The evidence is written, THEN the step goes red.
            "mkdir -p /scarab/artifacts/dist && \
             echo 'FAILED 3 tests' > /scarab/artifacts/dist/report.txt && \
             exit 1"
                .into(),
        ],
        env: vec![],
        secrets: vec![],
        run_as_root: true, // busybox
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: Some(120),
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        clone: None,
        build: None,
        artifacts: vec!["dist/*".into()],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    };
    let h = exec.launch(&step, &spec).await.expect("launch");
    let terminal = poll_to_terminal(&exec, &h, 120).await;
    assert_eq!(
        terminal,
        Some(ExecState::Failed {
            exit_code: Some(1),
            class: FailureClass::Step,
        }),
        "the harvest must not re-classify or mask the step's own failure"
    );

    // The point of the ticket: the failed step's evidence survived.
    let artifacts = exec.artifacts(&h).await.expect("artifacts");
    assert_eq!(artifacts.len(), 1, "{artifacts:?}");
    assert_eq!(artifacts[0].name, "dist/report.txt");
    let bytes = storage.get(&artifacts[0].object_key).await.expect("blob");
    assert_eq!(bytes, b"FAILED 3 tests\n");

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
        workspace_outputs: vec![],
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
    let terminal = poll_to_terminal(&exec, &h, 120).await;
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
    assert_eq!(
        poll_until_running(&exec, &h, 90).await,
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
