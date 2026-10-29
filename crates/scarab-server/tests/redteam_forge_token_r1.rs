//! Red team round 1 on ADR-0076 step 3 (HEAD 03dc2cb2): `forge_token`, the
//! first-party clone library and its trust, fork pull requests and `write`,
//! revocation, and the removed `clone` kind. Each test asserts the SECURE
//! behaviour: a failing test is a break.
//!
//! The promise under attack: a repository's own code never reads a
//! long-lived forge token; a fork pull request never gets `write`.
//!
//! Real admission, scheduler and launch decorator; in-memory Db, forge,
//! executor and stores. No network.

use std::sync::Arc;

use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{Clock, Db, RunId, RunStatus, Scheduler, StepId, StepStatus, Timestamp};
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

fn pr(fork: bool) -> Event {
    Event::PullRequest {
        actor: "mallory".into(),
        actor_id: String::new(),
        repo: repo(),
        number: 7,
        head: SHA.into(),
        title: "fix".into(),
        base: "main".into(),
        fork,
    }
}

fn long_lived(files: &[(&str, &str)]) -> FakeForge {
    files
        .iter()
        .fold(FakeForge::new(), |f, (path, yaml)| f.with_file(*path, *yaml))
        .with_commit("refs/heads/main", SHA)
        .with_commit("refs/heads/feature", SHA)
}

fn minting(files: &[(&str, &str)]) -> FakeForge {
    long_lived(files).with_minted_tokens()
}

