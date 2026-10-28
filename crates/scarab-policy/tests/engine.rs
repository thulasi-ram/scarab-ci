//! The policy engine end to end inside the crate: an authored Policy is
//! parsed, compiled for its level, and evaluated over a `run.create` input
//! given as JSON, the form a PolicyDecision stores.

use scarab_policy::check::check;
use scarab_policy::evaluate::{
    evaluate, policy_set_digest, DecisionOutcome, MatchContext, PolicyDecision, PolicyOutcome,
};
use scarab_policy::ir::{compile, Level, PolicyIr, Predicate};
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::run_create::{self, TriggerKind};
use scarab_policy::schema::Type;
use serde_json::{json, Value};

fn policy(v: Value) -> Policy {
    Policy::from_json(v).expect("a valid Policy")
}

fn org_ir(v: Value) -> PolicyIr {
    let p = policy(v);
    compile(&p, Level::Org)
        .unwrap_or_else(|d| panic!("compiles: {d:?}"))
        .with_identity(format!("pol-{}", p.name), 1)
}

fn errors(v: Value, level: Level) -> Vec<String> {
    match compile(&policy(v), level) {
        Ok(_) => panic!("expected compile errors"),
        Err(ds) => ds.iter().map(|d| d.to_string()).collect(),
    }
}

fn assert_error(v: Value, level: Level, expected: &str) {
    let errs = errors(v, level);
    assert!(
        errs.iter().any(|e| e.contains(expected)),
        "expected an error containing {expected:?}, got {errs:?}"
    );
}

fn deny_rule(name: &str, when: &str) -> Value {
    json!({
        "name": name,
        "checkpoint": "run.create",
        "rules": [{ "when": when, "effect": "deny", "message": format!("{name} denies") }]
    })
}

fn origin(kind: &str, name: &str, source: Option<&str>) -> Value {
    let mut o = json!({ "kind": kind, "name": name, "content_hash": "ab".repeat(32) });
    if let Some(s) = source {
        o["source"] = json!(s);
    }
    o
}

fn step(id: &str, origins: Vec<Value>) -> Value {
    json!({
        "id": id, "image": "rust:1.84", "grants": [], "k8s_overlay": false, "secrets": [],
        "placement_profiles": [], "deploy": false, "uses_environment_authority": false,
        "origin": origins, "upstream_gates": []
    })
}

fn gate(id: &str, origins: Vec<Value>) -> Value {
    json!({ "id": id, "kind": "plugin", "origin": origins, "dependents": [] })
}

/// A push to main of `payments-api`: a test Step from the Pipeline file, a
/// deploy Step from an acme library, and an argocd plugin gate.
fn input() -> Value {
    json!({
        "org": { "name": "acme" },
        "project": { "name": "payments-api" },
        "trigger": { "kind": "push", "ref": "refs/heads/main", "sha": "abc", "fork": false },
        "pipeline": { "name": "ci", "environment": "production", "deploy": false },
        "steps": [
            step("test", vec![]),
            step("deploy", vec![origin("library", "deploy-helm", Some("github.com/acme/libs//helm.yaml"))]),
            step("lint", vec![origin("file", ".scarab/lib/lint.yaml", None)]),
        ],
        "gates": [gate("sync", vec![origin("plugin", "argocd", None)])],
        "time": "2026-10-29T10:00:00Z"
    })
}

fn ctx(project: &str, environment: Option<&str>) -> MatchContext {
    MatchContext {
        project: project.to_string(),
        environment: environment.map(str::to_string),
        trigger_kind: TriggerKind::Push,
    }
}

fn run(policies: &[PolicyIr], input: &Value, ctx: &MatchContext) -> PolicyDecision {
    evaluate(
        Checkpoint::RunCreate,
        run_create::SCHEMA_VERSION,
        input,
        ctx,
        policies,
    )
}

fn outcome(d: &PolicyDecision, i: usize) -> &PolicyOutcome {
    &d.results[i].outcome
}

// --- compile errors -------------------------------------------------------

