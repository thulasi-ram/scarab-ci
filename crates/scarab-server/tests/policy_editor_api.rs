//! The policy editor's endpoints, through the real router with an in-memory
//! PolicyStore:
//!
//! - `POST /v1/orgs/{org}/policies/check`: a draft's errors and `has()`
//!   warnings at their rule, line and column, and what each rule reads;
//!   nothing is saved;
//! - `GET /v1/policy-checkpoints`: each checkpoint's input schema, effects
//!   and `match` keys;
//! - `POST /v1/orgs/{org}/policy-test` at `step.admit` and `gate.resolve`;
//! - `POST /v1/orgs/{org}/policy-test/replay`: the recorded decisions whose
//!   outcome a draft changes, skipping those with no recorded input;
//! - the decision log's `checkpoint`, `policy_id` and `since` filters, and
//!   `GET …/policy-decisions/counts`.

mod step_up_harness;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{json, Value};

use scarab_identity::{Role, Scope};
use scarab_policy::evaluate::{DecisionOutcome, PolicyDecision, PolicyOutcome, PolicyResult};
use scarab_policy::gate_resolve::{self as gr, GateResolveInput};
use scarab_policy::ir::Level;
use scarab_policy::policy::{Checkpoint, Mode};
use scarab_policy::run_create::{GateKind, Org, Project};
use scarab_policy::schema::{Capped, Timestamp};
use scarab_policy::step_admit::{self as sa, StepAdmitInput};
use scarab_policy::store::{DecisionRecord, PolicyStore};
use scarab_testkit::InMemoryPolicies;

use step_up_harness::*;

fn with_policies() -> (Harness, Arc<InMemoryPolicies>) {
    let policies = Arc::new(InMemoryPolicies::new());
    let p = policies.clone();
    (harness_with(move |st| st.with_policies(p)), policies)
}

/// oscar administers acme; mia is a member.
async fn sessions(h: &Harness) -> (Sess, Sess) {
    h.grant("oscar", Scope::Org("acme".into()), Role::Admin)
        .await;
    h.grant("mia", Scope::Org("acme".into()), Role::Member)
        .await;
    (
        h.browser_session(scoped("oscar")).await,
        h.browser_session(scoped("mia")).await,
    )
}

fn step_admit_input(image: &str) -> Value {
    serde_json::to_value(StepAdmitInput {
        org: Org {
            name: "acme".into(),
        },
        project: Project { name: "web".into() },
        step: sa::AdmittedStep {
            id: "deploy".into(),
            image: image.into(),
            deploy: false,
            secrets: Capped::new(Vec::new()).unwrap(),
            env_secrets: None,
            forge_token: None,
            sandbox: None,
            registry_auth: None,
            uses_environment_authority: false,
            origin: Capped::new(Vec::new()).unwrap(),
        },
        attempt: 1,
        run: sa::AdmittedRun {
            id: "run-1".into(),
            r#ref: None,
            environment: None,
        },
        environment: None,
        approvals: Capped::new(Vec::new()).unwrap(),
        time: Timestamp::from_unix_millis(1_000).unwrap(),
    })
    .unwrap()
}

fn gate_resolve_input(actor: &str) -> Value {
    serde_json::to_value(GateResolveInput {
        org: Org {
            name: "acme".into(),
        },
        project: Project { name: "web".into() },
        gate: gr::GateUnderResolve {
            id: "approve".into(),
            kind: GateKind::Manual,
            origin: Capped::new(Vec::new()).unwrap(),
            environment_approval_gate: false,
            dependents: Capped::new(vec!["ship".to_string()]).unwrap(),
        },
        verdict: gr::Verdict::Release,
        actor: gr::Actor {
            principal: actor.into(),
            principal_id: None,
        },
        approvals: Capped::new(vec![actor.to_string()]).unwrap(),
        approval_ids: None,
        run: gr::GateRun {
            id: "run-1".into(),
            r#ref: None,
            environment: None,
        },
        time: Timestamp::from_unix_millis(1_000).unwrap(),
    })
    .unwrap()
}

