//! ADR-0075 §7, §9–§11: the Policy API through the real router, with
//! step-up enforced (the step-up harness) and an in-memory PolicyStore.
//!
//! - org Policies: create, list, read, version history; Project typed
//!   settings, where a CEL body is a 400;
//! - step-up (§10): editing an enforced Policy or deleting one needs a
//!   step-up; editing an audit-mode one, or moving it to `enforce`, does not;
//!   a stale `expected_version` is a 409;
//! - the policy test API evaluates and records nothing;
//! - roles: a member cannot write org Policies; a Project admin writes its
//!   Project's typed settings and nothing else.

mod step_up_harness;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{json, Value};

use scarab_forge::{Event, RepoRef};
use scarab_identity::{Role, Scope};
use scarab_policy::store::PolicyStore;
use scarab_server::{trigger_runs_from_event, AdmissionPorts};
use scarab_testkit::{FakeClock, FakeForge, InMemoryPolicies};

use step_up_harness::*;

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

const LIB: &str = r#"
kind: library
steps:
  - { id: helm, image: busybox, command: ["true"] }
"#;

const CI_YAML: &str = r#"
on:
  push: {}
  manual: {}
steps:
  - { id: test, image: busybox, command: ["true"] }
  - { id: ship, invoke: file:.scarab/lib/deploy.yaml, needs: [test] }
"#;

const DENY_MESSAGE: &str = "The repository's own deploy library may not be used.";

/// An org Policy denying any Step inlined from `.scarab/lib/deploy.yaml`.
fn deny_local_library(mode: &str) -> Value {
    json!({
        "name": "no-local-deploy-lib",
        "checkpoint": "run.create",
        "mode": mode,
        "rules": [{
            "when": "steps.exists(s, s.origin.exists(o, o.kind == \"file\" && o.name == \".scarab/lib/deploy.yaml\"))",
            "effect": "deny",
            "message": DENY_MESSAGE
        }]
    })
}

/// The same Policy with a looser rule: only on `main`.
fn deny_local_library_on_main(mode: &str) -> Value {
    let mut p = deny_local_library(mode);
    p["rules"][0]["when"] = json!(
        "trigger.ref == \"refs/heads/main\" && steps.exists(s, s.origin.exists(o, o.kind == \"file\"))"
    );
    p
}

fn blocked_setting() -> Value {
    json!({
        "name": "no-local-libs",
        "checkpoint": "run.create",
        "setting": { "libraries_blocked": { "sources": [".scarab/lib/*"] } }
    })
}

fn with_policies() -> (Harness, Arc<InMemoryPolicies>) {
    let policies = Arc::new(InMemoryPolicies::new());
    let p = policies.clone();
    (harness_with(move |st| st.with_policies(p)), policies)
}

/// amy, an Owner with a passkey (no window open).
async fn admin(h: &Harness) -> (Sess, scarab_webauthn::soft::SoftPasskey) {
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
}

async fn create(h: &Harness, s: &Sess, uri: &str, policy: Value) -> Value {
    let (st, b) = h
        .cookie(
            "POST",
            uri,
            Some(json!({ "policy": policy, "reason": "r" })),
            s,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    b
}

fn put_body(policy: Value, expected_version: &Value) -> Option<Value> {
    Some(json!({ "policy": policy, "expected_version": expected_version, "reason": "edit" }))
}

#[tokio::test]
async fn org_policies_create_list_get_and_keep_their_version_history() {
    let (h, _) = with_policies();
    let (s, _) = admin(&h).await;

    let created = create(
        &h,
        &s,
        "/v1/orgs/acme/policies",
        deny_local_library("audit"),
    )
    .await;
    assert_eq!(created["version"], 1);
    assert_eq!(created["level"], "org");
    assert_eq!(created["policy"]["mode"], "audit");
    let id = created["id"].as_str().unwrap();

    let (st, list) = h.cookie("GET", "/v1/orgs/acme/policies", None, &s).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id);

    let uri = format!("/v1/orgs/acme/policies/{id}");
    let (st, got) = h.cookie("GET", &uri, None, &s).await;
    assert_eq!(st, StatusCode::OK, "{got}");
    assert_eq!(got["content_hash"], created["content_hash"]);
    // Another org does not see it.
    let (st, _) = h
        .cookie("GET", &format!("/v1/orgs/other/policies/{id}"), None, &s)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // An audit-mode Policy enforces nothing: edits and audit→enforce are
    // exempt, with no window open.
    let (st, v2) = h
        .cookie(
            "PUT",
            &uri,
            put_body(deny_local_library_on_main("audit"), &json!(1)),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v2}");
    let (st, v3) = h
        .cookie(
            "PUT",
            &uri,
            put_body(deny_local_library_on_main("enforce"), &json!(2)),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v3}");
    assert_eq!(v3["version"], 3);

    let (st, versions) = h.cookie("GET", &format!("{uri}/versions"), None, &s).await;
    assert_eq!(st, StatusCode::OK, "{versions}");
    let versions = versions.as_array().unwrap();
    assert_eq!(versions.len(), 3);
    assert_eq!(versions[0]["policy"]["mode"], "audit");
    assert_eq!(versions[2]["policy"]["mode"], "enforce");
    assert!(versions.iter().all(|v| v["author"] == "amy"));
    assert!(versions.iter().all(|v| v["step_up_ref"].is_null()));
    assert!(h.events_of("step_up.refused").is_empty());
}

