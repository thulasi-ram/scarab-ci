//! Scenario 2 — **crash/resume at stack grain, THE WEDGE.**
//!
//! Kill `scarab-server` (SIGKILL, no goodbye) while a step Pod is mid-flight
//! on the real kind cluster, let it restart, and prove the run completes
//! **exactly once**: the surviving attempt is re-adopted (`AttemptReadopted`),
//! never re-executed (no second `AttemptStarted` per step), and the run makes
//! exactly one terminal transition.
//!
//! The server under test is the release's own server Pod (`just e2e` runs the
//! release in its e2e mode, with its own database and bucket). In the Pod the
//! server is PID 1 with no init, and a signal sent from inside its PID
//! namespace that it has no handler for is dropped. So the SIGKILL comes from
//! the kind node's PID namespace (ADR-0077 Amendment §5): the server
//! container's host PID is read with `crictl inspect` on the node and killed
//! there with `kill -KILL`. The kubelet then restarts the container in the
//! SAME Pod, and the new process re-adopts the Attempt. The Depot runs the
//! same binary, so the target is chosen by container id (the server
//! Deployment's Pod, container `scarab-server`), never by process name, and
//! the PID's cgroup is checked against that id before the signal.

mod support;

use std::time::{Duration, Instant};

use support::*;

/// The server Pod's identity and its `scarab-server` container's state.
#[derive(Debug, Clone, PartialEq)]
struct ServerContainer {
    pod: String,
    uid: String,
    /// Bare container id (the `containerd://` prefix stripped).
    container_id: String,
    restarts: u64,
    ready: bool,
}

