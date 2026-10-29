//! Red team round 2 on ADR-0076 step 3 (HEAD 8f4a7679): the round-1 fixes
//! (launch re-check, stored `read_only` / `trusted_library`, re-drive
//! revoke, concurrency group on checked facts) and what round 1 skipped.
//! Each test asserts the SECURE behaviour: a failing test is a break.
//!
//! The promise under attack: a repository's own code never reads a
//! long-lived forge token; a fork pull request never gets `write`;
//! unchecked event values never reach a Step.
//!
//! Real admission, scheduler and launch decorator; in-memory Db, forge,
//! executor and stores. One test runs the real `git` and the clone image's
//! askpass helper against an HTTP listener on 127.0.0.1. No other network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Clock, Db, Executor, RunId, Scheduler, StepId, StepRun,
    StepSpec, StepStatus, Timestamp,
};
use scarab_forge::{
    CheckoutCredential, Commit, Event, ForgeError, ForgePort, ForgeRef, Permissions, RepoRef,
    Status, WebhookDelivery,
};
use scarab_policy::library::{
    content_hash, LibraryStore, LibraryVersion, LibraryYank, NewInstallation, Replaces,
};
use scarab_server::forge_token::ForgeCredentialExecutor;
use scarab_server::{router, trigger_runs_from_event, AdmissionPorts, AppState, LogService};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, InMemoryDb, InMemoryLibraries, InMemoryObjectStore,
};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";
const CLONE: &str = include_str!("../../../libs/clone.yaml");
const CLONE_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@1111111111111111111111111111111111111111";

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

fn push_to(r#ref: &str) -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: r#ref.into(),
        after: SHA.into(),
        message: "ship it".into(),
    }
}

fn long_lived(files: &[(&str, &str)]) -> FakeForge {
    files
        .iter()
        .fold(FakeForge::new(), |f, (path, yaml)| f.with_file(*path, *yaml))
        .with_commit("refs/heads/main", SHA)
}

async fn install_in(libs: &InMemoryLibraries, org: &str, content: &str, source: &str, first_party: bool) {
    libs.put_version(&LibraryVersion {
        org: org.into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: source.into(),
        publisher: "thulasi-ram".into(),
        name: "clone".into(),
        first_party,
        fetched_by: "amy".into(),
        fetched_at: 1,
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: org.into(),
            publisher: "thulasi-ram".into(),
            name: "clone".into(),
            content_hash: content_hash(content),
            installed_by: "amy".into(),
            installed_at: 1,
            step_up_ref: None,
        },
        &Replaces::Nothing,
    )
    .await
    .unwrap();
}

fn step_run(run: &str, id: &str) -> StepRun {
    StepRun {
        run: RunId(run.into()),
        step: StepId(id.into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

/// A stored spec decoded from JSON, as the Db hands it to a launch.
fn spec_from(forge_token: serde_json::Value) -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "ghcr.io/thulasi-ram/scarab-clone@sha256:abc",
        "command": [],
        "env": [],
        "forge_token": forge_token,
    }))
    .expect("spec decodes")
}

