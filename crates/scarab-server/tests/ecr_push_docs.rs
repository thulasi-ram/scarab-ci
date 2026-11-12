//! git-bug ace8135: the "Pushing images to Amazon ECR" docs page
//! (ui/scarab-docs-ui/src/content/docs/usage/pushing-to-ecr.mdx) works as
//! written. The page is read with `include_str!`, so each test checks the
//! page's own blocks, not a copy.
//!
//! - Every `yaml` and `json` block parses.
//! - The Pipeline compiles with the build and clone libraries installed and
//!   routes `ecr-push` to the build library's push Step only; a push to main
//!   creates a Run whose stored push Step names it and whose build Step does
//!   not.
//! - The org Policy checks clean in the editor's checker, as does the
//!   Environment variant the page gives in prose. It admits the page's
//!   Pipeline on main and refuses it on another branch, for a fork pull
//!   request, and with a Pipeline-file Step that names `ecr-push`.
//!
//! The profile's merge onto a push Step Pod is tested in scarab-executor-k8s
//! (`the_ecr_push_profile_from_the_docs_*`).
//!
//! Real compiler and admission path; in-memory Db, forge and stores.

use std::collections::BTreeMap;

use scarab_engine::{Db, RunId, StepId};
use scarab_forge::{Event, RepoRef};
use scarab_pipeline::{InstalledLibrary, Libraries};
use scarab_policy::check::check;
use scarab_policy::ir::{check_policy, Level};
use scarab_policy::library::{
    content_hash, trust_key, LibraryStore, LibraryVersion, NewInstallation, Replaces, TrustGrant,
    TrustedSource,
};
use scarab_policy::policy::Policy;
use scarab_policy::schema::Type;
use scarab_policy::store::{NewPolicy, PolicyScope, PolicyStore};
use scarab_server::{trigger_runs_from_event, AdmissionPorts, TriggerOutcome};
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryLibraries, InMemoryPolicies};

const PAGE: &str =
    include_str!("../../../ui/scarab-docs-ui/src/content/docs/usage/pushing-to-ecr.mdx");
const BUILD: &str = include_str!("../../../libs/build.yaml");
const CLONE: &str = include_str!("../../../libs/clone.yaml");
const BUILD_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/build.yaml@1111111111111111111111111111111111111111";
const CLONE_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@1111111111111111111111111111111111111111";
const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

const BRANCH_RULE: &str =
    "policy `ecr-push`: Only Runs on main, not from a fork, may name the ecr-push placement profile.";
const LIBRARY_RULE: &str =
    "policy `ecr-push`: Only the build library's steps may name the ecr-push placement profile.";

/// The page's fenced code blocks of `lang`, with the fence's indent removed
/// from each line (a block inside a list item is indented).
fn blocks(lang: &str) -> Vec<String> {
    let open = format!("```{lang}");
    let mut out = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for line in PAGE.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        match &mut current {
            None if trimmed == open => current = Some((indent, String::new())),
            None => {}
            Some(_) if trimmed == "```" => out.push(current.take().unwrap().1),
            Some((fence_indent, body)) => {
                body.push_str(line.get(*fence_indent..).unwrap_or(""));
                body.push('\n');
            }
        }
    }
    assert!(current.is_none(), "a `{lang}` block is not closed");
    out
}

fn yaml_blocks() -> Vec<serde_yaml::Value> {
    blocks("yaml")
        .iter()
        .map(|b| serde_yaml::from_str(b).unwrap_or_else(|e| panic!("{e}:\n{b}")))
        .collect()
}

/// The one block with a top-level `key`, as YAML text.
fn the_block_with(key: &str) -> String {
    let found: Vec<String> = blocks("yaml")
        .into_iter()
        .filter(|b| {
            serde_yaml::from_str::<serde_yaml::Value>(b)
                .unwrap()
                .get(key)
                .is_some()
        })
        .collect();
    assert_eq!(found.len(), 1, "blocks with `{key}`: {found:?}");
    found.into_iter().next().unwrap()
}

fn page_pipeline() -> String {
    the_block_with("steps")
}

fn page_policy() -> Policy {
    let policies: Vec<String> = blocks("yaml")
        .into_iter()
        .filter(|b| {
            serde_yaml::from_str::<serde_yaml::Value>(b).unwrap()["kind"].as_str() == Some("policy")
        })
        .collect();
    assert_eq!(policies.len(), 1, "{policies:?}");
    serde_yaml::from_str(&policies[0]).expect("the page's Policy parses")
}

/// The page's Pipeline with its `on:` replaced, as an author who wants it to
/// run elsewhere would change it, and `extra` Steps appended.
fn pipeline_with(on: &str, extra: &str) -> String {
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&page_pipeline()).unwrap();
    doc["on"] = serde_yaml::from_str(on).unwrap();
    if !extra.is_empty() {
        let steps = doc["steps"].as_sequence_mut().unwrap();
        let more: Vec<serde_yaml::Value> = serde_yaml::from_str(extra).unwrap();
        steps.extend(more);
    }
    serde_yaml::to_string(&doc).unwrap()
}

