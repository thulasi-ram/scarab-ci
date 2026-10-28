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
steps:
  - { id: helm, image: busybox, command: ["true"] }
"#;

const CI_YAML: &str = r#"
on:
  push: {}
  manual: {}
steps:
  - { id: test, image: busybox, command: ["true"] }
  - { id: ship, invoke: .scarab/lib/deploy.yaml, needs: [test] }
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
    assert!(b.as_str().unwrap().contains("not yet supported"), "{b}");

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
        repo: RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "ship it".into(),
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
  - { id: ship, invoke: .scarab/lib/deploy.yaml, needs: [test] }
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
        repo,
        number: 7,
        head: SHA.into(),
        title: "fix".into(),
        base: "main".into(),
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
