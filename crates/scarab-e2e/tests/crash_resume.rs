//! Scenario 2 — **crash/resume at stack grain, THE WEDGE.**
//!
//! Kill `scarab-server` (SIGKILL, no goodbye) while a step Pod is mid-flight
//! on the real kind cluster, let it restart, and prove the run completes
//! **exactly once**: the surviving attempt is re-adopted (`AttemptReadopted`),
//! never re-executed (no second `AttemptStarted` per step), and the run makes
//! exactly one terminal transition.
//!
//! The server under test is the release's own server Pod (`just test e2e` runs the
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
//!
//! The `log_*` cases (git-bug 2d78464) follow one step's log across a fault
//! and check what was stored: a step in an Environment prints `line 1` to
//! `line 90`, one a second, and every tenth line a secret of that
//! Environment. Across a server SIGKILL, then a kubelet restart and a
//! containerd restart on the node, the Run succeeds with one attempt, every
//! number is stored in order within each part of the log, a gap or a repeat
//! sits directly after a `source_restarted` marker, and the secret is in
//! neither the raw download nor the log stream. `just test e2e` runs this
//! binary with one test at a time, in name order.

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
/// namespace. Returns the node-side PID it signalled. The PID lookup and the
/// signal are the fault tier's (`scarab_chaos::node::Node`): `crictl inspect`
/// on the node, then a check that the PID's cgroup names the container, so
/// the Depot (same binary) is never the one killed.
async fn kill_server_from_node(target: &ServerContainer) -> i64 {
    let node = scarab_chaos::node::Node { name: kind_node() };
    let pid = node
        .container_pid(&target.container_id)
        .await
        .unwrap_or_else(|e| panic!("the server container's node PID: {e}"));
    let cmdline = on_node(&["sh", "-c", &format!("tr '\\0' ' ' < /proc/{pid}/cmdline")]);
    eprintln!(
        "crash_resume: SIGKILL from node {} to PID {pid} ({cmdline}), container {} of Pod {} (restarts before: {})",
        kind_node(),
        target.container_id,
        target.pod,
        target.restarts
    );
    node.signal(pid, "KILL")
        .await
        .unwrap_or_else(|e| panic!("SIGKILL to PID {pid}: {e}"));
    pid
}

