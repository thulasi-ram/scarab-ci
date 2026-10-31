//! Red team (ADR-0075 step 2, after the H1-H3 fixes): malformed, empty and
//! enormous Policies and inputs, and attacks on the static cost bound and on
//! the `matches()` rename. Tests that pass are regression tests for an attack
//! that failed; a test that fails names a finding (see redteam.md).

use std::time::{Duration, Instant};

use scarab_policy::check::{check, MAX_COST, MAX_DEPTH, MAX_SOURCE_LEN};
use scarab_policy::evaluate::{evaluate, evaluate_stored, MatchContext, PolicyOutcome};
use scarab_policy::ir::{compile, Level, PolicyIr, Predicate, MAX_LIST, MAX_RULES, MAX_TEXT_LEN};
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::run_create::{
    self, input_from_pipeline, RunCreateFacts, Trigger, TriggerKind, MAX_GATES, MAX_ORIGIN,
    MAX_STEPS, SCHEMA_VERSION,
};
use scarab_policy::schema::{Timestamp, Type};
use serde_json::{json, Value};

fn ctx() -> MatchContext {
    MatchContext {
        project: "p".into(),
        environment: None,
        trigger_kind: TriggerKind::Push,
    }
}

fn rule_policy(when: &str) -> Value {
    json!({
        "name": "rt", "checkpoint": "run.create", "mode": "enforce",
        "rules": [{ "when": when, "effect": "deny", "message": "hit" }]
    })
}

fn compile_json(v: Value) -> Result<PolicyIr, Vec<String>> {
    let p = Policy::from_json(v).map_err(|e| vec![e])?;
    compile(&p, Level::Org).map_err(|ds| ds.iter().map(|d| d.to_string()).collect())
}

fn cost_of(when: &str) -> Result<u64, String> {
    check(&run_create::schema(), when, &Type::Bool)
        .map(|c| c.cost)
        .map_err(|ds| ds[0].message.clone())
}

fn origin(name: &str) -> Value {
    json!({ "kind": "file", "name": name, "content_hash": "ab".repeat(32) })
}

fn step(i: usize, origins: usize, name: &str) -> Value {
    json!({
        "id": format!("s{i}"), "image": "rust:1.84", "grants": [], "k8s_overlay": false,
        "secrets": [], "placement_profiles": [], "deploy": false,
        "uses_environment_authority": false,
        "origin": vec![origin(name); origins], "upstream_gates": []
    })
}

fn input_with(steps: usize, origins: usize, pipeline_name: &str) -> Value {
    json!({
        "org": { "name": "acme" },
        "project": { "name": "p" },
        "trigger": { "kind": "push", "ref": "refs/heads/main", "fork": false },
        "pipeline": { "name": pipeline_name, "deploy": false },
        "steps": (0..steps).map(|i| step(i, origins, "lib.yaml")).collect::<Vec<_>>(),
        "gates": [],
        "time": "2026-10-29T00:00:00Z"
    })
}

fn eval_one(ir: &PolicyIr, input: &Value) -> (PolicyOutcome, Duration) {
    let t = Instant::now();
    let d = evaluate(
        Checkpoint::RunCreate,
        SCHEMA_VERSION,
        input,
        &ctx(),
        std::slice::from_ref(ir),
    );
    (d.results[0].outcome.clone(), t.elapsed())
}

// --- malformed ----------------------------------------------------------------

