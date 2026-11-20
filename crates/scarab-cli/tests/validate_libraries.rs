//! ADR-0076 §4: `scarab validate` with a `lib:` target. Offline, the
//! installed library is unresolved and the rest is checked; with `--server`,
//! the server compiles it against the org's installed libraries.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use scarab_policy::library::{
    content_hash, LibraryStore, LibraryVersion, NewInstallation, Replaces,
};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryLibraries, InMemoryObjectStore};

/// Reads an output of the installed library and needs it.
const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - id: ship
    invoke: lib:acme/deploy
    with: { release: web }
  - id: notify
    image: busybox
    needs: [ship]
    env: [[HELM, "${{ outputs.ship.helm }}"]]
    command: [sh, -c, 'echo "$HELM"']
"#;

/// The same, with an error outside the library: an unknown need.
const BROKEN_YAML: &str = r#"
on:
  push: {}
steps:
  - id: ship
    invoke: lib:acme/deploy
  - { id: notify, image: busybox, needs: [nowhere] }
"#;

/// A Step whose command reads a file-only secret as a variable: the local
/// compile warns.
const SECRET_YAML: &str = r#"
on:
  push: {}
steps:
  - id: deploy
    image: busybox
    secrets: [TOKEN]
    command: [sh, -c, 'curl -H "$TOKEN" https://example.com']
"#;

/// A Step with a `k8s_overlay`, which Run creation refuses for a Run that
/// targets no Environment permitting overlays.
const OVERLAY_YAML: &str = r#"
on:
  push: {}
steps:
  - id: build
    image: busybox
    k8s_overlay: { spec: { hostNetwork: true } }
"#;

const DEPLOY: &str = r#"
kind: library
interface:
  inputs:
    - { name: release, type: string }
  outputs: [helm]
steps:
  - id: helm
    image: alpine/helm@sha256:aaaa
    command: [helm, upgrade, "${{ inputs.release }}"]
  - id: verify
    image: alpine/helm@sha256:aaaa
    needs: [helm]
    command: [helm, status, "${{ inputs.release }}"]
"#;

async fn serve() -> String {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let libraries = Arc::new(InMemoryLibraries::new());
    libraries
        .put_version(&LibraryVersion {
            org: "acme".into(),
            content_hash: content_hash(DEPLOY),
            content: DEPLOY.into(),
            source: format!("github.com/acme/libs//deploy.yaml@{}", "1".repeat(40)),
            name: "acme/deploy".into(),
            fetched_by: "amy".into(),
            fetched_at: 1,
            requested_ref: None,
            resolved_commit: String::new(),
        })
        .await
        .unwrap();
    libraries
        .install(
            &NewInstallation {
                org: "acme".into(),
                name: "acme/deploy".into(),
                content_hash: content_hash(DEPLOY),
                installed_by: "amy".into(),
                installed_at: 1,
                step_up_ref: None,
                requested_ref: None,
                resolved_commit: String::new(),
            },
            &Replaces::Nothing,
        )
        .await
        .unwrap();
    let st = AppState::new(db, Arc::new(FakeClock::new(1_000_000_000)), logs)
        .with_policies(Arc::new(scarab_testkit::InMemoryPolicies::new()))
        .with_libraries(libraries);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(st);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn repo() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "scarab-validate-lib-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".scarab")).unwrap();
    std::fs::write(dir.join(".scarab/ci.yaml"), CI_YAML).unwrap();
    std::fs::write(dir.join(".scarab/broken.yaml"), BROKEN_YAML).unwrap();
    std::fs::write(dir.join(".scarab/secret.yaml"), SECRET_YAML).unwrap();
    std::fs::write(dir.join(".scarab/overlay.yaml"), OVERLAY_YAML).unwrap();
    for args in [
        &["init", "-q"][..],
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    ] {
        let mut c = Command::new("git");
        c.arg("-C").arg(&dir).args(args);
        clean_env(&mut c);
        assert!(c.status().expect("git").success(), "git {args:?}");
    }
    dir
}

fn clean_env(c: &mut Command) {
    for v in ["SCARAB_SERVER", "SCARAB_TOKEN", "GIT_DIR", "GIT_WORK_TREE"] {
        c.env_remove(v);
    }
}

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
async fn offline_reports_a_lib_target_as_unresolved_and_checks_the_rest() {
    let dir = repo();
    let out = output(scarab(&dir, &["validate", ".scarab/ci.yaml"])).await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains(
            "unresolved: `lib:acme/deploy` is installed on the server, not on disk; its Steps \
             were not checked (use --server to check them)"
        ),
        "{stdout}"
    );
    // The library is one stand-in Step offline.
    assert!(stdout.contains("ok: 2 step(s)"), "{stdout}");

    // The rest is still checked.
    let out = output(scarab(&dir, &["validate", ".scarab/broken.yaml"])).await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(stdout.contains("unresolved: `lib:acme/deploy`"), "{stdout}");
    assert!(stderr.contains("nowhere"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_a_server_the_installed_library_is_compiled_there() {
    let base = serve().await;
    let dir = repo();
    let args = |org: &'static str| {
        [
            "validate",
            ".scarab/ci.yaml",
            "--server",
            Box::leak(base.clone().into_boxed_str()),
            "--org",
            org,
            "--project",
            "web",
        ]
    };
    let out = output(scarab(&dir, &args("acme"))).await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("outcome: allow"), "{stdout}");
    // git-bug 1aa19e6: the server checked the library, so the output says
    // so, and counts the Run's Steps as the server compiled them (the
    // library's two, and `notify`), not the offline stand-in.
    assert!(!stdout.contains("not checked"), "{stdout}");
    assert!(stdout.contains("ok: 3 step(s), ir v"), "{stdout}");
    assert!(
        stdout.contains("compiled by the server with `lib:acme/deploy` as installed in org `acme`"),
        "{stdout}"
    );

    // In an org that has not installed it, the server says so.
    let out = output(scarab(&dir, &args("globex"))).await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_ne!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("`lib:acme/deploy` is not installed in this org"),
        "{stdout}{stderr}"
    );
}

// git-bug C5: Run creation refuses a Step's `k8s_overlay` (here: no
// Environment permits one) after its Policies are consulted; `validate
// --server` answered allow for it. It now answers deny with the reason.
#[tokio::test(flavor = "multi_thread")]
async fn with_a_server_a_refused_k8s_overlay_is_a_deny() {
    let base = serve().await;
    let dir = repo();
    let out = output(scarab(
        &dir,
        &[
            "validate",
            ".scarab/overlay.yaml",
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
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(stdout.contains("outcome: deny"), "{stdout}");
    assert!(
        stdout.contains("step `build`") && stdout.contains("k8s_overlay"),
        "{stdout}"
    );
}

// git-bug C6: the gates and warnings come from the local compile; a server
// that cannot be reached still leaves the author with them.
#[tokio::test(flavor = "multi_thread")]
async fn with_an_unreachable_server_the_local_warnings_still_print() {
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let dir = repo();
    let out = output(scarab(
        &dir,
        &[
            "validate",
            ".scarab/secret.yaml",
            "--server",
            &closed,
            "--org",
            "acme",
            "--project",
            "web",
        ],
    ))
    .await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_ne!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("warning: step `deploy`") && stdout.contains("`$TOKEN`"),
        "{stdout}{stderr}"
    );
}
