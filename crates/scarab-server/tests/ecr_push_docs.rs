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
//! - The page's multi-architecture step (issue #218, the build-multiarch
//!   library) routes `ecr-push` to its push Step only, and the Policy admits
//!   it; the pipeline syntax page's multi-architecture examples compile and
//!   render.
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
/// The pipeline syntax page, whose image index example is checked here too.
const SYNTAX_PAGE: &str =
    include_str!("../../../ui/scarab-docs-ui/src/content/docs/usage/pipeline-syntax.mdx");
const BUILD: &str = include_str!("../../../libs/build.yaml");
const CLONE: &str = include_str!("../../../libs/clone.yaml");
const INDEX: &str = include_str!("../../../libs/image-index.yaml");
const MULTIARCH: &str = include_str!("../../../libs/build-multiarch.yaml");
const LAYOUT: &str = include_str!("../../../libs/build-layout.yaml");
const BUILD_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/build.yaml@1111111111111111111111111111111111111111";
const CLONE_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@1111111111111111111111111111111111111111";
const INDEX_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/image-index.yaml@1111111111111111111111111111111111111111";
const MULTIARCH_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/build-multiarch.yaml@1111111111111111111111111111111111111111";
const LAYOUT_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/build-layout.yaml@1111111111111111111111111111111111111111";
const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

const BRANCH_RULE: &str =
    "policy `ecr-push`: Only Runs on main, not from a fork, may name the ecr-push placement profile.";
const LIBRARY_RULE: &str =
    "policy `ecr-push`: Only the build, multi-architecture build and image index libraries' steps may name the ecr-push placement profile.";

/// The page's fenced code blocks of `lang`, with the fence's indent removed
/// from each line (a block inside a list item is indented).
fn blocks(lang: &str) -> Vec<String> {
    page_blocks(PAGE, lang)
}

/// `page`'s fenced code blocks of `lang`, as [`blocks`] reads them.
fn page_blocks(page: &str, lang: &str) -> Vec<String> {
    let open = format!("```{lang}");
    let mut out = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for line in page.lines() {
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
            let v = serde_yaml::from_str::<serde_yaml::Value>(b).unwrap();
            v["kind"].as_str() == Some("policy") && v["name"].as_str() == Some("ecr-push")
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
    // The ServiceAccount, the chart values, the Pipeline, the image index
    // steps, the Policy, and for private base images the pull profile's
    // chart values, its placement map and its Policy.
    assert_eq!(yaml.len(), 8, "{yaml:?}");
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
                "thulasi-ram::scarab-ci::build".to_string(),
                InstalledLibrary {
                    content: BUILD.into(),
                    source: BUILD_SOURCE.into(),
                },
            ),
            (
                "thulasi-ram::scarab-ci::clone".to_string(),
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
        before: String::new(),
        changed_paths: Default::default(),
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
        head_branch: "feature".into(),
        fork: true,
    }
}