#[test]
fn redteam_malformed_policy_bodies_are_refused_not_panicked() {
    let bad = [
        // unknown top-level field
        json!({"name":"a","checkpoint":"run.create","rules":[{"when":"true","effect":"deny","message":"m"}],"extra":1}),
        // unknown field in a rule
        json!({"name":"a","checkpoint":"run.create","rules":[{"when":"true","effect":"deny","message":"m","x":1}]}),
        // unknown field in match
        json!({"name":"a","checkpoint":"run.create","match":{"project":["x"]},"rules":[{"when":"true","effect":"deny","message":"m"}]}),
        // wrong types
        json!({"name":"a","checkpoint":"run.create","rules":"true"}),
        json!({"name":1,"checkpoint":"run.create","rules":[]}),
        json!({"name":"a","checkpoint":"run.create","mode":"warn","rules":[{"when":"true","effect":"deny","message":"m"}]}),
        json!({"name":"a","checkpoint":"run.create","rules":[{"when":true,"effect":"deny","message":"m"}]}),
        // effect with an unknown shape
        json!({"name":"a","checkpoint":"run.create","rules":[{"when":"true","effect":{"deny":{}},"message":"m"}]}),
        // both bodies; neither body
        json!({"name":"a","checkpoint":"run.create","rules":[],"setting":{"libraries_blocked":{"sources":["x"]}}}),
        json!({"name":"a","checkpoint":"run.create"}),
        // kind other than policy
        json!({"kind":"Policy","name":"a","checkpoint":"run.create","rules":[{"when":"true","effect":"deny","message":"m"}]}),
        // unknown checkpoint
        json!({"name":"a","checkpoint":"run.delete","rules":[{"when":"true","effect":"deny","message":"m"}]}),
        // a JSON array, a string, null
        json!([1, 2]),
        json!("policy"),
        Value::Null,
    ];
    for v in bad {
        assert!(compile_json(v.clone()).is_err(), "accepted: {v}");
    }
}

#[test]
fn redteam_names_with_nul_unicode_or_spaces_are_refused() {
    for name in ["a\u{0}b", "pölicy", "a b", "", "a/b", "名前", "a\nb"] {
        let mut v = rule_policy("true");
        v["name"] = json!(name);
        let errs = compile_json(v).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("name")),
            "{name:?}: {errs:?}"
        );
    }
    let mut v = rule_policy("true");
    v["name"] = json!("x".repeat(100));
    compile_json(v).expect("100 bytes is the limit");
    let mut v = rule_policy("true");
    v["name"] = json!("x".repeat(101));
    assert!(compile_json(v).is_err());
}

/// A glob with NUL or multibyte characters is accepted and matches by
/// character; a NUL glob matches only a NUL name.
#[test]
fn redteam_globs_with_nul_and_unicode_match_by_character() {
    let mut v = rule_policy("true");
    v["match"] = json!({ "projects": ["pä*", "a\u{0}*"] });
    let ir = compile_json(v).expect("a glob is any non-blank text");
    let mut c = ctx();
    for (project, hit) in [
        ("päy", true),
        ("pay", false),
        ("a\u{0}x", true),
        ("a", false),
    ] {
        c.project = project.into();
        assert_eq!(
            scarab_policy::evaluate::matches(&ir.match_, &c),
            hit,
            "{project:?}"
        );
    }
}

#[test]
fn redteam_a_when_that_is_not_boolean_is_refused() {
    for when in [
        "1",
        "\"deny\"",
        "trigger",
        "steps",
        "trigger.ref",
        "null",
        "size(steps)",
        "true ? 1 : 2",
        "[true]",
        "{\"a\": true}",
    ] {
        let errs = compile_json(rule_policy(when)).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("must return bool") || e.contains("expected")),
            "{when}: {errs:?}"
        );
    }
}

// --- empty --------------------------------------------------------------------

#[test]
fn redteam_empty_rules_empty_lists_and_empty_inputs() {
    // Zero rules is refused.
    let mut v = rule_policy("true");
    v["rules"] = json!([]);
    assert!(compile_json(v)
        .unwrap_err()
        .iter()
        .any(|e| e.contains("at least one rule")));
    // Empty `match` lists match everything; an empty setting list is allowed.
    let mut v = rule_policy("true");
    v["match"] =
        json!({ "projects": [], "except_projects": [], "environments": [], "trigger_kinds": [] });
    let ir = compile_json(v).unwrap();
    assert!(scarab_policy::evaluate::matches(&ir.match_, &ctx()));
    let blank = json!({"name":"a","checkpoint":"run.create","setting":{"libraries_allowed":{"sources":[""]}}});
    assert!(compile_json(blank).is_err(), "a blank pattern is refused");
    let empty_allow = json!({"name":"a","checkpoint":"run.create","setting":{"libraries_allowed":{"sources":[]}}});
    compile_json(empty_allow).expect("an empty allow list is allowed: it allows no library");

    // No Policies: allow, nothing matched.
    let d = evaluate_stored(
        Checkpoint::RunCreate,
        SCHEMA_VERSION,
        Ok(&input_with(0, 0, "ci")),
        None,
        &ctx(),
        &[],
    );
    assert!(!d.is_denied());
    assert!(d.results.is_empty());

    // An empty Pipeline, and one with only gates.
    let ir = scarab_pipeline::compile_yaml("steps:\n  - { id: g, gate: manual }\n")
        .expect("a gate-only Pipeline compiles");
    let input = serde_json::to_value(input_from_pipeline(&ir, facts()).expect("builds")).unwrap();
    assert_eq!(input["steps"], json!([]));
    assert_eq!(input["gates"].as_array().unwrap().len(), 1);
    // `steps.all(...)` over no Steps is true: a rule written as
    // "every Step is X" denies a gate-only Run. Recorded, not a finding.
    let all = compile_json(rule_policy("steps.all(s, s.image != \"\")")).unwrap();
    let (o, _) = eval_one(&all, &input);
    assert!(matches!(o, PolicyOutcome::Denied { .. }));
}