/// Waits until the API answers `/readyz` at `base_url()`, on a fresh
/// connection each try. The container's `ready` is not enough: the
/// Service's endpoints and kube-proxy's rules follow the Pod's Ready
/// condition a moment later, and until then the host port accepts a
/// connection and closes it without an answer (reqwest: IncompleteMessage).
/// Each case also waits here before it starts, so a case that failed inside
/// that window does not fail the next one too.
async fn wait_api_answers() {
    let base = base_url();
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let last = match http.get(format!("{base}/readyz")).send().await {
            Ok(r) if r.status().is_success() => return,
            Ok(r) => r.status().to_string(),
            Err(e) => format!("{e:?}"),
        };
        assert!(
            Instant::now() < deadline,
            "the API at {base} did not answer /readyz within 60s: {last}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Waits for the kubelet to restart the killed container in the same Pod,
/// for it to be ready again and for the API to answer through the Service;
/// returns its new state and the last termination.
async fn wait_restarted(before: &ServerContainer) -> (ServerContainer, serde_json::Value) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let now = server_container();
        if now.restarts > before.restarts && now.ready {
            wait_api_answers().await;
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
    wait_api_answers().await;

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
    assert!(
        before.ready,
        "the server must be ready before the kill: {before:?}"
    );
    kill_server_from_node(&before).await;
    let (after, last) = wait_restarted(&before).await;
    eprintln!(
        "crash_resume: container restarted in place: Pod {} uid {} restarts {} -> {}, container {} -> {}, last termination {last}",
        after.pod, after.uid, before.restarts, after.restarts, before.container_id, after.container_id
    );
    // The same Pod (a restart, not a replacement) with a new container.
    assert_eq!(
        after.uid, before.uid,
        "the kill must restart the container, not replace the Pod"
    );
    assert_ne!(after.container_id, before.container_id);
    assert!(
        last["signal"] == 9 || last["exitCode"] == 137,
        "the last termination must be the SIGKILL: {last}"
    );

    // --- the run completes; evidence says exactly-once -----------------------
    // A new client: the old one's pooled keep-alive connection died with the
    // killed process, and its next request fails with IncompleteMessage.
    let client = support::client();
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
        assert_eq!(
            n, 1,
            "expected exactly one AttemptStarted for `{step}`, got {n}"
        );
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
    assert!(
        text.contains("slow step done"),
        "missing step-1 log; got: {text}"
    );
    assert!(
        text.contains("second step done"),
        "missing step-2 log; got: {text}"
    );
}

// --- a step's log across a fault (git-bug 2d78464) ---------------------------

/// The Environment the log cases' step runs in, and the secret's name.
const LOG_ENVIRONMENT: &str = "crash";

/// How many numbered lines the step prints, one a second.
const LOG_LINES: u32 = 90;

/// The pipeline the log cases dispatch: one step in Environment `crash` that
/// prints `line 1` to `line 90`, one a second, and after every tenth line
/// `secret <CRASH_SECRET>`. busybox runs as root, so the step takes the
/// `run_as_root` grant, as happy_path's does.
const LOG_PIPELINE_YAML: &str = r#"
on:
  manual: {}
environment: crash
steps:
  - id: count
    image: busybox:latest
    security:
      run_as_root: true
    secrets: [CRASH_SECRET]
    command:
      - sh
      - -c
      - |
        i=1
        while [ "$i" -le 90 ]; do
          echo "line $i"
          if [ $((i % 10)) -eq 0 ]; then echo "secret $(cat "$SCARAB_SECRETS/CRASH_SECRET")"; fi
          i=$((i + 1))
          sleep 1
        done
"#;

/// The commit the fake forge serves.
const LOG_SHA: &str = "c4a5000000000000000000000000000000000000";

/// A fake Forgejo serving `.scarab/crash.yaml` at [`LOG_SHA`]: the slice of
/// the API a dispatch reads (the commits list, the `main` branch and raw
/// contents), with every other write acknowledged. The server calls it at the host address a Pod
/// reaches, as happy_path's does.
async fn fake_forgejo() -> String {
    use axum::extract::Path;
    use axum::response::IntoResponse;
    use axum::routing::get;
    async fn commits() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!([
            { "sha": LOG_SHA, "commit": { "message": "crash_resume log fixture" } }
        ]))
    }
    // `GET branches/{name}`: the one branch, `main`, at the fixture commit.
    // Dispatch looks a bare ref name up as a branch and as a tag; `tags/main`
    // and every other GET fall through to a 404, so `main` is a branch only.
    async fn branch(
        Path((_o, _r, name)): Path<(String, String, String)>,
    ) -> axum::response::Response {
        if name == "main" {
            axum::Json(serde_json::json!({ "name": "main", "commit": { "id": LOG_SHA } }))
                .into_response()
        } else {
            axum::response::Response::builder()
                .status(404)
                .body(axum::body::Body::from("{}"))
                .unwrap()
        }
    }
    async fn raw(Path((_o, _r, path)): Path<(String, String, String)>) -> axum::response::Response {
        let (status, body) = if path == ".scarab/crash.yaml" {
            (200, LOG_PIPELINE_YAML)
        } else {
            (404, "no such file")
        };
        axum::response::Response::builder()
            .status(status)
            .body(axum::body::Body::from(body))
            .unwrap()
    }
    async fn fallback(req: axum::extract::Request) -> axum::response::Response {
        let status = if req.method() == axum::http::Method::GET {
            404
        } else {
            200
        };
        axum::response::Response::builder()
            .status(status)
            .body(axum::body::Body::from("{}"))
            .unwrap()
    }
    let app = axum::Router::new()
        .route("/api/v1/repos/{owner}/{repo}/commits", get(commits))
        .route("/api/v1/repos/{owner}/{repo}/branches/{*name}", get(branch))
        .route("/api/v1/repos/{owner}/{repo}/raw/{*path}", get(raw))
        .fallback(fallback);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind fake forgejo");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{}:{port}", pod_host())
}