#[tokio::test]
async fn editing_an_enforced_policy_needs_a_step_up_and_records_it() {
    let (h, policies) = with_policies();
    let (s, mut soft) = admin(&h).await;
    let created = create(
        &h,
        &s,
        "/v1/orgs/acme/policies",
        deny_local_library("enforce"),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let uri = format!("/v1/orgs/acme/policies/{id}");

    // A looser rule, and a move to audit: each a loosening.
    for body in [
        deny_local_library_on_main("enforce"),
        deny_local_library("audit"),
    ] {
        let (st, b) = h.cookie("PUT", &uri, put_body(body, &json!(1)), &s).await;
        assert_step_up(st, &b, "window");
        assert_eq!(b["op"], "policy.update", "{b}");
    }
    assert_eq!(policies.get(id).await.unwrap().unwrap().version, 1);

    h.open_window(&s, &mut soft).await;
    let (st, b) = h
        .cookie(
            "PUT",
            &uri,
            put_body(deny_local_library_on_main("enforce"), &json!(1)),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let versions = policies.versions(id).await.unwrap();
    assert_eq!(versions.len(), 2);
    assert!(
        versions[1].step_up_ref.is_some(),
        "the confirmation is recorded: {versions:?}"
    );
}

#[tokio::test]
async fn deleting_an_enforced_policy_needs_a_step_up_and_an_audit_one_does_not() {
    let (h, policies) = with_policies();
    let (s, mut soft) = admin(&h).await;
    let enforced = create(
        &h,
        &s,
        "/v1/orgs/acme/policies",
        deny_local_library("enforce"),
    )
    .await;
    // A typed setting, enforced by default.
    let setting = create(&h, &s, "/v1/orgs/acme/policies", blocked_setting()).await;
    let mut audit_body = blocked_setting();
    audit_body["name"] = json!("audit-only");
    audit_body["mode"] = json!("audit");
    let audit_only = create(&h, &s, "/v1/orgs/acme/policies", audit_body).await;

    let del = |p: &Value| {
        format!(
            "/v1/orgs/acme/policies/{}?expected_version=1&reason=cleanup",
            p["id"].as_str().unwrap()
        )
    };
    let (st, b) = h.cookie("DELETE", &del(&audit_only), None, &s).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["deleted"], true);
    assert_eq!(b["version"], 2);

    for p in [&enforced, &setting] {
        let (st, b) = h.cookie("DELETE", &del(p), None, &s).await;
        assert_step_up(st, &b, "window");
        assert_eq!(b["op"], "policy.delete", "{b}");
    }
    h.open_window(&s, &mut soft).await;
    let (st, b) = h.cookie("DELETE", &del(&enforced), None, &s).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let id = enforced["id"].as_str().unwrap();
    let stored = policies.get(id).await.unwrap().unwrap();
    assert!(stored.deleted);
    // The history stays and the Policy is no longer listed.
    assert_eq!(policies.versions(id).await.unwrap().len(), 2);
    let (_, list) = h.cookie("GET", "/v1/orgs/acme/policies", None, &s).await;
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
}

#[tokio::test]
async fn a_stale_expected_version_is_a_409_and_writes_nothing() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    let created = create(
        &h,
        &s,
        "/v1/orgs/acme/policies",
        deny_local_library("audit"),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let uri = format!("/v1/orgs/acme/policies/{id}");

    let (st, b) = h
        .cookie(
            "PUT",
            &uri,
            put_body(deny_local_library("enforce"), &json!(7)),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    let (st, b) = h
        .cookie("DELETE", &format!("{uri}?expected_version=7"), None, &s)
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    let stored = policies.get(id).await.unwrap().unwrap();
    assert_eq!((stored.version, stored.deleted), (1, false));
}

#[tokio::test]
async fn a_project_takes_typed_settings_only_and_unconsulted_checkpoints_are_refused() {
    let (h, _) = with_policies();
    let (s, _) = admin(&h).await;
    let uri = "/v1/orgs/acme/projects/web/policies";

    let (st, b) = h
        .cookie(
            "POST",
            uri,
            Some(json!({ "policy": deny_local_library("enforce") })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(b.as_str().unwrap().contains("ADR-0075 §7"), "{b}");

    let mut later = blocked_setting();
    later["checkpoint"] = json!("step.admit");
    let (st, b) = h
        .cookie("POST", uri, Some(json!({ "policy": later })), &s)
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b.as_str()
            .unwrap()
            .contains("compiles to `run.create`, not `step.admit`"),
        "{b}"
    );

    let created = create(&h, &s, uri, blocked_setting()).await;
    assert_eq!(created["level"], "project");
    assert_eq!(created["project"], "web");
    // A Project's settings are not the org's.
    let (_, org_list) = h.cookie("GET", "/v1/orgs/acme/policies", None, &s).await;
    assert!(org_list.as_array().unwrap().is_empty(), "{org_list}");
    let (_, list) = h.cookie("GET", uri, None, &s).await;
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    // Nor is it reachable under the org path.
    let (st, _) = h
        .cookie(
            "GET",
            &format!("/v1/orgs/acme/policies/{}", created["id"].as_str().unwrap()),
            None,
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn roles_a_member_cannot_write_and_a_project_admin_writes_only_its_project() {
    let (h, policies) = with_policies();
    h.grant("mia", Scope::Org("acme".into()), Role::Member)
        .await;
    h.grant(
        "pat",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Admin,
    )
    .await;
    h.grant("oscar", Scope::Org("acme".into()), Role::Admin)
        .await;
    let mia = h.browser_session(scoped("mia")).await;
    let pat = h.browser_session(scoped("pat")).await;
    let oscar = h.browser_session(scoped("oscar")).await;
    let refused = (
        StatusCode::FORBIDDEN,
        Value::String("insufficient role".into()),
    );
    let body = |p: Value| Some(json!({ "policy": p }));

    let r = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies",
            body(deny_local_library("enforce")),
            &mia,
        )
        .await;
    assert_eq!(r, refused);
    let r = h
        .cookie(
            "POST",
            "/v1/orgs/acme/projects/web/policies",
            body(blocked_setting()),
            &mia,
        )
        .await;
    assert_eq!(r, refused);
    // A member reads them.
    let (st, _) = h.cookie("GET", "/v1/orgs/acme/policies", None, &mia).await;
    assert_eq!(st, StatusCode::OK);

    let r = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies",
            body(deny_local_library("enforce")),
            &pat,
        )
        .await;
    assert_eq!(r, refused);
    let r = h
        .cookie(
            "POST",
            "/v1/orgs/acme/projects/api/policies",
            body(blocked_setting()),
            &pat,
        )
        .await;
    assert_eq!(r, refused, "another Project");
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/projects/web/policies",
            body(blocked_setting()),
            &pat,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    // An org admin passes at the Project.
    let mut other = blocked_setting();
    other["name"] = json!("by-org-admin");
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/projects/web/policies",
            body(other),
            &oscar,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    let web = scarab_policy::store::PolicyScope::Project {
        org: "acme".into(),
        project: "web".into(),
    };
    assert_eq!(policies.list(&web).await.unwrap().len(), 2);
    let org = scarab_policy::store::PolicyScope::Org { org: "acme".into() };
    assert!(policies.list(&org).await.unwrap().is_empty());
}

/// A change's `reason` is at most 2,000 bytes, and a scope holds at most
/// 256 live Policies.
#[tokio::test]
async fn reasons_and_policies_per_scope_are_capped() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    let long = "r".repeat(2_001);
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies",
            Some(json!({ "policy": deny_local_library("audit"), "reason": long })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(b.to_string().contains("at most 2000 bytes"), "{b}");
    let created = create(
        &h,
        &s,
        "/v1/orgs/acme/policies",
        deny_local_library("audit"),
    )
    .await;
    let (st, b) = h
        .cookie(
            "PUT",
            &format!("/v1/orgs/acme/policies/{}", created["id"].as_str().unwrap()),
            Some(json!({
                "policy": deny_local_library("audit"),
                "expected_version": created["version"],
                "reason": "r".repeat(2_001)
            })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    let (st, b) = h
        .cookie(
            "DELETE",
            &format!(
                "/v1/orgs/acme/policies/{}?expected_version=1&reason={}",
                created["id"].as_str().unwrap(),
                "r".repeat(2_001)
            ),
            None,
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");

    // Fill the org to the cap (one live Policy is there already).
    for i in 1..256 {
        let mut body = deny_local_library("audit");
        body["name"] = json!(format!("p{i}"));
        policies
            .create(&scarab_policy::store::NewPolicy {
                id: format!("seed-{i}"),
                scope: scarab_policy::store::PolicyScope::Org { org: "acme".into() },
                body: scarab_policy::policy::Policy::from_json(body).unwrap(),
                author: "amy".into(),
                reason: String::new(),
                at: 1,
            })
            .await
            .unwrap();
    }
    let mut one_more = deny_local_library("audit");
    one_more["name"] = json!("one-more");
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies",
            Some(json!({ "policy": one_more, "reason": "r" })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert!(b.to_string().contains("256"), "{b}");
    // The cap is per scope: a Project's typed settings are counted apart.
    create(
        &h,
        &s,
        "/v1/orgs/acme/projects/web/policies",
        blocked_setting(),
    )
    .await;
}

fn pipeline_form() -> Value {
    json!({
        "yaml": CI_YAML,
        "path": ".scarab/ci.yaml",
        "libs": { ".scarab/lib/deploy.yaml": LIB },
        "project": "web",
        "trigger": { "kind": "push", "ref": "refs/heads/main", "sha": SHA }
    })
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "ship it".into(),
        before: String::new(),
        changed_paths: Default::default(),
    }
}

#[tokio::test]
async fn policy_test_evaluates_every_form_and_records_nothing() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    let org_policy = create(
        &h,
        &s,
        "/v1/orgs/acme/policies",
        deny_local_library_on_main("enforce"),
    )
    .await;
    create(
        &h,
        &s,
        "/v1/orgs/acme/projects/web/policies",
        blocked_setting(),
    )
    .await;

    // A real refused Run records one decision in the org log.
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", CI_YAML)
        .with_file(".scarab/lib/deploy.yaml", LIB)
        .with_commit("refs/heads/main", SHA);
    let outcome = trigger_runs_from_event(
        &forge,
        h.db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_policies(policies.as_ref()),
        &push(),
    )
    .await
    .unwrap();
    assert!(outcome.runs.is_empty());
    let log = policies.org_decisions("acme", 0, None).await.unwrap();
    assert_eq!(log.len(), 1);
    let decision_id = log[0].id.clone();

    let test = |body: Value| {
        let h = &h;
        let s = &s;
        async move {
            h.cookie("POST", "/v1/orgs/acme/policy-test", Some(body), s)
                .await
        }
    };

    // (a) The Pipeline form, over the effective Policies: the library origin
    // is denied by both the org rule and the Project's typed setting.
    let (st, b) = test(json!({ "pipeline": pipeline_form() })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let d = &b["decision"];
    assert_eq!(d["outcome"]["kind"], "deny", "{b}");
    assert_eq!(d["results"].as_array().unwrap().len(), 2, "{b}");
    assert_eq!(d["results"][0]["outcome"]["kind"], "denied", "{b}");
    assert!(d["outcome"]["messages"]
        .as_array()
        .unwrap()
        .contains(&json!(DENY_MESSAGE)));
    let ship = d["input"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "ship/helm")
        .expect("the inlined Step");
    assert_eq!(ship["origin"][0]["name"], ".scarab/lib/deploy.yaml");
    // What each rule reads: the org rule depends on the trigger.
    let pols = b["policies"].as_array().unwrap();
    let org_reads = &pols
        .iter()
        .find(|p| p["policy_id"] == org_policy["id"])
        .expect("the org Policy's reads")["rules"][0];
    assert_eq!(org_reads["predicate"], "cel");
    let reads: Vec<&str> = org_reads["reads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert!(reads.contains(&"trigger.ref"), "{reads:?}");
    assert!(pols.iter().any(|p| p["rules"][0]["predicate"] == "typed"));
    let input = d["input"].clone();

    // The same Pipeline on another branch: the org rule does not hold.
    let mut other = pipeline_form();
    other["trigger"]["ref"] = json!("refs/heads/dev");
    let (_, b) = test(json!({ "pipeline": other, "policy_id": org_policy["id"] })).await;
    assert_eq!(b["decision"]["outcome"]["kind"], "allow", "{b}");

    // (b) A draft Policy over a supplied input.
    let (st, b) = test(json!({ "policy": deny_local_library("enforce"), "input": input })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
    assert_eq!(b["decision"]["results"][0]["policy_id"], "");
    assert_eq!(b["decision"]["results"][0]["version"], 0);
    // A draft that does not compile is a 400.
    let mut broken = deny_local_library("enforce");
    broken["rules"][0]["when"] = json!("steps.nope");
    let (st, _) = test(json!({ "policy": broken, "input": input })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // (c) A stored Policy over a recorded decision's input.
    let (st, b) = test(json!({ "policy_id": org_policy["id"], "decision_id": decision_id })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
    assert_eq!(b["decision"]["results"].as_array().unwrap().len(), 1);
    assert_eq!(b["decision"]["input"], log[0].decision.input);

    // Exactly one input form.
    let (st, _) = test(json!({ "input": input, "decision_id": decision_id })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Nothing was recorded.
    assert_eq!(policies.org_decisions("acme", 0, None).await.unwrap(), log);
    let (st, page) = h
        .cookie("GET", "/v1/orgs/acme/policy-decisions", None, &s)
        .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(page["decisions"].as_array().unwrap().len(), 1, "{page}");
    assert_eq!(page["decisions"][0]["id"], json!(decision_id));
}

const PR_YAML: &str = r#"
on:
  push: {}
  pull_request: {}
steps:
  - { id: test, image: busybox, command: ["true"] }
  - { id: ship, invoke: file:.scarab/lib/deploy.yaml, needs: [test] }
"#;

/// The policy test API builds its input through admission's own code: for
/// the same event, the input it evaluates is the input admission recorded
/// (the time aside, which is each one's clock).
#[tokio::test]
async fn policy_test_evaluates_the_input_admission_records() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    create(
        &h,
        &s,
        "/v1/orgs/acme/policies",
        json!({
            "name": "watch-forks",
            "checkpoint": "run.create",
            "mode": "audit",
            "rules": [{ "when": "trigger.fork", "effect": "deny", "message": "a fork" }]
        }),
    )
    .await;
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", PR_YAML)
        .with_file(".scarab/lib/deploy.yaml", LIB)
        .with_commit("refs/heads/main", SHA);
    let repo = RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    };
    let fork_pr = Event::PullRequest {
        actor: "mallory".into(),
        actor_id: String::new(),
        repo,
        number: 7,
        head: SHA.into(),
        title: "fix".into(),
        base: "main".into(),
        head_branch: "feature".into(),
        fork: true,
    };
    let cases = [
        (
            push(),
            json!({ "kind": "push", "ref": "refs/heads/main", "sha": SHA, "actor": "octocat" }),
        ),
        (
            fork_pr,
            json!({
                "kind": "pull_request", "ref": "refs/pull/7/head", "sha": SHA,
                "actor": "mallory", "fork": true
            }),
        ),
    ];
    let without_time = |mut v: Value| {
        assert!(v["time"].is_string(), "{v}");
        v.as_object_mut().unwrap().remove("time");
        v
    };
    for (event, trigger) in cases {
        let outcome = trigger_runs_from_event(
            &forge,
            h.db.as_ref(),
            &FakeClock::new(1_000),
            AdmissionPorts::default().with_policies(policies.as_ref()),
            &event,
        )
        .await
        .unwrap();
        assert_eq!(outcome.runs.len(), 1, "audit mode admits it");
        let recorded = policies.org_decisions("acme", 1, None).await.unwrap()[0]
            .decision
            .input
            .clone();

        let mut form = pipeline_form();
        form["yaml"] = json!(PR_YAML);
        form["trigger"] = trigger;
        let (st, b) = h
            .cookie(
                "POST",
                "/v1/orgs/acme/policy-test",
                Some(json!({ "pipeline": form })),
                &s,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            without_time(b["decision"]["input"].clone()),
            without_time(recorded),
            "{}",
            event.trigger_kind().as_str()
        );
    }

    // A pull request's ref is the one admission sees, not its branch.
    let mut form = pipeline_form();
    form["trigger"] = json!({ "kind": "pull_request", "ref": "refs/heads/fix" });
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(json!({ "pipeline": form })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(b.to_string().contains("refs/pull/<n>/head"), "{b}");
}

#[tokio::test]
async fn decision_logs_page_the_org_and_list_a_runs_own() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    create(
        &h,
        &s,
        "/v1/orgs/acme/policies",
        deny_local_library("audit"),
    )
    .await;
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", CI_YAML)
        .with_file(".scarab/lib/deploy.yaml", LIB)
        .with_commit("refs/heads/main", SHA);
    let mut runs = Vec::new();
    for t in [1_000, 2_000] {
        let outcome = trigger_runs_from_event(
            &forge,
            h.db.as_ref(),
            &FakeClock::new(t),
            AdmissionPorts::default().with_policies(policies.as_ref()),
            &push(),
        )
        .await
        .unwrap();
        runs.extend(outcome.runs);
    }
    assert_eq!(runs.len(), 2, "audit mode admits both");

    let (st, first) = h
        .cookie("GET", "/v1/orgs/acme/policy-decisions?limit=1", None, &s)
        .await;
    assert_eq!(st, StatusCode::OK, "{first}");
    assert_eq!(first["decisions"][0]["at"], 2_000, "newest first");
    let next = first["next"].as_str().expect("a next page");
    let (_, second) = h
        .cookie(
            "GET",
            &format!("/v1/orgs/acme/policy-decisions?limit=1&before={next}"),
            None,
            &s,
        )
        .await;
    assert_eq!(second["decisions"][0]["at"], 1_000, "{second}");

    let (st, mine) = h
        .cookie(
            "GET",
            &format!("/v1/runs/{}/policy-decisions", runs[0].0),
            None,
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{mine}");
    let mine = mine.as_array().unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["run_id"], json!(runs[0].0));
    assert_eq!(
        mine[0]["decision"]["results"][0]["outcome"]["kind"],
        "would_deny"
    );
    let (st, _) = h
        .cookie("GET", "/v1/runs/no-such-run/policy-decisions", None, &s)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// A library whose Step runs its own command in the build sandbox.
const SANDBOXED_LIB: &str = r#"kind: library
steps:
  - id: run
    image: "moby/buildkit:rootless@sha256:5987568f92dafbfb679879de59c1cd8d26597f5f6635c314e166164e2615870c"
    command: [/bin/sh, -c, id]
    security: { sandbox: rootless-buildkit }
"#;

const SANDBOXED_SOURCE: &str =
    "github.com/evil/libs//build.yaml@2222222222222222222222222222222222222222";

const SANDBOXED_CI: &str = "on: { push: {} }\nsteps:\n  - id: b\n    invoke: lib:evil/build\n";

// git-bug 1aa19e6: the policy test answered `allow` for a Run that Run
// creation refuses because the org does not trust the library's source for
// the build sandbox. It now runs Run creation's own check and answers a deny
// with the same reason, and says what it compiled.
#[tokio::test]
async fn policy_test_denies_a_run_that_run_creation_refuses_for_the_sandbox() {
    use scarab_policy::library::{
        content_hash, trust_key, LibraryStore, LibraryVersion, NewInstallation, Replaces,
        TrustGrant, TrustedSource,
    };
    let libs = Arc::new(scarab_testkit::InMemoryLibraries::new());
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(SANDBOXED_LIB),
        content: SANDBOXED_LIB.into(),
        source: SANDBOXED_SOURCE.into(),
        name: "evil/build".into(),
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
            name: "evil/build".into(),
            content_hash: content_hash(SANDBOXED_LIB),
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
    let policies = Arc::new(InMemoryPolicies::new());
    let (p, l) = (policies.clone(), libs.clone());
    let h = harness_with(move |st| st.with_policies(p).with_libraries(l));
    let (s, _) = admin(&h).await;

    // Run creation refuses the Run.
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", SANDBOXED_CI)
        .with_commit("refs/heads/main", SHA);
    let outcome = trigger_runs_from_event(
        &forge,
        h.db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs.as_ref()),
        &push(),
    )
    .await
    .unwrap();
    assert!(outcome.runs.is_empty(), "{:?}", outcome.runs);
    let reason = outcome.refused[0].reason.clone();
    assert!(
        reason.contains("does not trust for the build sandbox"),
        "{reason}"
    );

    // The policy test answers a deny with the same reason.
    let form = json!({
        "pipeline": {
            "yaml": SANDBOXED_CI,
            "path": ".scarab/ci.yaml",
            "project": "web",
            "trigger": { "kind": "push", "ref": "refs/heads/main", "sha": SHA }
        }
    });
    let (st, b) = h
        .cookie("POST", "/v1/orgs/acme/policy-test", Some(form.clone()), &s)
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["refusal"], json!(reason), "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
    assert_eq!(
        b["decision"]["outcome"]["messages"][0],
        json!(reason),
        "{b}"
    );
    assert_eq!(b["pipeline"]["steps"], 1, "{b}");
    assert_eq!(b["pipeline"]["libraries"], json!(["evil/build"]), "{b}");

    // Trusted for the sandbox, both admit it.
    libs.trust_source(&TrustedSource {
        org: "acme".into(),
        source: trust_key(SANDBOXED_SOURCE).into(),
        grant: TrustGrant::BuildSandbox,
        trusted_by: "amy".into(),
        trusted_at: 2,
        step_up_ref: Some("su-1".into()),
    })
    .await
    .unwrap();
    let (st, b) = h
        .cookie("POST", "/v1/orgs/acme/policy-test", Some(form), &s)
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "allow", "{b}");
    assert!(b.get("refusal").is_none(), "{b}");
}

/// A Pipeline into `prod`, run by push and pull request, whose Step carries
/// `overlay` as its `k8s_overlay`.
fn overlay_ci(overlay: &str) -> String {
    format!(
        "on: {{ push: {{}}, pull_request: {{}} }}\nenvironment: prod\nsteps:\n  - id: b\n    \
         image: busybox\n    command: [\"true\"]\n    k8s_overlay: {overlay}\n"
    )
}

/// `prod` admits every Project, and `web`'s grant there permits overlays.
async fn prod_permitting_overlays() -> Arc<scarab_testkit::InMemoryEnvironmentGovernance> {
    use scarab_project::{
        EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectPrivilegeGrant,
    };
    let governance = Arc::new(
        scarab_testkit::InMemoryEnvironmentGovernance::new().with_environment(
            "acme",
            "prod",
            EnvironmentProjectEligibility::AllProjects,
        ),
    );
    governance
        .put_environment_project_privilege_grant(
            &EnvironmentProjectPrivilegeGrant {
                org: "acme".into(),
                environment: "prod".into(),
                project: "web".into(),
                privileged_images: vec![],
                permit_k8s_overlay: true,
            },
            "amy",
            1,
        )
        .await
        .unwrap();
    governance
}

/// The policy test's Pipeline form for `yaml` under `trigger`.
fn form(yaml: &str, trigger: Value) -> Value {
    json!({
        "pipeline": {
            "yaml": yaml,
            "path": ".scarab/ci.yaml",
            "project": "web",
            "trigger": trigger
        }
    })
}

// git-bug C5: Run creation admits a Step's privilege request and
// `k8s_overlay` after its Policies are consulted, and the policy test did
// not run either check, so `validate --server` answered allow for a Run that
// Run creation then fails. It now runs the same check and answers a deny
// with the reason Run creation gives.
#[tokio::test]
async fn policy_test_denies_a_run_that_run_creation_refuses_for_its_k8s_overlay() {
    let governance = prod_permitting_overlays().await;
    let g = governance.clone();
    let h = harness_with(move |st| {
        st.with_policies(Arc::new(InMemoryPolicies::new()))
            .with_environment_governance(g)
    });
    let (s, _) = admin(&h).await;
    let yaml = overlay_ci("{ spec: { hostNetwork: true } }");

    // Run creation refuses it, naming the field.
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", yaml.as_str())
        .with_commit("refs/heads/main", SHA);
    let err = trigger_runs_from_event(
        &forge,
        h.db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::governance(Some(governance.as_ref())),
        &push(),
    )
    .await
    .expect_err("Run creation refuses the overlay");
    assert!(err.to_string().contains("`spec.hostNetwork`"), "{err}");

    // The policy test answers a deny with the same reason.
    let push_trigger = json!({ "kind": "push", "ref": "refs/heads/main", "sha": SHA });
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(form(&yaml, push_trigger)),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
    let refusal = b["refusal"].as_str().expect("a refusal").to_string();
    assert!(refusal.contains("`spec.hostNetwork`"), "{b}");
    assert!(err.to_string().contains(&refusal), "{err} / {refusal}");

    // An allowed overlay from a fork pull request is refused for the
    // lock-out; the same pull request from the repository is allowed.
    let yaml = overlay_ci("{ spec: { nodeSelector: { pool: gpu } } }");
    for (fork, denied) in [(true, true), (false, false)] {
        let trigger = json!({
            "kind": "pull_request", "ref": "refs/pull/7/head", "sha": SHA, "fork": fork
        });
        let (st, b) = h
            .cookie(
                "POST",
                "/v1/orgs/acme/policy-test",
                Some(form(&yaml, trigger)),
                &s,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        if denied {
            assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
            assert!(
                b["refusal"]
                    .as_str()
                    .is_some_and(|r| r.contains("fork pull request")),
                "{b}"
            );
        } else {
            assert_eq!(b["decision"]["outcome"]["kind"], "allow", "{b}");
            assert!(b.get("refusal").is_none(), "{b}");
        }
    }
}

// AUD-11: the policy test refuses a yanked library version as Run creation
// does, with the same reason.
#[tokio::test]
async fn policy_test_denies_a_run_that_run_creation_refuses_for_a_yanked_version() {
    use scarab_policy::library::{
        content_hash, LibraryStore, LibraryVersion, LibraryYank, NewInstallation, Replaces,
    };
    let libs = Arc::new(scarab_testkit::InMemoryLibraries::new());
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(LIB),
        content: LIB.into(),
        source: format!("github.com/acme/libs//deploy.yaml@{}", "3".repeat(40)),
        name: "acme/deploy".into(),
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
            name: "acme/deploy".into(),
            content_hash: content_hash(LIB),
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
    libs.yank(&LibraryYank {
        org: "acme".into(),
        content_hash: content_hash(LIB),
        reason: "writes the token to the log".into(),
        yanked_by: "amy".into(),
        yanked_at: 2,
    })
    .await
    .unwrap();
    let l = libs.clone();
    let h = harness_with(move |st| {
        st.with_policies(Arc::new(InMemoryPolicies::new()))
            .with_libraries(l)
    });
    let (s, _) = admin(&h).await;
    let yaml = "on: { push: {} }\nsteps:\n  - id: ship\n    invoke: lib:acme/deploy\n";

    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", yaml)
        .with_commit("refs/heads/main", SHA);
    let outcome = trigger_runs_from_event(
        &forge,
        h.db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs.as_ref()),
        &push(),
    )
    .await
    .unwrap();
    assert!(outcome.runs.is_empty(), "{:?}", outcome.runs);
    let reason = outcome.refused[0].reason.clone();
    assert!(reason.contains("writes the token to the log"), "{reason}");

    let trigger = json!({ "kind": "push", "ref": "refs/heads/main", "sha": SHA });
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(form(yaml, trigger)),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
    assert_eq!(b["refusal"], json!(reason), "{b}");
}

// AUD-11: a Step of the repository's own Pipeline asking for a forge
// token. A forge that mints tokens admits it at Run creation and in the
// policy test; one that hands out a long-lived token refuses it in both,
// with the same reason. A server with no forge cannot ask, and Run creation
// always has one, so the policy test leaves it unchecked instead of
// answering a deny Run creation might not give.
#[tokio::test]
async fn policy_test_checks_a_forge_token_as_run_creation_does() {
    let yaml = "on: { push: {} }\nsteps:\n  - id: t\n    image: busybox\n    command: [\"true\"]\n    forge_token: read\n";
    let trigger = json!({ "kind": "push", "ref": "refs/heads/main", "sha": SHA });
    let forge = |mints: bool| {
        let f = FakeForge::new()
            .with_file(".scarab/ci.yaml", yaml)
            .with_commit("refs/heads/main", SHA);
        if mints {
            f.with_minted_tokens()
        } else {
            f
        }
    };
    for mints in [true, false] {
        let f = Arc::new(forge(mints));
        let server_forge = f.clone();
        let h = harness_with(move |st| {
            st.with_policies(Arc::new(InMemoryPolicies::new()))
                .with_forge(server_forge)
        });
        let (s, _) = admin(&h).await;
        let outcome = trigger_runs_from_event(
            f.as_ref(),
            h.db.as_ref(),
            &FakeClock::new(1_000),
            AdmissionPorts::default(),
            &push(),
        )
        .await
        .unwrap();
        let (st, b) = h
            .cookie(
                "POST",
                "/v1/orgs/acme/policy-test",
                Some(form(yaml, trigger.clone())),
                &s,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        if mints {
            assert_eq!(outcome.runs.len(), 1, "{outcome:?}");
            assert_eq!(b["decision"]["outcome"]["kind"], "allow", "{b}");
        } else {
            assert!(outcome.runs.is_empty(), "{outcome:?}");
            let reason = outcome.refused[0].reason.clone();
            assert!(reason.contains("long-lived token"), "{reason}");
            assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
            assert_eq!(b["refusal"], json!(reason), "{b}");
        }
    }

    // No forge on the server: not checked, so no deny for it.
    let h = harness_with(|st| st.with_policies(Arc::new(InMemoryPolicies::new())));
    let (s, _) = admin(&h).await;
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(form(yaml, trigger)),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "allow", "{b}");
    assert!(b.get("refusal").is_none(), "{b}");
}

/// The operator's placement config: profiles `big` and `ecr-push`, the
/// second carrying a registry credential.
fn placement() -> scarab_executor_k8s::PlacementConfig {
    serde_json::from_value(json!({
        "profiles": [
            { "name": "big", "k8s": { "spec": { "nodeSelector": { "pool": "big" } } } },
            { "name": "ecr-push", "registry_auth_only": true,
              "k8s": { "spec": { "serviceAccountName": "ecr-pusher" } } }
        ]
    }))
    .unwrap()
}

// RT-R2-2: a Step's `placement_profiles` come from the Pipeline file, and
// the executor's error for a name its registry does not hold, or for a
// `registry_auth_only` profile on a Step without `registry_auth`, was found
// only at launch: retried for 300 s, then the Run dead-lettered. Run
// creation now refuses both, naming the Step and the profile, and the policy
// test (what `validate --server` asks) answers a deny with the same text.
#[tokio::test]
async fn run_creation_and_the_policy_test_refuse_a_placement_profile_the_registry_refuses() {
    let h = harness_with(|st| {
        st.with_policies(Arc::new(InMemoryPolicies::new()))
            .with_placement(placement())
    });
    let (s, _) = admin(&h).await;
    let config = placement();
    let ports = AdmissionPorts {
        placement: Some(&config),
        ..AdmissionPorts::default()
    };
    for (yaml, says) in [
        (
            "on: { push: {} }\nsteps:\n  - { id: b, image: busybox, placement_profiles: [bgi] }\n",
            vec!["step `b`", "`bgi`"],
        ),
        (
            "on: { push: {} }\nsteps:\n  - { id: b, image: busybox, placement_profiles: [ecr-push] }\n",
            vec!["step `b`", "`ecr-push`", "registry_auth"],
        ),
    ] {
        let forge = FakeForge::new()
            .with_file(".scarab/ci.yaml", yaml)
            .with_commit("refs/heads/main", SHA);
        let err = trigger_runs_from_event(
            &forge,
            h.db.as_ref(),
            &FakeClock::new(1_000),
            ports,
            &push(),
        )
        .await
        .expect_err("Run creation refuses the profile");
        let err = err.to_string();
        for want in &says {
            assert!(err.contains(want), "{want}: {err}");
        }
        assert!(
            !err.contains("`big`"),
            "the refusal lists no other profile: {err}"
        );

        let trigger = json!({ "kind": "push", "ref": "refs/heads/main", "sha": SHA });
        let (st, b) = h
            .cookie(
                "POST",
                "/v1/orgs/acme/policy-test",
                Some(form(yaml, trigger)),
                &s,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
        let refusal = b["refusal"].as_str().expect("a refusal").to_string();
        assert!(err.contains(&refusal), "{err} / {refusal}");
    }

    // A name the registry holds, and the push profile on a `registry_auth`
    // Step, are admitted.
    let yaml = "on: { push: {} }\nsteps:\n  - { id: b, image: busybox, placement_profiles: [big] }\n  - { id: p, image: busybox, registry_auth: true, placement_profiles: [ecr-push] }\n";
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", yaml)
        .with_commit("refs/heads/main", SHA);
    let created = trigger_runs_from_event(
        &forge,
        h.db.as_ref(),
        &FakeClock::new(1_000),
        ports,
        &push(),
    )
    .await
    .expect("admitted");
    assert_eq!(created.runs.len(), 1);
}

// RT-R2-T4: the operator's `author_overlay` lists reach Run creation
// through `AppState::admission_ports`. Built `with_author_overlay(label_keys:
// [team])`, a Run whose overlay sets `metadata.labels.team` is admitted; on
// the shipped default (no label keys) it is refused.
#[tokio::test]
async fn run_creation_on_app_state_reads_the_operators_author_overlay_keys() {
    let governance = prod_permitting_overlays().await;
    let yaml = overlay_ci("{ metadata: { labels: { team: web } } }");
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", yaml.as_str())
        .with_commit("refs/heads/main", SHA);
    let state = |listed: bool| {
        let db = Arc::new(scarab_testkit::InMemoryDb::new());
        let logs = Arc::new(scarab_server::LogService::new(
            Arc::new(scarab_testkit::InMemoryObjectStore::new()),
            db.clone(),
        ));
        let st = scarab_server::AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_environment_governance(governance.clone());
        if listed {
            st.with_author_overlay(scarab_executor_k8s::AuthorOverlayConfig {
                label_keys: vec!["team".into()],
                ..Default::default()
            })
        } else {
            st
        }
    };

    let st = state(true);
    let created = trigger_runs_from_event(
        &forge,
        st.db.as_ref(),
        &FakeClock::new(1_000),
        st.admission_ports(),
        &push(),
    )
    .await
    .expect("a listed label key is admitted");
    assert_eq!(created.runs.len(), 1);

    let st = state(false);
    let err = trigger_runs_from_event(
        &forge,
        st.db.as_ref(),
        &FakeClock::new(1_000),
        st.admission_ports(),
        &push(),
    )
    .await
    .expect_err("the shipped default lists no label key");
    assert!(
        err.to_string().contains("author_overlay.label_keys"),
        "{err}"
    );
}

// RT-R2-5: the policy test rendered a Step's image with no launch
// parameters. A manual Run whose image takes an input default
// (`reg/x@${{ inputs.digest }}`) is admitted by Run creation, which resolves
// the default, against a grant naming that digest; the policy test admitted
// the unrendered template and answered deny. It now renders with the
// declared defaults and answers allow.
#[tokio::test]
async fn policy_test_renders_a_manual_runs_image_with_its_input_defaults() {
    use scarab_project::{
        EnvironmentGovernanceStore, EnvironmentProjectEligibility,
        EnvironmentProjectPrivilegeGrant, ImageGrant,
    };
    let digest = format!("sha256:{}", "a".repeat(64));
    let governance = Arc::new(
        scarab_testkit::InMemoryEnvironmentGovernance::new().with_environment(
            "acme",
            "prod",
            EnvironmentProjectEligibility::AllProjects,
        ),
    );
    governance
        .put_environment_project_privilege_grant(
            &EnvironmentProjectPrivilegeGrant {
                org: "acme".into(),
                environment: "prod".into(),
                project: "web".into(),
                privileged_images: vec![ImageGrant {
                    image_digest: digest.clone(),
                    privileged: true,
                    capabilities: vec![],
                }],
                permit_k8s_overlay: false,
            },
            "amy",
            1,
        )
        .await
        .unwrap();
    let yaml = format!(
        "on: {{ manual: {{}} }}\nenvironment: prod\ninterface:\n  inputs:\n    - {{ name: digest, \
         type: string, required: false, default: \"{digest}\" }}\nsteps:\n  - id: b\n    \
         image: \"reg/x@${{{{ inputs.digest }}}}\"\n    command: [\"true\"]\n    \
         security: {{ privileged: true }}\n"
    );

    // Run creation: a dispatch with no values resolves the default.
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", yaml.as_str())
        .with_commit("refs/heads/main", SHA);
    let g = governance.clone();
    let h = harness_with(move |st| {
        st.with_policies(Arc::new(InMemoryPolicies::new()))
            .with_environment_governance(g)
    });
    scarab_server::dispatch_run(
        &forge,
        h.db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::governance(Some(governance.as_ref())),
        "amy".into(),
        RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        "refs/heads/main".into(),
        "ci".into(),
        Default::default(),
        scarab_server::DispatchKind::Manual,
        None,
    )
    .await
    .expect("Run creation admits the rendered, granted image");

    // The policy test answers as Run creation does.
    let (s, _) = admin(&h).await;
    let trigger = json!({ "kind": "manual", "ref": "refs/heads/main", "sha": SHA });
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(form(&yaml, trigger)),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert!(b.get("refusal").is_none(), "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "allow", "{b}");
}