async fn install(libs: &InMemoryLibraries, name: &str, content: &str, source: &str) {
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: source.into(),
        name: name.into(),
        fetched_by: "amy".into(),
        fetched_at: 1,
        requested_ref: None,
        resolved_commit: String::new(),
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: "acme".into(),
            name: name.into(),
            content_hash: content_hash(content),
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

/// The clone library trusted with forge credentials, the build library
/// trusted as the page says: for the build sandbox and with the registry
/// credential, not with forge credentials; the multi-architecture build
/// library trusted the same way; and the image index library trusted with
/// the registry credential.
async fn libraries() -> InMemoryLibraries {
    let libs = InMemoryLibraries::new();
    install(&libs, "thulasi-ram::scarab-ci::clone", CLONE, CLONE_SOURCE).await;
    trust(&libs, CLONE_SOURCE, TrustGrant::ForgeCredentials).await;
    install(&libs, "thulasi-ram::scarab-ci::build", BUILD, BUILD_SOURCE).await;
    trust(&libs, BUILD_SOURCE, TrustGrant::BuildSandbox).await;
    trust(&libs, BUILD_SOURCE, TrustGrant::RegistryCredential).await;
    install(
        &libs,
        "thulasi-ram::scarab-ci::image-index",
        INDEX,
        INDEX_SOURCE,
    )
    .await;
    trust(&libs, INDEX_SOURCE, TrustGrant::RegistryCredential).await;
    install(
        &libs,
        "thulasi-ram::scarab-ci::build-multiarch",
        MULTIARCH,
        MULTIARCH_SOURCE,
    )
    .await;
    trust(&libs, MULTIARCH_SOURCE, TrustGrant::BuildSandbox).await;
    trust(&libs, MULTIARCH_SOURCE, TrustGrant::RegistryCredential).await;
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

/// The page's multi-architecture step (the build-multiarch library),
/// added to the page's Pipeline in place of its one build.
fn two_architecture_pipeline() -> String {
    let found: Vec<String> = blocks("yaml")
        .into_iter()
        .filter(|b| b.contains("lib:thulasi-ram::scarab-ci::build-multiarch"))
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&page_pipeline()).unwrap();
    let steps = doc["steps"].as_sequence_mut().unwrap();
    steps.retain(|s| s["id"].as_str() != Some("image"));
    let more: Vec<serde_yaml::Value> = serde_yaml::from_str(&found[0]).unwrap();
    steps.extend(more);
    serde_yaml::to_string(&doc).unwrap()
}

/// Every library the pages name, installed.
fn the_libraries() -> Libraries {
    let installed = |name: &str, content: &str, source: &str| {
        (
            format!("thulasi-ram::scarab-ci::{name}"),
            InstalledLibrary {
                content: content.into(),
                source: source.into(),
            },
        )
    };
    Libraries {
        installed: BTreeMap::from([
            installed("build", BUILD, BUILD_SOURCE),
            installed("clone", CLONE, CLONE_SOURCE),
            installed("image-index", INDEX, INDEX_SOURCE),
            installed("build-multiarch", MULTIARCH, MULTIARCH_SOURCE),
            installed("build-layout", LAYOUT, LAYOUT_SOURCE),
        ]),
        ..Default::default()
    }
}

/// A page block as a Pipeline: a list of Steps is wrapped in `steps:`,
/// after the page's `checkout` Step when the list needs one it does not
/// hold.
fn as_pipeline(block: &str) -> String {
    let v: serde_yaml::Value = serde_yaml::from_str(block).unwrap();
    let Some(steps) = v.as_sequence() else {
        return block.to_string();
    };
    let mut steps = steps.clone();
    if !steps.iter().any(|s| s["id"].as_str() == Some("checkout")) {
        steps.insert(
            0,
            serde_yaml::from_str("{ id: checkout, invoke: \"lib:thulasi-ram::scarab-ci::clone\" }")
                .unwrap(),
        );
    }
    let mut doc = serde_yaml::Mapping::new();
    doc.insert("steps".into(), serde_yaml::Value::Sequence(steps));
    serde_yaml::to_string(&doc).unwrap()
}

/// The page block on `page` that invokes `lib` (and holds `also`).
fn the_page_block(page: &str, lib: &str, also: &str) -> String {
    let found: Vec<String> = page_blocks(page, "yaml")
        .into_iter()
        .filter(|b| b.contains(&format!("lib:thulasi-ram::scarab-ci::{lib}")) && b.contains(also))
        .collect();
    assert_eq!(found.len(), 1, "{lib} {also}: {found:?}");
    as_pipeline(&found[0])
}

/// The env `step` of `ir` renders at launch with `results`.
fn launch_env(
    ir: &scarab_pipeline::PipelineIr,
    step: &str,
    results: serde_json::Value,
) -> Result<Vec<(String, String)>, String> {
    let s = ir
        .steps
        .iter()
        .find(|s| s.id == step)
        .unwrap_or_else(|| panic!("no step `{step}`"));
    let results: scarab_pipeline::Results = serde_json::from_value(results).unwrap();
    let event = serde_json::json!({ "sha": SHA });
    scarab_pipeline::render(
        scarab_pipeline::Templates {
            image: &s.image,
            command: &s.command,
            env: &s.env,
        },
        s.scope.as_ref(),
        &s.matrix_values,
        scarab_pipeline::Facts::Launch {
            params: &BTreeMap::new(),
            declared: &[],
            results: &results,
            event: event.as_object().unwrap(),
        },
    )
    .map(|r| r.env)
}

fn env_value(env: &[(String, String)], name: &str) -> String {
    env.iter()
        .find(|(k, _)| k == name)
        .unwrap_or_else(|| panic!("no {name} in {env:?}"))
        .1
        .clone()
}

/// A layout path as a build Step in a Pod reports it.
fn layout_path(c: char) -> String {
    format!("/workspace/.scarab-build/{}/oci", c.to_string().repeat(32))
}

/// Compile `pipeline`, check that its `app/index` Step needs the two
/// builds' push Steps only, and render that Step's `SCARAB_INDEX_IMAGES` at
/// launch: with no `push` result from either build (a fork pull request) it
/// is blank; with both pushed it is their references, one per line.
fn assert_index_step_renders(pipeline: &str) {
    let ir = scarab_pipeline::compile_with_libraries(pipeline, &the_libraries())
        .unwrap_or_else(|e| panic!("the Pipeline compiles: {e:?}\n{pipeline}"));
    let index = ir
        .steps
        .iter()
        .find(|s| s.id == "app/index")
        .expect("the index step");
    let mut needs = index.needs.0.clone();
    needs.sort();
    assert_eq!(needs, ["amd64/push", "arm64/push"]);
    let images = |results: serde_json::Value| -> Result<String, String> {
        Ok(env_value(
            &launch_env(&ir, "app/index", results)?,
            "SCARAB_INDEX_IMAGES",
        ))
    };
    let fork = images(serde_json::json!({ "amd64/push": {}, "arm64/push": {} }))
        .expect("the index step renders when no build pushed");
    assert!(fork.trim().is_empty(), "{fork:?}");
    let repo = "444455556666.dkr.ecr.eu-west-1.amazonaws.com/app";
    let amd64 = format!("sha256:{}", "1".repeat(64));
    let arm64 = format!("sha256:{}", "2".repeat(64));
    let pushed = images(serde_json::json!({
        "amd64/push": { "push": { "ref": format!("{repo}@{amd64}"), "digest": amd64 } },
        "arm64/push": { "push": { "ref": format!("{repo}@{arm64}"), "digest": arm64 } },
    }))
    .expect("the index step renders the pushed references");
    assert_eq!(pushed, format!("{repo}@{amd64}\n{repo}@{arm64}\n"));
}

/// The page's build instances: `build-multiarch`'s build Step, once per
/// platform of its `platforms` default.
const AMD: &str = "app/build[arch=linux/amd64]";
const ARM: &str = "app/build[arch=linux/arm64]";

/// The page's multi-architecture step compiles to two builds and one push
/// Step that receives only the builds' workspaces; at launch its
/// `SCARAB_INDEX_LAYOUTS` is the two builds' layouts and digests, one per
/// line, and a placeholder line for a build with no layout (a fork's, whose
/// push Step exits 0 before it reads the line).
#[test]
fn the_page_multiarch_push_step_renders_the_two_layouts() {
    let pipeline = two_architecture_pipeline();
    let ir = scarab_pipeline::compile_with_libraries(&pipeline, &the_libraries())
        .unwrap_or_else(|e| panic!("the Pipeline compiles: {e:?}\n{pipeline}"));
    let push = ir.steps.iter().find(|s| s.id == "app/push").unwrap();
    let mut inputs = push.inputs.clone().expect("the push Step names its inputs");
    inputs.sort();
    assert_eq!(inputs, [AMD, ARM]);
    let amd64 = format!("sha256:{}", "1".repeat(64));
    let arm64 = format!("sha256:{}", "2".repeat(64));
    let built = |l: String, d: &str| serde_json::json!({ "image": { "containerimage.digest": d, "digest": d, "layout": l } });
    let env = launch_env(
        &ir,
        "app/push",
        serde_json::json!({
            AMD: built(layout_path('a'), &amd64),
            ARM: built(layout_path('b'), &arm64),
        }),
    )
    .unwrap();
    assert_eq!(
        env_value(&env, "SCARAB_INDEX_LAYOUTS"),
        format!("{} {amd64}\n{} {arm64}", layout_path('a'), layout_path('b'))
    );
    assert_eq!(env_value(&env, "SCARAB_INDEX_EXTRA_TAGS"), "latest");
    assert_eq!(
        env_value(&env, "SCARAB_INDEX_IMAGE"),
        format!("444455556666.dkr.ecr.eu-west-1.amazonaws.com/app:{SHA}")
    );
    let fork = launch_env(&ir, "app/push", serde_json::json!({})).unwrap();
    assert_eq!(
        env_value(&fork, "SCARAB_INDEX_LAYOUTS"),
        "(linux/amd64 wrote no layout)\n(linux/arm64 wrote no layout)"
    );
}

/// AUD-15: the pipeline syntax page's multi-architecture examples compile
/// with the libraries and render: the image index library joining pushed
/// references, as the ECR page's did; build-layout builds feeding it
/// layouts (blank lines from a fork's builds, `<layout> <digest>` lines
/// otherwise); and build-multiarch, whose deploy Step needs its push Step.
#[test]
fn the_pipeline_syntax_multiarch_examples_compile_and_render() {
    assert_index_step_renders(&the_page_block(SYNTAX_PAGE, "image-index", "images: |"));

    let pipeline = the_page_block(SYNTAX_PAGE, "image-index", "layouts: |");
    let ir = scarab_pipeline::compile_with_libraries(&pipeline, &the_libraries())
        .unwrap_or_else(|e| panic!("the Pipeline compiles: {e:?}\n{pipeline}"));
    let digest = format!("sha256:{}", "1".repeat(64));
    let fork = launch_env(
        &ir,
        "app/index",
        serde_json::json!({
            "amd64/image": { "image": { "digest": digest } },
            "arm64/image": { "image": { "digest": digest } },
        }),
    )
    .expect("a fork's builds render");
    assert_eq!(env_value(&fork, "SCARAB_INDEX_LAYOUTS"), "\n\n");
    let built = launch_env(
        &ir,
        "app/index",
        serde_json::json!({
            "amd64/image": { "image": { "digest": digest, "layout": layout_path('a') } },
            "arm64/image": { "image": { "digest": digest, "layout": layout_path('b') } },
        }),
    )
    .expect("the builds' layouts render");
    assert_eq!(
        env_value(&built, "SCARAB_INDEX_LAYOUTS"),
        format!(
            "{} {digest}\n{} {digest}\n",
            layout_path('a'),
            layout_path('b')
        )
    );

    let pipeline = the_page_block(SYNTAX_PAGE, "build-multiarch", "deploy");
    let ir = scarab_pipeline::compile_with_libraries(&pipeline, &the_libraries())
        .unwrap_or_else(|e| panic!("the Pipeline compiles: {e:?}\n{pipeline}"));
    let deploy = ir.steps.iter().find(|s| s.id == "deploy").unwrap();
    assert_eq!(deploy.needs.0, ["app/push"]);
    for (id, builders) in [(AMD, "amd64-builders"), (ARM, "arm64-builders")] {
        let build = ir.steps.iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            build.placement_profiles.compiled().unwrap(),
            [builders.to_string()],
            "{id}"
        );
    }
}