#[test]
fn project_level_accepts_typed_settings_only() {
    assert_error(
        deny_rule("p", "pipeline.deploy"),
        Level::Project,
        "Project admins write typed settings only",
    );
    let setting = json!({ "name": "libs", "checkpoint": "run.create",
        "setting": { "libraries_blocked": { "sources": ["github.com/evil/*"] } } });
    assert!(compile(&policy(setting), Level::Project).is_ok());
}

#[test]
fn an_effect_the_checkpoint_does_not_allow_is_refused() {
    let p = json!({ "name": "p", "checkpoint": "run.create", "rules": [
        { "when": "pipeline.deploy", "effect": { "require_approval": { "principals": ["user:a"], "count": 1 } }, "message": "m" },
        { "when": "pipeline.deploy", "effect": "require_step_up", "message": "m" },
    ]});
    let errs = errors(p, Level::Org);
    assert!(errs[0].contains("rules[0].effect: `require_approval` is not allowed at `run.create`"));
    assert!(errs[1].contains("rules[1].effect: `require_step_up` is not allowed"));
}

#[test]
fn unknown_and_unbuilt_checkpoints() {
    let unknown = Policy::from_json(json!({ "name": "p", "checkpoint": "run.start", "rules": [] }));
    assert!(unknown.unwrap_err().contains("unknown variant `run.start`"));
    for cp in ["step.admit", "gate.resolve"] {
        let p = json!({ "name": "p", "checkpoint": cp,
            "rules": [{ "when": "true", "effect": "deny", "message": "m" }] });
        assert_error(
            p,
            Level::Org,
            &format!("checkpoint `{cp}` is not yet supported"),
        );
    }
}

#[test]
fn malformed_policies_are_refused_with_every_finding() {
    let both = Policy::from_json(
        json!({ "name": "p", "checkpoint": "run.create", "rules": [],
        "setting": { "plugins_allowed": { "installations": [] } } }),
    );
    assert!(both.unwrap_err().contains("not both"));
    let misspelt = Policy::from_json(json!({ "name": "p", "checkpoint": "run.create",
        "match": { "project": ["x"] }, "rules": [] }));
    assert!(misspelt.unwrap_err().contains("unknown field `project`"));
    let wrong_kind = Policy::from_json(json!({ "kind": "pipeline", "name": "p",
        "checkpoint": "run.create", "rules": [] }));
    assert!(wrong_kind
        .unwrap_err()
        .contains("a Policy's kind is `policy`"));

    let p = json!({ "name": "bad name!", "checkpoint": "run.create", "rules": [
        { "when": "pipline.deploy", "effect": "deny", "message": "" },
    ]});
    let errs = errors(p, Level::Org);
    assert!(
        errs.iter().any(|e| e.starts_with("name: `bad name!`")),
        "{errs:?}"
    );
    assert!(errs
        .iter()
        .any(|e| e.starts_with("rules[0].message: a message cannot be empty")));
    assert!(errs
        .iter()
        .any(|e| e.starts_with("rules[0].when: 1:1: `pipline`: no variable")));

    let pattern = json!({ "name": "p", "checkpoint": "run.create",
        "setting": { "libraries_allowed": { "sources": ["github.com/*/libs"] } } });
    assert_error(pattern, Level::Org, "a prefix ending in `*`");
}

#[test]
fn deep_nesting_is_a_diagnostic_not_an_abort() {
    // A debug build of cel 0.15's parser overflows a 2 MiB stack at about 12
    // nested parentheses; this test runs on such a thread.
    let deep = format!("{}pipeline.deploy{}", "(".repeat(500), ")".repeat(500));
    assert_error(
        deny_rule("p", &deep),
        Level::Org,
        "brackets nest 500 deep; the limit is 64",
    );
    let lists = format!("{}1{} == 1", "[".repeat(65), "]".repeat(65));
    assert!(
        check(&run_create::schema(), &lists, &Type::Bool).unwrap_err()[0]
            .message
            .contains("brackets nest 65 deep")
    );

    // Within the limit, the rule parses, checks and evaluates.
    let ok = format!("{}pipeline.deploy{}", "(".repeat(40), ")".repeat(40));
    let ir = org_ir(deny_rule("p", &ok));
    let d = run(&[ir], &input(), &ctx("payments-api", Some("production")));
    assert_eq!(d.outcome, DecisionOutcome::Allow);
}