/// A repository the log cases dispatch in: bound to a fake Forgejo
/// connection, with Environment `crash` admitting it and the secret
/// `CRASH_SECRET` at that Environment.
struct LogRepo {
    org: String,
    repo: String,
    secret: String,
}

/// Seed [`LogRepo`] through the e2e database (the connection, the binding
/// and the Environment, as happy_path seeds its connection) and the API
/// (the secrets). Done before any fault: the database's port-forward goes
/// through the kubelet and does not survive a kubelet restart.
async fn seed_log_repo() -> LogRepo {
    use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
    use scarab_project::{
        EnvironmentGovernanceStore, EnvironmentProjectEligibility, OrgEnvironment,
        OrgEnvironmentStore,
    };
    wait_api_answers().await;
    let base = base_url();
    let http = client();
    let forge_url = fake_forgejo().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    // Per invocation: the server caches adapters by connection id, and a
    // repo binds to one connection.
    let tag = format!("{}-{now}", std::process::id());
    let conn_id = format!("e2e-crash-{tag}");
    let cred_ref = format!("{conn_id}-token");
    let org = "e2e".to_string();
    let repo = format!("crash-{tag}");

    let db = scarab_db_postgres::PostgresDb::connect(&database_url())
        .await
        .expect("connect the e2e database (`just test e2e` port-forwards it)");
    db.put_connection(&ForgeConnection {
        id: conn_id.clone(),
        kind: ForgeKind::Forgejo,
        base_url: forge_url,
        credential_ref: cred_ref.clone(),
        org: Some(org.clone()),
        installation_id: None,
        account_id: None,
        account_login: None,
    })
    .await
    .expect("register the connection");
    db.bind_repo(
        &conn_id,
        &RepoRef {
            owner: org.clone(),
            name: repo.clone(),
        },
        &org,
        &repo,
    )
    .await
    .expect("bind the repo");
    let at = i64::try_from(now).expect("ms");
    db.put_org_environment(&OrgEnvironment {
        org: org.clone(),
        name: LOG_ENVIRONMENT.into(),
        display_name: None,
        description: "crash_resume log cases".into(),
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: at,
        updated_at: at,
        updated_by: "e2e".into(),
    })
    .await
    .expect("create Environment `crash`");
    db.put_environment_eligibility(
        &org,
        LOG_ENVIRONMENT,
        &EnvironmentProjectEligibility::AllProjects,
        "e2e",
        at,
    )
    .await
    .expect("admit every Project to `crash`");

    let secret = format!("crash-sekret-{tag}");
    for body in [
        serde_json::json!({ "org": "_forge", "name": cred_ref, "value": "e2e-token" }),
        serde_json::json!({
            "org": org, "repo": repo, "environment": LOG_ENVIRONMENT,
            "name": "CRASH_SECRET", "value": secret,
        }),
    ] {
        let resp = http
            .post(format!("{base}/v1/secrets"))
            .json(&body)
            .send()
            .await
            .expect("POST /v1/secrets");
        assert!(
            resp.status().is_success(),
            "storing a secret failed: {}",
            resp.status()
        );
    }
    LogRepo { org, repo, secret }
}