fn deny(name: &str, checkpoint: &str, mode: &str, when: &str) -> Value {
    json!({
        "name": name,
        "checkpoint": checkpoint,
        "mode": mode,
        "rules": [{ "when": when, "effect": "deny", "message": format!("{name} denies") }]
    })
}

fn result(policy_id: &str, mode: Mode, outcome: PolicyOutcome) -> PolicyResult {
    PolicyResult {
        policy_id: policy_id.into(),
        version: 1,
        name: policy_id.into(),
        level: Level::Org,
        mode,
        outcome,
        before_approval: None,
    }
}

/// A recorded decision at `checkpoint` with `results`; `input` `None`
/// records a null input, as a Run over an input list cap does.
fn record(
    id: &str,
    at: i64,
    checkpoint: Checkpoint,
    input: Option<Value>,
    results: Vec<PolicyResult>,
) -> DecisionRecord {
    let denied = results
        .iter()
        .any(|r| matches!(r.outcome, PolicyOutcome::Denied { .. }));
    DecisionRecord {
        id: id.into(),
        org: "acme".into(),
        project: Some("web".into()),
        run_id: Some(format!("run-{id}")),
        step_id: (checkpoint == Checkpoint::StepAdmit).then(|| "deploy".to_string()),
        attempt: (checkpoint == Checkpoint::StepAdmit).then_some(1),
        decision: PolicyDecision {
            checkpoint,
            schema_version: 1,
            input: input.unwrap_or(Value::Null),
            policy_set_digest: "0".repeat(64),
            results,
            outcome: if denied {
                DecisionOutcome::Deny {
                    messages: vec!["no".into()],
                }
            } else {
                DecisionOutcome::Allow
            },
        },
        at,
    }
}

