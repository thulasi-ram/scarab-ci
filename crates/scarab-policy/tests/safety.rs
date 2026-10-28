//! Resource bounds a Policy author or a Pipeline author can reach: the
//! static cost bound, the order `run.create`'s input builder checks its caps
//! in, and the parser's stack. Each asserts a refusal, a cost or a count,
//! never a wall-clock time. (`regex_compiled_once.rs` counts `matches()`
//! compiles in a binary of its own.)

use scarab_policy::check::{check, MAX_COST};
use scarab_policy::ir::{compile, Level};
use scarab_policy::policy::Policy;
use scarab_policy::run_create::{
    self, input_from_pipeline, RunCreateFacts, Trigger, TriggerKind, MAX_GATES, MAX_SECRETS,
    MAX_STEPS,
};
use scarab_policy::schema::{Timestamp, Type};
use serde_json::json;

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

/// `steps.exists(v0, steps.exists(v1, … v0.id == "x" …))`, `depth` macros.
fn nested_exists(depth: usize) -> String {
    let mut s = String::new();
    for i in 0..depth {
        s.push_str(&format!("steps.exists(v{i}, "));
    }
    s.push_str("v0.id == \"x\"");
    s.push_str(&")".repeat(depth));
    s
}

fn cost_refusal(source: &str) -> String {
    match check(&run_create::schema(), source, &Type::Bool) {
        Ok(c) => panic!("accepted with cost {}: {source}", c.cost),
        Err(ds) => ds[0].message.clone(),
    }
}

/// Seven nested macros over the 512 Steps exceed `u64`. The bound saturates,
/// so wrapping it with `|| true` or `!( … )` does not bring it back under
/// MAX_COST (in a release build it used to wrap to 1 and 0).
#[test]
fn a_cost_bound_past_u64_saturates_and_is_refused() {
    let seven = nested_exists(7);
    for source in [
        seven.clone(),
        format!("{seven} || true"),
        format!("true || {seven}"),
        format!("!({seven})"),
        format!("{seven} ? true : false"),
        format!("[{seven}, true].exists(b, b)"),
        format!("{{\"k\": {seven}}}.k"),
        nested_exists(8),
    ] {
        let message = cost_refusal(&source);
        assert!(
            message.contains("worst-case cost is 18446744073709551615")
                && message.contains(&format!("limit of {MAX_COST}")),
            "{source}: {message}"
        );
    }
}

#[test]
fn two_nested_macros_over_the_steps_are_over_the_limit() {
    let message = cost_refusal(&nested_exists(2));
    assert!(message.contains("worst-case cost is"), "{message}");
}

fn chain_yaml(n: usize, gate_every: Option<usize>) -> String {
    let mut y = String::from("steps:\n  - { id: s0, image: busybox }\n");
    for i in 1..n {
        let prev = i - 1;
        match gate_every {
            Some(k) if i % k == 0 => y.push_str(&format!(
                "  - {{ id: s{i}, gate: manual, needs: [s{prev}] }}\n"
            )),
            _ => y.push_str(&format!(
                "  - {{ id: s{i}, image: busybox, needs: [s{prev}] }}\n"
            )),
        }
    }
    y
}

/// A Run over the Step cap is refused by the Run-level cap, checked before
/// anything else: here the first Step also has too many secrets, and the
/// refusal names the Run's Steps, not that Step's secrets. (Before, the
/// builder computed every Step's transitive upstream, O(N²), and refused at
/// the end.)
#[test]
fn a_run_over_the_step_cap_is_refused_before_its_steps_are_built() {
    let secrets: Vec<String> = (0..=MAX_SECRETS).map(|i| format!("S{i}")).collect();
    let mut yaml = format!(
        "steps:\n  - {{ id: s0, image: busybox, secrets: [{}] }}\n",
        secrets.join(", ")
    );
    yaml.push_str(
        &chain_yaml(MAX_STEPS + 1, None)["steps:\n  - { id: s0, image: busybox }\n".len()..],
    );
    let ir = scarab_pipeline::compile_yaml(&yaml).expect("compiles");
    let err = input_from_pipeline(&ir, facts()).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "the Run has {} Steps, more than the {MAX_STEPS} a run.create Policy can check, so \
             the Run is refused",
            MAX_STEPS + 1
        )
    );
}