/// The single server Pod of the release: the Deployment's Pods carry
/// `app.kubernetes.io/name=scarab` (the Depot's carry `scarab-workspace`).
fn server_container() -> ServerContainer {
    let raw = kubectl(&[
        "get",
        "pods",
        "-n",
        &namespace(),
        "-l",
        "app.kubernetes.io/name=scarab,app.kubernetes.io/instance=scarab",
        "-o",
        "json",
    ]);
    let v: serde_json::Value = serde_json::from_str(&raw).expect("pods JSON");
    let pods: Vec<&serde_json::Value> = v["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|p| p["metadata"]["deletionTimestamp"].is_null())
        .collect();
    assert_eq!(
        pods.len(),
        1,
        "expected exactly one server Pod (replicaCount 1), got {}",
        pods.len()
    );
    let pod = pods[0];
    let status = pod["status"]["containerStatuses"]
        .as_array()
        .expect("containerStatuses")
        .iter()
        .find(|c| c["name"] == "scarab-server")
        .expect("a scarab-server container");
    let id = status["containerID"].as_str().unwrap_or_default();
    ServerContainer {
        pod: pod["metadata"]["name"].as_str().expect("name").to_string(),
        uid: pod["metadata"]["uid"].as_str().expect("uid").to_string(),
        container_id: id.rsplit("://").next().unwrap_or_default().to_string(),
        restarts: status["restartCount"].as_u64().unwrap_or(0),
        ready: status["ready"].as_bool().unwrap_or(false),
    }
}

/// `docker exec <kind node> <args>`: a command in the node's namespaces.
fn on_node(args: &[&str]) -> String {
    let out = std::process::Command::new("docker")
        .arg("exec")
        .arg(kind_node())
        .args(args)
        .output()
        .expect("docker runs");
    assert!(
        out.status.success(),
        "docker exec {} {args:?} failed: {}",
        kind_node(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// SIGKILL the server container's process from the kind node's PID
/// namespace. Returns the node-side PID it signalled.
fn kill_server_from_node(target: &ServerContainer) -> String {
    let pid = on_node(&[
        "crictl",
        "inspect",
        "--output",
        "go-template",
        "--template",
        "{{.info.pid}}",
        &target.container_id,
    ]);
    assert!(
        !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()) && pid != "0",
        "crictl gave no PID for container {}: {pid:?}",
        target.container_id
    );
    // The PID belongs to the server container, not the Depot's: its cgroup
    // names the container id.
    let cgroup = on_node(&["cat", &format!("/proc/{pid}/cgroup")]);
    assert!(
        cgroup.contains(&target.container_id),
        "PID {pid} is not in container {}'s cgroup: {cgroup}",
        target.container_id
    );
    let cmdline = on_node(&["sh", "-c", &format!("tr '\\0' ' ' < /proc/{pid}/cmdline")]);
    eprintln!(
        "crash_resume: SIGKILL from node {} to PID {pid} ({cmdline}), container {} of Pod {} (restarts before: {})",
        kind_node(),
        target.container_id,
        target.pod,
        target.restarts
    );
    on_node(&["kill", "-KILL", &pid]);
    pid
}

/// Waits for the kubelet to restart the killed container in the same Pod and
/// for it to be ready again; returns its new state and the last termination.
async fn wait_restarted(before: &ServerContainer) -> (ServerContainer, serde_json::Value) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let now = server_container();
        if now.restarts > before.restarts && now.ready {
            let raw = kubectl(&["get", "pod", "-n", &namespace(), &now.pod, "-o", "json"]);
            let v: serde_json::Value = serde_json::from_str(&raw).expect("pod JSON");
            let last = v["status"]["containerStatuses"]
                .as_array()
                .and_then(|a| a.iter().find(|c| c["name"] == "scarab-server"))
                .map(|c| c["lastState"]["terminated"].clone())
                .unwrap_or_default();
            return (now, last);
        }
        assert!(
            Instant::now() < deadline,
            "the server container did not restart and become ready within 120s: before {before:?}, now {now:?}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_resume_completes_exactly_once() {
    require_e2e!();

    let base = base_url();
    let client = client();

    // --- a 2-step run whose first step is slow enough to die under ----------
    let run = create_inline_run(
        &client,
        &base,
        serde_json::json!([
            { "id": "slow", "image": "busybox:latest",
              "security": { "run_as_root": true },
              "command": ["sh", "-c", "sleep 20 && echo slow step done"] },
            { "id": "second", "image": "busybox:latest", "needs": ["slow"],
              "security": { "run_as_root": true },
              "command": ["sh", "-c", "echo second step done"] }
        ]),
    )
    .await;

    // Wait until step 1 is observably EXECUTING: attempt started AND its Pod
    // exists on the cluster (so the durable launch marker is written and the
    // kill lands mid-execution, not pre-launch).
    wait_for_run(
        &client,
        &base,
        &run,
        Duration::from_secs(120),
        "step `slow` running with an attempt",
        |rs| {
            let s = rs.step("slow");
            s.status == "running" && s.attempts >= 1
        },
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while pods_of_run(&run).is_empty() {
        assert!(
            Instant::now() < deadline,
            "run {run}: step `slow` never got a Pod on the cluster"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // A beat more, so the attempt handle is durably recorded before the kill.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // --- SIGKILL mid-step-1 from the node; the kubelet restarts it ----------
    let before = server_container();
    assert!(before.ready, "the server must be ready before the kill: {before:?}");
    kill_server_from_node(&before);
    let (after, last) = wait_restarted(&before).await;
    eprintln!(
        "crash_resume: container restarted in place: Pod {} uid {} restarts {} -> {}, container {} -> {}, last termination {last}",
        after.pod, after.uid, before.restarts, after.restarts, before.container_id, after.container_id
    );
    // The same Pod (a restart, not a replacement) with a new container.
    assert_eq!(after.uid, before.uid, "the kill must restart the container, not replace the Pod");
    assert_ne!(after.container_id, before.container_id);
    assert!(
        last["signal"] == 9 || last["exitCode"] == 137,
        "the last termination must be the SIGKILL: {last}"
    );

    // --- the run completes; evidence says exactly-once -----------------------
    // Budget: outbox claim-lease (30s) + the 20s sleep + step 2 + slack.
    let rs = wait_for_terminal(&client, &base, &run, Duration::from_secs(240)).await;
    assert_eq!(
        rs.status, "succeeded",
        "crash/resume run must succeed, got `{}`",
        rs.status
    );

    // Exactly one attempt per step — no duplicate execution.
    for step in ["slow", "second"] {
        assert_eq!(
            rs.step(step).attempts,
            1,
            "step `{step}` must have exactly one attempt (no re-execution after the crash)"
        );
    }

    let evs = events(&client, &base, &run).await;

    // Exactly one AttemptStarted per step (the durable launch, once).
    let started = events_of_kind(&evs, "AttemptStarted");
    for step in ["slow", "second"] {
        let n = started.iter().filter(|p| p["step"] == *step).count();
        assert_eq!(n, 1, "expected exactly one AttemptStarted for `{step}`, got {n}");
    }

    // The restarted control plane RE-ADOPTED the in-flight attempt — the
    // wedge made visible — rather than starting a second execution.
    let readopted = events_of_kind(&evs, "AttemptReadopted");
    assert!(
        readopted.iter().any(|p| p["step"] == "slow"),
        "expected an AttemptReadopted for `slow` after the restart; events: {:?}",
        evs.iter().map(support::event_kind).collect::<Vec<_>>()
    );
    eprintln!("crash_resume: run {run} AttemptReadopted: {readopted:?}");

    // Exactly one terminal transition, and it is Succeeded.
    let terminal: Vec<_> = events_of_kind(&evs, "RunTransitioned")
        .into_iter()
        .filter(|p| {
            matches!(
                p["to"].as_str(),
                Some("Succeeded" | "Failed" | "Cancelled" | "DeadLettered")
            )
        })
        .collect();
    assert_eq!(
        terminal.len(),
        1,
        "expected exactly one terminal transition, got {terminal:?}"
    );
    assert_eq!(terminal[0]["to"], "Succeeded");

    // And the logs prove both steps really ran (once).
    let text = logs(&client, &base, &run).await;
    assert!(text.contains("slow step done"), "missing step-1 log; got: {text}");
    assert!(text.contains("second step done"), "missing step-2 log; got: {text}");
}