async fn install_in(
    libs: &InMemoryLibraries,
    org: &str,
    publisher: &str,
    name: &str,
    content: &str,
    source: &str,
    first_party: bool,
) {
    libs.put_version(&LibraryVersion {
        org: org.into(),
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
        .installation(org, publisher, name)
        .await
        .unwrap()
        .map(|i| Replaces::Version(i.content_hash))
        .unwrap_or(Replaces::Nothing);
    libs.install(
        &NewInstallation {
            org: org.into(),
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

async fn install(
    libs: &InMemoryLibraries,
    publisher: &str,
    name: &str,
    content: &str,
    source: &str,
    first_party: bool,
) {
    install_in(libs, "acme", publisher, name, content, source, first_party).await
}

async fn trust(libs: &InMemoryLibraries, source: &str) {
    libs.trust_source(&TrustedSource {
        org: "acme".into(),
        source: source.into(),
        trusted_by: "amy".into(),
        trusted_at: 2,
        step_up_ref: Some("su-1".into()),
    })
    .await
    .unwrap();
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

async fn stored_token(db: &InMemoryDb, run: &RunId, step: &str) -> Option<ForgeToken> {
    db.step_spec(run, &StepId(step.into()))
        .await
        .unwrap()
        .unwrap()
        .forge_token
        .map(|f| f.access)
}

/// Drive `run` to the end with `exec` over `forge`, every launch succeeding.
async fn drive(db: &InMemoryDb, inner: &FakeExecutor, exec: &ForgeCredentialExecutor, run: &RunId) {
    let clock = FakeClock::new(1_000);
    let sched = Scheduler::new(db, &clock as &dyn Clock, exec, "sched");
    for _ in 0..8 {
        inner.script_outcome(ExecState::Succeeded);
        sched.tick(run).await.unwrap();
    }
}

fn handle(run: &RunId, step: &str, attempt: &str) -> ExecHandle {
    ExecHandle(format!("fake://{}/{step}/{attempt}", run.0))
}

// ---------------------------------------------------------------------------
// 1. A token reaching the repository's own code
// ---------------------------------------------------------------------------

const OWN_STEP_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: tag, image: alpine/git, command: [sh, -c, "cat /scarab/secrets/forge-token"], forge_token: read }
"#;

/// ATTACK: a repository's own `forge_token` Step is admitted while the
/// repository's connection is a GitHub App (it mints a short-lived token).
/// The admin then moves the repository to a Forgejo / PAT connection (one
/// long-lived token). A rerun of the old Run re-launches the stored spec:
/// admission is not repeated, and the launch decorator mints from whatever
/// the connection is now. The repository's own `cat` must never receive
/// the long-lived token.
#[tokio::test]
#[ignore = "red: s3 r1"]
async fn rt_rerun_after_the_connection_turns_long_lived_must_not_hand_repo_code_the_token() {
    let db = InMemoryDb::new();
    let app = minting(&[(".scarab/ci.yaml", OWN_STEP_YAML)]);
    let out = trigger(&db, &app, AdmissionPorts::default(), &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    assert_eq!(stored_token(&db, &run, "tag").await, Some(ForgeToken::Read));

    // First Attempt under the App: fine, a short-lived repository token.
    let inner = Arc::new(FakeExecutor::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), Arc::new(app));
    drive(&db, &inner, &exec, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));

    // The connection is now long-lived (Forgejo / GitHub PAT).
    let pat = Arc::new(long_lived(&[(".scarab/ci.yaml", OWN_STEP_YAML)]));
    let inner2 = Arc::new(FakeExecutor::new());
    let exec2 = ForgeCredentialExecutor::new(inner2.clone(), pat.clone());
    let clock = FakeClock::new(2_000);
    scarab_engine::rerun_step(&db, &clock, &run, &StepId("tag".into()), Some("amy".into()))
        .await
        .unwrap();
    drive(&db, &inner2, &exec2, &run).await;
    let attempts = db.attempts_of_step(&run, &StepId("tag".into())).await.unwrap();
    let last = attempts.last().unwrap().id.0.clone();
    let launched = inner2.launched_spec(&handle(&run, "tag", &last));
    let got = launched
        .as_ref()
        .and_then(|s| s.forge_token.as_ref())
        .and_then(|f| f.credential.clone());
    assert!(
        got.is_none(),
        "the repository's own Step `tag` was launched holding the long-lived token {:?} \
         (admitted under a minting connection, re-launched after it became long-lived)",
        got.map(|c| c.token)
    );
}

const FILE_LIB: &str = r#"kind: library
steps:
  - id: git
    image: "alpine/git@sha256:0000000000000000000000000000000000000000000000000000000000000000"
    forge_token: read
"#;

/// ATTACK: a `file:` library is the repository's own code: its
/// `forge_token` Step on a long-lived connection must be refused.
#[tokio::test]
async fn rt_a_file_library_forge_token_step_is_refused_on_a_long_lived_connection() {
    let db = InMemoryDb::new();
    let libs = InMemoryLibraries::new();
    install(&libs, "thulasi-ram", "clone", CLONE, CLONE_SOURCE, true).await;
    let yaml = "on: { push: {} }\nsteps: [{ id: g, invoke: file:.scarab/lib/clone.yaml }]\n";
    // The file is even byte-identical to the first-party clone library.
    for content in [FILE_LIB, CLONE] {
        let forge = long_lived(&[(".scarab/ci.yaml", yaml), (".scarab/lib/clone.yaml", content)]);
        let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &push()).await;
        assert!(out.runs.is_empty(), "a `file:` library Step got a long-lived token: {:?}", out.runs);
        assert!(
            out.refused.iter().any(|r| r.reason.contains("never receive it")),
            "refused for the token, not something else: {:?}",
            out.refused
        );
        assert_eq!(forge.credential_calls(), 0);
    }
}

const CHECKOUT_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: checkout, invoke: lib:thulasi-ram/clone }
"#;

/// ATTACK: the org installs a copy of clone.yaml under the same publisher
/// and name from an untrusted source, while another org holds the
/// first-party version of that very content. Trust must not leak across
/// orgs.
#[tokio::test]
async fn rt_an_untrusted_copy_is_refused_even_when_another_org_holds_it_first_party() {
    let db = InMemoryDb::new();
    let libs = InMemoryLibraries::new();
    install_in(&libs, "other", "thulasi-ram", "clone", CLONE, CLONE_SOURCE, true).await;
    install(
        &libs,
        "thulasi-ram",
        "clone",
        CLONE,
        "github.com/mallory/scarab-ci//libs/clone.yaml@2222222222222222222222222222222222222222",
        false,
    )
    .await;
    let forge = long_lived(&[(".scarab/ci.yaml", CHECKOUT_YAML)]);
    let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &push()).await;
    assert!(out.runs.is_empty(), "admitted: {:?}", out.runs);
    assert!(out.refused.iter().any(|r| r.reason.contains("step `checkout/git`")), "{:?}", out.refused);
}

/// A library from a source the org trusts, whose image comes from an input
/// with a permissive `validate:` (ADR-0076 §2 accepts any `validate:`).
const IMAGE_FROM_INPUT: &str = r#"kind: library
interface:
  inputs:
    - { name: image, type: string, validate: 'value.size() > 0' }
steps:
  - id: git
    image: "${{ inputs.image }}"
    forge_token: read
"#;

/// ATTACK: §2 says an input reaching a `forge_token` Step must be a checked
/// value, so "its image and command come from the library, not from the
/// repository". A permissive `validate:` satisfies the letter of the rule;
/// the caller then picks the image of the Step that holds the long-lived
/// token. The repository's own image must never hold it.
#[tokio::test]
async fn rt_a_caller_must_not_choose_the_image_of_a_trusted_token_step() {
    let db = InMemoryDb::new();
    let libs = InMemoryLibraries::new();
    let source = format!("github.com/acme/libs//git.yaml@{}", "1".repeat(40));
    install(&libs, "acme", "git", IMAGE_FROM_INPUT, &source, false).await;
    trust(&libs, "github.com/acme/libs//git.yaml").await;
    let yaml = r#"
on:
  push: {}
steps:
  - { id: g, invoke: lib:acme/git, with: { image: "ghcr.io/mallory/exfil:1" } }
"#;
    let forge = Arc::new(long_lived(&[(".scarab/ci.yaml", yaml)]));
    // A compile error refuses the Pipeline outright.
    let out = match trigger_runs_from_event(
        forge.as_ref(),
        &db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(&libs),
        &push(),
    )
    .await
    {
        Ok(out) => out,
        Err(e) => {
            assert!(e.to_string().contains("`image`"), "refused for its image: {e}");
            return;
        }
    };
    if out.runs.len() == 1 {
        let run = out.runs[0].clone();
        let inner = Arc::new(FakeExecutor::new());
        let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
        drive(&db, &inner, &exec, &run).await;
        let launched = inner.launched_spec(&handle(&run, "g/git", "a1"));
        let (image, cred) = launched
            .map(|s| (s.image.clone(), s.forge_token.and_then(|f| f.credential)))
            .unwrap_or_default();
        panic!(
            "admitted: the caller's image `{image}` launched holding the long-lived token {:?}",
            cred.map(|c| c.token)
        );
    }
}

const CLONE_WITH_ENV: &str = r#"
on:
  push: {}
steps:
  - id: checkout
    invoke: lib:thulasi-ram/clone
    placement_profiles: [evil]
    timeout: 60
"#;

/// ATTACK: what an invoke Step carries onto the trusted clone Step: no
/// placement profile (an arbitrary Pod patch), no caller env, no inputs
/// workspace, no services or overlay.
#[tokio::test]
async fn rt_the_invoke_step_carries_nothing_that_shapes_the_token_pod() {
    let db = InMemoryDb::new();
    let libs = InMemoryLibraries::new();
    install(&libs, "thulasi-ram", "clone", CLONE, CLONE_SOURCE, true).await;
    let forge = long_lived(&[(".scarab/ci.yaml", CLONE_WITH_ENV)]);
    let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    let spec = db
        .step_spec(&run, &StepId("checkout/git".into()))
        .await
        .unwrap()
        .unwrap();
    assert!(spec.placement_profiles.is_empty(), "{:?}", spec.placement_profiles);
    assert!(spec.k8s_overlay.is_none());
    assert!(spec.services.is_empty() && spec.uses.is_empty() && spec.secrets.is_empty());
    assert!(
        spec.env.iter().all(|(k, _)| k.starts_with("SCARAB_CLONE_")),
        "{:?}",
        spec.env
    );
}

const PREP_THEN_CLONE: &str = r#"
on:
  push: {}
steps:
  - { id: prep, image: busybox, results: [depth] }
  - id: checkout
    invoke: lib:thulasi-ram/clone
    needs: [prep]
    with: { depth: "${{ outputs.prep.depth }}" }
"#;

/// ATTACK: a repository Step's result flows into the trusted clone Step's
/// `depth` through a `with:` template. A value that fails the input's
/// `validate:` must fail the Step before any credential is minted.
#[tokio::test]
async fn rt_a_repo_result_failing_validate_fails_the_token_step_before_any_mint() {
    let db = InMemoryDb::new();
    let libs = InMemoryLibraries::new();
    install(&libs, "thulasi-ram", "clone", CLONE, CLONE_SOURCE, true).await;
    let forge = Arc::new(long_lived(&[(".scarab/ci.yaml", PREP_THEN_CLONE)]));
    let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    // Empty explicit inputs: nothing `prep` wrote is in /workspace.
    assert_eq!(
        db.step_inputs(&run, &StepId("checkout/git".into())).await.unwrap(),
        Some(vec![])
    );
    let inner = Arc::new(FakeExecutor::new());
    inner.set_results(
        "prep",
        std::collections::BTreeMap::from([("depth".to_string(), serde_json::json!("1;id"))]),
    );
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    drive(&db, &inner, &exec, &run).await;
    let git = StepId("checkout/git".into());
    let status = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == git)
        .map(|s| s.status);
    assert_ne!(status, Some(StepStatus::Succeeded));
    assert!(
        inner.launched_spec(&handle(&run, "checkout/git", "a1")).is_none(),
        "the token Step launched with a depth that fails its validate"
    );
    assert_eq!(forge.credential_calls(), 0, "no credential minted");
}