#[tokio::test]
async fn check_answers_errors_and_warnings_at_their_rule_and_saves_nothing() {
    let (h, policies) = with_policies();
    let (oscar, mia) = sessions(&h).await;
    let check = |policy: Value, s: Sess| {
        let h = &h;
        async move {
            h.cookie(
                "POST",
                "/v1/orgs/acme/policies/check",
                Some(json!({ "policy": policy })),
                &s,
            )
            .await
        }
    };

    // Rule 0 reads a field run.create does not have; rule 1 reads an
    // optional field without `has()`.
    let draft = json!({
        "name": "two-rules",
        "checkpoint": "run.create",
        "mode": "audit",
        "rules": [
            { "when": "true &&\n  pipeline.nope", "effect": "deny", "message": "a" },
            { "when": "trigger.ref == 'refs/heads/dev'", "effect": "deny", "message": "b" }
        ]
    });
    let (st, b) = check(draft.clone(), oscar.clone()).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], false, "{b}");
    let diags = b["diagnostics"].as_array().unwrap();
    let error = diags
        .iter()
        .find(|d| d["severity"] == "error")
        .expect("an error");
    assert_eq!(error["at"], "rules[0].when", "{b}");
    assert_eq!(
        error["line"], 2,
        "the error is on the rule's second line: {b}"
    );
    assert!(error["column"].as_u64().unwrap() >= 1, "{b}");
    let warning = diags
        .iter()
        .find(|d| d["severity"] == "warning")
        .expect("the has() warning of the rule that compiled");
    assert_eq!(warning["at"], "rules[1].when", "{b}");
    // Only rule 1 compiled; it reads the trigger's ref.
    let rules = b["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1, "{b}");
    assert_eq!(rules[0]["index"], 1);
    assert!(rules[0]["reads"]
        .as_array()
        .unwrap()
        .contains(&json!("trigger.ref")));

    // A Policy that compiles is ok, with its warning still reported.
    let mut fine = draft.clone();
    fine["rules"].as_array_mut().unwrap().remove(0);
    let (st, b) = check(fine, oscar.clone()).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], true, "{b}");
    assert_eq!(b["diagnostics"][0]["severity"], "warning", "{b}");

    // A body that is not a Policy is one error at no path.
    let (st, b) = check(json!({ "name": "x" }), oscar.clone()).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], false);
    assert_eq!(b["diagnostics"].as_array().unwrap().len(), 1, "{b}");
    assert_eq!(b["diagnostics"][0]["at"], "");

    // YAML, as pasted: parsed, checked, and answered as JSON with `kind`
    // dropped.
    let yaml = "kind: policy\nname: no-forks\ncheckpoint: run.create\nrules:\n  - when: trigger.fork\n    effect: deny\n    message: No forks.\n";
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies/check",
            Some(json!({ "yaml": yaml })),
            &oscar,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], true, "{b}");
    assert_eq!(b["policy"]["name"], "no-forks");
    assert_eq!(b["policy"]["rules"][0]["when"], "trigger.fork");
    assert!(b["policy"].get("kind").is_none(), "{b}");
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies/check",
            Some(json!({ "yaml": "name: [unclosed" })),
            &oscar,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], false);
    assert!(b["policy"].is_null(), "{b}");
    let (st, _) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies/check",
            Some(json!({ "yaml": yaml, "policy": {} })),
            &oscar,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // A member may not run the checker; nothing was saved either way.
    let (st, _) = check(draft, mia).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(policies
        .list(&scarab_policy::store::PolicyScope::Org { org: "acme".into() })
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn checkpoints_serve_each_schema_with_its_effects_and_match_keys() {
    let (h, _) = with_policies();
    let (_, mia) = sessions(&h).await;
    let (st, b) = h.cookie("GET", "/v1/policy-checkpoints", None, &mia).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let all = b.as_array().unwrap();
    let names: Vec<&str> = all
        .iter()
        .map(|c| c["checkpoint"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["run.create", "step.admit", "gate.resolve"]);
    for c in all {
        assert_eq!(c["effects"], json!(["deny"]), "{c}");
        assert_eq!(c["schema_version"], 1);
    }

    let run_create = &all[0];
    assert_eq!(
        run_create["match_keys"],
        json!([
            "projects",
            "except_projects",
            "environments",
            "trigger_kinds"
        ])
    );
    assert!(run_create["trigger_kinds"]
        .as_array()
        .unwrap()
        .contains(&json!("push")));
    let schema = &run_create["schema"];
    let trigger = schema["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "trigger")
        .expect("the trigger variable");
    assert_eq!(trigger["ty"], json!({ "kind": "struct", "of": "Trigger" }));
    let trigger_ref = schema["structs"]["Trigger"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "ref")
        .expect("trigger.ref");
    assert_eq!(trigger_ref["optional"], true);
    assert!(
        trigger_ref["doc"]
            .as_str()
            .unwrap()
            .starts_with("The symbolic ref"),
        "{trigger_ref}"
    );
    let steps = schema["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "steps")
        .expect("the steps variable");
    assert_eq!(steps["ty"]["kind"], "list");
    assert_eq!(steps["ty"]["of"]["cap"], 512);

    // trigger_kinds is matched at run.create only.
    for c in &all[1..] {
        assert_eq!(
            c["match_keys"],
            json!(["projects", "except_projects", "environments"])
        );
        assert_eq!(c["trigger_kinds"], json!([]));
    }
    assert!(all[1]["schema"]["variables"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["name"] == "step"));
}

#[tokio::test]
async fn policy_test_evaluates_step_admit_and_gate_resolve_inputs() {
    let (h, policies) = with_policies();
    let (oscar, _) = sessions(&h).await;
    let test = |body: Value| {
        let h = &h;
        let s = &oscar;
        async move {
            h.cookie("POST", "/v1/orgs/acme/policy-test", Some(body), s)
                .await
        }
    };

    // A step.admit draft over a supplied input: the input's checkpoint is the
    // draft's.
    let no_alpine = deny(
        "no-alpine",
        "step.admit",
        "enforce",
        "step.image == 'alpine'",
    );
    let (st, b) = test(json!({ "policy": no_alpine, "input": step_admit_input("alpine") })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["checkpoint"], "step.admit", "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
    let (_, b) = test(json!({ "policy": no_alpine, "input": step_admit_input("busybox") })).await;
    assert_eq!(b["decision"]["outcome"]["kind"], "allow", "{b}");

    // gate.resolve: separation of duties over a supplied input.
    let not_amy = deny(
        "not-amy",
        "gate.resolve",
        "enforce",
        "actor.principal == 'amy'",
    );
    let (st, b) = test(json!({
        "policy": not_amy, "input": gate_resolve_input("amy"), "checkpoint": "gate.resolve"
    }))
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
    assert_eq!(b["decision"]["results"][0]["name"], "not-amy");

    // A Policy at another checkpoint than the input is a 400.
    let (st, b) = test(json!({
        "policy": no_alpine, "input": step_admit_input("alpine"), "checkpoint": "run.create"
    }))
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    let (st, _) = test(json!({
        "policy": no_alpine, "input": step_admit_input("alpine"), "checkpoint": "step.nope"
    }))
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // A recorded step.admit decision is evaluated again at its checkpoint.
    let recorded = record(
        "s1",
        1_000,
        Checkpoint::StepAdmit,
        Some(step_admit_input("alpine")),
        Vec::new(),
    );
    policies.record_decision(&recorded).await.unwrap();
    let (st, b) = test(json!({ "policy": no_alpine, "decision_id": "s1" })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
    assert_eq!(b["decision"]["input"], recorded.decision.input);
    // The stored Policies of the Project at step.admit: none yet, so allow.
    let (st, b) = test(json!({ "decision_id": "s1" })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "allow", "{b}");
}

#[tokio::test]
async fn replay_lists_the_decisions_a_draft_changes_and_skips_ones_without_input() {
    let (h, policies) = with_policies();
    let (oscar, mia) = sessions(&h).await;

    // The stored Policy being edited denies busybox at step.admit.
    let (st, stored) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies",
            Some(json!({ "policy": deny("images", "step.admit", "enforce", "step.image == 'busybox'") })),
            &oscar,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{stored}");
    let id = stored["id"].as_str().unwrap().to_string();
    let passed = || result(&id, Mode::Enforce, PolicyOutcome::Passed);
    let denied = || {
        result(
            &id,
            Mode::Enforce,
            PolicyOutcome::Denied {
                messages: vec!["images denies".into()],
            },
        )
    };
    for r in [
        record(
            "a",
            1_000,
            Checkpoint::StepAdmit,
            Some(step_admit_input("alpine")),
            vec![passed()],
        ),
        record(
            "b",
            2_000,
            Checkpoint::StepAdmit,
            Some(step_admit_input("busybox")),
            vec![denied()],
        ),
        record("c", 3_000, Checkpoint::StepAdmit, None, vec![passed()]),
        record(
            "d",
            4_000,
            Checkpoint::StepAdmit,
            Some(step_admit_input("debian")),
            vec![passed()],
        ),
        // At another checkpoint: never read.
        record(
            "g",
            5_000,
            Checkpoint::GateResolve,
            Some(gate_resolve_input("amy")),
            Vec::new(),
        ),
    ] {
        policies.record_decision(&r).await.unwrap();
    }
    let replay = |body: Value, s: Sess| {
        let h = &h;
        async move {
            h.cookie("POST", "/v1/orgs/acme/policy-test/replay", Some(body), &s)
                .await
        }
    };

    // A new audit-mode Policy denying alpine: one decision changes, from
    // allow to would_deny.
    let alpine_audit = deny("no-alpine", "step.admit", "audit", "step.image == 'alpine'");
    let (st, b) = replay(json!({ "policy": alpine_audit }), oscar.clone()).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["checkpoint"], "step.admit");
    assert_eq!(b["read"], 4, "{b}");
    assert_eq!(b["evaluated"], 3, "{b}");
    assert_eq!(b["skipped"], 1, "the decision with no input: {b}");
    let changed = b["changed"].as_array().unwrap();
    assert_eq!(changed.len(), 1, "{b}");
    assert_eq!(changed[0]["decision_id"], "a");
    assert_eq!(changed[0]["run_id"], "run-a");
    assert_eq!(changed[0]["step_id"], "deploy");
    assert_eq!(changed[0]["before"], "allow");
    assert_eq!(changed[0]["after"], "would_deny");
    assert_eq!(changed[0]["messages"], json!(["no-alpine denies"]));

    // Editing the stored Policy to deny alpine instead: busybox is allowed
    // now and alpine denied, newest first.
    let mut edit = deny("images", "step.admit", "enforce", "step.image == 'alpine'");
    let (st, b) = replay(
        json!({ "policy": edit.clone(), "policy_id": id }),
        oscar.clone(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let changed: Vec<(String, String, String)> = b["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["decision_id"].as_str().unwrap().into(),
                c["before"].as_str().unwrap().into(),
                c["after"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        changed,
        [
            ("b".to_string(), "deny".to_string(), "allow".to_string()),
            ("a".to_string(), "allow".to_string(), "deny".to_string()),
        ]
    );

    // `limit` reads only the newest decisions at the checkpoint.
    let (_, b) = replay(
        json!({ "policy": edit.clone(), "policy_id": id, "limit": 1 }),
        oscar.clone(),
    )
    .await;
    assert_eq!(b["read"], 1, "{b}");
    assert_eq!(b["changed"], json!([]), "{b}");

    // A draft that does not compile is a 400, a Policy of no such id a 404,
    // and a member may not replay.
    edit["rules"][0]["when"] = json!("step.nope");
    let (st, _) = replay(json!({ "policy": edit }), oscar.clone()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = replay(
        json!({ "policy": alpine_audit, "policy_id": "nope" }),
        oscar.clone(),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = replay(json!({ "policy": alpine_audit }), mia).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_decision_log_filters_and_counts_per_policy() {
    let (h, policies) = with_policies();
    let (_, mia) = sessions(&h).await;
    let deny_p1 = || {
        result(
            "p1",
            Mode::Enforce,
            PolicyOutcome::Denied {
                messages: vec!["x".into()],
            },
        )
    };
    let would_p2 = || {
        result(
            "p2",
            Mode::Audit,
            PolicyOutcome::WouldDeny {
                messages: vec!["y".into()],
            },
        )
    };
    for r in [
        record("r1", 1_000, Checkpoint::RunCreate, None, vec![deny_p1()]),
        record(
            "r2",
            2_000,
            Checkpoint::StepAdmit,
            None,
            vec![
                would_p2(),
                result("p1", Mode::Enforce, PolicyOutcome::NotMatched),
            ],
        ),
        record(
            "r3",
            3_000,
            Checkpoint::RunCreate,
            None,
            vec![
                deny_p1(),
                result(
                    "p2",
                    Mode::Audit,
                    PolicyOutcome::Error {
                        message: "boom".into(),
                    },
                ),
            ],
        ),
    ] {
        policies.record_decision(&r).await.unwrap();
    }
    let ids = |b: &Value| -> Vec<String> {
        b["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap().to_string())
            .collect()
    };
    let get = |q: &str| {
        let h = &h;
        let s = &mia;
        let uri = format!("/v1/orgs/acme/policy-decisions{q}");
        async move { h.cookie("GET", &uri, None, s).await }
    };

    let (st, b) = get("").await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(ids(&b), ["r3", "r2", "r1"]);
    let (_, b) = get("?checkpoint=step.admit").await;
    assert_eq!(ids(&b), ["r2"]);
    let (_, b) = get("?policy_id=p2").await;
    assert_eq!(ids(&b), ["r3", "r2"]);
    let (_, b) = get("?since=2000").await;
    assert_eq!(ids(&b), ["r3", "r2"]);
    let (_, b) = get("?checkpoint=run.create&since=2000&limit=1").await;
    assert_eq!(ids(&b), ["r3"]);
    let (st, _) = get("?checkpoint=nope").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Counts: a failed rule of an audit-mode Policy is a would-deny.
    let (st, b) = h
        .cookie(
            "GET",
            "/v1/orgs/acme/policy-decisions/counts?since=0",
            None,
            &mia,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({
            "since": 0,
            "policies": [
                { "policy_id": "p1", "matched": 2, "denied": 2, "would_deny": 0 },
                { "policy_id": "p2", "matched": 2, "denied": 0, "would_deny": 2 }
            ]
        })
    );
    let (_, b) = h
        .cookie(
            "GET",
            "/v1/orgs/acme/policy-decisions/counts?since=2500",
            None,
            &mia,
        )
        .await;
    assert_eq!(
        b["policies"],
        json!([
            { "policy_id": "p1", "matched": 1, "denied": 1, "would_deny": 0 },
            { "policy_id": "p2", "matched": 1, "denied": 0, "would_deny": 1 }
        ])
    );
}
