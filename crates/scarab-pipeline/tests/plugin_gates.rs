//! Plugin gates (ADR-0076 §7): `invoke: plugin:<installation>/<gate>` inlines
//! one `gate: plugin` node whose releaser is the installation, whose inputs
//! render when the gate is reached, and whose typed outputs a downstream Step
//! reads as values from outside the Pipeline.

use std::collections::BTreeMap;

use scarab_pipeline::{
    check_gate_outputs, compile_with_libraries, render, render_inputs, Facts, Interface, Libraries,
    OriginKind, OutputSpec, ParamSpec, ParamType, PipelineError, PipelineIr, PluginGateDef,
    Templates,
};
use serde_json::{json, Map, Value};

fn param(name: &str) -> ParamSpec {
    serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap()
}

/// `argocd-prod/wait-live`, as the Argo CD plugin's manifest declares it.
fn wait_live() -> PluginGateDef {
    PluginGateDef {
        interface: Interface {
            inputs: vec![param("app"), param("image")],
            outputs: vec![
                OutputSpec::named("revision"),
                OutputSpec {
                    name: "healthy".into(),
                    r#type: ParamType::Boolean,
                    description: None,
                },
            ],
        },
        manifest_sha256: "a".repeat(64),
    }
}

fn libraries() -> Libraries {
    Libraries {
        plugin_gates: BTreeMap::from([("argocd-prod/wait-live".to_string(), wait_live())]),
        ..Default::default()
    }
}

fn compile(yaml: &str) -> Result<PipelineIr, PipelineError> {
    compile_with_libraries(yaml, &libraries())
}

fn errors(yaml: &str) -> Vec<String> {
    match compile(yaml) {
        Err(PipelineError::Validation(d)) => d,
        other => panic!("expected validation errors, got {other:?}"),
    }
}

const PIPELINE: &str = r#"
steps:
  - { id: bump, image: alpine, results: [image] }
  - id: live
    needs: [bump]
    invoke: plugin:argocd-prod/wait-live
    with:
      app: payments-api
      image: ${{ outputs.bump.image }}
    gate_expires_after: 1800
  - { id: after, image: alpine, needs: [live], env: [[REV, "${{ outputs.live.revision }}"]] }
"#;

#[test]
fn a_plugin_invoke_inlines_one_gate_its_installation_releases() {
    let ir = compile(PIPELINE).unwrap();
    let live = ir.steps.iter().find(|s| s.id == "live").unwrap();
    assert_eq!(live.gate.as_deref(), Some("plugin"));
    let pg = live.plugin_gate.as_ref().unwrap();
    assert_eq!(pg.installation, "argocd-prod");
    assert_eq!(pg.gate, "wait-live");
    assert_eq!(pg.releaser(), "plugin:argocd-prod");
    assert_eq!(pg.outputs, wait_live().interface.outputs);
    assert_eq!(live.gate_expires_after, Some(1800));
    assert_eq!(live.needs.0, vec!["bump".to_string()]);
    assert!(live.invoke.is_none() && live.with.is_empty());
    assert_eq!(live.origin.len(), 1);
    assert_eq!(live.origin[0].kind, OriginKind::Plugin);
    assert_eq!(live.origin[0].name, "argocd-prod");
    assert!(
        live.scope.is_some(),
        "its inputs are a scope, rendered at the wait"
    );
    // A Pipeline with a scope is marked, so a server that predates scopes
    // refuses it.
    assert_eq!(ir.ir_version, scarab_pipeline::IR_VERSION_SCOPED);
}

#[test]
fn a_plugin_gate_requires_an_expiry_at_compile() {
    let yaml = PIPELINE.replace("    gate_expires_after: 1800\n", "");
    let errs = errors(&yaml);
    assert!(
        errs.iter()
            .any(|e| e.contains("a plugin gate must set `gate_expires_after`")),
        "{errs:?}"
    );
}

#[test]
fn only_a_gate_the_org_installed_resolves() {
    for target in [
        "plugin:argocd-staging/wait-live",
        "plugin:argocd-prod/wait-synced",
    ] {
        let errs = errors(&PIPELINE.replace("plugin:argocd-prod/wait-live", target));
        assert!(
            errs.iter()
                .any(|e| e.contains("is not a gate of a plugin installed in this org")),
            "{target}: {errs:?}"
        );
    }
}

#[test]
fn with_is_checked_against_the_gate_s_inputs() {
    let errs = errors(&PIPELINE.replace("app: payments-api", "application: payments-api"));
    assert!(
        errs.iter()
            .any(|e| e.contains("missing required input `app`")),
        "{errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| e.contains("unknown input `application`")),
        "{errs:?}"
    );
}