// --- enormous -----------------------------------------------------------------

fn facts() -> RunCreateFacts {
    RunCreateFacts {
        org: "acme".into(),
        project: "p".into(),
        trigger: Trigger {
            kind: TriggerKind::Push,
            r#ref: Some("refs/heads/main".into()),
            sha: None,
            fork: false,
            actor: None,
            principal: None,
            reason: None,
        },
        pipeline_path: ".scarab/ci.yaml".into(),
        environment: None,
        principal: None,
        time: Timestamp::from_unix_millis(0).unwrap(),
    }
}

#[test]
fn redteam_rule_count_list_length_text_and_source_limits_at_the_boundary() {
    let rules = |n: usize| {
        (0..n)
            .map(|_| json!({"when":"true","effect":"deny","message":"m"}))
            .collect::<Vec<_>>()
    };
    let mut v = rule_policy("true");
    v["rules"] = json!(rules(MAX_RULES));
    compile_json(v).expect("32 rules");
    let mut v = rule_policy("true");
    v["rules"] = json!(rules(MAX_RULES + 1));
    assert!(compile_json(v).is_err());

    let list = |n: usize| (0..n).map(|i| format!("p{i}")).collect::<Vec<_>>();
    let mut v = rule_policy("true");
    v["match"] = json!({ "projects": list(MAX_LIST) });
    compile_json(v).expect("256 globs");
    let mut v = rule_policy("true");
    v["match"] = json!({ "projects": list(MAX_LIST + 1) });
    assert!(compile_json(v).is_err());

    let mut v = rule_policy("true");
    v["rules"][0]["message"] = json!("m".repeat(MAX_TEXT_LEN));
    compile_json(v).expect("1024-byte message");
    let mut v = rule_policy("true");
    v["rules"][0]["message"] = json!("é".repeat(MAX_TEXT_LEN / 2 + 1));
    assert!(compile_json(v).is_err(), "bytes, not characters");

    // 4096 bytes accepted, 4097 refused.
    let pad = |n: usize| format!("true{}", " ".repeat(n - 4));
    compile_json(rule_policy(&pad(MAX_SOURCE_LEN))).expect("4096 bytes");
    let errs = compile_json(rule_policy(&pad(MAX_SOURCE_LEN + 1))).unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("at most 4096 bytes")),
        "{errs:?}"
    );

    // 64 brackets accepted, 65 refused, as diagnostics.
    let nest = |n: usize| format!("{}true{}", "(".repeat(n), ")".repeat(n));
    let at = compile_json(rule_policy(&nest(MAX_DEPTH as usize - 2)));
    assert!(at.is_ok(), "{at:?}");
    let errs = compile_json(rule_policy(&nest(MAX_DEPTH as usize + 1))).unwrap_err();
    assert!(errs.iter().any(|e| e.contains("nest")), "{errs:?}");
}

