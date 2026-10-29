//! ADR-0076 §9: which Steps may receive a forge credential, decided when the
//! Run is created, and the `event` values a Run's templates read.
//!
//! - A Step receives a forge credential only when the forge mints a
//!   short-lived repository token (GitHub App mode), or when it comes from a
//!   library the org trusts: the first-party clone library, or a source an
//!   org admin trusted. A repository's own Step never gets a Forgejo or PAT
//!   connection's token.
//! - A fork pull request gets `read` whatever it asks for.
//! - `write` needs a minting forge, a Run governed by an Environment that
//!   restricts refs, and, where that Environment has approvers, a manual gate
//!   upstream.
//! - An `event` field a Pipeline reads is checked; a bad value refuses only
//!   the Pipelines that read it.
//!
//! Real admission and launch decorator; in-memory Db, forge and stores.

use std::sync::Arc;

use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{Clock, Db, RunId, RunStatus, Scheduler, StepId};
use scarab_forge::{Event, RepoRef};
use scarab_pipeline::ForgeToken;
use scarab_policy::library::{
    content_hash, LibraryStore, LibraryVersion, NewInstallation, Replaces, TrustedSource,
};
use scarab_project::{
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProtectionRules,
};
use scarab_server::forge_token::ForgeCredentialExecutor;
use scarab_server::{trigger_runs_from_event, AdmissionPorts, TriggerOutcome};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, InMemoryDb, InMemoryEnvironmentGovernance,
    InMemoryLibraries,
};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";
const CLONE: &str = include_str!("../../../libs/clone.yaml");
const CLONE_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@1111111111111111111111111111111111111111";

const CHECKOUT_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: prep, image: busybox }
  - { id: checkout, invoke: lib:thulasi-ram/clone, needs: [prep] }
  - { id: build, image: busybox, needs: [checkout] }
"#;

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

fn push() -> Event {
    push_to("refs/heads/main")
}

fn fork_pr() -> Event {
    Event::PullRequest {
        actor: "mallory".into(),
        actor_id: String::new(),
        repo: repo(),
        number: 7,
        head: SHA.into(),
        title: "fix".into(),
        base: "main".into(),
        fork: true,
    }
}

/// A Forgejo or GitHub PAT connection: one long-lived token.
fn long_lived(files: &[(&str, &str)]) -> FakeForge {
    files
        .iter()
        .fold(FakeForge::new(), |f, (path, yaml)| f.with_file(*path, *yaml))
        .with_commit("refs/heads/main", SHA)
}

/// GitHub App mode: a short-lived repository token per mint.
fn minting(files: &[(&str, &str)]) -> FakeForge {
    long_lived(files).with_minted_tokens()
}

/// `content` installed in `acme` as `<publisher>/<name>` from `source`.
async fn install(
    libs: &InMemoryLibraries,
    publisher: &str,
    name: &str,
    content: &str,
    source: &str,
    first_party: bool,
) {
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: source.into(),
        publisher: publisher.into(),
        name: name.into(),
        first_party,
        fetched_by: "amy".into(),
        fetched_at: 1,
    })
    .await
    .unwrap();
    let current = libs
        .installation("acme", publisher, name)
        .await
        .unwrap()
        .map(|i| Replaces::Version(i.content_hash))
        .unwrap_or(Replaces::Nothing);
    libs.install(
        &NewInstallation {
            org: "acme".into(),
            publisher: publisher.into(),
            name: name.into(),
            content_hash: content_hash(content),
            installed_by: "amy".into(),
            installed_at: 1,
            step_up_ref: None,
        },
        &current,
    )
    .await
    .unwrap();
}

async fn with_first_party_clone() -> InMemoryLibraries {
    let libs = InMemoryLibraries::new();
    install(&libs, "thulasi-ram", "clone", CLONE, CLONE_SOURCE, true).await;
    libs
}

async fn trigger(
    db: &dyn Db,
    forge: &FakeForge,
    ports: AdmissionPorts<'_>,
    event: &Event,
) -> TriggerOutcome {
    trigger_runs_from_event(forge, db, &FakeClock::new(1_000), ports, event)
        .await
        .unwrap()
}

fn refused_reason(out: &TriggerOutcome) -> String {
    assert!(out.runs.is_empty(), "expected no Run: {:?}", out.runs);
    assert_eq!(out.refused.len(), 1, "{:?}", out.refused);
    out.refused[0].reason.clone()
}

