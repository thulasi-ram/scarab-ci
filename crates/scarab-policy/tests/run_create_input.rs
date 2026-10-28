//! The `run.create` input built from a compiled Pipeline (ADR-0075 §2, §5):
//! gates apart from Steps, transitive upstream gates and dependents, and
//! which Steps use the Environment's authority.

use scarab_policy::run_create::{
    input_from_pipeline, RunCreateFacts, Trigger, TriggerKind, MAX_SECRETS,
};
use scarab_policy::schema::Timestamp;

fn facts() -> RunCreateFacts {
    RunCreateFacts {
        org: "acme".into(),
        project: "web".into(),
        trigger: Trigger {
            kind: TriggerKind::Push,
            r#ref: Some("refs/heads/main".into()),
            sha: None,
            fork: false,
            actor: None,
            principal: None,
            reason: None,
        },
        pipeline_path: ".scarab/deploy.yaml".into(),
        environment: Some("prod".into()),
        principal: None,
        time: Timestamp::from_unix_millis(0).unwrap(),
    }
}

#[test]
fn gates_upstream_dependents_and_environment_authority() {
    let ir = scarab_pipeline::compile_yaml(
        r#"
steps:
  - { id: build, image: busybox }
  - { id: approve, gate: manual, needs: [build] }
  - { id: migrate, image: busybox, needs: [approve], secrets: [DB_URL] }
  - { id: ship, image: "busybox@sha256:abc", needs: [migrate], deploy: true }
"#,
    )
    .unwrap();
    let input = serde_json::to_value(input_from_pipeline(&ir, facts()).unwrap()).unwrap();

    assert_eq!(input["pipeline"]["name"], ".scarab/deploy.yaml");
    assert_eq!(input["pipeline"]["environment"], "prod");
    let ids: Vec<&str> = input["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["build", "migrate", "ship"], "gates are listed apart");
    assert_eq!(
        input["gates"],
        serde_json::json!([{
            "id": "approve", "kind": "manual", "origin": [], "dependents": ["migrate", "ship"]
        }])
    );
    let step = |i: usize| &input["steps"][i];
    assert_eq!(step(0)["upstream_gates"], serde_json::json!([]));
    assert_eq!(step(0)["uses_environment_authority"], false);
    // Transitive: `ship` is behind the gate through `migrate`.
    assert_eq!(
        step(2)["upstream_gates"],
        serde_json::json!([{ "id": "approve", "kind": "manual" }])
    );
    assert_eq!(step(1)["uses_environment_authority"], true, "secrets");
    assert_eq!(step(2)["uses_environment_authority"], true, "deploy");
    assert_eq!(step(2)["digest"], "sha256:abc");
}

#[test]
fn a_list_over_its_cap_names_the_cap() {
    let secrets: Vec<String> = (0..=MAX_SECRETS).map(|i| format!("S{i}")).collect();
    let ir = scarab_pipeline::compile_yaml(&format!(
        "steps: [{{ id: big, image: busybox, secrets: [{}] }}]",
        secrets.join(", ")
    ))
    .unwrap();
    let err = input_from_pipeline(&ir, facts()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Step `big` has 33 secrets, more than the 32 a run.create Policy can check, so the \
         Run is refused"
    );
}

/// The deepest invoke chain the compiler admits fits the `origin` cap, so a
/// Run is never refused for nesting its compiler accepted.
#[test]
fn the_deepest_invoke_chain_fits_the_origin_cap() {
    let depth = scarab_pipeline::MAX_INVOKE_DEPTH;
    let mut libs = std::collections::BTreeMap::new();
    for i in 0..depth {
        let src = if i + 1 < depth {
            format!(
                "steps: [{{ id: s{i}, invoke: .scarab/lib/l{}.yaml }}]",
                i + 1
            )
        } else {
            format!("steps: [{{ id: s{i}, image: busybox }}]")
        };
        libs.insert(format!(".scarab/lib/l{i}.yaml"), src);
    }
    let ir = scarab_pipeline::compile_yaml_with_libs(
        "steps: [{ id: t, invoke: .scarab/lib/l0.yaml }]",
        &libs,
    )
    .expect("a chain at the cap compiles");
    let input = input_from_pipeline(&ir, facts()).expect("fits the origin cap");
    let input = serde_json::to_value(input).unwrap();
    assert_eq!(input["steps"].as_array().unwrap().len(), 1);
    assert_eq!(
        input["steps"][0]["origin"].as_array().unwrap().len(),
        scarab_policy::run_create::MAX_ORIGIN
    );
}