/// Dispatch the `crash` pipeline in `seeded` through the CLI and wait until
/// its step runs in a Pod, plus `settle` (lines printed before the fault).
/// Returns the run id.
async fn dispatch_counting_run(seeded: &LogRepo, settle: Duration) -> String {
    let base = base_url();
    let out = std::process::Command::new(cli_bin())
        .args([
            "run",
            &format!("{}/{}", seeded.org, seeded.repo),
            "crash",
            "--ref",
            "main",
            "--server",
            &base,
        ])
        .output()
        .expect("run the scarab CLI (`just test e2e` builds it)");
    assert!(
        out.status.success(),
        "`scarab run` failed (exit {:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let run = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!run.is_empty(), "`scarab run` printed no run id");
    wait_for_run(
        &client(),
        &base,
        &run,
        Duration::from_secs(180),
        "step `count` running with an attempt",
        |rs| {
            let s = rs.step("count");
            s.status == "running" && s.attempts >= 1
        },
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while pods_of_run(&run).is_empty() {
        assert!(
            Instant::now() < deadline,
            "run {run}: step `count` never got a Pod on the cluster"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    tokio::time::sleep(settle).await;
    run
}

/// The marker text of a `source_restarted` row, as a raw download writes it.
const RESTARTED: &str = "[scarab] the log source restarted";

/// Check a raw download of the counting step's log (markers as lines):
/// within each part every number follows the one before it; a jump or a
/// repeat sits directly after a `source_restarted` marker; no other marker
/// is there; the last line is `line 90`; every number missing was skipped
/// at such a marker; every secret line is redacted. A line a restart cut
/// short is allowed directly before the marker. Returns how many
/// `source_restarted` markers the log has.
fn check_counted_log(text: &str) -> usize {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut expected = 1u32;
    let mut after_marker = false;
    let mut markers = 0;
    let mut stored = std::collections::BTreeSet::new();
    let mut skipped = std::collections::BTreeSet::new();
    let mut secrets = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with("[scarab] ") {
            assert!(
                line.starts_with(RESTARTED),
                "a marker other than source_restarted: {line}\n{text}"
            );
            markers += 1;
            after_marker = true;
            continue;
        }
        if let Some(n) = line
            .strip_prefix("line ")
            .and_then(|n| n.parse::<u32>().ok())
        {
            if n != expected {
                assert!(
                    after_marker,
                    "`line {n}` where `line {expected}` was due, with no source_restarted \
                     marker before it:\n{text}"
                );
                skipped.extend(expected..n);
            }
            stored.insert(n);
            expected = n + 1;
            after_marker = false;
            continue;
        }
        if *line == "secret ***" {
            secrets += 1;
            after_marker = false;
            continue;
        }
        let next = lines[i + 1..].iter().find(|l| !l.is_empty());
        assert!(
            next.is_some_and(|l| l.starts_with(RESTARTED)),
            "an unexpected line {line:?} that is not cut short by a restart:\n{text}"
        );
    }
    assert_eq!(
        stored.last(),
        Some(&LOG_LINES),
        "the log ends at its last line:\n{text}"
    );
    for n in 1..=LOG_LINES {
        assert!(
            stored.contains(&n) || skipped.contains(&n),
            "`line {n}` is missing and no restart marker explains it:\n{text}"
        );
    }
    assert!(secrets > 0, "no redacted secret line:\n{text}");
    markers
}

/// After the fault: the Run succeeds with one attempt, the step's log is
/// whole ([`check_counted_log`]) and the secret is in neither raw download
/// (with and without marker lines) nor the log stream.
async fn assert_log_whole_and_redacted(run: &str, secret: &str, fault: &str) {
    wait_api_answers().await;
    let base = base_url();
    // A new client: a fault can kill the old one's pooled connection.
    let http = client();
    let rs = wait_for_terminal(&http, &base, run, Duration::from_secs(420)).await;
    assert_eq!(rs.status, "succeeded", "{fault}: the Run must succeed");
    assert_eq!(
        rs.step("count").attempts,
        1,
        "{fault}: one attempt, not re-executed"
    );

    let v: serde_json::Value = http
        .get(format!("{base}/v1/runs/{run}"))
        .send()
        .await
        .expect("GET run")
        .json()
        .await
        .expect("run JSON");
    let step = v["steps"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["id"] == "count"))
        .expect("step `count`");
    let key = step["key"].as_str().expect("step key");
    let attempt = step["attempt_list"][0]["id"].as_str().expect("attempt id");
    let raw = format!("{base}/v1/runs/{run}/steps/{key}/logs/raw?attempt={attempt}");
    let download = |url: String| {
        let http = http.clone();
        async move {
            let resp = http.get(&url).send().await.expect("GET raw log");
            assert!(resp.status().is_success(), "GET {url}: {}", resp.status());
            let markers: usize = resp
                .headers()
                .get("x-scarab-log-markers")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.parse().ok())
                .expect("the marker count header");
            (resp.text().await.expect("raw log body"), markers)
        }
    };
    let (marked, counted) = download(raw.clone()).await;
    let (plain, _) = download(format!("{raw}&markers=none")).await;
    let stream = logs(&http, &base, run).await;
    for (what, text) in [
        ("the raw download", &marked),
        ("the raw download without markers", &plain),
        ("the log stream", &stream),
    ] {
        assert!(
            !text.contains(secret),
            "{fault}: {what} holds the secret:\n{text}"
        );
    }
    let markers = check_counted_log(&marked);
    assert_eq!(markers, counted, "{fault}: marker lines and the header");
    eprintln!("crash_resume: {fault}: run {run} log whole, {markers} source_restarted marker(s)");
}

/// SIGKILL the server while it follows the step's log: the restarted server
/// reloads the step's value ledger, opens the log again, compares it with
/// what was stored and goes on; the log is whole and redacted.
#[tokio::test(flavor = "multi_thread")]
async fn log_1_across_a_server_sigkill_is_whole_and_redacted() {
    require_e2e!();
    let seeded = seed_log_repo().await;
    let run = dispatch_counting_run(&seeded, Duration::from_secs(20)).await;

    let before = server_container();
    assert!(
        before.ready,
        "the server must be ready before the kill: {before:?}"
    );
    kill_server_from_node(&before).await;
    let (after, last) = wait_restarted(&before).await;
    assert_eq!(after.uid, before.uid, "a restart, not a new Pod");
    assert!(
        last["signal"] == 9 || last["exitCode"] == 137,
        "the last termination must be the SIGKILL: {last}"
    );

    assert_log_whole_and_redacted(&run, &seeded.secret, "server SIGKILL").await;
}

/// Whether systemd unit `unit` is active on the kind node.
fn node_unit_active(unit: &str) -> bool {
    std::process::Command::new("docker")
        .args([
            "exec",
            &kind_node(),
            "systemctl",
            "is-active",
            "--quiet",
            unit,
        ])
        .status()
        .is_ok_and(|s| s.success())
}

/// Restart systemd unit `unit` on the kind node and wait until it and the
/// node are back (the unit active, the node `Ready`).
async fn restart_on_node(unit: &str) {
    eprintln!("crash_resume: systemctl restart {unit} on {}", kind_node());
    on_node(&["systemctl", "restart", unit]);
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let ready = node_unit_active(unit)
            && std::process::Command::new("kubectl")
                .env("KUBECONFIG", kubeconfig())
                .args([
                    "get",
                    "nodes",
                    "-o",
                    "jsonpath={.items[*].status.conditions[?(@.type==\"Ready\")].status}",
                ])
                .output()
                .is_ok_and(|o| {
                    o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "True"
                });
        if ready {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{unit} or the node was not back within 120s"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Restart the kubelet, then (a second Run) containerd, on the node while
/// the server follows the step's log. The follow breaks (the kubelet serves
/// it) and is opened again; the containers keep running. Both in one case,
/// in that order, seeded before either fault: the database port-forward the
/// seeding uses does not survive a kubelet restart.
#[tokio::test(flavor = "multi_thread")]
async fn log_2_across_a_kubelet_then_a_containerd_restart_is_whole_and_redacted() {
    require_e2e!();
    let seeded = seed_log_repo().await;
    for unit in ["kubelet", "containerd"] {
        let run = dispatch_counting_run(&seeded, Duration::from_secs(20)).await;
        restart_on_node(unit).await;
        assert_log_whole_and_redacted(&run, &seeded.secret, &format!("{unit} restart")).await;
    }
}
