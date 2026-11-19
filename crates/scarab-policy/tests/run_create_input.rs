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
            principal_id: None,
            reason: None,
        },
        pipeline_path: ".scarab/deploy.yaml".into(),
        environment: Some("prod".into()),
        principal: None,
        principal_id: None,
        time: Timestamp::from_unix_millis(0).unwrap(),
        params: Default::default(),
        event: Default::default(),
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

/// X7: a Policy can tell a secret that is also an environment variable
/// (`env: true`) from one that is only a file.
#[test]
fn env_secrets_lists_the_env_true_entries() {
    let ir = scarab_pipeline::compile_yaml(
        "steps:\n  - { id: s, image: busybox, secrets: [DB_URL, { name: TOKEN, env: true }] }\n",
    )
    .unwrap();
    let input = serde_json::to_value(input_from_pipeline(&ir, facts()).unwrap()).unwrap();
    assert_eq!(
        input["steps"][0]["secrets"],
        serde_json::json!(["DB_URL", "TOKEN"])
    );
    assert_eq!(
        input["steps"][0]["env_secrets"],
        serde_json::json!(["TOKEN"])
    );
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
                "kind: library\nsteps: [{{ id: s{i}, invoke: file:.scarab/lib/l{}.yaml }}]",
                i + 1
            )
        } else {
            format!("kind: library\nsteps: [{{ id: s{i}, image: busybox }}]")
        };
        libs.insert(format!(".scarab/lib/l{i}.yaml"), src);
    }
    let ir = scarab_pipeline::compile_yaml_with_libs(
        "steps: [{ id: t, invoke: file:.scarab/lib/l0.yaml }]",
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

/// A Step's `image` and `digest` are the image as Run creation knows it,
/// rendered by the launch's own renderer (ADR-0076): a library input bound to
/// a literal is known, so is one bound to a caller's template that reads only
/// launch parameters, one whose template reads an output is not, and a `${{`
/// left in an image pins nothing.
#[test]
fn a_steps_image_is_previewed_and_a_template_pins_nothing() {
    let lib = "kind: library\ninterface: { inputs: [{ name: d, type: string }] }\nsteps: [{ id: s, image: \"app@${{ inputs.d }}\" }]\n";
    let ir = scarab_pipeline::compile_with_libraries(
        r#"
interface: { inputs: [{ name: tag, type: string }] }
steps:
  - { id: lit, invoke: file:l.yaml, with: { d: "sha256:abc" } }
  - { id: tpl, invoke: file:l.yaml, with: { d: "${{ inputs.tag }}" } }
  - { id: b, image: busybox, results: [d] }
  - { id: out, invoke: file:l.yaml, needs: [b], with: { d: "${{ outputs.b.d }}" } }
  - { id: own, image: "app@${{ inputs.tag }}" }
"#,
        &scarab_pipeline::Libraries {
            files: [("l.yaml".to_string(), lib.to_string())].into(),
            installed: Default::default(),
            plugin_gates: Default::default(),
        },
    )
    .unwrap();
    let mut f = facts();
    f.params = [("tag".to_string(), serde_json::json!("sha256:run"))].into();
    let input = serde_json::to_value(input_from_pipeline(&ir, f).unwrap()).unwrap();
    let by_id = |id: &str| {
        input["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id("lit/s")["image"], "app@sha256:abc");
    assert_eq!(by_id("lit/s")["digest"], "sha256:abc");
    // The caller's template reads a launch parameter: known now, as at
    // launch.
    assert_eq!(by_id("tpl/s")["image"], "app@sha256:run");
    assert_eq!(by_id("tpl/s")["digest"], "sha256:run");
    // One that reads an output renders when the Step launches; until then
    // the image holds the template and has no digest.
    assert_eq!(by_id("out/s")["image"], "app@${{ inputs.d }}");
    assert_eq!(by_id("out/s")["digest"], serde_json::Value::Null);
    // A Step in the Pipeline file reads the Run's launch parameters.
    assert_eq!(by_id("own")["image"], "app@sha256:run");
    assert_eq!(by_id("own")["digest"], "sha256:run");
}

/// git-bug 1a20384: `run_as_user: 0` is the `run-as-root` request, so a
/// Policy that denies root refuses it as it refuses `run_as_root: true`; a
/// non-zero uid is no grant, and `run_as_user` carries the uid either way.
#[test]
fn run_as_user_zero_is_the_run_as_root_grant() {
    let ir = scarab_pipeline::compile_yaml(
        r#"
steps:
  - { id: root, image: busybox, security: { run_as_root: true } }
  - { id: zero, image: busybox, security: { run_as_user: 0 } }
  - { id: user, image: busybox, security: { run_as_user: 1000 } }
  - { id: plain, image: busybox }
"#,
    )
    .unwrap();
    let input = serde_json::to_value(input_from_pipeline(&ir, facts()).unwrap()).unwrap();
    let step = |i: usize| &input["steps"][i];
    assert_eq!(step(0)["grants"], serde_json::json!(["run-as-root"]));
    assert_eq!(step(0)["run_as_user"], serde_json::Value::Null);
    assert_eq!(step(1)["grants"], serde_json::json!(["run-as-root"]));
    assert_eq!(step(1)["run_as_user"], 0);
    assert_eq!(step(2)["grants"], serde_json::json!([]));
    assert_eq!(step(2)["run_as_user"], 1000);
    assert_eq!(step(3)["run_as_user"], serde_json::Value::Null);
}

/// git-bug 328010f: a Policy sees each service. A sidecar lists under its
/// Step's `services` and a shared service under the Run's `services`, with
/// `run_as_root` true for `run_as_root: true` and for `run_as_user: 0`. A
/// root sidecar also puts `run-as-root` in its Step's `grants`, so a rule
/// over Step grants covers it; a non-zero uid is no grant.
#[test]
fn services_are_listed_and_run_as_user_zero_is_root() {
    let ir = scarab_pipeline::compile_yaml(
        r#"
services:
  - { name: db, image: "postgres:16@sha256:db", run_as_user: 0 }
  - { name: cache, image: redis:7, run_as_user: 999 }
steps:
  - id: t
    image: busybox
    uses: [db, cache]
    services:
      - { image: "postgres:16", run_as_user: 999 }
      - { image: minio, run_as_user: 0 }
      - { image: mongo, run_as_root: true }
  - id: plain
    image: busybox
    services: [{ image: "redis:7@sha256:r", run_as_user: 1000 }]
  - { id: none, image: busybox }
"#,
    )
    .unwrap();
    let input = serde_json::to_value(input_from_pipeline(&ir, facts()).unwrap()).unwrap();

    assert_eq!(
        input["services"],
        serde_json::json!([
            { "name": "db", "image": "postgres:16@sha256:db", "digest": "sha256:db",
              "grants": ["run-as-root"], "run_as_root": true, "run_as_user": 0 },
            { "name": "cache", "image": "redis:7", "digest": null,
              "grants": [], "run_as_root": false, "run_as_user": 999 },
        ])
    );
    let step = |i: usize| &input["steps"][i];
    assert_eq!(
        step(0)["services"],
        serde_json::json!([
            { "index": 0, "image": "postgres:16", "digest": null,
              "grants": [], "run_as_root": false, "run_as_user": 999 },
            { "index": 1, "image": "minio", "digest": null,
              "grants": ["run-as-root"], "run_as_root": true, "run_as_user": 0 },
            { "index": 2, "image": "mongo", "digest": null,
              "grants": ["run-as-root"], "run_as_root": true, "run_as_user": null },
        ])
    );
    assert_eq!(
        step(0)["grants"],
        serde_json::json!(["run-as-root"]),
        "a root sidecar is a root container in the Step's Pod"
    );
    assert_eq!(step(1)["services"][0]["digest"], "sha256:r");
    assert_eq!(step(1)["services"][0]["run_as_root"], false);
    assert_eq!(step(1)["grants"], serde_json::json!([]));
    assert_eq!(step(2)["services"], serde_json::json!([]));
}

/// A Step whose own `security` and a sidecar both ask for root lists
/// `run-as-root` once.
#[test]
fn a_root_step_with_a_root_sidecar_lists_run_as_root_once() {
    let ir = scarab_pipeline::compile_yaml(
        r#"
steps:
  - id: t
    image: busybox
    security: { run_as_root: true }
    services: [{ image: minio, run_as_root: true }]
"#,
    )
    .unwrap();
    let input = serde_json::to_value(input_from_pipeline(&ir, facts()).unwrap()).unwrap();
    assert_eq!(
        input["steps"][0]["grants"],
        serde_json::json!(["run-as-root"])
    );
}