// ---------------------------------------------------------------------------
// 2. Trust
// ---------------------------------------------------------------------------

const DEPLOY_LIB: &str = r#"kind: library
steps:
  - id: git
    image: "alpine/git@sha256:0000000000000000000000000000000000000000000000000000000000000000"
    command: [sh, -c, "push-the-release"]
    forge_token: read
"#;

/// ATTACK: first-party trust is `first_party` (the content hash is in a
/// release manifest) plus a source path ending in `clone.yaml`. A
/// first-party library that is not the clone library, fetched from a mirror
/// path that names it `clone.yaml`, must not inherit the clone library's
/// default trust with a long-lived token.
#[tokio::test]
async fn rt_first_party_content_that_is_not_clone_is_not_trusted_by_its_file_name() {
    let db = InMemoryDb::new();
    let libs = InMemoryLibraries::new();
    install(
        &libs,
        "thulasi-ram",
        "deploy",
        DEPLOY_LIB,
        "git.example.com/mirror/scarab-ci//libs/deploy/clone.yaml@1111111111111111111111111111111111111111",
        true,
    )
    .await;
    let yaml = "on: { push: {} }\nsteps: [{ id: d, invoke: lib:thulasi-ram/deploy }]\n";
    let forge = long_lived(&[(".scarab/ci.yaml", yaml)]);
    let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &push()).await;
    assert!(
        out.runs.is_empty(),
        "a first-party library other than clone was trusted with the long-lived token because \
         its source path ends in clone.yaml"
    );
}

