//! `matches()` compiles each pattern once per evaluation, not once per call:
//! `cel` 0.15's own `matches` calls `Regex::new` every time, and the checker
//! costs a call at 2 whatever the pattern. The count is process-wide, so this
//! binary holds one test.

use scarab_policy::eval::regex_compiles;
use scarab_policy::evaluate::{evaluate, MatchContext, PolicyOutcome};
use scarab_policy::ir::{compile, Level, Predicate};
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::run_create::{TriggerKind, MAX_ORIGIN, MAX_STEPS, SCHEMA_VERSION};
use serde_json::json;

fn input(name: &str) -> serde_json::Value {
    let origin = json!({ "kind": "file", "name": name, "content_hash": "ab".repeat(32) });
    let step = |i: usize| {
        json!({
            "id": format!("s{i}"), "image": "rust:1.84", "grants": [], "k8s_overlay": false,
            "secrets": [], "placement_profiles": [], "deploy": false,
            "uses_environment_authority": false,
            "origin": vec![origin.clone(); MAX_ORIGIN], "upstream_gates": []
        })
    };
    json!({
        "org": { "name": "acme" },
        "project": { "name": "p" },
        "trigger": { "kind": "push", "ref": "refs/heads/main", "fork": false },
        "pipeline": { "name": "ci", "deploy": false },
        "steps": (0..MAX_STEPS).map(step).collect::<Vec<_>>(),
        "gates": [],
        "time": "2026-10-29T00:00:00Z"
    })
}

#[test]
fn matches_compiles_each_pattern_once_per_evaluation() {
    // Two rules: one pattern twice in the first, another in the second.
    let p = Policy::from_json(json!({
        "name": "names", "checkpoint": "run.create", "mode": "enforce",
        "rules": [
            { "when": r#"steps.exists(s, s.origin.exists(o, o.name.matches("^\\w{20}$")))
                         && steps.all(s, s.origin.all(o, o.name.matches("^\\w{20}$")))"#,
              "effect": "deny", "message": "twenty word characters" },
            { "when": r#"steps.exists(s, s.origin.exists(o, o.name.matches("^lib-")))"#,
              "effect": "deny", "message": "a lib- name" }
        ]
    }))
    .unwrap();
    let ir = compile(&p, Level::Org).expect("the checker accepts it");
    let Predicate::Cel(c) = &ir.rules[0].predicate else {
        panic!("a CEL rule")
    };
    assert!(c.cost <= scarab_policy::check::MAX_COST, "{}", c.cost);
    let ctx = MatchContext {
        project: "p".into(),
        environment: None,
        trigger_kind: TriggerKind::Push,
    };

    // No name matches: every one of the 512 × 8 origins is tested, by both
    // macros of the first rule and by the second.
    let before = regex_compiles();
    let d = evaluate(
        Checkpoint::RunCreate,
        SCHEMA_VERSION,
        &input("x"),
        &ctx,
        std::slice::from_ref(&ir),
    );
    assert_eq!(d.results[0].outcome, PolicyOutcome::Passed);
    assert_eq!(
        regex_compiles() - before,
        2,
        "one compile per distinct pattern"
    );

    // Every name matches the first pattern: the evaluated answer is still
    // the regex's.
    let before = regex_compiles();
    let d = evaluate(
        Checkpoint::RunCreate,
        SCHEMA_VERSION,
        &input("abcdefghij0123456789"),
        &ctx,
        std::slice::from_ref(&ir),
    );
    assert_eq!(
        d.results[0].outcome,
        PolicyOutcome::Denied {
            messages: vec!["twenty word characters".into()]
        }
    );
    assert_eq!(regex_compiles() - before, 2);
}