#[test]
fn every_yaml_and_json_block_on_the_page_parses() {
    let yaml = yaml_blocks();
    // The ServiceAccount, the chart values, the Pipeline and the Policy.
    assert_eq!(yaml.len(), 4, "{yaml:?}");
    let json = blocks("json");
    // The push role's policy and its hardened variant.
    assert_eq!(json.len(), 2);
    for b in &json {
        let v: serde_json::Value = serde_json::from_str(b).unwrap_or_else(|e| panic!("{e}:\n{b}"));
        assert_eq!(v["Version"], "2012-10-17");
        let text = b.as_str();
        assert!(!text.contains("ecr:*"), "no wildcard ECR action: {text}");
        assert!(!text.contains("CreateRepository"), "{text}");
    }
    let sa = yaml
        .iter()
        .find(|v| v["kind"].as_str() == Some("ServiceAccount"))
        .expect("the ServiceAccount");
    assert_eq!(sa["metadata"]["name"].as_str(), Some("scarab-ecr-push"));
    assert_eq!(sa["automountServiceAccountToken"].as_bool(), Some(false));
}

#[test]
fn the_page_pipeline_routes_ecr_push_only_to_the_build_librarys_push_step() {
    let libraries = Libraries {
        installed: BTreeMap::from([
            (
                "thulasi-ram/build".to_string(),
                InstalledLibrary {
                    content: BUILD.into(),
                    source: BUILD_SOURCE.into(),
                },
            ),
            (
                "thulasi-ram/clone".to_string(),
                InstalledLibrary {
                    content: CLONE.into(),
                    source: CLONE_SOURCE.into(),
                },
            ),
        ]),
        ..Default::default()
    };
    let ir = scarab_pipeline::compile_with_libraries(&page_pipeline(), &libraries)
        .expect("the page's Pipeline compiles");
    let profiles = |id: &str| -> Vec<String> {
        ir.steps
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no step `{id}`"))
            .placement_profiles
            .compiled()
            .unwrap()
            .to_vec()
    };
    assert_eq!(profiles("image/push"), vec!["ecr-push".to_string()]);
    assert_eq!(profiles("image/image"), vec!["build-cache".to_string()]);
    let named: Vec<&str> = ir
        .steps
        .iter()
        .filter(|s| {
            s.placement_profiles
                .compiled()
                .unwrap()
                .iter()
                .any(|p| p == "ecr-push")
        })
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(named, vec!["image/push"]);
    let push = ir.steps.iter().find(|s| s.id == "image/push").unwrap();
    assert!(push.registry_auth, "the push step asks for registry_auth");
}

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
        message: "release".into(),
    }
}

fn fork_pr() -> Event {
    Event::PullRequest {
        actor: "mallory".into(),
        actor_id: String::new(),
        repo: repo(),
        number: 7,
        head: SHA.into(),
        title: "bump".into(),
        base: "main".into(),
        fork: true,
    }
}