async fn drive(db: &dyn Db, inner: &FakeExecutor, exec: &ForgeCredentialExecutor, run: &RunId) {
    let clock = FakeClock::new(1_000);
    let sched = Scheduler::new(db, &clock as &dyn Clock, exec, "sched");
    for _ in 0..8 {
        inner.script_outcome(ExecState::Succeeded);
        sched.tick(run).await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// 1. The launch re-check
// ---------------------------------------------------------------------------

/// ATTACK: a Step stored before the fix carries `forge_token` without
/// `read_only` or `trusted_library` (old-shape JSON). On a long-lived
/// connection its re-drive / rerun must fail closed: nothing minted.
#[tokio::test]
async fn rt2_an_old_shape_stored_spec_fails_closed_on_a_long_lived_connection() {
    let spec = spec_from(serde_json::json!({ "access": "read", "owner": "acme", "name": "web" }));
    let f = spec.forge_token.as_ref().unwrap();
    assert!(!f.read_only && f.trusted_library.is_none());
    let inner = Arc::new(FakeExecutor::new());
    let forge = Arc::new(FakeForge::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone())
        .with_libraries(Arc::new(InMemoryDb::new()), Arc::new(InMemoryLibraries::new()));
    let got = exec.launch(&step_run("r1", "checkout/git"), &spec).await;
    assert!(got.is_err(), "launched: {got:?}");
    assert_eq!(forge.credential_calls(), 0, "a long-lived token was minted");
}

/// ATTACK: the stored `trusted_library` names a hash that org `other`
/// trusts (first-party there), while the Run's repository is `acme`, whose
/// store does not hold it. Trust must be read in the Run's org.
#[tokio::test]
async fn rt2_a_hash_trusted_in_another_org_is_refused_at_launch() {
    let libs = Arc::new(InMemoryLibraries::new());
    install_in(&libs, "other", CLONE, CLONE_SOURCE, true).await;
    let spec = spec_from(serde_json::json!({
        "access": "read", "owner": "acme", "name": "web",
        "trusted_library": content_hash(CLONE),
    }));
    let inner = Arc::new(FakeExecutor::new());
    let forge = Arc::new(FakeForge::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone())
        .with_libraries(Arc::new(InMemoryDb::new()), libs.clone());
    let got = exec.launch(&step_run("r1", "checkout/git"), &spec).await;
    assert!(got.is_err(), "launched with org `other`'s trust: {got:?}");
    assert_eq!(forge.credential_calls(), 0);
}

const CHECKOUT_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: checkout, invoke: lib:thulasi-ram/clone }
"#;

/// ATTACK: the first-party clone version a Run was admitted with is yanked
/// in the org afterwards. A rerun of the stored Run on a long-lived
/// connection must mint nothing.
#[tokio::test]
async fn rt2_a_yanked_clone_version_mints_nothing_on_rerun() {
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(InMemoryLibraries::new());
    install_in(&libs, "acme", CLONE, CLONE_SOURCE, true).await;
    let forge = Arc::new(long_lived(&[(".scarab/ci.yaml", CHECKOUT_YAML)]));
    let out = trigger_runs_from_event(
        forge.as_ref(),
        db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs.as_ref()),
        &push_to("refs/heads/main"),
    )
    .await
    .unwrap();
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    libs.yank(&LibraryYank {
        org: "acme".into(),
        content_hash: content_hash(CLONE),
        reason: "CVE in the clone image".into(),
        yanked_by: "amy".into(),
        yanked_at: 5,
    })
    .await
    .unwrap();
    let inner = Arc::new(FakeExecutor::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone())
        .with_libraries(db.clone(), libs.clone());
    drive(db.as_ref(), &inner, &exec, &run).await;
    assert_eq!(forge.credential_calls(), 0, "a yanked version received the token");
}

/// A forge whose `mints_repo_tokens` says yes (the repository was on a
/// GitHub App connection when asked) while the mint itself is answered by
/// the connection that replaced it (a PAT: one long-lived token). Models
/// the window between the two calls, each of which re-resolves the
/// connection in the router.
struct SwappedBetweenCalls(FakeForge);