#[test]
fn a_plugin_gate_and_its_releaser_cannot_be_written_by_hand() {
    let errs = errors("steps: [{ id: g, gate: plugin, gate_expires_after: 60 }]");
    assert!(
        errs.iter()
            .any(|e| e.contains("comes only from `invoke: plugin:")),
        "{errs:?}"
    );
    let errs = errors(
        "steps: [{ id: g, gate: manual, plugin_gate: { installation: argocd-prod, gate: wait-live } }]",
    );
    assert!(
        errs.iter()
            .any(|e| e.contains("`plugin_gate` is written by the compiler")),
        "{errs:?}"
    );
}

#[test]
fn what_runs_nothing_cannot_carry_step_fields() {
    let yaml = PIPELINE.replace(
        "    gate_expires_after: 1800\n",
        "    gate_expires_after: 1800\n    timeout: 60\n    deploy: true\n",
    );
    let errs = errors(&yaml);
    for field in ["timeout", "deploy"] {
        assert!(
            errs.iter()
                .any(|e| e.contains(&format!("`{field}` cannot be set on a plugin gate"))),
            "{field}: {errs:?}"
        );
    }
}

#[test]
fn a_downstream_read_names_a_declared_output() {
    let errs = errors(&PIPELINE.replace("outputs.live.revision", "outputs.live.digest"));
    assert!(
        errs.iter()
            .any(|e| e.contains("reads `outputs.live.digest`")),
        "{errs:?}"
    );
}

#[test]
fn a_gate_output_in_a_script_is_an_outside_value_and_refused() {
    let yaml = PIPELINE.replace(
        r#"env: [[REV, "${{ outputs.live.revision }}"]]"#,
        r#"command: [sh, -c, "echo ${{ outputs.live.revision }}"]"#,
    );
    let errs = errors(&yaml);
    assert!(
        errs.iter()
            .any(|e| e.contains("reads a value from outside the Pipeline")),
        "{errs:?}"
    );
}

/// A gate's outputs are free text from outside the Pipeline (ADR-0076): a
/// command that reads one is refused at launch, whatever the output holds and
/// wherever in the argument it sits (a raw IR can carry such a command past
/// the compile check). The same output reaches the Step through `env:`.
#[test]
fn a_gate_output_is_refused_in_a_command_at_launch() {
    let ir = compile(PIPELINE).unwrap();
    let after = ir.steps.iter().find(|s| s.id == "after").unwrap();
    let params = BTreeMap::new();
    let event = Map::new();
    let launch = |revision: &str, command: &[String], env: &[(String, String)]| {
        let results = BTreeMap::from([(
            "live".to_string(),
            BTreeMap::from([("revision".to_string(), json!(revision))]),
        )]);
        render(
            Templates {
                image: "alpine",
                command,
                env,
            },
            after.scope.as_ref(),
            &after.matrix_values,
            Facts::Launch {
                params: &params,
                declared: &[],
                results: &results,
                event: &event,
            },
        )
    };
    let reference = "${{ outputs.live.revision }}";
    for revision in ["--upload-pack=touch /x", "/bin/sh", "abc123", "0"] {
        for command in [
            vec!["git".to_string(), reference.to_string()],
            vec![reference.to_string()],
            vec!["git".to_string(), format!("--rev={reference}")],
            vec!["sh".to_string(), "-c".to_string(), format!("echo {reference}")],
        ] {
            let err = launch(revision, &command, &[]).unwrap_err();
            assert!(
                err.contains("reads a value from outside the Pipeline")
                    && err.contains("pass it in `env:`"),
                "{revision:?} in {command:?}: {err}"
            );
        }
        // Through `env:` the output is data, passed as it is.
        let ok = launch(
            revision,
            &["sh".into(), "-c".into(), "echo \"$REV\"".into()],
            &[("REV".to_string(), reference.to_string())],
        )
        .unwrap();
        assert!(
            ok.env.iter().any(|(k, v)| k == "REV" && v == revision),
            "{:?}",
            ok.env
        );
    }
}

#[test]
fn a_forge_token_step_takes_no_gate_output_outside_a_scarab_name() {
    let yaml = PIPELINE.replace(
        r#"env: [[REV, "${{ outputs.live.revision }}"]]"#,
        r#"env: [[GIT_SSH_COMMAND, "${{ outputs.live.revision }}"]], forge_token: read"#,
    );
    let errs = errors(&yaml);
    assert!(errs.iter().any(|e| e.contains("forge_token")), "{errs:?}");
}

#[test]
fn inputs_render_at_the_wait_from_the_run_s_facts() {
    let ir = compile(PIPELINE).unwrap();
    let live = ir.steps.iter().find(|s| s.id == "live").unwrap();
    let scope = live.scope.as_ref().unwrap();
    let results = BTreeMap::from([(
        "bump".to_string(),
        BTreeMap::from([(
            "image".to_string(),
            json!("ghcr.io/acme/pay@sha256:${{ x }}"),
        )]),
    )]);
    let inputs = render_inputs(scope, &BTreeMap::new(), &results, &Map::new()).unwrap();
    assert_eq!(inputs["app"], json!("payments-api"));
    // A value is data: what it holds is never rendered again.
    assert_eq!(inputs["image"], json!("ghcr.io/acme/pay@sha256:${{ x }}"));
    // A reference that does not resolve is the gate's failure reason.
    let err = render_inputs(scope, &BTreeMap::new(), &BTreeMap::new(), &Map::new()).unwrap_err();
    assert!(err.contains("input `image`"), "{err}");
}