async fn stored_token(db: &InMemoryDb, run: &RunId, step: &str) -> Option<ForgeToken> {
    db.step_spec(run, &StepId(step.into()))
        .await
        .unwrap()
        .unwrap()
        .forge_token
        .map(|f| f.access)
}

// ---------------------------------------------------------------------------
// Which Steps may receive a credential
// ---------------------------------------------------------------------------

const OWN_STEP_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: tag, image: alpine/git, forge_token: read }
"#;

/// The repository's own Step on a Forgejo or PAT connection: the Run is
/// refused, so its code never reads the long-lived token.
#[tokio::test]
async fn a_repositorys_own_forge_token_step_on_a_long_lived_connection_is_refused() {
    let db = InMemoryDb::new();
    let libs = with_first_party_clone().await;
    let forge = long_lived(&[(".scarab/ci.yaml", OWN_STEP_YAML)]);
    let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &push()).await;
    let reason = refused_reason(&out);
    assert!(
        reason.contains("step `tag`") && reason.contains("a repository's own Steps never receive it"),
        "{reason}"
    );
    assert_eq!(forge.credential_calls(), 0);

    // The same Step on a GitHub App connection, which mints a short-lived
    // repository token: admitted.
    let forge = minting(&[(".scarab/ci.yaml", OWN_STEP_YAML)]);
    let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    assert_eq!(stored_token(&db, &out.runs[0], "tag").await, Some(ForgeToken::Read));
}

/// The trusted first-party clone library's Step on a long-lived connection:
/// admitted, it reads the Run's commit and clone URL, consumes no
/// workspace, and at launch gets the credential beside its env — no env
/// value is the token.
#[tokio::test]
async fn the_first_party_clone_step_gets_the_credential_and_no_env_carries_it() {
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(with_first_party_clone().await);
    let forge = Arc::new(long_lived(&[(".scarab/ci.yaml", CHECKOUT_YAML)]));
    let out = trigger(
        db.as_ref(),
        forge.as_ref(),
        AdmissionPorts::default().with_libraries(libs.as_ref()),
        &push(),
    )
    .await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    let git = StepId("checkout/git".into());
    assert_eq!(stored_token(&db, &run, "checkout/git").await, Some(ForgeToken::Read));
    // Trusted only for a long-lived token: it consumes no workspace, so no
    // file `prep` wrote is in /workspace while the token is mounted.
    assert_eq!(db.step_inputs(&run, &git).await.unwrap(), Some(vec![]));
    let facts = db.run_event_facts(&run).await.unwrap();
    assert_eq!(facts["sha"], SHA);
    assert_eq!(facts["repo"]["clone_url"], "https://github.com/acme/web.git");

    let inner = Arc::new(FakeExecutor::new());
    // Each launch on a long-lived connection checks the org still trusts
    // the Step's library, so the launch reads the library store too.
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone())
        .with_libraries(db.clone(), libs.clone());
    let clock = FakeClock::new(1_000);
    let sched = Scheduler::new(db.as_ref(), &clock as &dyn Clock, &exec, "sched");
    inner.script_outcome(ExecState::Succeeded); // prep
    for _ in 0..6 {
        sched.tick(&run).await.unwrap();
    }
    let attempts = db.attempts_of_step(&run, &git).await.unwrap();
    let handle = ExecHandle(format!("fake://{}/checkout/git/{}", run.0, attempts[0].id.0));
    let spec = inner.launched_spec(&handle).expect("checkout/git launched");
    let cred = spec
        .forge_token
        .as_ref()
        .and_then(|f| f.credential.clone())
        .expect("the credential reaches the executor");
    let env: std::collections::BTreeMap<_, _> = spec.env.iter().cloned().collect();
    assert!(
        spec.env.iter().all(|(_, v)| v != &cred.token),
        "no env value is the token: {:?}",
        spec.env
    );
    assert_eq!(env["SCARAB_CLONE_SHA"], SHA, "a library reads event.sha");
    assert_eq!(env["SCARAB_CLONE_URL"], "https://github.com/acme/web.git");
}