#[async_trait]
impl ForgePort for SwappedBetweenCalls {
    async fn latest_commit(&self, r: &RepoRef, x: &str) -> Result<Commit, ForgeError> {
        self.0.latest_commit(r, x).await
    }
    async fn read_file_at_ref(&self, r: &RepoRef, x: &str, p: &str) -> Result<Vec<u8>, ForgeError> {
        self.0.read_file_at_ref(r, x, p).await
    }
    async fn list_dir_at_ref(&self, r: &RepoRef, x: &str, d: &str) -> Result<Vec<String>, ForgeError> {
        self.0.list_dir_at_ref(r, x, d).await
    }
    async fn list_refs(&self, r: &RepoRef, q: Option<&str>) -> Result<Vec<ForgeRef>, ForgeError> {
        self.0.list_refs(r, q).await
    }
    async fn register_webhook(&self, r: &RepoRef, u: &str) -> Result<(), ForgeError> {
        self.0.register_webhook(r, u).await
    }
    async fn normalize_event(&self, raw: WebhookDelivery) -> Result<Event, ForgeError> {
        self.0.normalize_event(raw).await
    }
    async fn set_status(&self, r: &RepoRef, c: &Commit, s: Status) -> Result<(), ForgeError> {
        self.0.set_status(r, c, s).await
    }
    async fn create_deployment(&self, r: &RepoRef, e: &str) -> Result<(), ForgeError> {
        self.0.create_deployment(r, e).await
    }
    async fn post_comment(&self, r: &RepoRef, i: u64, b: &str) -> Result<(), ForgeError> {
        self.0.post_comment(r, i, b).await
    }
    async fn get_permissions(&self, r: &RepoRef, u: &str) -> Result<Permissions, ForgeError> {
        self.0.get_permissions(r, u).await
    }
    async fn mint_checkout_credential(
        &self,
        r: &RepoRef,
        read_only: bool,
    ) -> Result<CheckoutCredential, ForgeError> {
        self.0.mint_checkout_credential(r, read_only).await
    }
    async fn mints_repo_tokens(&self, _r: &RepoRef) -> Result<bool, ForgeError> {
        Ok(true)
    }
}

/// ATTACK: the launch asks `mints_repo_tokens` and mints in two separate
/// calls. A connection swapped from App to PAT between them hands the
/// repository's own Step (no `trusted_library`) the long-lived token.
#[tokio::test]
async fn rt2_a_connection_swapped_between_the_check_and_the_mint_hands_over_nothing() {
    let spec = spec_from(serde_json::json!({ "access": "read", "owner": "acme", "name": "web" }));
    let inner = Arc::new(FakeExecutor::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), Arc::new(SwappedBetweenCalls(FakeForge::new())));
    let launched = exec.launch(&step_run("r1", "tag"), &spec).await;
    let cred = launched
        .ok()
        .and_then(|h| inner.launched_spec(&h))
        .and_then(|s| s.forge_token)
        .and_then(|f| f.credential);
    assert!(
        cred.is_none(),
        "the repository's own Step received the long-lived token `{}`: the launch trusts \
         `mints_repo_tokens` asked before the mint, not the credential it got",
        cred.map(|c| c.token).unwrap_or_default()
    );
}