#[test]
fn a_plugin_gate_in_an_installed_library_is_namespaced_and_scoped() {
    let lib = r#"
kind: library
interface: { inputs: [{ name: app, type: string }] }
steps:
  - id: live
    invoke: plugin:argocd-prod/wait-live
    with: { app: "${{ inputs.app }}", image: fixed }
    gate_expires_after: 60
  - { id: note, image: alpine, needs: [live], env: [[R, "${{ outputs.live.revision }}"]] }
"#;
    let mut libraries = libraries();
    libraries.installed.insert(
        "acme/deploy".into(),
        scarab_pipeline::InstalledLibrary {
            content: lib.into(),
            source: "github.com/acme/libs//deploy.yaml@1111111111111111111111111111111111111111"
                .into(),
        },
    );
    let ir = compile_with_libraries(
        "steps: [{ id: d, invoke: lib:acme/deploy, with: { app: web } }]",
        &libraries,
    )
    .unwrap();
    let gate = ir.steps.iter().find(|s| s.id == "d/live").unwrap();
    assert_eq!(gate.gate.as_deref(), Some("plugin"));
    let kinds: Vec<_> = gate.origin.iter().map(|o| o.kind).collect();
    assert_eq!(kinds, [OriginKind::Library, OriginKind::Plugin]);
    let inputs = render_inputs(
        gate.scope.as_ref().unwrap(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &Map::new(),
    )
    .unwrap();
    assert_eq!(inputs["app"], json!("web"));
    let note = ir.steps.iter().find(|s| s.id == "d/note").unwrap();
    assert!(
        note.needs.0.contains(&"d/live".to_string()),
        "{:?}",
        note.needs
    );
}

#[test]
fn a_matrixed_plugin_invoke_waits_once_per_coordinate() {
    let yaml = r#"
steps:
  - id: live
    invoke: plugin:argocd-prod/wait-live
    matrix: { dimensions: { app: [api, web] } }
    with: { app: "${{ matrix.app }}", image: x }
    gate_expires_after: 60
  - { id: after, image: alpine, needs: [live] }
"#;
    let ir = compile(yaml).unwrap();
    let gates: Vec<_> = ir.steps.iter().filter(|s| s.is_gate()).collect();
    assert_eq!(gates.len(), 2);
    for g in &gates {
        let app = g.matrix_values["app"].clone();
        let inputs = render_inputs(
            g.scope.as_ref().unwrap(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &Map::new(),
        )
        .unwrap();
        assert_eq!(inputs["app"], json!(app));
    }
    let after = ir.steps.iter().find(|s| s.id == "after").unwrap();
    assert_eq!(after.needs.0.len(), 2, "{:?}", after.needs);
}

#[test]
fn outputs_are_checked_against_their_declared_types() {
    let declared = wait_live().interface.outputs;
    let obj = |v: Value| v.as_object().unwrap().clone();
    let ok = check_gate_outputs(&declared, &obj(json!({"revision": "abc", "healthy": true})));
    assert_eq!(ok.unwrap()["healthy"], json!(true));
    for (bad, why) in [
        (json!({"revision": "abc"}), "output `healthy` is missing"),
        (
            json!({"revision": "abc", "healthy": "true"}),
            "must be a boolean",
        ),
        (json!({"revision": 7, "healthy": true}), "must be a string"),
        (
            json!({"revision": "abc", "healthy": true, "extra": 1}),
            "is not one the gate declares",
        ),
        (
            json!({"revision": "x".repeat(70 * 1024), "healthy": true}),
            "at most",
        ),
    ] {
        let err = check_gate_outputs(&declared, &obj(bad.clone())).unwrap_err();
        assert!(err.contains(why), "{bad}: {err}");
    }
}

#[test]
fn an_output_is_a_bare_name_or_a_typed_map() {
    let iface: Interface = serde_yaml::from_str(
        "outputs: [url, { name: ok, type: boolean, description: whether it passed }]",
    )
    .unwrap();
    assert_eq!(iface.outputs[0], OutputSpec::named("url"));
    assert_eq!(iface.outputs[1].r#type, ParamType::Boolean);
    // A bare name is written back as one.
    let json = serde_json::to_value(&iface).unwrap();
    assert_eq!(json["outputs"][0], json!("url"));
    assert_eq!(json["outputs"][1]["type"], json!("boolean"));
    assert!(serde_yaml::from_str::<Interface>("outputs: [{ name: x, kind: string }]").is_err());

    let mut d = Vec::new();
    scarab_pipeline::validate_gate_interface(
        &serde_yaml::from_str("outputs: [{ name: c, type: choice }, a, a, 'b c']").unwrap(),
        "gate",
        &mut d,
    );
    assert_eq!(d.len(), 3, "{d:?}");
}