/// The page's multi-architecture step: a push to main is admitted under
/// the page's Policy, and `ecr-push` reaches the push Step alone, which
/// holds the registry credential from the library the org trusted with it.
#[tokio::test]
async fn the_page_multiarch_step_runs_with_ecr_push_on_the_push_step_only() {
    let ci = two_architecture_pipeline();

    let db = InMemoryDb::new();
    let out = trigger(&db, &ci, true, &push_to("refs/heads/main")).await;
    assert!(out.refused.is_empty(), "{:?}", out.refused);
    assert_eq!(out.runs.len(), 1);
    let run = &out.runs[0];
    assert_eq!(
        stored_profiles(&db, run, "app/push").await,
        vec!["ecr-push".to_string()]
    );
    for (id, builders) in [(AMD, "amd64-builders"), (ARM, "arm64-builders")] {
        assert_eq!(
            stored_profiles(&db, run, id).await,
            vec![builders.to_string(), "build-cache".to_string()],
            "{id}: the page's coordinate key places it"
        );
        let build = db
            .step_spec(run, &StepId(id.into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(build.registry_auth, None, "{id}");
    }
    let push = db
        .step_spec(run, &StepId("app/push".into()))
        .await
        .unwrap()
        .unwrap();
    let auth = push.registry_auth.expect("registry_auth");
    assert_eq!(auth.library, Some(content_hash(MULTIARCH)));
}