/// 512 Steps and 32 gates fit; 513 Steps or 33 gates are refused by the
/// Run-level cap, as is a 125,000-Step matrix, in time linear in the Run.
#[test]
fn redteam_step_and_gate_caps_at_the_boundary_and_a_huge_matrix() {
    let chain = |n: usize| {
        let mut y = String::from("steps:\n  - { id: s0, image: busybox }\n");
        for i in 1..n {
            y.push_str(&format!(
                "  - {{ id: s{i}, image: busybox, needs: [s{}] }}\n",
                i - 1
            ));
        }
        y
    };
    let ir = scarab_pipeline::compile_yaml(&chain(MAX_STEPS)).unwrap();
    input_from_pipeline(&ir, facts()).expect("512 Steps fit");
    let ir = scarab_pipeline::compile_yaml(&chain(MAX_STEPS + 1)).unwrap();
    assert!(input_from_pipeline(&ir, facts())
        .unwrap_err()
        .to_string()
        .starts_with("the Run has 513 Steps"));

    let gates = |n: usize| {
        let mut y = String::from("steps:\n");
        for i in 0..n {
            y.push_str(&format!("  - {{ id: g{i}, gate: manual }}\n"));
        }
        y.push_str("  - { id: last, image: busybox, needs: [");
        y.push_str(
            &(0..n)
                .map(|i| format!("g{i}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        y.push_str("] }\n");
        y
    };
    let ir = scarab_pipeline::compile_yaml(&gates(MAX_GATES)).unwrap();
    let input =
        serde_json::to_value(input_from_pipeline(&ir, facts()).expect("32 gates fit")).unwrap();
    assert_eq!(
        input["steps"][0]["upstream_gates"]
            .as_array()
            .unwrap()
            .len(),
        MAX_GATES
    );
    let ir = scarab_pipeline::compile_yaml(&gates(MAX_GATES + 1)).unwrap();
    assert!(input_from_pipeline(&ir, facts())
        .unwrap_err()
        .to_string()
        .starts_with("the Run has 33 gates"));

    // A wide fan-in DAG: every Step needs every gate and the previous 8
    // Steps. Bit sets keep it linear.
    let mut y = String::from("steps:\n");
    for i in 0..MAX_GATES {
        y.push_str(&format!("  - {{ id: g{i}, gate: manual }}\n"));
    }
    for i in 0..MAX_STEPS {
        let mut needs: Vec<String> = (0..MAX_GATES).map(|g| format!("g{g}")).collect();
        needs.extend((i.saturating_sub(8)..i).map(|j| format!("s{j}")));
        y.push_str(&format!(
            "  - {{ id: s{i}, image: busybox, needs: [{}] }}\n",
            needs.join(", ")
        ));
    }
    let ir = scarab_pipeline::compile_yaml(&y).unwrap();
    let t = Instant::now();
    let input = serde_json::to_value(input_from_pipeline(&ir, facts()).expect("fits")).unwrap();
    assert!(input["steps"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["upstream_gates"].as_array().unwrap().len() == MAX_GATES));
    eprintln!("fan-in 512x32 built in {:?}", t.elapsed());
    assert!(t.elapsed() < Duration::from_secs(5));
}

// --- H1: the cost bound -------------------------------------------------------

/// Shapes that could carry a list's cap away or multiply without the bound
/// seeing it: list and map literals of lists, a ternary between an empty
/// list and the Steps, `in` inside nested macros, an empty literal whose
/// cap is 0. Each is refused or has a sound bound.
#[test]
fn redteam_cost_bound_survives_literal_wrappers_and_zero_caps() {
    let over = [
        "[steps, steps].exists(l, l.exists(a, l.exists(b, a.id == b.id)))",
        "{\"k\": steps}.k.exists(a, steps.exists(b, a.id == b.id))",
        "(true ? [] : steps).exists(a, steps.exists(b, a.id == b.id))",
        // (`[] == []` itself is refused now: comparing lists, finding F1.)
        "(size([]) == 0 ? steps : []).exists(a, steps.exists(b, a.id == b.id))",
        "steps.exists(a, a.id in [\"x\"] && steps.exists(b, b.id == a.id))",
        "steps.exists(a, steps.exists_one(b, b.id == a.id))",
        "steps.all(a, steps.all(b, a.id != b.id))",
        "gates.exists(g, g.dependents.exists(d, steps.exists(s, s.id == d)))",
    ];
    for w in over {
        let r = cost_of(w);
        assert!(
            r.as_ref().is_err_and(|m| m.contains("worst-case cost")),
            "{w}: {r:?}"
        );
    }
    // An empty list literal has cap 0: its predicate costs nothing, and it
    // also never runs.
    let c = cost_of("[].exists(x, steps.exists(a, steps.exists(b, a.id == b.id)))");
    assert!(
        c.is_err() || c.unwrap() <= 3,
        "the inner nesting still counts or is free"
    );
    // `in` over a map costs 1 per lookup: a hash lookup.
    let c = cost_of("steps.exists(s, s.id in {\"a\": 1, \"b\": 2})").unwrap();
    assert!(c <= MAX_COST);
}

/// F1: `==` between two lists was costed as one node while the evaluator
/// compares them element by element, deep into every Step's origins (7.2 s
/// for one rule of 40 comparisons in release). Comparing lists, maps or
/// structs is refused, with `==`, `!=` and `in`; scalar, enum, timestamp and
/// duration comparisons are kept.
#[test]
fn redteam_deep_list_equality_is_refused() {
    let inner = vec!["steps == steps"; 10].join(" && ");
    let refused = [
        format!("steps.exists(s, {inner} && s.id == \"none\")"),
        "steps == steps".to_string(),
        "steps != []".to_string(),
        "steps.exists(s, s.origin != s.origin)".to_string(),
        "steps.exists(s, s == s)".to_string(),
        "trigger == trigger".to_string(),
        "gates.exists(g, g.dependents == g.dependents)".to_string(),
        "{\"a\": 1} == {\"a\": 1}".to_string(),
        "steps.exists(s, s in steps)".to_string(),
        "[1] in [[1]]".to_string(),
    ];
    for when in &refused {
        let r = cost_of(when);
        assert!(
            r.as_ref().is_err_and(
                |m| m.starts_with("comparing ") && m.contains("is not supported in policies")
            ),
            "{when}: {r:?}"
        );
    }
    assert_eq!(
        cost_of("steps == steps").unwrap_err(),
        "comparing lists is not supported in policies"
    );
    assert_eq!(
        cost_of("trigger == trigger").unwrap_err(),
        "comparing structs is not supported in policies"
    );
    for kept in [
        "trigger.kind == \"push\"",
        "trigger.kind != \"tag\"",
        "pipeline.name == \"ci\"",
        "principal == null",
        "principal != null && principal == \"user:x\"",
        "time < timestamp(\"2030-01-01T00:00:00Z\")",
        "duration(\"1s\") == duration(\"1s\")",
        "size(steps) == 3",
        "steps.exists(s, s.deploy == true && s.id in [\"a\", \"b\"])",
        "has(trigger.sha) && trigger.sha != \"\"",
    ] {
        let c = cost_of(kept);
        assert!(c.is_ok(), "{kept}: {c:?}");
    }
}

/// F2: string functions cost 1 whatever the length, and a supplied input
/// could carry a 1.2 MB `pipeline.name` scanned 4,096 times by one rule
/// (4 s in release). Every input string is now capped: binding refuses a
/// longer one, naming the field, so the rule is an error (fail closed) and
/// never scans it.
#[test]
fn redteam_long_input_strings_are_refused() {
    use scarab_policy::schema::{check_string_lengths, MAX_FREE_TEXT_LEN, MAX_STRING_LEN};
    let when = "steps.exists(s, s.origin.exists(o, pipeline.name.contains(o.content_hash) \
                || pipeline.name.endsWith(o.content_hash) && pipeline.name.contains(o.name)))";
    let cost = cost_of(when).expect("the checker accepts it");
    assert!(cost <= MAX_COST);
    let ir = compile_json(rule_policy(when)).unwrap();
    let input = input_with(MAX_STEPS, MAX_ORIGIN, &"a".repeat(1_200_000));
    let (o, _) = eval_one(&ir, &input);
    let PolicyOutcome::Error { message } = &o else {
        panic!("a 1.2 MB name is evaluated: {o:?}")
    };
    assert!(
        message.contains("pipeline.name: 1200000 bytes, more than the 1024"),
        "{message}"
    );

    // At the cap, accepted; one byte over, refused and named.
    let schema = run_create::schema();
    let at = input_with(1, 1, &"a".repeat(MAX_STRING_LEN));
    check_string_lengths(&schema, &at).expect("1024 bytes fit");
    let (o, _) = eval_one(&ir, &at);
    assert_eq!(o, PolicyOutcome::Passed);
    let mut over = input_with(2, 1, "ci");
    over["steps"][1]["image"] = json!("i".repeat(MAX_STRING_LEN + 1));
    let e = check_string_lengths(&schema, &over).unwrap_err();
    assert_eq!(
        (e.path.as_str(), e.len, e.max),
        ("steps[1].image", 1025, 1024)
    );
    let mut over = input_with(1, 2, "ci");
    over["steps"][0]["secrets"] = json!(["k", "s".repeat(MAX_STRING_LEN + 1)]);
    assert_eq!(
        check_string_lengths(&schema, &over).unwrap_err().path,
        "steps[0].secrets[1]"
    );
    let mut over = input_with(1, 2, "ci");
    over["steps"][0]["origin"][1]["source"] = json!("x".repeat(MAX_STRING_LEN + 1));
    assert_eq!(
        check_string_lengths(&schema, &over).unwrap_err().path,
        "steps[0].origin[1].source"
    );
    // The dispatch reason is free text: 4 KiB.
    let mut reason = input_with(0, 0, "ci");
    reason["trigger"]["reason"] = json!("r".repeat(MAX_FREE_TEXT_LEN));
    check_string_lengths(&schema, &reason).expect("4096 bytes of reason fit");
    reason["trigger"]["reason"] = json!("r".repeat(MAX_FREE_TEXT_LEN + 1));
    assert_eq!(
        check_string_lengths(&schema, &reason).unwrap_err().path,
        "trigger.reason"
    );

    // Admission's input: a Pipeline named with 1025 bytes is refused, as a
    // Run over a list cap is.
    let yaml = format!(
        "name: {}\nsteps:\n  - {{ id: s, image: busybox }}\n",
        "n".repeat(MAX_STRING_LEN + 1)
    );
    let pir = scarab_pipeline::compile_yaml(&yaml).expect("compiles");
    let err = run_create::input_value(&pir, facts())
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("`pipeline.name` is 1025 bytes, more than the 1024"),
        "{err}"
    );
    let yaml = format!(
        "name: {}\nsteps:\n  - {{ id: s, image: busybox }}\n",
        "n".repeat(MAX_STRING_LEN)
    );
    let pir = scarab_pipeline::compile_yaml(&yaml).expect("compiles");
    run_create::input_value(&pir, facts()).expect("1024 bytes fit");
}