const PUSHER: &str = r#"kind: library
steps:
  - id: git
    image: "alpine/git@sha256:0000000000000000000000000000000000000000000000000000000000000000"
    forge_token: read
"#;

/// ATTACK: trust is an exact match on the source without its commit. A
/// look-alike source (another host, case, trailing slash, a doubled `/`, a
/// fork) must not match.
#[tokio::test]
async fn rt_look_alike_sources_do_not_match_a_trusted_source() {
    let yaml = "on: { push: {} }\nsteps: [{ id: s, invoke: lib:acme/pusher }]\n";
    for source in [
        "gitlab.com/acme/libs//pusher.yaml",
        "github.com/ACME/libs//pusher.yaml",
        "github.com/acme/libs//pusher.yaml/",
        "github.com/acme/libs///pusher.yaml",
        "github.com/acme-fork/libs//pusher.yaml",
        "github.com/acme/libs//sub/../pusher.yaml",
        "github.com/acme/libs//pusher.yaml@deadbeef/x",
    ] {
        let db = InMemoryDb::new();
        let libs = InMemoryLibraries::new();
        install(
            &libs,
            "acme",
            "pusher",
            PUSHER,
            &format!("{source}@{}", "1".repeat(40)),
            false,
        )
        .await;
        trust(&libs, "github.com/acme/libs//pusher.yaml").await;
        let forge = long_lived(&[(".scarab/ci.yaml", yaml)]);
        let out = trigger(&db, &forge, AdmissionPorts::default().with_libraries(&libs), &push()).await;
        assert!(out.runs.is_empty(), "`{source}` matched the trusted source");
        assert!(
            out.refused.iter().any(|r| r.reason.contains("step `s/git`")),
            "`{source}`: refused for the token, not something else: {:?}",
            out.refused
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Fork pull requests and `write`
// ---------------------------------------------------------------------------

const WRITE_YAML: &str = r#"
on:
  push: {}
  pull_request: {}
steps:
  - { id: tag, image: alpine/git, forge_token: write }
"#;

/// ATTACK: a fork PR asks for `write`. Its Attempt must never mint a write
/// token, launch after launch.
#[tokio::test]
async fn rt_a_fork_pull_request_never_mints_a_write_token() {
    let db = InMemoryDb::new();
    let forge = Arc::new(minting(&[(".scarab/ci.yaml", WRITE_YAML)]));
    let out = trigger(&db, &forge, AdmissionPorts::default(), &pr(true)).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    let inner = Arc::new(FakeExecutor::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    drive(&db, &inner, &exec, &run).await;
    let launched = inner
        .launched_spec(&handle(&run, "tag", "a1"))
        .expect("tag launched");
    let f = launched.forge_token.unwrap();
    assert_eq!(f.access, ForgeToken::Read);
    let token = f.credential.unwrap().token;
    assert!(!token.starts_with("fake-write-token"), "{token}");
}

/// ATTACK: a same-repository pull request (not a fork) asks for `write`
/// in an ungoverned Run.
#[tokio::test]
async fn rt_a_same_repo_pull_request_asking_write_ungoverned_is_refused() {
    let db = InMemoryDb::new();
    let forge = minting(&[(".scarab/ci.yaml", WRITE_YAML)]);
    let out = trigger(&db, &forge, AdmissionPorts::default(), &pr(false)).await;
    assert!(out.runs.is_empty(), "admitted: {:?}", out.runs);
}

const DEPLOY_WRITE_YAML: &str = r#"
on:
  push: {}
  pull_request: {}
environment: prod
steps:
  - { id: tag, image: alpine/git, forge_token: write }
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

/// ATTACK: an Environment restricts refs, and the push is from a ref it
/// does not allow; a pull request (same repo) to the governed Pipeline.
#[tokio::test]
async fn rt_write_from_a_ref_the_environment_does_not_allow_is_refused() {
    let db = InMemoryDb::new();
    let gov = prod(&["refs/heads/main"], &[]).await;
    let forge = minting(&[(".scarab/deploy.yaml", DEPLOY_WRITE_YAML)]);
    for event in [push_to("refs/heads/feature"), pr(false), pr(true)] {
        let out = trigger(&db, &forge, AdmissionPorts::governance(Some(&gov)), &event).await;
        for run in &out.runs {
            assert_ne!(
                stored_token(&db, run, "tag").await,
                Some(ForgeToken::Write),
                "{event:?} got write"
            );
        }
    }
}

const GATED_SKIPPABLE: &str = r#"
on:
  push: {}
environment: prod
steps:
  - { id: approve, gate: manual, when: 'event.branch == "nope"' }
  - { id: tag, image: alpine/git, forge_token: write, needs: [approve] }
"#;

/// ATTACK: the manual gate the approvers rule asks for is skipped by its
/// own `when:`. The write Step must not then run without approval.
#[tokio::test]
async fn rt_a_skipped_gate_does_not_release_a_write_step() {
    let db = InMemoryDb::new();
    let gov = prod(&["refs/heads/main"], &["amy"]).await;
    let forge = Arc::new(minting(&[(".scarab/deploy.yaml", GATED_SKIPPABLE)]));
    let res = trigger_runs_from_event(
        forge.as_ref(),
        &db,
        &FakeClock::new(1_000),
        AdmissionPorts::governance(Some(&gov)),
        &push(),
    )
    .await;
    let out = res.expect("compiles");
    assert_eq!(out.runs.len(), 1, "control: the gated Run is admitted: {:?}", out.refused);
    for run in &out.runs {
        let inner = Arc::new(FakeExecutor::new());
        let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
        drive(&db, &inner, &exec, run).await;
        assert!(
            inner.launched_spec(&handle(run, "tag", "a1")).is_none(),
            "the write Step launched without its gate being approved"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Revocation
// ---------------------------------------------------------------------------

/// ATTACK: a re-drive of the same Attempt mints a second token. The first
/// is no longer the one mounted; it should end before (or when) the new one
/// is handed over, not stay live until the Attempt ends.
#[tokio::test]
#[ignore = "red: s3 r1"]
async fn rt_a_re_drive_revokes_the_token_it_replaces() {
    let db = InMemoryDb::new();
    let forge = Arc::new(minting(&[(".scarab/ci.yaml", OWN_STEP_YAML)]));
    let out = trigger(&db, &forge, AdmissionPorts::default(), &push()).await;
    let run = out.runs[0].clone();
    let inner = Arc::new(FakeExecutor::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    let step = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == "tag")
        .unwrap();
    let mut step = step;
    step.attempts.push(scarab_engine::Attempt {
        id: scarab_engine::AttemptId("a1".into()),
        started_at: Timestamp(0),
        failure: None,
        failure_detail: None,
        outcome: scarab_engine::AttemptOutcome::Running,
    });
    let spec = db.step_spec(&run, &step.step).await.unwrap().unwrap();
    use scarab_engine::Executor;
    exec.launch(&step, &spec).await.unwrap();
    exec.launch(&step, &spec).await.unwrap(); // the re-drive
    assert_eq!(
        forge.revoked().len(),
        1,
        "the first launch's token is still live after the re-drive replaced it"
    );
}

// ---------------------------------------------------------------------------
// 6. The removed `clone` kind
// ---------------------------------------------------------------------------

/// ATTACK: a Run stored before the `clone` kind was removed holds a clone
/// spec. Launching (a rerun, a re-drive) must fail the Step with a reason,
/// never panic and never mint a credential.
#[tokio::test]
async fn rt_a_stored_clone_spec_fails_clearly_and_mints_nothing() {
    let db = InMemoryDb::new();
    let run = RunId("old-run".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    let old = serde_json::json!({
        "image": "",
        "command": [],
        "env": [],
        "clone": {
            "owner": "acme", "name": "web", "sha": SHA, "depth_full": false,
            "submodules": false, "lfs": false, "read_only": false,
            "url": "", "credential": null
        }
    });
    let spec: scarab_engine::StepSpec = serde_json::from_value(old).expect("old spec decodes");
    assert!(spec.forge_token.is_none());
    db.create_step_run(&run, &StepId("checkout".into()), Some(&spec), &[], Timestamp(0))
        .await
        .unwrap();
    let forge = Arc::new(FakeForge::new().with_minted_tokens());
    let inner = Arc::new(FakeExecutor::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    drive(&db, &inner, &exec, &run).await;
    assert!(inner.launched_spec(&handle(&run, "checkout", "a1")).is_none());
    assert_eq!(forge.credential_calls(), 0);
    let attempts = db.attempts_of_step(&run, &StepId("checkout".into())).await.unwrap();
    let detail = attempts
        .first()
        .and_then(|a| a.failure_detail.clone())
        .unwrap_or_default();
    assert!(detail.contains("clone"), "failure names the removed kind: {detail:?}");
}
