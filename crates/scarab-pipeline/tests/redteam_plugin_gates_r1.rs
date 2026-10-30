//! Red team, round 1, ADR-0076 step 4 (plugin gates), compile side: a plugin
//! gate comes only from `invoke: plugin:<installation>/<gate>` of a gate the
//! org installed, carries nothing that runs, must end, and its outputs are
//! values from outside wherever they flow. Each test asserts the SECURE
//! behaviour.

use std::collections::BTreeMap;

use scarab_pipeline::{
    check_gate_outputs, compile_with_libraries, render, render_inputs, Facts, Interface, Libraries, OutputSpec,
    ParamSpec, ParamType, PipelineError, PipelineIr, PluginGateDef, Templates,
};
use serde_json::{json, Map, Value};

fn param(name: &str) -> ParamSpec {
    serde_json::from_value(json!({ "name": name, "type": "string" })).unwrap()
}

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
                OutputSpec {
                    name: "replicas".into(),
                    r#type: ParamType::Number,
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

fn refused(yaml: &str) -> Vec<String> {
    match compile(yaml) {
        Err(PipelineError::Validation(d)) => d,
        Err(other) => vec![format!("{other:?}")],
        Ok(ir) => panic!("compiled, want a refusal:\n{yaml}\n{ir:#?}"),
    }
}

/// A gate `live` releasing `argocd-prod`, then `extra` written into the
/// Pipeline after it.
fn pipeline(gate_extra: &str, after: &str) -> String {
    format!(
        r#"
steps:
  - {{ id: bump, image: alpine, results: [image] }}
  - id: live
    needs: [bump]
    invoke: plugin:argocd-prod/wait-live
    with: {{ app: web, image: "${{{{ outputs.bump.image }}}}" }}
    gate_expires_after: 600
{gate_extra}
{after}
"#
    )
}

// -------------------------------------------------------- hand-written gates

#[test]
fn a_hand_written_plugin_gate_is_refused_however_complete_it_looks() {
    for yaml in [
        "steps: [{ id: g, gate: plugin, gate_expires_after: 60, plugin_gate: { installation: argocd-prod, gate: wait-live, outputs: [revision] } }]",
        "steps: [{ id: g, gate: plugin, gate_expires_after: 60, plugin_gate: { installation: argocd-prod, gate: wait-live } }]",
        "steps: [{ id: g, gate: Plugin, gate_expires_after: 60 }]",
        "steps: [{ id: g, gate: plugin, gate_expires_after: 60, origin: [{ kind: plugin, name: argocd-prod, content_hash: x }] }]",
        "steps: [{ id: g, gate: external, plugin_gate: { installation: argocd-prod, gate: wait-live } }]",
        "steps: [{ id: g, image: alpine, plugin_gate: { installation: argocd-prod, gate: wait-live } }]",
    ] {
        let errs = refused(yaml);
        assert!(!errs.is_empty(), "{yaml}");
    }
}

#[test]
fn an_installed_library_cannot_hand_write_a_plugin_gate_either() {
    let lib = "kind: library\nsteps:\n  - { id: g, gate: plugin, gate_expires_after: 60, plugin_gate: { installation: argocd-prod, gate: wait-live, outputs: [revision] } }\n";
    let mut libs = libraries();
    libs.installed.insert(
        "acme/sneaky".into(),
        scarab_pipeline::InstalledLibrary {
            content: lib.into(),
            source: format!("github.com/acme/libs//s.yaml@{}", "1".repeat(40)),
        },
    );
    match compile_with_libraries("steps: [{ id: d, invoke: lib:acme/sneaky }]", &libs) {
        Err(_) => {}
        Ok(ir) => panic!("a library's hand-written plugin gate compiled: {ir:#?}"),
    }
}

#[test]
fn a_plugin_target_is_matched_exactly() {
    for target in [
        "plugin:ArgoCD-Prod/wait-live",
        "plugin:argocd-prod/Wait-Live",
        "plugin:argocd-prod/wait-live/extra",
        "plugin:../argocd-prod/wait-live",
        "plugin:argocd-prod/../argocd-prod/wait-live",
        "plugin: argocd-prod/wait-live",
        "plugin:argocd-prod//wait-live",
        "plugin:argocd\u{2010}prod/wait-live",
        "PLUGIN:argocd-prod/wait-live",
        "plugin:/wait-live",
        "plugin:argocd-prod/",
    ] {
        let yaml = format!(
            "steps: [{{ id: s, invoke: \"{target}\", with: {{ app: a, image: b }}, gate_expires_after: 60 }}]"
        );
        let errs = refused(&yaml);
        assert!(!errs.is_empty(), "{target}");
    }
}

#[test]
fn a_plugin_invoke_carries_nothing_that_runs_or_changes_when_it_passes() {
    for (field, extra) in [
        ("resources", "    resources: { cpu_millis: 500 }"),
        ("placement_profiles", "    placement_profiles: [gpu]"),
        ("gate_after", "    gate_after: 1"),
        ("forge_token", "    forge_token: write"),
        ("image", "    image: alpine"),
        ("command", "    command: [sh, -c, id]"),
        ("retry", "    retry: { max: 3 }"),
        ("deploy", "    deploy: true"),
        ("timeout", "    timeout: 5"),
        ("gate", "    gate: manual"),
    ] {
        let yaml = pipeline(extra, "");
        let errs = refused(&yaml);
        assert!(!errs.is_empty(), "{field}");
    }
}

#[test]
fn an_expiry_is_positive_and_fits() {
    for v in ["0", "-1", "4294967296", "4294967295", "2592001", "1e9", "\"600\"", "600.5"] {
        let yaml = pipeline("", "").replace("gate_expires_after: 600", &format!("gate_expires_after: {v}"));
        let errs = refused(&yaml);
        assert!(!errs.is_empty(), "{v}");
    }
    // 30 days is the most.
    let yaml = pipeline("", "").replace("gate_expires_after: 600", "gate_expires_after: 2592000");
    compile(&yaml).expect("30 days compiles");
}

// ------------------------------------------------------- inputs at the wait

/// A `with:` that reads a secret, the server's env, a downstream Step or the
/// gate itself is refused at compile, or fails the gate when it renders at
/// the wait: it never renders to a value.
#[test]
fn inputs_cannot_read_secrets_or_a_downstream_step() {
    let mut compiled = Vec::new();
    let mut leaked = Vec::new();
    for with in [
        r#"{ app: "${{ secrets.ARGO_TOKEN }}", image: x }"#,
        r#"{ app: "${{ env.HOME }}", image: x }"#,
        r#"{ app: "${{ outputs.after.x }}", image: x }"#,
        r#"{ app: "${{ outputs.live.revision }}", image: x }"#,
        r#"{ app: "${{ outputs.bump.nope }}", image: x }"#,
    ] {
        let yaml = format!(
            r#"
steps:
  - {{ id: bump, image: alpine, results: [image] }}
  - id: live
    needs: [bump]
    invoke: plugin:argocd-prod/wait-live
    with: {with}
    gate_expires_after: 600
  - {{ id: after, image: alpine, needs: [live], results: [x] }}
"#
        );
        let Ok(ir) = compile(&yaml) else {
            continue;
        };
        compiled.push(with);
        let live = ir.steps.iter().find(|s| s.id == "live").unwrap();
        // Every fact a wait could hold.
        let results = BTreeMap::from([
            ("bump".to_string(), BTreeMap::from([("image".to_string(), json!("i")), ("nope".to_string(), json!("n"))])),
            ("after".to_string(), BTreeMap::from([("x".to_string(), json!("x"))])),
            ("live".to_string(), BTreeMap::from([("revision".to_string(), json!("r"))])),
        ]);
        let params = BTreeMap::from([("ARGO_TOKEN".to_string(), json!("p"))]);
        let event = json!({ "secrets": { "ARGO_TOKEN": "e" } }).as_object().unwrap().clone();
        let rendered = render_inputs(live.scope.as_ref().unwrap(), &params, &results, &event);
        if rendered.is_ok() {
            leaked.push(format!("{with} -> {rendered:?}"));
        }
    }
    eprintln!("compiled (refused only at the wait unless listed below): {compiled:?}");
    assert!(leaked.is_empty(), "compiled AND rendered: {leaked:?}");
}

/// The refusals above come at compile, each saying why.
#[test]
fn inputs_that_read_a_secret_env_or_an_undeclared_result_are_refused_at_compile() {
    for (with, want) in [
        (r#"{ app: "${{ secrets.ARGO_TOKEN }}", image: x }"#, "reads `secrets`"),
        (r#"{ app: "x-${{ env.HOME }}", image: x }"#, "reads `env`"),
        (r#"{ app: "${{ outputs.bump.nope }}", image: x }"#, "does not declare"),
    ] {
        let yaml = format!(
            "steps:\n  - {{ id: bump, image: alpine, results: [image] }}\n  - id: live\n    \
             needs: [bump]\n    invoke: plugin:argocd-prod/wait-live\n    with: {with}\n    \
             gate_expires_after: 600\n"
        );
        let errs = refused(&yaml);
        assert!(errs.iter().any(|e| e.contains(want)), "{with}: {errs:?}");
    }
}

// ------------------------------------------------------ outputs downstream

#[test]
fn a_gate_output_is_refused_where_it_would_decide_what_runs() {
    for after in [
        // A script.
        r#"  - { id: after, image: alpine, needs: [live], command: [sh, -c, "deploy ${{ outputs.live.revision }}"] }"#,
        r#"  - { id: after, image: alpine, needs: [live], command: [bash, -lc, "echo ${{ outputs.live.revision }}"] }"#,
        r#"  - { id: after, image: alpine, needs: [live], command: [env, X=1, sh, -c, "${{ outputs.live.revision }}"] }"#,
        // A `when:` guard is decided at Run creation, before any output.
        r#"  - { id: after, image: alpine, needs: [live], when: "${{ outputs.live.healthy }}" }"#,
        // A forge-token Step: only a `SCARAB_<NAME>` env name, not one Scarab sets.
        r#"  - { id: after, image: alpine, needs: [live], forge_token: read, env: [[PATH, "${{ outputs.live.revision }}"]] }"#,
        r#"  - { id: after, image: alpine, needs: [live], forge_token: read, env: [[SCARAB_RESULTS_TOKEN, "${{ outputs.live.revision }}"]] }"#,
        r#"  - { id: after, image: alpine, needs: [live], forge_token: read, env: [[SCARAB_PARAM_X, "${{ outputs.live.revision }}"]] }"#,
        r#"  - { id: after, image: "${{ outputs.live.revision }}", needs: [live], forge_token: read }"#,
        r#"  - { id: after, image: alpine, needs: [live], forge_token: read, command: [git, "${{ outputs.live.revision }}"] }"#,
    ] {
        let yaml = pipeline("", after);
        assert!(compile(&yaml).is_err(), "compiled: {after}");
    }
}

/// What reaches argv today: a gate output in a non-script argument of a
/// Step with no `forge_token`, after `needs`. It must still never start an
/// argument with `-` or name the program, however it is wrapped.
#[test]
fn a_gate_output_in_argv_is_data_and_never_an_option_or_a_program() {
    let after = r#"  - { id: after, image: alpine, needs: [live], command: [deploy, "${{ outputs.live.revision }}"] }"#;
    let ir = compile(&pipeline("", after)).expect("a plain argument compiles today");
    let step = ir.steps.iter().find(|s| s.id == "after").unwrap();
    let params = BTreeMap::new();
    let event = Map::new();
    let launch = |rev: Value, command: &[&str]| {
        let results = BTreeMap::from([(
            "live".to_string(),
            BTreeMap::from([("revision".to_string(), rev)]),
        )]);
        let command: Vec<String> = command.iter().map(|s| s.to_string()).collect();
        render(
            Templates {
                image: "alpine",
                command: &command,
                env: &[],
            },
            step.scope.as_ref(),
            &step.matrix_values,
            Facts::Launch {
                params: &params,
                results: &results,
                event: &event,
            },
        )
    };
    let o = "${{ outputs.live.revision }}";
    for (rev, cmd) in [
        (json!("-rf"), vec!["rm", o]),
        (json!("--exec=sh"), vec!["find", ".", o]),
        (json!("\u{2212}-x"), vec!["x", o]),
        (json!("/bin/sh"), vec![o]),
        (json!("sh"), vec!["env", "A=1", o]),
        (json!("sh"), vec!["sudo", o]),
        (json!("sh"), vec!["timeout", "5", o]),
        (json!("sh"), vec!["xargs", o]),
        (json!("sh"), vec!["nice", "-n", "5", o]),
        (json!("sh"), vec!["busybox", o]),
    ] {
        let out = launch(rev.clone(), &cmd);
        if let Ok(r) = &out {
            // A Unicode minus is not an ASCII option dash: allowed, as data.
            if rev == json!("\u{2212}-x") {
                continue;
            }
            panic!("{rev} in {cmd:?} rendered: {:?}", r.command);
        }
    }
    // A value that holds a template is not rendered again.
    let ok = launch(json!("${{ secrets.X }}"), &["deploy", o]).unwrap();
    assert_eq!(ok.command[1], "${{ secrets.X }}");
}

// --------------------------------------------------------- output checking

#[test]
fn outputs_are_never_coerced() {
    let declared = wait_live().interface.outputs;
    let base = || json!({ "revision": "r", "healthy": true, "replicas": 3 });
    let with = |k: &str, v: Value| {
        let mut o = base();
        o[k] = v;
        o.as_object().unwrap().clone()
    };
    for (k, v) in [
        ("healthy", json!("true")),
        ("healthy", json!(1)),
        ("healthy", json!(null)),
        ("healthy", json!([true])),
        ("replicas", json!("3")),
        ("replicas", json!(true)),
        ("replicas", json!(null)),
        ("replicas", json!({ "n": 3 })),
        ("revision", json!(1)),
        ("revision", json!(null)),
        ("revision", json!(["r"])),
        ("revision", json!({ "r": "r" })),
        ("revision", json!(false)),
    ] {
        let got = with(k, v.clone());
        assert!(check_gate_outputs(&declared, &got).is_err(), "{k}={v}");
    }
    // Keys are case-sensitive and exact: a near-miss is extra AND missing.
    let mut got = base().as_object().unwrap().clone();
    let v = got.remove("revision").unwrap();
    got.insert("Revision".into(), v);
    assert!(check_gate_outputs(&declared, &got).is_err());
    let mut got = base().as_object().unwrap().clone();
    got.insert("revision\u{0}".into(), json!("x"));
    assert!(check_gate_outputs(&declared, &got).is_err());
}

#[test]
fn the_size_cap_counts_bytes_not_characters() {
    let declared = vec![OutputSpec::named("revision")];
    // `{"revision":"` + v + `"}` = 15 + |v| bytes.
    let obj = |v: String| json!({ "revision": v }).as_object().unwrap().clone();
    let at = format!("{}a", "é".repeat(32_760));
    assert_eq!(at.len() + 15, 64 * 1024);
    assert!(check_gate_outputs(&declared, &obj(at)).is_ok());
    let over = format!("{}ab", "é".repeat(32_760));
    assert!(check_gate_outputs(&declared, &obj(over)).is_err());
    // Escaped control characters count as their escaped bytes.
    let ctl = "\u{1}".repeat(11_000); // 6 bytes each once escaped
    assert!(check_gate_outputs(&declared, &obj(ctl)).is_err());
}