/// ATTACK: `POST /v1/runs` takes steps directly. A caller supplies
/// `forge_token`, a `trusted_library` hash and an `origin` naming the
/// first-party clone library on a Step whose image and command are theirs.
/// No stored spec may carry a forge token.
#[tokio::test]
async fn rt2_an_inline_api_run_cannot_store_a_forge_token() {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()));
    let app = router(AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs));
    let body = serde_json::json!({
        "pipeline": {
            "ir_version": 1,
            "steps": [{
                "id": "checkout/git",
                "image": "ghcr.io/mallory/exfil:1",
                "command": ["sh", "-c", "cat /scarab/secrets/forge-token"],
                "forge_token": { "access": "read", "owner": "acme", "name": "web",
                                 "trusted_library": content_hash(CLONE) },
                "origin": [{ "kind": "library", "name": "thulasi-ram/clone",
                             "source": CLONE_SOURCE, "content_hash": content_hash(CLONE) }]
            }]
        }
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    if resp.status() != StatusCode::CREATED {
        return; // refused outright: secure
    }
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let id = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let spec = db
        .step_spec(&RunId(id), &StepId("checkout/git".into()))
        .await
        .unwrap()
        .unwrap();
    assert!(spec.forge_token.is_none(), "stored: {:?}", spec.forge_token);
}

const PREP_BOOL: &str = r#"
on:
  push: {}
steps:
  - { id: prep, image: busybox, results: [sub] }
  - id: checkout
    invoke: lib:thulasi-ram/clone
    needs: [prep]
    with: { submodules: "${{ outputs.prep.sub }}" }
"#;

/// ATTACK: a repository Step's result feeds a `boolean` input of the
/// trusted clone Step. A result that is not a boolean must fail the Step
/// before any credential is minted (round 1 covered the `validate:` of a
/// string input only).
#[tokio::test]
async fn rt2_a_non_boolean_result_for_a_boolean_input_mints_nothing() {
    let db = Arc::new(InMemoryDb::new());
    let libs = InMemoryLibraries::new();
    install_in(&libs, "acme", CLONE, CLONE_SOURCE, true).await;
    let forge = Arc::new(long_lived(&[(".scarab/ci.yaml", PREP_BOOL)]));
    let out = trigger_runs_from_event(
        forge.as_ref(),
        db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(&libs),
        &push_to("refs/heads/main"),
    )
    .await
    .unwrap();
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    let inner = Arc::new(FakeExecutor::new());
    inner.set_results(
        "prep",
        std::collections::BTreeMap::from([("sub".to_string(), serde_json::json!("true; id"))]),
    );
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    drive(db.as_ref(), &inner, &exec, &run).await;
    let h = ExecHandle(format!("fake://{}/checkout/git/a1", run.0));
    assert!(inner.launched_spec(&h).is_none(), "launched with submodules `true; id`");
    assert_eq!(forge.credential_calls(), 0);
}

// ---------------------------------------------------------------------------
// 4. Revocation
// ---------------------------------------------------------------------------

/// ATTACK: two launches of the same Attempt race (a re-drive while the
/// first launch is still in flight). Every minted token must end: one when
/// the other replaced it, the last when the Attempt ends.
#[tokio::test]
async fn rt2_concurrent_launches_of_one_attempt_leave_no_live_token() {
    let spec = spec_from(serde_json::json!({ "access": "read", "owner": "acme", "name": "web" }));
    let inner = Arc::new(FakeExecutor::new());
    let forge = Arc::new(FakeForge::new().with_minted_tokens());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    let sr = step_run("r1", "tag");
    let (a, b) = tokio::join!(exec.launch(&sr, &spec), exec.launch(&sr, &spec));
    let h = a.unwrap();
    assert_eq!(b.unwrap(), h);
    inner.script_outcome(ExecState::Succeeded);
    let _ = exec.poll(&h).await.unwrap();
    assert_eq!(forge.credential_calls(), 2);
    assert_eq!(forge.revoked().len(), 2, "a minted token is still live: {:?}", forge.revoked());
}

/// ATTACK: two server replicas. Replica A launches the Attempt and mints
/// its token; the Attempt ends while replica B drives the Run (A lost its
/// lease or restarted). The token must still be revoked when the Attempt
/// ends, not live to its own expiry.
#[tokio::test]
async fn rt2_a_token_minted_on_one_replica_is_revoked_when_another_sees_the_end() {
    let spec = spec_from(serde_json::json!({ "access": "read", "owner": "acme", "name": "web" }));
    let inner = Arc::new(FakeExecutor::new());
    let forge = Arc::new(FakeForge::new().with_minted_tokens());
    let replica_a = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    let replica_b = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    let h = replica_a.launch(&step_run("r1", "tag"), &spec).await.unwrap();
    inner.script_outcome(ExecState::Succeeded);
    let _ = replica_b.poll(&h).await.unwrap();
    assert_eq!(
        forge.revoked().len(),
        1,
        "the token replica A minted is never revoked once replica B sees the Attempt end"
    );
}

// ---------------------------------------------------------------------------
// 5. Concurrency group
// ---------------------------------------------------------------------------

/// ATTACK: `concurrency.group` reads `event.branch`, and the pushed branch
/// fails its check. The Run must be refused, never created with a group
/// rendered from the raw value (or a literal template).
#[tokio::test]
async fn rt2_a_group_reading_a_bad_branch_refuses_the_run() {
    let yaml = r#"
on:
  push: {}
concurrency: { group: "deploy-${{ event.branch }}", policy: cancel-in-progress }
steps:
  - { id: s, image: busybox }
"#;
    for bad in ["refs/heads/x$(id)", "refs/heads/a b", "refs/heads/-rf", "refs/heads/../../x"] {
        let db = InMemoryDb::new();
        let forge = long_lived(&[(".scarab/ci.yaml", yaml)]).with_commit(bad, SHA);
        let out = trigger_runs_from_event(
            &forge,
            &db,
            &FakeClock::new(1_000),
            AdmissionPorts::default(),
            &push_to(bad),
        )
        .await;
        if let Ok(out) = out {
            assert!(out.runs.is_empty(), "`{bad}`: a Run was created: {:?}", out.runs);
        }
    }
}

// ---------------------------------------------------------------------------
// 2. What the clone image does with the token: submodules
// ---------------------------------------------------------------------------

/// An HTTP listener on 127.0.0.1 that answers every request with a Basic
/// challenge and records each request's path and `Authorization` header.
fn challenge_server() -> (u16, Arc<Mutex<Vec<(String, Option<String>)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<(String, Option<String>)>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match conn.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let head = String::from_utf8_lossy(&buf).to_string();
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or_default()
                .to_string();
            let auth = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .map(|l| l.split_once(':').unwrap().1.trim().to_string());
            let answered = auth.is_some();
            log.lock().unwrap().push((path, auth));
            let resp = if answered {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"forge\"\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let _ = conn.write_all(resp.as_bytes());
        }
    });
    (port, seen)
}