// --- match ----------------------------------------------------------------

#[test]
fn match_globs_exceptions_environments_and_trigger_kinds() {
    let p = |m: Value| {
        let mut v = deny_rule("p", "true");
        v["match"] = m;
        org_ir(v)
    };
    let cases = [
        (
            json!({ "projects": ["payments-*"] }),
            ctx("payments-api", None),
            true,
        ),
        (
            json!({ "projects": ["payments-*"] }),
            ctx("billing", None),
            false,
        ),
        (
            json!({ "projects": ["payments-*"], "except_projects": ["payments-sandbox"] }),
            ctx("payments-sandbox", None),
            false,
        ),
        (
            json!({ "environments": ["prod*"] }),
            ctx("x", Some("production")),
            true,
        ),
        (json!({ "environments": ["prod*"] }), ctx("x", None), false),
        (
            json!({ "environments": ["prod*"] }),
            ctx("x", Some("staging")),
            false,
        ),
        (
            json!({ "trigger_kinds": ["manual"] }),
            ctx("x", None),
            false,
        ),
        (
            json!({ "trigger_kinds": ["push", "manual"] }),
            ctx("x", None),
            true,
        ),
        (json!({}), ctx("anything", None), true),
    ];
    for (m, c, applies) in cases {
        let d = run(&[p(m.clone())], &input(), &c);
        let want = if applies {
            PolicyOutcome::Denied {
                messages: vec!["p denies".into()],
            }
        } else {
            PolicyOutcome::NotMatched
        };
        assert_eq!(outcome(&d, 0), &want, "match {m} in {c:?}");
        assert_eq!(d.is_denied(), applies);
    }
}

// --- combination, audit, fail closed --------------------------------------

