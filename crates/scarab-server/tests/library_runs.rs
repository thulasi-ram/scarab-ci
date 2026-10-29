//! ADR-0076 §4, §6: Runs of Pipelines that invoke an installed library.
//! Compiling resolves `lib:` against the org's installations and records the
//! version in `origin`; a yanked version is refused at Run creation, at
//! Step launch (Reruns and Retries included) and at gate release.
//!
//! Real admission, router and executor decorator; in-memory Db, forge and
//! stores.

use scarab_engine::Db;
use scarab_forge::{Event, RepoRef};
use scarab_policy::library::{
    content_hash, LibraryStore, LibraryVersion, NewInstallation, Replaces,
};
use scarab_server::{trigger_runs_from_event, AdmissionPorts, TriggerError};
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryLibraries};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

const DEPLOY: &str = "kind: library\nsteps:\n  - id: helm\n    image: alpine/helm@sha256:aaaa\n";

const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: test, image: busybox }
  - { id: ship, invoke: lib:acme/deploy, needs: [test] }
"#;

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "ship it".into(),
    }
}

fn forge(ci: &str) -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/ci.yaml", ci)
        .with_commit("refs/heads/main", SHA)
}

fn source() -> String {
    format!("github.com/acme/libs//deploy.yaml@{}", "1".repeat(40))
}

async fn installed(org: &str, content: &str) -> InMemoryLibraries {
    let libs = InMemoryLibraries::new();
    libs.put_version(&LibraryVersion {
        org: org.into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: source(),
        publisher: "acme".into(),
        name: "deploy".into(),
        first_party: false,
        fetched_by: "amy".into(),
        fetched_at: 1,
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: org.into(),
            publisher: "acme".into(),
            name: "deploy".into(),
            content_hash: content_hash(content),
            installed_by: "amy".into(),
            installed_at: 1,
            step_up_ref: None,
        },
        &Replaces::Nothing,
    )
    .await
    .unwrap();
    libs
}

async fn push_with(
    db: &dyn Db,
    libs: &dyn LibraryStore,
    forge: &FakeForge,
) -> Result<scarab_server::TriggerOutcome, TriggerError> {
    trigger_runs_from_event(
        forge,
        db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs),
        &push(),
    )
    .await
}

fn origin_hashes(ir: &serde_json::Value, step: &str) -> Vec<String> {
    ir["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == step)
        .unwrap()["origin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["content_hash"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_lib_target_resolves_to_the_orgs_installed_version() {
    let db = InMemoryDb::new();
    let libs = installed("acme", DEPLOY).await;
    let out = push_with(&db, &libs, &forge(CI_YAML)).await.unwrap();
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let ir = db.run_ir(&out.runs[0]).await.unwrap().unwrap();
    assert_eq!(origin_hashes(&ir, "ship/helm"), vec![content_hash(DEPLOY)]);
    let link = &ir["steps"].as_array().unwrap()[1]["origin"][0];
    assert_eq!(link["kind"], "library");
    assert_eq!(link["name"], "acme/deploy");
    assert_eq!(link["source"], source());
}

#[tokio::test]
async fn a_lib_target_not_installed_in_this_org_does_not_compile() {
    let db = InMemoryDb::new();
    // Installed in another org only.
    let libs = installed("globex", DEPLOY).await;
    let err = push_with(&db, &libs, &forge(CI_YAML)).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("`lib:acme/deploy` is not installed in this org"),
        "{err}"
    );
}