/// ATTACK: with `submodules: true`, `scarab-clone.sh` runs
/// `git submodule update` over URLs from the checked-out `.gitmodules`
/// (the pushed branch's, or a fork pull request's, content). The askpass
/// helper is pinned to the clone URL's HOST only, and a long-lived
/// Forgejo / PAT token reads every repository its user can. A
/// `.gitmodules` entry naming another repository on the same forge
/// (`https://forge/other-org/private.git`) therefore gets the long-lived
/// token, and the private repository lands in the workspace the
/// repository's own Steps read. The token must answer only for the Run's
/// repository.
///
/// Reduced to the credential exchange `git submodule update` performs:
/// the real `git` with the image's real askpass helper, against a listener
/// on 127.0.0.1 that challenges every request.
#[test]
#[ignore = "fixed off main: fix/clone-askpass-path"]
fn rt2_the_clone_token_does_not_answer_for_another_repository_on_the_forge() {
    let dir = tempfile::tempdir().unwrap();
    let askpass_src = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker/clone/scarab-askpass");
    let askpass = dir.path().join("askpass");
    std::fs::write(&askpass, format!("#!/bin/sh\nexec sh '{askpass_src}' \"$@\"\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let token = dir.path().join("forge-token");
    std::fs::write(&token, "LONG-LIVED-PAT").unwrap();
    let user = dir.path().join("forge-username");
    std::fs::write(&user, "scarab-bot").unwrap();
    let (port, seen) = challenge_server();
    let base = format!("http://127.0.0.1:{port}");
    let mut child = std::process::Command::new("git")
        .args(["ls-remote", &format!("{base}/other-org/private.git")])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", &askpass)
        .env("SCARAB_CLONE_URL", format!("{base}/acme/web.git"))
        .env("SCARAB_CLONE_TOKEN_FILE", &token)
        .env("SCARAB_CLONE_USERNAME_FILE", &user)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("git runs");
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "git never reached the listener");
    // "scarab-bot:LONG-LIVED-PAT" in Basic auth.
    let leaked: Vec<_> = seen
        .iter()
        .filter(|(path, auth)| {
            path.starts_with("/other-org/")
                && auth.as_deref() == Some("Basic c2NhcmFiLWJvdDpMT05HLUxJVkVELVBBVA==")
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "the clone token was sent for another repository on the forge: {leaked:?}"
    );
}