/// F3: each distinct `matches()` pattern is compiled at check time and
/// again once per evaluation, and a Unicode `\w{20}` fits the 1 MiB size
/// limit and takes milliseconds to compile; a 4 KiB rule held 107 of them.
/// A rule may use 8 distinct patterns, a Policy 32.
#[test]
fn redteam_many_distinct_large_patterns_are_capped() {
    use scarab_policy::check::MAX_PATTERNS;
    use scarab_policy::ir::MAX_POLICY_PATTERNS;
    let pat = |i: usize| format!("pipeline.name.matches(\"\\\\w{{20}}{i}\")");
    let rule_of = |range: std::ops::Range<usize>| {
        format!(
            "trigger.kind == \"push\" && ({})",
            range.map(pat).collect::<Vec<_>>().join(" || ")
        )
    };
    // The red team's rule: as many distinct patterns as 4 KiB holds.
    let mut n = 0;
    while rule_of(0..n + 1).len() <= MAX_SOURCE_LEN {
        n += 1;
    }
    assert!(n > 90, "{n}");
    let errs = compile_json(rule_policy(&rule_of(0..n))).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| e.contains("a rule uses at most 8 distinct matches() patterns")),
        "{errs:?}"
    );
    // 8 distinct are accepted; a 9th is refused; repeating one is free.
    let ir = compile_json(rule_policy(&rule_of(0..MAX_PATTERNS))).expect("8 patterns");
    let (o, _) = eval_one(&ir, &input_with(1, 0, "ci"));
    assert_eq!(o, PolicyOutcome::Passed);
    assert!(compile_json(rule_policy(&rule_of(0..MAX_PATTERNS + 1))).is_err());
    let same = vec![pat(0); 20].join(" || ");
    compile_json(rule_policy(&same)).expect("one pattern used 20 times");

    // Per Policy: 4 rules of 8 distinct patterns fit, 5 do not.
    let policy_of = |rules: usize| {
        let mut v = rule_policy("true");
        v["rules"] = json!((0..rules)
            .map(|r| json!({
                "when": rule_of(r * MAX_PATTERNS..(r + 1) * MAX_PATTERNS),
                "effect": "deny", "message": "m"
            }))
            .collect::<Vec<_>>());
        v
    };
    compile_json(policy_of(MAX_POLICY_PATTERNS / MAX_PATTERNS)).expect("32 patterns");
    let errs = compile_json(policy_of(MAX_POLICY_PATTERNS / MAX_PATTERNS + 1)).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| e
                .contains("a Policy uses at most 32 distinct matches() patterns across its rules")),
        "{errs:?}"
    );
}

