//! ADR-0075 §2: `scarab validate`, driven as the real `scarab` binary.
//! Offline it reads the libraries a Pipeline invokes from the repository on
//! disk; with `--server` it dry-runs the `run.create` Policies through the
//! REAL server router (in process, on an ephemeral port) over an in-memory
//! PolicyStore holding an org Policy that denies a local library on `main`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use serde_json::json;

use scarab_policy::policy::Policy;
use scarab_policy::store::{NewPolicy, PolicyScope, PolicyStore};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore, InMemoryPolicies};

const DENY_MESSAGE: &str = "The repository's own deploy library may not be used on main.";

const LIB: &str = r#"
steps:
  - { id: helm, image: busybox, command: ["true"] }
"#;

/// Invokes the library, which invokes nothing.
const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: test, image: busybox, command: ["true"] }
  - { id: ship, invoke: .scarab/lib/deploy.yaml, needs: [test] }
"#;

/// Invokes no library.
const PLAIN_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: test, image: busybox, command: ["true"] }
"#;

/// The real router with policies configured and no login (a dev server
/// treats every caller as Owner), on an ephemeral port.
async fn serve() -> String {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let policies = Arc::new(InMemoryPolicies::new());
    let body = Policy::from_json(json!({
        "name": "no-deploy-lib-on-main",
        "checkpoint": "run.create",
        "mode": "enforce",
        "rules": [{
            "when": "trigger.ref == \"refs/heads/main\" && steps.exists(s, s.origin.exists(o, o.kind == \"file\" && o.name == \".scarab/lib/deploy.yaml\"))",
            "effect": "deny",
            "message": DENY_MESSAGE
        }]
    }))
    .unwrap();
    policies
        .create(&NewPolicy {
            id: "pol-1".into(),
            scope: PolicyScope::Org { org: "acme".into() },
            body,
            author: "amy".into(),
            reason: "test".into(),
            at: 1,
        })
        .await
        .unwrap();
    let st =
        AppState::new(db, Arc::new(FakeClock::new(1_000_000_000)), logs).with_policies(policies);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(st);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A fresh git repository on `main` holding `.scarab/ci.yaml` (the
/// invoking Pipeline), `.scarab/plain.yaml` and the library.
fn repo() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "scarab-validate-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".scarab/lib")).unwrap();
    std::fs::write(dir.join(".scarab/ci.yaml"), CI_YAML).unwrap();
    std::fs::write(dir.join(".scarab/plain.yaml"), PLAIN_YAML).unwrap();
    std::fs::write(dir.join(".scarab/lib/deploy.yaml"), LIB).unwrap();
    for args in [
        &["init", "-q"][..],
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    ] {
        let st = git(&dir).args(args).status().expect("git");
        assert!(st.success(), "git {args:?}");
    }
    dir
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    clean_env(&mut c);
    c
}

fn clean_env(c: &mut Command) {
    for v in ["SCARAB_SERVER", "SCARAB_TOKEN", "GIT_DIR", "GIT_WORK_TREE"] {
        c.env_remove(v);
    }
}

/// The real `scarab` binary, run in `dir`.
fn scarab(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_scarab"));
    c.args(args).current_dir(dir).stdin(Stdio::null());
    clean_env(&mut c);
    c
}

async fn output(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().expect("run scarab"))
        .await
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_validate_reads_invoked_libraries_from_the_repository() {
    let dir = repo();
    let out = output(scarab(&dir, &["validate", ".scarab/ci.yaml"])).await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    // `test` and the inlined `ship/helm`.
    assert!(stdout.contains("ok: 2 step(s)"), "{stdout}");

    // A library that is not on disk is named.
    std::fs::remove_file(dir.join(".scarab/lib/deploy.yaml")).unwrap();
    let out = output(scarab(&dir, &["validate", ".scarab/ci.yaml"])).await;
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("`.scarab/lib/deploy.yaml`"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn server_dry_run_allows_a_pipeline_without_the_library() {
    let base = serve().await;
    let dir = repo();
    let out = output(scarab(
        &dir,
        &[
            "validate",
            ".scarab/plain.yaml",
            "--server",
            &base,
            "--org",
            "acme",
            "--project",
            "web",
        ],
    ))
    .await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("policy no-deploy-lib-on-main (org, enforce): passed"),
        "{stdout}"
    );
    assert!(
        stdout.contains("decided for push on refs/heads/main"),
        "the rule reads the trigger: {stdout}"
    );
    assert!(stdout.contains("outcome: allow"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread")]
async fn server_dry_run_denies_the_library_with_the_policys_message() {
    let base = serve().await;
    let dir = repo();
    let args = [
        "validate",
        ".scarab/ci.yaml",
        "--server",
        &base,
        "--org",
        "acme",
        "--project",
        "web",
    ];
    // The ref defaults to the current branch, `main`.
    let out = output(scarab(&dir, &args)).await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(stdout.contains("(org, enforce): denied"), "{stdout}");
    assert!(stdout.contains(DENY_MESSAGE), "{stdout}");
    assert!(
        stdout.contains("rule 0 reads trigger.ref: decided for push on refs/heads/main"),
        "{stdout}"
    );
    assert!(stdout.contains("outcome: deny"), "{stdout}");

    // The same Pipeline for a push to another branch is allowed.
    let mut other = args.to_vec();
    other.extend(["--ref", "refs/heads/dev"]);
    let out = output(scarab(&dir, &other)).await;
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains("decided for push on refs/heads/dev"),
        "{stdout}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn server_dry_run_without_a_project_is_a_usage_error() {
    let base = serve().await;
    let dir = repo();
    let out = output(scarab(
        &dir,
        &[
            "validate",
            ".scarab/ci.yaml",
            "--server",
            &base,
            "--org",
            "acme",
        ],
    ))
    .await;
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("--project"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn scarab_server_in_the_environment_does_not_contact_a_server() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let dir = repo();
    let mut cmd = scarab(&dir, &["validate", ".scarab/ci.yaml"]);
    cmd.env("SCARAB_SERVER", format!("http://{addr}"));
    let out = output(cmd).await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("ok: 2 step(s)"), "{stdout}");
    assert!(!stdout.contains("outcome"), "{stdout}");
    // The binary has exited: any connection it made is in the backlog.
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("the CLI connected to SCARAB_SERVER: {other:?}"),
    }
}