/// A forge connection served over `http://`: a Run with a forge-token Step
/// is refused, so the token is never sent over plain HTTP. A Run without
/// one is admitted.
#[tokio::test]
async fn a_forge_token_step_on_an_http_connection_is_refused() {
    use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind};
    let db = InMemoryDb::new();
    db.put_connection(&ForgeConnection {
        id: "fj".into(),
        kind: ForgeKind::Forgejo,
        base_url: "http://git.example.com".into(),
        credential_ref: "fj-token".into(),
    })
    .await
    .unwrap();
    db.bind_repo("fj", &repo(), "acme", "web").await.unwrap();
    let libs = with_first_party_clone().await;
    let forge = minting(&[(".scarab/ci.yaml", CHECKOUT_YAML)]);
    let ports = AdmissionPorts {
        connections: Some(&db),
        ..AdmissionPorts::default().with_libraries(&libs)
    };
    let reason = refused_reason(&trigger(&db, &forge, ports, &push()).await);
    assert!(
        reason.contains("step `checkout/git`") && reason.contains("not `https://`"),
        "{reason}"
    );
    let plain = "on: { push: {} }\nsteps: [{ id: s, image: busybox }]\n";
    let forge = minting(&[(".scarab/ci.yaml", plain)]);
    let ports = AdmissionPorts {
        connections: Some(&db),
        ..AdmissionPorts::default().with_libraries(&libs)
    };
    assert_eq!(trigger(&db, &forge, ports, &push()).await.runs.len(), 1);
}

/// The same clone library content, not first-party, from another source:
/// the org has not trusted it, so it is the repository's choice of code.
#[tokio::test]
async fn a_clone_copy_that_is_not_first_party_is_refused_a_long_lived_token() {
    let db = InMemoryDb::new();
    let libs = InMemoryLibraries::new();
    install(
        &libs,
        "thulasi-ram",
        "clone",
        CLONE,
        "github.com/mallory/clone//libs/clone.yaml@2222222222222222222222222222222222222222",
        false,
    )
    .await;
    let forge = long_lived(&[(".scarab/ci.yaml", CHECKOUT_YAML)]);
    let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &push()).await;
    assert!(refused_reason(&out).contains("step `checkout/git`"));
}

const PUSHER: &str = r#"kind: library
steps:
  - id: git
    image: "alpine/git@sha256:0000000000000000000000000000000000000000000000000000000000000000"
    forge_token: read
"#;

const PUSHER_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: sync, invoke: lib:acme/pusher }
"#;

/// Trust is keyed by the installation's source: it carries across an
/// upgrade from that source, and a different source is not trusted.
#[tokio::test]
async fn trust_follows_the_source_across_upgrades() {
    let db = InMemoryDb::new();
    let libs = InMemoryLibraries::new();
    let at = |commit: char| format!("github.com/acme/libs//pusher.yaml@{}", commit.to_string().repeat(40));
    install(&libs, "acme", "pusher", PUSHER, &at('1'), false).await;
    let forge = long_lived(&[(".scarab/ci.yaml", PUSHER_YAML)]);
    let ports = || AdmissionPorts::default().with_libraries(&libs);
    assert!(refused_reason(&trigger(&db, &forge, ports(), &push()).await).contains("step `sync/git`"));

    libs.trust_source(&TrustedSource {
        org: "acme".into(),
        source: "github.com/acme/libs//pusher.yaml".into(),
        trusted_by: "amy".into(),
        trusted_at: 2,
        step_up_ref: Some("su-1".into()),
    })
    .await
    .unwrap();
    let out = trigger(&db, &forge, ports(), &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);

    // An upgrade from the same source keeps the trust.
    let v2 = format!("{PUSHER}    timeout: 60\n");
    install(&libs, "acme", "pusher", &v2, &at('2'), false).await;
    let out = trigger(&db, &forge, ports(), &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);

    // The same name installed from another source does not.
    let v3 = format!("{PUSHER}    timeout: 90\n");
    install(
        &libs,
        "acme",
        "pusher",
        &v3,
        &format!("github.com/mallory/libs//pusher.yaml@{}", "3".repeat(40)),
        false,
    )
    .await;
    assert!(refused_reason(&trigger(&db, &forge, ports(), &push()).await).contains("step `sync/git`"));

    // Untrusting refuses again.
    install(&libs, "acme", "pusher", &v2, &at('2'), false).await;
    libs.untrust_source("acme", "github.com/acme/libs//pusher.yaml")
        .await
        .unwrap();
    assert!(refused_reason(&trigger(&db, &forge, ports(), &push()).await).contains("step `sync/git`"));
}

// ---------------------------------------------------------------------------
// `read` and `write`
// ---------------------------------------------------------------------------