// --- H3: the `matches` rename --------------------------------------------------

#[test]
fn redteam_matches_still_works_inside_macros_ternaries_enums_and_guards() {
    let input = {
        let mut i = input_with(3, 2, "ci");
        i["steps"][1]["origin"][1]["name"] = json!("lib-deploy.yaml");
        i
    };
    let cases: [(&str, bool); 9] = [
        ("steps.exists(s, s.origin.exists(o, o.name.matches(\"^lib-\")))", true),
        ("steps.all(s, s.origin.all(o, o.name.matches(\"^lib\")))", true),
        ("steps.exists_one(s, s.origin.exists(o, o.name.matches(\"deploy\")))", true),
        ("trigger.kind.matches(\"^pu\")", true),
        ("has(trigger.ref) && trigger.ref.matches(\"^refs/heads/\")", true),
        ("(trigger.kind.matches(\"push\") ? \"a\" : \"b\").matches(\"^a$\")", true),
        ("!pipeline.name.matches(\"^ci$\")", false),
        ("pipeline.name.matches(\"\")", true),
        ("steps.exists(s, s.id.matches(\"s2\") && s.origin.exists(o, o.name.matches(\"^lib-\")))", false),
    ];
    for (when, deny) in cases {
        let ir = compile_json(rule_policy(when)).unwrap_or_else(|e| panic!("{when}: {e:?}"));
        let (o, _) = eval_one(&ir, &input);
        let want = if deny {
            PolicyOutcome::Denied {
                messages: vec!["hit".into()],
            }
        } else {
            PolicyOutcome::Passed
        };
        assert_eq!(o, want, "{when}");
    }
    // An absent optional field is an error (fail closed), not a pass.
    let ir = compile_json(rule_policy("trigger.sha.matches(\"^a\")")).unwrap();
    let (o, _) = eval_one(&ir, &input);
    assert!(matches!(o, PolicyOutcome::Error { .. }), "{o:?}");
    // The function form and a non-literal pattern are refused at check.
    for when in [
        "matches(pipeline.name, \"x\")",
        "pipeline.name.matches(pipeline.name)",
        "pipeline.name.matches(\"(\")",
        "pipeline.name.@scarab.matches(\"x\")",
    ] {
        assert!(compile_json(rule_policy(when)).is_err(), "{when}");
    }
}