#[test]
fn enforced_denies_combine_and_audit_only_records() {
    let a = org_ir(deny_rule("a", r#"pipeline.environment == "production""#));
    let b = org_ir(deny_rule(
        "b",
        "steps.exists(s, s.origin.exists(o, o.kind == \"file\"))",
    ));
    let mut audit = deny_rule("c", "true");
    audit["mode"] = json!("audit");
    let c = org_ir(audit);
    let passes = org_ir(deny_rule("d", "pipeline.deploy"));

    let d = run(
        &[a, b, c, passes],
        &input(),
        &ctx("payments-api", Some("production")),
    );
    assert_eq!(
        d.outcome,
        DecisionOutcome::Deny {
            messages: vec!["a denies".into(), "b denies".into()]
        }
    );
    assert_eq!(
        outcome(&d, 2),
        &PolicyOutcome::WouldDeny {
            messages: vec!["c denies".into()]
        }
    );
    assert_eq!(outcome(&d, 3), &PolicyOutcome::Passed);

    // Audit alone never denies.
    let mut audit = deny_rule("c", "true");
    audit["mode"] = json!("audit");
    let d = run(&[org_ir(audit)], &input(), &ctx("x", None));
    assert_eq!(d.outcome, DecisionOutcome::Allow);
}

#[test]
fn an_evaluation_error_denies_when_enforced() {
    // `trigger.reason` is optional: reading it unguarded warns when saved and
    // fails closed when it is absent.
    let ir = org_ir(deny_rule("r", r#"trigger.reason.startsWith("x")"#));
    assert_eq!(ir.warnings().count(), 1);
    let d = run(std::slice::from_ref(&ir), &input(), &ctx("x", None));
    let PolicyOutcome::Error { message } = outcome(&d, 0) else {
        panic!("expected an error, got {:?}", outcome(&d, 0));
    };
    assert!(message.starts_with("r denies (the rule could not be evaluated, so it denies:"));
    assert!(d.is_denied());

    // An input that does not fit the schema denies every CEL rule.
    let mut bad = input();
    bad["steps"] = json!("not a list");
    let d = run(
        &[org_ir(deny_rule("s", "pipeline.deploy"))],
        &bad,
        &ctx("x", None),
    );
    assert!(
        d.messages()[0].contains("input does not match the schema"),
        "{d:?}"
    );

    // The same in audit mode is recorded, not enforced.
    let mut audit = deny_rule("r", r#"trigger.reason.startsWith("x")"#);
    audit["mode"] = json!("audit");
    let d = run(&[org_ir(audit)], &input(), &ctx("x", None));
    assert!(matches!(outcome(&d, 0), PolicyOutcome::Error { .. }));
    assert_eq!(d.outcome, DecisionOutcome::Allow);

    // A schema version the engine does not know fails closed too.
    let d = evaluate(
        Checkpoint::RunCreate,
        99,
        &input(),
        &ctx("x", None),
        &[org_ir(deny_rule("s", "pipeline.deploy"))],
    );
    assert!(d.messages()[0].contains("no input schema for run.create version 99"));
}

// --- digest ---------------------------------------------------------------

#[test]
fn policy_set_digest_is_order_free_and_names_versions() {
    let a = org_ir(deny_rule("a", "pipeline.deploy"));
    let b = org_ir(deny_rule("b", "pipeline.deploy"));
    let d1 = policy_set_digest(Checkpoint::RunCreate, &[a.clone(), b.clone()]);
    assert_eq!(
        d1,
        policy_set_digest(Checkpoint::RunCreate, &[b.clone(), a.clone()])
    );
    assert_eq!(d1.len(), 64);

    let bumped = a.clone().with_identity(&a.policy_id, 2);
    assert_ne!(
        d1,
        policy_set_digest(Checkpoint::RunCreate, &[bumped, b.clone()])
    );

    // A Policy at another checkpoint is not part of this one's set.
    let mut other = b.clone();
    other.checkpoint = Checkpoint::StepAdmit;
    other.policy_id = "pol-other".into();
    assert_eq!(
        d1,
        policy_set_digest(
            Checkpoint::RunCreate,
            &[a.clone(), b.clone(), other.clone()]
        )
    );

    // Matched or not, every Policy at the checkpoint is in the decision's digest.
    let mut unmatched = deny_rule("u", "true");
    unmatched["match"] = json!({ "projects": ["nothing"] });
    let u = org_ir(unmatched);
    let d = run(&[a.clone(), u.clone(), other], &input(), &ctx("x", None));
    assert_eq!(
        d.policy_set_digest,
        policy_set_digest(Checkpoint::RunCreate, &[a, u])
    );
    assert_eq!(d.results.len(), 2);
}

#[test]
fn content_hash_is_canonical() {
    let a = policy(
        json!({ "name": "p", "checkpoint": "run.create", "mode": "enforce",
        "match": { "projects": [] },
        "rules": [{ "message": "m", "effect": "deny", "when": "true" }] }),
    );
    let b = policy(
        json!({ "rules": [{ "when": "true", "effect": "deny", "message": "m" }],
        "checkpoint": "run.create", "name": "p" }),
    );
    assert_eq!(a, b);
    assert_eq!(a.content_hash(), b.content_hash());
    let mut c = b.clone();
    c.mode = scarab_policy::policy::Mode::Audit;
    assert_ne!(a.content_hash(), c.content_hash());

    // Round trip through the wire form.
    let back: Policy = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
    assert_eq!(back, a);
}

// --- typed settings -------------------------------------------------------

fn setting(name: &str, s: Value) -> PolicyIr {
    let p = policy(json!({ "name": name, "checkpoint": "run.create", "setting": s }));
    compile(&p, Level::Project)
        .expect("compiles")
        .with_identity(name, 1)
}

#[test]
fn libraries_allowed() {
    let ok = setting(
        "allowed",
        json!({ "libraries_allowed": { "sources": ["github.com/acme/*", ".scarab/lib/lint.yaml"] } }),
    );
    let d = run(&[ok], &input(), &ctx("x", None));
    assert_eq!(d.outcome, DecisionOutcome::Allow);

    let narrow = setting(
        "allowed",
        json!({ "libraries_allowed": { "sources": ["github.com/acme/other//*"] } }),
    );
    let d = run(&[narrow], &input(), &ctx("x", None));
    assert_eq!(
        d.messages(),
        [
            "Step `deploy` comes from library `github.com/acme/libs//helm.yaml`, which is not in the libraries allowed here.",
            "Step `lint` comes from file `.scarab/lib/lint.yaml`, which is not in the libraries allowed here.",
        ]
    );
    let Predicate::Typed(_) = &compile(
        &policy(json!({ "name": "x", "checkpoint": "run.create",
            "setting": { "libraries_allowed": { "sources": [] } } })),
        Level::Org,
    )
    .unwrap()
    .rules[0]
        .predicate
    else {
        panic!("a typed setting compiles to a typed predicate");
    };
}

#[test]
fn libraries_blocked() {
    let blocked = setting(
        "blocked",
        json!({ "libraries_blocked": { "sources": ["github.com/acme/libs//helm.yaml"] } }),
    );
    let d = run(&[blocked], &input(), &ctx("x", None));
    assert_eq!(
        d.messages(),
        ["Step `deploy` comes from library `github.com/acme/libs//helm.yaml`, which is blocked here."]
    );
    let other = setting(
        "blocked",
        json!({ "libraries_blocked": { "sources": ["github.com/evil/*"] } }),
    );
    assert_eq!(
        run(&[other], &input(), &ctx("x", None)).outcome,
        DecisionOutcome::Allow
    );
}

#[test]
fn plugins_allowed() {
    let listed = setting(
        "plugins",
        json!({ "plugins_allowed": { "installations": ["argocd"] } }),
    );
    assert_eq!(
        run(&[listed], &input(), &ctx("x", None)).outcome,
        DecisionOutcome::Allow
    );
    let unlisted = setting(
        "plugins",
        json!({ "plugins_allowed": { "installations": [] } }),
    );
    let d = run(&[unlisted], &input(), &ctx("x", None));
    assert_eq!(
        d.messages(),
        ["Gate `sync` comes from plugin `argocd`, which is not in the plugins whose gates may run here."]
    );
}

#[test]
fn many_offenders_are_counted_past_ten() {
    let mut big = input();
    big["steps"] = Value::Array(
        (0..25)
            .map(|i| step(&format!("s{i}"), vec![origin("file", "x.yaml", None)]))
            .collect(),
    );
    let blocked = setting(
        "blocked",
        json!({ "libraries_blocked": { "sources": ["x.yaml"] } }),
    );
    let d = run(&[blocked], &big, &ctx("x", None));
    assert_eq!(d.messages().len(), 11);
    assert_eq!(d.messages()[10], "…and 15 more Steps or gates.");
}

// --- decisions and reads --------------------------------------------------

#[test]
fn a_stored_decision_round_trips_and_re_evaluates_the_same() {
    let ir = org_ir(deny_rule(
        "a",
        r#"has(trigger.ref) && trigger.ref == "refs/heads/main""#,
    ));
    let d = run(std::slice::from_ref(&ir), &input(), &ctx("x", None));
    let stored: PolicyDecision = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
    assert_eq!(stored, d);
    let again = run(&[ir], &stored.input, &ctx("x", None));
    assert_eq!(again, d);
    assert_eq!(
        serde_json::to_value(&d.outcome).unwrap(),
        json!({ "kind": "deny", "messages": ["a denies"] })
    );
}

#[test]
fn a_rule_records_what_it_reads() {
    let ir = org_ir(deny_rule(
        "a",
        r#"has(trigger.ref) && steps.exists(s, s.deploy) && project.name == "x""#,
    ));
    let Predicate::Cel(c) = &ir.rules[0].predicate else {
        panic!()
    };
    assert_eq!(
        c.reads,
        ["project", "project.name", "steps", "trigger", "trigger.ref"]
    );
}

#[test]
fn an_input_over_a_cap_fails_closed_for_typed_settings_too() {
    let mut big = input();
    big["steps"] = Value::Array(
        (0..=run_create::MAX_STEPS)
            .map(|i| step(&format!("s{i}"), vec![]))
            .collect(),
    );
    let blocked = setting(
        "blocked",
        json!({ "libraries_blocked": { "sources": ["x.yaml"] } }),
    );
    let d = run(&[blocked], &big, &ctx("x", None));
    assert!(
        d.messages()[0].contains("steps: 513 items, above the cap of 512"),
        "{:?}",
        d.messages()
    );
    // No Policy applies: nothing is bound, nothing is refused.
    assert_eq!(
        run(&[], &big, &ctx("x", None)).outcome,
        DecisionOutcome::Allow
    );
}