/// A matrix fans one Step out to 8,000; the cap refuses the Run.
#[test]
fn a_matrix_over_the_step_cap_is_refused() {
    let vals = (0..20)
        .map(|i| format!("v{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let yaml = format!(
        "steps:\n  - id: m\n    image: busybox\n    matrix: {{ dimensions: {{ a: [{vals}], b: [{vals}], c: [{vals}] }} }}\n"
    );
    let ir = scarab_pipeline::compile_yaml(&yaml).expect("compiles");
    assert_eq!(ir.steps.len(), 8000);
    let err = input_from_pipeline(&ir, facts()).unwrap_err();
    assert!(
        err.to_string().starts_with("the Run has 8000 Steps"),
        "{err}"
    );
}

#[test]
fn a_run_over_the_gate_cap_is_refused() {
    let ir = scarab_pipeline::compile_yaml(&chain_yaml(2 * (MAX_GATES + 1) + 1, Some(2)))
        .expect("compiles");
    let err = input_from_pipeline(&ir, facts()).unwrap_err();
    assert!(
        err.to_string()
            .starts_with(&format!("the Run has {} gates", MAX_GATES + 1)),
        "{err}"
    );
}

/// The largest Run the caps admit: 512 Steps and 32 gates in one chain.
/// Every gate is upstream of every later Step, transitively.
#[test]
fn a_chain_at_the_caps_builds() {
    // s0 … s543; every 16th position from 16 to 512 is a gate: 32 gates, 512 Steps.
    let total = MAX_STEPS + MAX_GATES;
    let mut y = String::from("steps:\n  - { id: s0, image: busybox }\n");
    let mut gate_ids = Vec::new();
    for i in 1..total {
        let prev = i - 1;
        if i % 16 == 0 && gate_ids.len() < MAX_GATES {
            gate_ids.push(format!("s{i}"));
            y.push_str(&format!(
                "  - {{ id: s{i}, gate: manual, needs: [s{prev}] }}\n"
            ));
        } else {
            y.push_str(&format!(
                "  - {{ id: s{i}, image: busybox, needs: [s{prev}] }}\n"
            ));
        }
    }
    assert_eq!(gate_ids.len(), MAX_GATES);
    let ir = scarab_pipeline::compile_yaml(&y).expect("compiles");
    let input = input_from_pipeline(&ir, facts()).expect("fits the caps");
    let input = serde_json::to_value(input).unwrap();
    let steps = input["steps"].as_array().unwrap();
    let gates = input["gates"].as_array().unwrap();
    assert_eq!((steps.len(), gates.len()), (MAX_STEPS, MAX_GATES));

    let last = steps.last().unwrap();
    let upstream: Vec<&str> = last["upstream_gates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].as_str().unwrap())
        .collect();
    assert_eq!(upstream, gate_ids, "every gate, in Pipeline order");
    assert_eq!(steps[0]["upstream_gates"], json!([]));
    // The first gate is at s16: every Step after it depends on it.
    let first = &gates[0];
    assert_eq!(first["id"], "s16");
    assert_eq!(
        first["dependents"].as_array().unwrap().len(),
        MAX_STEPS - 16
    );
    assert_eq!(first["dependents"][0], "s17");
    // The last gate's dependents are the Steps after it.
    let last_gate = gates.last().unwrap()["id"].as_str().unwrap();
    let after: usize = last_gate[1..].parse().unwrap();
    assert_eq!(
        gates.last().unwrap()["dependents"]
            .as_array()
            .unwrap()
            .len(),
        total - 1 - after
    );
}

/// Diamond `needs`: a gate reached along two paths is listed once.
#[test]
fn a_gate_reached_twice_is_listed_once() {
    let ir = scarab_pipeline::compile_yaml(
        r#"
steps:
  - { id: g, gate: manual }
  - { id: a, image: busybox, needs: [g] }
  - { id: b, image: busybox, needs: [g] }
  - { id: c, image: busybox, needs: [a, b] }
"#,
    )
    .unwrap();
    let input = serde_json::to_value(input_from_pipeline(&ir, facts()).unwrap()).unwrap();
    assert_eq!(
        input["steps"][2]["upstream_gates"],
        json!([{ "id": "g", "kind": "manual" }])
    );
    assert_eq!(input["gates"][0]["dependents"], json!(["a", "b", "c"]));
}

fn compile_rule(when: &str) -> Result<(), String> {
    let p = Policy::from_json(json!({
        "name": "x", "checkpoint": "run.create",
        "rules": [{ "when": when, "effect": "deny", "message": "m" }]
    }))
    .unwrap();
    compile(&p, Level::Org)
        .map(|_| ())
        .map_err(|d| d[0].message.clone())
}

/// The longest rules the 4 KiB / 64-bracket caps admit, in left-recursive
/// shapes, through `compile` (what a Policy save runs). A stack overflow
/// would abort this test binary; each is answered instead.
#[test]
fn left_recursive_chains_at_the_length_cap_are_answered() {
    let shapes = [
        ("trigger.fork", " || trigger.fork"),
        ("org", ".name"),
        ("trigger.fork", " == true"),
        ("\"a\"", " in [\"a\"] == true"),
        ("org.name", ".startsWith(\"a\")"),
    ];
    for (head, link) in shapes {
        let mut s = head.to_string();
        while s.len() + link.len() <= 4096 {
            s.push_str(link);
        }
        // Accepted or refused, the save answers.
        let _ = compile_rule(&s);
    }
    let mut not = "!".repeat(4096 - "trigger.fork".len());
    not.push_str("trigger.fork");
    let _ = compile_rule(&not);
    // One byte over the cap is refused before parsing.
    let over = format!("{}{}", "trigger.fork", " || trigger.fork".repeat(256));
    assert!(over.len() > 4096);
    let message = compile_rule(&over).unwrap_err();
    assert!(message.contains("at most 4096 bytes"), "{message}");
}