// --- the plugin default and the caps ------------------------------------------

/// The closed plugin default is consulted whenever a Run has a plugin gate,
/// with no Policy stored. When the Run is over an input cap the input is an
/// `Err`, and `evaluate_stored` skips the default: the decision allows.
#[test]
fn redteam_the_plugin_default_still_denies_a_run_over_the_caps() {
    let mut y = String::from("steps:\n  - { id: sync, gate: manual }\n");
    for i in 0..=MAX_STEPS {
        y.push_str(&format!(
            "  - {{ id: s{i}, image: busybox, needs: [sync] }}\n"
        ));
    }
    let mut ir = scarab_pipeline::compile_yaml(&y).unwrap();
    ir.steps[0].origin.push(scarab_pipeline::OriginEntry {
        kind: scarab_pipeline::OriginKind::Plugin,
        name: "argocd".into(),
        source: None,
        content_hash: "ab".repeat(32),
    });
    assert!(
        run_create::has_plugin_gate(&ir),
        "admission consults this Run"
    );
    let err = input_from_pipeline(&ir, facts()).unwrap_err().to_string();
    // What consult_run_create passes when the caps refuse the input: the
    // gates come from the Pipeline, not the input.
    let gates = run_create::plugin_gates(&ir);
    assert_eq!(gates.len(), 1);
    let d = evaluate_stored(
        Checkpoint::RunCreate,
        SCHEMA_VERSION,
        Err(&err),
        Some(&gates),
        &ctx(),
        &[],
    );
    assert!(
        d.is_denied(),
        "a plugin gate in a 513-Step Run is admitted: {:?}",
        d.outcome
    );
    // The deny names the plugin and the cap.
    let m = d.messages();
    assert!(m.iter().any(|m| m.contains("plugin `argocd`")), "{m:?}");
    assert!(
        m.iter().any(|m| m.contains("the Run has 513 Steps")),
        "{m:?}"
    );

    // Listed by an enforced org setting: the default passes even though the
    // input could not be built (no stored Policy was consulted here, so
    // nothing else denies).
    let listed = scarab_policy::store::StoredPolicy {
        id: "p".into(),
        scope: scarab_policy::store::PolicyScope::Org { org: "acme".into() },
        name: "plugins".into(),
        version: 1,
        content_hash: String::new(),
        body: Policy::from_json(json!({
            "name": "plugins", "checkpoint": "run.create", "mode": "enforce",
            "setting": { "plugins_allowed": { "installations": ["argocd"] } }
        }))
        .unwrap(),
        created_at: 0,
        updated_at: 0,
        deleted: false,
    };
    let d = evaluate_stored(
        Checkpoint::RunCreate,
        SCHEMA_VERSION,
        Err(&err),
        Some(&gates),
        &ctx(),
        std::slice::from_ref(&listed),
    );
    let default = d
        .results
        .iter()
        .find(|r| r.policy_id == "builtin:plugins_allowed");
    assert_eq!(default.map(|r| &r.outcome), Some(&PolicyOutcome::Passed));
    // The listing Policy itself cannot check the input: an enforced error.
    assert!(d.is_denied(), "{:?}", d.results);
}