async fn install(libs: &InMemoryLibraries, name: &str, content: &str, source: &str) {
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: source.into(),
        publisher: "thulasi-ram".into(),
        name: name.into(),
        fetched_by: "amy".into(),
        fetched_at: 1,
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: "acme".into(),
            publisher: "thulasi-ram".into(),
            name: name.into(),
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

async fn trust(libs: &InMemoryLibraries, source: &str, grant: TrustGrant) {
    libs.trust_source(&TrustedSource {
        org: "acme".into(),
        source: trust_key(source).into(),
        grant,
        trusted_by: "amy".into(),
        trusted_at: 2,
        step_up_ref: Some("su-1".into()),
    })
    .await
    .unwrap();
}

/// The clone library trusted with forge credentials, and the build library
/// trusted as the page says: for the build sandbox and with the registry
/// credential, not with forge credentials.
async fn libraries() -> InMemoryLibraries {
    let libs = InMemoryLibraries::new();
    install(&libs, "clone", CLONE, CLONE_SOURCE).await;
    trust(&libs, CLONE_SOURCE, TrustGrant::ForgeCredentials).await;
    install(&libs, "build", BUILD, BUILD_SOURCE).await;
    trust(&libs, BUILD_SOURCE, TrustGrant::BuildSandbox).await;
    trust(&libs, BUILD_SOURCE, TrustGrant::RegistryCredential).await;
    libs
}

async fn policies() -> InMemoryPolicies {
    let policies = InMemoryPolicies::new();
    policies
        .create(&NewPolicy {
            id: "pol-ecr".into(),
            scope: PolicyScope::Org { org: "acme".into() },
            body: page_policy(),
            author: "admin".into(),
            reason: "test".into(),
            at: 1,
        })
        .await
        .expect("saved");
    policies
}

async fn trigger(db: &InMemoryDb, ci: &str, with_policy: bool, event: &Event) -> TriggerOutcome {
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", ci)
        .with_commit("refs/heads/main", SHA)
        .with_commit("refs/heads/feature", SHA);
    let libs = libraries().await;
    let policies = policies().await;
    let mut ports = AdmissionPorts::default().with_libraries(&libs);
    if with_policy {
        ports = ports.with_policies(&policies);
    }
    let out = trigger_runs_from_event(&forge, db as &dyn Db, &FakeClock::new(1_000), ports, event)
        .await
        .unwrap();
    assert_eq!(
        db.list_runs(100).await.unwrap().len(),
        out.runs.len(),
        "a refused Run leaves no row"
    );
    out
}

async fn stored_profiles(db: &InMemoryDb, run: &RunId, id: &str) -> Vec<String> {
    db.step_spec(run, &StepId(id.into()))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no stored step `{id}`"))
        .placement_profiles
}

fn refused_messages(out: &TriggerOutcome) -> Vec<String> {
    assert!(out.runs.is_empty(), "expected no Run: {:?}", out.runs);
    assert_eq!(out.refused.len(), 1, "{:?}", out.refused);
    out.refused[0].messages.clone()
}

/// A push to main creates a Run from the page's Pipeline, with and without
/// the page's Policy, and the stored push Step names `ecr-push` while the
/// build Step does not.
#[tokio::test]
async fn a_push_to_main_runs_the_page_pipeline_with_ecr_push_on_the_push_step_only() {
    for with_policy in [false, true] {
        let db = InMemoryDb::new();
        let out = trigger(
            &db,
            &page_pipeline(),
            with_policy,
            &push_to("refs/heads/main"),
        )
        .await;
        assert!(out.refused.is_empty(), "{with_policy}: {:?}", out.refused);
        assert_eq!(out.runs.len(), 1);
        let run = &out.runs[0];
        assert_eq!(
            stored_profiles(&db, run, "image/push").await,
            vec!["ecr-push".to_string()]
        );
        assert_eq!(
            stored_profiles(&db, run, "image/image").await,
            vec!["build-cache".to_string()]
        );
        let push = db
            .step_spec(run, &StepId("image/push".into()))
            .await
            .unwrap()
            .unwrap();
        let auth = push.registry_auth.expect("registry_auth");
        assert_eq!(
            auth.library,
            Some(content_hash(BUILD)),
            "trusted with the registry credential, so the profile may apply at launch"
        );
        assert_eq!(
            auth.forge_library, None,
            "not trusted with forge credentials, so no GHCR credential collides"
        );
    }
}

#[test]
fn the_page_policy_and_its_environment_variant_check_clean() {
    let policy = page_policy();
    let checked = check_policy(&policy, Level::Org);
    assert!(checked.errors.is_empty(), "{:?}", checked.errors);
    assert_eq!(checked.rules.len(), 2);
    assert_eq!(checked.warnings().count(), 0, "{:?}", checked.rules);

    let ref_check = "!trigger.fork && has(trigger.ref) && trigger.ref == 'refs/heads/main'";
    let environment = "has(pipeline.environment) && pipeline.environment == 'production'";
    assert!(PAGE.contains(&format!("`{environment}`")));
    let scarab_policy::policy::Body::Rules { rules } = &policy.body else {
        panic!("the page's Policy has rules");
    };
    let first = &rules[0].when;
    assert!(first.contains(ref_check), "{first}");
    let variant = first.replace(ref_check, environment);
    let schema = scarab_policy::run_create::schema();
    let checked = check(&schema, &variant, &Type::Bool).unwrap_or_else(|d| panic!("{d:?}"));
    assert!(
        checked.warnings.is_empty(),
        "{variant}: {:?}",
        checked.warnings
    );
}

#[tokio::test]
async fn the_page_policy_refuses_another_branch() {
    let ci = pipeline_with("{ push: {} }", "");
    let db = InMemoryDb::new();
    let out = trigger(&db, &ci, true, &push_to("refs/heads/feature")).await;
    assert_eq!(refused_messages(&out), vec![BRANCH_RULE.to_string()]);
}

#[tokio::test]
async fn the_page_policy_refuses_a_fork_pull_request() {
    let ci = pipeline_with("{ pull_request: {} }", "");
    let db = InMemoryDb::new();
    let out = trigger(&db, &ci, true, &fork_pr()).await;
    assert_eq!(refused_messages(&out), vec![BRANCH_RULE.to_string()]);
}

// Attack: a Pipeline author on main adds a Step of their own that names the push profile, hoping to run repository code with the push role. Safe outcome: the page's Policy refuses the Run when it is created (the launch would fail anyway, by the profile's `registry_auth_only` marker; tested in scarab-executor-k8s).
#[tokio::test]
async fn the_page_policy_refuses_a_pipeline_file_step_naming_ecr_push() {
    let ci = pipeline_with(
        "{ push: {} }",
        "[{ id: steal, image: alpine, command: [env], placement_profiles: [ecr-push] }]",
    );
    let db = InMemoryDb::new();
    let out = trigger(&db, &ci, true, &push_to("refs/heads/main")).await;
    assert_eq!(refused_messages(&out), vec![LIBRARY_RULE.to_string()]);
}