const WRITE_YAML: &str = r#"
on:
  push: {}
  pull_request: {}
steps:
  - { id: tag, image: alpine/git, forge_token: write }
"#;

/// A fork pull request gets `read`, even where the Step asks for `write`.
#[tokio::test]
async fn a_fork_pull_request_gets_read_even_if_write_is_asked() {
    let db = InMemoryDb::new();
    let forge = minting(&[(".scarab/ci.yaml", WRITE_YAML)]);
    let out = trigger(&db, &forge, AdmissionPorts::default(), &fork_pr()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    assert_eq!(stored_token(&db, &out.runs[0], "tag").await, Some(ForgeToken::Read));
}

/// `write` on a Forgejo or PAT connection: refused, whoever asks.
#[tokio::test]
async fn write_is_refused_on_a_long_lived_connection() {
    let db = InMemoryDb::new();
    let forge = long_lived(&[(".scarab/ci.yaml", WRITE_YAML)]);
    let out = trigger(&db, &forge, AdmissionPorts::default(), &push()).await;
    let reason = refused_reason(&out);
    assert!(
        reason.contains("`forge_token: write`") && reason.contains("long-lived token"),
        "{reason}"
    );
}

const DEPLOY_WRITE_YAML: &str = r#"
on:
  push: {}
environment: prod
steps:
  - { id: tag, image: alpine/git, forge_token: write }
"#;

const GATED_WRITE_YAML: &str = r#"
on:
  push: {}
environment: prod
steps:
  - { id: approve, gate: manual }
  - { id: tag, image: alpine/git, forge_token: write, needs: [approve] }
"#;

async fn prod(allowed_refs: &[&str], approvers: &[&str]) -> InMemoryEnvironmentGovernance {
    let g = InMemoryEnvironmentGovernance::new().with_environment(
        "acme",
        "prod",
        EnvironmentProjectEligibility::AllProjects,
    );
    g.put_environment_protection(
        "acme",
        "prod",
        &EnvironmentProtectionRules {
            approvers: approvers.iter().map(|a| a.to_string()).collect(),
            allowed_refs: allowed_refs.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        },
        "amy",
        1,
    )
    .await
    .unwrap();
    g
}

/// By default `write` needs a Run governed by an Environment with non-empty
/// effective allowed refs.
#[tokio::test]
async fn write_needs_an_environment_that_restricts_refs() {
    let db = InMemoryDb::new();
    // Ungoverned.
    let forge = minting(&[(".scarab/ci.yaml", WRITE_YAML)]);
    let reason = refused_reason(&trigger(&db, &forge, AdmissionPorts::default(), &push()).await);
    assert!(reason.contains("restricts the refs"), "{reason}");

    // Governed, but any ref may deploy.
    let forge = minting(&[(".scarab/deploy.yaml", DEPLOY_WRITE_YAML)]);
    let open = prod(&[], &[]).await;
    let reason =
        refused_reason(&trigger(&db, &forge, AdmissionPorts::governance(Some(&open)), &push()).await);
    assert!(reason.contains("restricts the refs"), "{reason}");

    // Governed, refs restricted, no approvers: admitted with `write`.
    let main_only = prod(&["refs/heads/main"], &[]).await;
    let out = trigger(&db, &forge, AdmissionPorts::governance(Some(&main_only)), &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    assert_eq!(stored_token(&db, &out.runs[0], "tag").await, Some(ForgeToken::Write));
}

/// Where the Environment has approvers, `write` needs a manual gate
/// upstream, so an approver releases the Step first.
#[tokio::test]
async fn write_needs_a_manual_gate_where_the_environment_has_approvers() {
    let db = InMemoryDb::new();
    let gov = prod(&["refs/heads/main"], &["amy"]).await;
    let forge = minting(&[(".scarab/deploy.yaml", DEPLOY_WRITE_YAML)]);
    let reason =
        refused_reason(&trigger(&db, &forge, AdmissionPorts::governance(Some(&gov)), &push()).await);
    assert!(reason.contains("gate: manual"), "{reason}");

    let forge = minting(&[(".scarab/deploy.yaml", GATED_WRITE_YAML)]);
    let out = trigger(&db, &forge, AdmissionPorts::governance(Some(&gov)), &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
}

/// Each launch mints a credential, a rerun included, and the token of the
/// first Attempt is revoked when it ends.
#[tokio::test]
async fn a_rerun_mints_again() {
    let db = Arc::new(InMemoryDb::new());
    let forge = Arc::new(minting(&[(".scarab/ci.yaml", OWN_STEP_YAML)]));
    let out = trigger(db.as_ref(), forge.as_ref(), AdmissionPorts::default(), &push()).await;
    let run = out.runs[0].clone();
    let inner = Arc::new(FakeExecutor::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    let clock = FakeClock::new(1_000);
    let sched = Scheduler::new(db.as_ref(), &clock as &dyn Clock, &exec, "sched");
    inner.script_outcome(ExecState::Succeeded);
    for _ in 0..4 {
        sched.tick(&run).await.unwrap();
    }
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    assert_eq!(forge.credential_calls(), 1);
    assert_eq!(forge.revoked().len(), 1, "revoked when the Attempt ended");

    scarab_engine::rerun_step(db.as_ref(), &clock, &run, &StepId("tag".into()), Some("amy".into()))
        .await
        .unwrap();
    inner.script_outcome(ExecState::Succeeded);
    for _ in 0..4 {
        sched.tick(&run).await.unwrap();
    }
    assert_eq!(forge.credential_calls(), 2, "the rerun's Attempt minted its own");
    let revoked = forge.revoked();
    assert_eq!(revoked.len(), 2);
    assert_ne!(revoked[0], revoked[1]);
}

// ---------------------------------------------------------------------------
// `event` values
// ---------------------------------------------------------------------------

const READS_BRANCH: &str = r#"
on:
  push: {}
steps:
  - { id: say, image: busybox, env: [[BRANCH, "${{ event.branch }}"]] }
"#;

const READS_SHA_ONLY: &str = r#"
on:
  push: {}
steps:
  - { id: say, image: busybox, env: [[SHA, "${{ event.sha }}"]] }
"#;

/// A branch name with `;` refuses the Pipeline that reads `event.branch`
/// and no other; the bad value is not stored.
#[tokio::test]
async fn a_bad_branch_refuses_only_the_pipelines_that_read_it() {
    let db = InMemoryDb::new();
    let forge = long_lived(&[
        (".scarab/a.yaml", READS_BRANCH),
        (".scarab/b.yaml", READS_SHA_ONLY),
    ])
    .with_commit("refs/heads/x;id", SHA);
    let out = trigger(&db, &forge, AdmissionPorts::default(), &push_to("refs/heads/x;id")).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    assert_eq!(out.refused.len(), 1);
    assert_eq!(out.refused[0].pipeline, ".scarab/a.yaml");
    assert!(
        out.refused[0].reason.contains("`${{ event.branch }}` does not accept")
            && out.refused[0].reason.contains("contains `;`"),
        "{}",
        out.refused[0].reason
    );
    let facts = db.run_event_facts(&out.runs[0]).await.unwrap();
    assert_eq!(facts["sha"], SHA);
    assert!(facts.get("branch").is_none() && facts.get("ref").is_none());
}

/// A trigger with no commit refuses a Pipeline that reads `event.sha`: the
/// clone library on a tag, as the clone kind refused it.
#[tokio::test]
async fn reading_event_sha_on_a_trigger_without_one_refuses_the_run() {
    let db = InMemoryDb::new();
    let libs = with_first_party_clone().await;
    let yaml = "on: { tag: {} }\nsteps: [{ id: checkout, invoke: lib:thulasi-ram/clone }]\n";
    let forge = long_lived(&[(".scarab/ci.yaml", yaml)]).with_commit("refs/tags/v1", SHA);
    let tag = Event::Tag {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        tag: "v1".into(),
    };
    let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &tag).await;
    let reason = refused_reason(&out);
    assert!(
        reason.contains("`${{ event.sha }}`, which a `tag` trigger does not have"),
        "{reason}"
    );
}

/// A `clone:` Step fails the Pipeline check naming the library.
#[tokio::test]
async fn a_clone_step_fails_naming_the_library() {
    let db = InMemoryDb::new();
    let yaml = "on: { push: {} }\nsteps: [{ id: checkout, clone: {} }]\n";
    let forge = long_lived(&[(".scarab/ci.yaml", yaml)]);
    let err = trigger_runs_from_event(&forge, &db, &FakeClock::new(1), AdmissionPorts::default(), &push())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invoke: lib:thulasi-ram/clone"), "{err}");
}