/// The plugin default caps its messages as a typed setting does: a supplied
/// input of one gate with 10,000 plugin origins is denied with
/// `MAX_TYPED_MESSAGES` messages and a count of the rest, not 10,000.
#[test]
fn redteam_the_plugin_default_caps_its_messages() {
    use scarab_policy::evaluate::MAX_TYPED_MESSAGES;
    let mut input = input_with(1, 0, "ci");
    let origins: Vec<Value> = (0..10_000)
        .map(|i| json!({ "kind": "plugin", "name": format!("p{i}"), "content_hash": "ab".repeat(32) }))
        .collect();
    input["gates"] = json!([{ "id": "g", "origin": origins, "dependents": [] }]);
    let d = evaluate_stored(
        Checkpoint::RunCreate,
        SCHEMA_VERSION,
        Ok(&input),
        None,
        &ctx(),
        &[],
    );
    assert!(d.is_denied(), "{:?}", d.outcome);
    let m = d.messages();
    assert!(m.len() <= MAX_TYPED_MESSAGES + 1, "{} messages", m.len());
    assert!(m[0].contains("plugin `p0`"), "{m:?}");
    assert!(
        m.last().is_some_and(|l| l.contains("and 9990 more")),
        "{m:?}"
    );
}

#[test]
fn redteam_the_draft_reads_and_cost_are_reported_per_rule() {
    let ir = compile_json(rule_policy("steps.exists(s, s.deploy)")).unwrap();
    let Predicate::Cel(c) = &ir.rules[0].predicate else {
        panic!()
    };
    assert_eq!(c.reads, vec!["steps".to_string()]);
    assert!(c.cost > 0 && c.cost <= MAX_COST);
}
