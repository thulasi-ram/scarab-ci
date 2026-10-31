//! Red-team round 1 on the command boundary for outside values (ADR-0076
//! amendment 2026-10-30, commit 9b8b6ea8): "Scarab never puts a value from
//! outside the Pipeline into a Step's command, except a `choice` value the
//! Pipeline listed, `true`/`false`, a non-negative decimal number, or a hex
//! commit SHA; every other outside value reaches the Step only as the value
//! of an environment variable the Pipeline named, passed byte for byte."
//!
//! Each test asserts the SECURE behaviour: a failing test is a break.

use std::collections::BTreeMap;

use scarab_pipeline::params::resolve_params;
use scarab_pipeline::{
    compile_with_libraries, render, Binding, BoundValue, Facts, Interface, Libraries, ParamSpec,
    ParamType, PipelineError, PipelineIr, Results, Scope, ScopeInputs, StepSpec, Templates,
};
use serde_json::{json, Value};

// ------------------------------------------------------------------ helpers

fn files(entries: &[(&str, &str)]) -> Libraries {
    Libraries {
        files: entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        installed: BTreeMap::new(),
        plugin_gates: Default::default(),
    }
}

fn compile(yaml: &str, libs: &Libraries) -> Result<PipelineIr, Vec<String>> {
    match compile_with_libraries(yaml, libs) {
        Ok(ir) => Ok(ir),
        Err(PipelineError::Validation(d)) => Err(d),
        Err(e) => Err(vec![e.to_string()]),
    }
}

fn ok(yaml: &str, libs: &Libraries) -> PipelineIr {
    compile(yaml, libs).unwrap_or_else(|e| panic!("expected a valid pipeline, got {e:?}"))
}

fn step<'a>(ir: &'a PipelineIr, id: &str) -> &'a StepSpec {
    ir.steps
        .iter()
        .find(|s| s.id == id)
        .unwrap_or_else(|| panic!("no step `{id}`"))
}

fn param(name: &str, ty: ParamType, options: Option<&[&str]>) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        r#type: ty,
        required: true,
        default: None,
        options: options.map(|o| o.iter().map(|s| s.to_string()).collect()),
        validate: None,
        description: None,
    }
}

/// The Run interface used by most attacks below.
fn declared() -> Vec<ParamSpec> {
    vec![
        param("n", ParamType::Number, None),
        param("b", ParamType::Boolean, None),
        param("c", ParamType::Choice, Some(&["us-east-1", "eu-west-1"])),
        param("s", ParamType::String, None),
    ]
}

fn map(v: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

fn cmd(c: &[&str]) -> Vec<String> {
    c.iter().map(|s| s.to_string()).collect()
}

/// Render a command at launch: the engine's call.
fn launch_full(
    declared: &[ParamSpec],
    command: &[String],
    env: &[(String, String)],
    scope: Option<&Scope>,
    params: &BTreeMap<String, Value>,
    results: &Results,
) -> Result<scarab_pipeline::Rendered, String> {
    render(
        Templates {
            image: "busybox",
            command,
            env,
        },
        scope,
        &BTreeMap::new(),
        Facts::Launch {
            params,
            declared,
            results,
            event: &serde_json::Map::new(),
        },
    )
}

fn launch(
    declared: &[ParamSpec],
    command: &[&str],
    scope: Option<&Scope>,
    params: Value,
) -> Result<Vec<String>, String> {
    launch_full(
        declared,
        &cmd(command),
        &[],
        scope,
        &map(params),
        &Results::new(),
    )
    .map(|r| r.command)
}

/// Render a command in the Run-creation preview: the server's call.
fn preview(
    declared: &[ParamSpec],
    command: &[&str],
    scope: Option<&Scope>,
    params: Value,
) -> Result<Vec<String>, String> {
    let c = cmd(command);
    render(
        Templates {
            image: "",
            command: &c,
            env: &[],
        },
        scope,
        &BTreeMap::new(),
        Facts::Preview {
            params: &map(params),
            declared,
            event: &serde_json::Map::new(),
        },
    )
    .map(|r| r.command)
}

/// The engine at launch for a compiled Step, with the Run's interface taken
/// from the IR the way `scarab_engine::render::render_launch_spec` reads it:
/// out of the stored JSON, `interface.inputs`.
fn launch_compiled(
    ir: &PipelineIr,
    spec: &StepSpec,
    params: Value,
    results: Value,
) -> Result<scarab_pipeline::Rendered, String> {
    let stored = serde_json::to_value(ir).unwrap();
    let declared: Vec<ParamSpec> = stored
        .get("interface")
        .and_then(|i| i.get("inputs").cloned())
        .and_then(|i| serde_json::from_value(i).ok())
        .unwrap_or_default();
    let params = map(params);
    let results: Results = serde_json::from_value(results).unwrap();
    render(
        Templates {
            image: &spec.image,
            command: &spec.command,
            env: &spec.env,
        },
        spec.scope.as_ref(),
        &spec.matrix_values,
        Facts::Launch {
            params: &params,
            declared: &declared,
            results: &results,
            event: &serde_json::Map::new(),
        },
    )
}

/// `^[0-9]+(\.[0-9]+)?$`, the documented number shape.
fn is_decimal(s: &str) -> bool {
    let d = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    match s.split_once('.') {
        Some((a, b)) => d(a) && d(b),
        None => d(s),
    }
}

// ======================================= 1. stored values of a fixed type

/// A `number` input whose stored value is text (a dispatch path that stored
/// the raw string, an old Run, a raw IR): never reaches a command.
#[test]
fn a_number_input_stored_as_text_never_reaches_a_command() {
    for raw in ["1; id", "-1", "1e9", " 1", "١", "1\n", "5", "0x10", "+1"] {
        let r = launch(
            &declared(),
            &["tool", "${{ inputs.n }}"],
            None,
            json!({ "n": raw }),
        );
        assert!(r.is_err(), "number stored as text {raw:?} rendered: {r:?}");
        let p = preview(
            &declared(),
            &["tool", "${{ inputs.n }}"],
            None,
            json!({ "n": raw }),
        );
        assert!(
            p.is_err(),
            "preview of {raw:?} disagrees with launch: {p:?}"
        );
    }
}

/// The dispatch coercion path (`resolve_params`, used by POST /v1/runs,
/// manual/api dispatch and defaults), composed with the launch: whatever a
/// caller supplies for a `number`, the command holds either nothing (refused
/// at dispatch or launch) or a non-negative decimal.
#[test]
fn a_number_through_dispatch_coercion_is_decimal_or_refused() {
    let iface = Interface {
        inputs: vec![param("n", ParamType::Number, None)],
        outputs: vec![],
    };
    for raw in [
        json!("1; id"),
        json!("-1"),
        json!("1e9"),
        json!(" 1"),
        json!("١"),
        json!("1\n"),
        json!("1e300"),
        json!("-0.0"),
        json!("inf"),
        json!("NaN"),
        json!("+5"),
        json!(-3),
        json!(1e300),
        json!(18446744073709551615u64),
        json!(0.1),
        json!("1_000"),
        json!("  7  "),
    ] {
        let Ok(resolved) = resolve_params(&iface, &map(json!({ "n": raw.clone() }))) else {
            continue;
        };
        let params = Value::Object(resolved.into_iter().collect());
        match launch(
            &[param("n", ParamType::Number, None)],
            &["tool", "--n=${{ inputs.n }}"],
            None,
            params,
        ) {
            Ok(c) => {
                let v = c[1].strip_prefix("--n=").unwrap();
                assert!(is_decimal(v), "{raw} rendered `{v}` into a command");
            }
            Err(_) => {}
        }
    }
}

/// A `boolean` stored as text, through dispatch coercion and directly.
#[test]
fn a_boolean_stored_as_text_never_reaches_a_command() {
    for raw in ["true; id", "true", "yes", "1"] {
        let r = launch(
            &declared(),
            &["tool", "${{ inputs.b }}"],
            None,
            json!({ "b": raw }),
        );
        assert!(r.is_err(), "boolean stored as text {raw:?} rendered: {r:?}");
    }
    let iface = Interface {
        inputs: vec![param("b", ParamType::Boolean, None)],
        outputs: vec![],
    };
    assert!(resolve_params(&iface, &map(json!({ "b": "true; id" }))).is_err());
    let yes = resolve_params(&iface, &map(json!({ "b": "YES" }))).unwrap();
    let c = launch(
        &[param("b", ParamType::Boolean, None)],
        &["tool", "${{ inputs.b }}"],
        None,
        Value::Object(yes.into_iter().collect()),
    )
    .unwrap();
    assert_eq!(c, ["tool", "true"]);
}

/// A `choice` whose stored value is not in its options (an old Run, an
/// interface that changed): refused at launch and in the preview.
#[test]
fn a_choice_value_not_listed_never_reaches_a_command() {
    for raw in [
        json!("; id"),
        json!("us-east-1 "),
        json!("US-EAST-1"),
        json!(["us-east-1"]),
    ] {
        let r = launch(
            &declared(),
            &["tool", "${{ inputs.c }}"],
            None,
            json!({ "c": raw.clone() }),
        );
        assert!(r.is_err(), "unlisted choice {raw} rendered: {r:?}");
        let p = preview(
            &declared(),
            &["tool", "${{ inputs.c }}"],
            None,
            json!({ "c": raw.clone() }),
        );
        assert!(
            p.is_err(),
            "preview of unlisted choice {raw} disagrees: {p:?}"
        );
    }
    // Listed: allowed, at launch and in the preview, the same text.
    let r = launch(
        &declared(),
        &["tool", "${{ inputs.c }}"],
        None,
        json!({ "c": "eu-west-1" }),
    );
    let p = preview(
        &declared(),
        &["tool", "${{ inputs.c }}"],
        None,
        json!({ "c": "eu-west-1" }),
    );
    assert_eq!(
        r.as_deref().ok(),
        Some(&["tool".to_string(), "eu-west-1".into()][..])
    );
    assert_eq!(r, p);
}

/// No readable interface (a stored IR without `interface`, or one that does
/// not parse): nothing is fixed, so a `choice`, boolean or number value is
/// refused like free text.
#[test]
fn no_readable_interface_means_nothing_is_fixed() {
    for (cmd, params) in [
        ("${{ inputs.c }}", json!({ "c": "us-east-1" })),
        ("${{ inputs.b }}", json!({ "b": true })),
        ("${{ inputs.n }}", json!({ "n": 5 })),
    ] {
        assert!(
            launch(&[], &["tool", cmd], None, params.clone()).is_err(),
            "{cmd}"
        );
        assert!(preview(&[], &["tool", cmd], None, params).is_err(), "{cmd}");
    }
    // A stored IR whose `interface.inputs` is malformed reads as no interface
    // (the engine's `.ok()`): a hand-edited IR cannot drop the check.
    let malformed: Option<Vec<ParamSpec>> =
        serde_json::from_value(json!([{ "name": "c", "type": "choice", "bogus": 1 }])).ok();
    let declared = malformed.unwrap_or_default();
    let r = launch(
        &declared,
        &["tool", "${{ inputs.c }}"],
        None,
        json!({ "c": "; id" }),
    );
    assert!(r.is_err(), "{r:?}");
}

/// A launch parameter the interface does not declare is free text, whatever
/// its stored JSON type.
#[test]
fn an_undeclared_parameter_is_free_text_even_when_a_number() {
    let r = launch(
        &declared(),
        &["tool", "${{ inputs.extra }}"],
        None,
        json!({ "extra": 5 }),
    );
    assert!(r.is_err(), "{r:?}");
}

// ================================= 1b. reads of fixed inputs, by expression

#[test]
fn index_syntax_reads_a_choice_like_dot_syntax() {
    let r = launch(
        &declared(),
        &["tool", "${{ inputs['c'] }}"],
        None,
        json!({ "c": "us-east-1" }),
    );
    assert_eq!(r.unwrap(), ["tool", "us-east-1"]);
    let r = launch(
        &declared(),
        &["tool", "${{ inputs['c'] }}"],
        None,
        json!({ "c": "; id" }),
    );
    assert!(r.is_err());
    let r = launch(
        &declared(),
        &["tool", "${{ inputs['s'] }}"],
        None,
        json!({ "s": "x" }),
    );
    assert!(r.is_err());
}

/// Reads that hide which input they read (`dyn(inputs)`, a computed key, a
/// comprehension, a map/list wrapper) are as far outside as `inputs` whole:
/// free text.
#[test]
fn opaque_reads_of_inputs_are_free_text() {
    let p = json!({ "c": "us-east-1", "s": "--upload-pack=touch /tmp/pwn", "n": 1, "b": true });
    for e in [
        "${{ dyn(inputs).c }}",
        "${{ dyn(inputs).s }}",
        "${{ inputs[inputs.c] }}",
        "${{ inputs[['s'][0]] }}",
        "${{ [inputs.s][0] }}",
        "${{ {'k': inputs.s}.k }}",
        "${{ [1].map(x, inputs.s)[0] }}",
        "${{ [inputs.s].map(matrix, matrix)[0] }}",
        "${{ ['a'].map(inputs, inputs)[0] }}",
        "${{ inputs.s.size() > 0 ? inputs.c : inputs.c }}",
        "${{ has(inputs.s) }}",
        "${{ size(inputs) }}",
        "${{ event.message }}",
        "${{ secrets.TOKEN }}",
        "${{ outputs.a.v }}",
    ] {
        let r = launch(&declared(), &["tool", e], None, p.clone());
        assert!(r.is_err(), "`{e}` rendered into a command: {r:?}");
        let pr = preview(&declared(), &["tool", e], None, p.clone());
        assert!(
            pr.is_err(),
            "preview of `{e}` disagrees with launch: {pr:?}"
        );
    }
}

/// A ternary or concatenation that produces text from fixed inputs is not a
/// listed value: refused even when the text is a listed value.
#[test]
fn computed_strings_over_fixed_inputs_are_refused() {
    let p = json!({ "c": "us-east-1", "n": 1, "b": true });
    for e in [
        "${{ inputs.b ? inputs.c : 'eu-west-1' }}",
        "${{ inputs.c + '' }}",
        "${{ string(inputs.n) }}",
        "${{ string(inputs.b) }}",
        "${{ inputs.b ? '; id' : 'x' }}",
    ] {
        let r = launch(&declared(), &["tool", e], None, p.clone());
        assert!(r.is_err(), "`{e}` rendered into a command: {r:?}");
    }
}

/// Computed booleans and numbers over fixed inputs are shape-checked.
#[test]
fn computed_numbers_are_shape_checked() {
    let n = |v: Value, e: &str| launch(&declared(), &["tool", e], None, json!({ "n": v }));
    assert!(n(json!(5), "${{ inputs.n - 100 }}").is_err(), "negative");
    assert!(
        n(json!(5), "${{ double(inputs.n) * 1e300 }}").is_err(),
        "1e300"
    );
    assert!(n(json!(5), "${{ double(inputs.n) / 0.0 }}").is_err(), "inf");
    assert!(n(json!(0), "${{ double(inputs.n) / 0.0 }}").is_err(), "NaN");
    assert!(
        n(json!(5), "${{ -double(inputs.n) * 0.0 }}").is_err(),
        "-0.0"
    );
    assert!(
        n(json!(9223372036854775807i64), "${{ inputs.n + 1 }}").is_err(),
        "overflow"
    );
    assert_eq!(n(json!(5), "${{ inputs.n * 2 }}").unwrap(), ["tool", "10"]);
    assert_eq!(
        n(json!(5), "${{ inputs.n > 3 }}").unwrap(),
        ["tool", "true"]
    );
    assert_eq!(n(json!(5), "${{ uint(inputs.n) }}").unwrap(), ["tool", "5"]);
    // A float input renders in serde's form; it must still be decimal.
    for v in [json!(1e16), json!(1e-7), json!(0.1)] {
        if let Ok(c) = n(v.clone(), "${{ inputs.n }}") {
            assert!(is_decimal(&c[1]), "{v} rendered `{}`", c[1]);
        }
    }
}

// =================================================== 4. embedded positions

#[test]
fn embedded_fixed_values_are_allowed_and_free_text_is_not() {
    let p = json!({ "c": "us-east-1", "n": 3, "b": false, "s": "x y" });
    assert_eq!(
        launch(
            &declared(),
            &["tool", "--n=${{ inputs.n }}", "x${{ inputs.b }}y"],
            None,
            p.clone()
        )
        .unwrap(),
        ["tool", "--n=3", "xfalsey"]
    );
    for e in [
        "${{ inputs.n }}${{ inputs.s }}",
        "--r=${{ inputs.c }},${{ inputs.s }}",
        "a${{inputs.s}}b",
        "${{ inputs.n }} ${{ outputs.a.v }}",
    ] {
        let r = launch(&declared(), &["tool", e], None, p.clone());
        assert!(r.is_err(), "`{e}`: {r:?}");
        let pr = preview(&declared(), &["tool", e], None, p.clone());
        assert!(pr.is_err(), "preview `{e}`: {pr:?}");
    }
    // The program position too.
    assert!(launch(&declared(), &["${{ inputs.s }}"], None, p.clone()).is_err());
}

/// A free-text value that itself holds `${{ … }}` is never parsed again, in
/// env (where it is allowed) or anywhere.
#[test]
fn env_values_are_byte_for_byte_and_not_reparsed() {
    let nasty = "$(id) `id` ${{ inputs.c }} '\"\\\n\t;|&<>*?~ -- --upload-pack=x \u{202e}";
    let r = launch_full(
        &declared(),
        &cmd(&["tool"]),
        &[("V".into(), "${{ inputs.s }}".into())],
        None,
        &map(json!({ "s": nasty, "c": "us-east-1" })),
        &Results::new(),
    )
    .unwrap();
    let v = r
        .env
        .iter()
        .find(|(k, _)| k == "V")
        .map(|(_, v)| v.as_str());
    assert_eq!(v, Some(nasty));
    let sp = r
        .env
        .iter()
        .find(|(k, _)| k == "SCARAB_PARAM_S")
        .map(|(_, v)| v.as_str());
    assert_eq!(sp, Some(nasty));
}

// ===================================================== 2. library paths

/// A library `number` input bound by the caller to free text: classed by the
/// library's declared type, converted, then shape-checked at launch.
#[test]
fn a_library_number_bound_to_caller_free_text_is_converted_and_shape_checked() {
    let lib = "kind: library\ninterface: { inputs: [{ name: n, type: number }] }\nsteps:\n  - { id: s, image: busybox, command: [tool, \"--n=${{ inputs.n }}\"] }\n";
    let yaml = "interface: { inputs: [{ name: s, type: string }] }\nsteps:\n  - { id: c, invoke: file:l.yaml, with: { n: \"${{ inputs.s }}\" } }\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    let s = step(&ir, "c/s");
    for (raw, want) in [
        ("5", Some("--n=5")),
        ("1e9", Some("--n=1000000000.0")),
        ("-1", None),
        ("1e300", None),
        ("1; id", None),
        ("١", None),
        ("inf", None),
    ] {
        let r = launch_compiled(&ir, s, json!({ "s": raw }), json!({}));
        let pv = render(
            Templates {
                image: "",
                command: &s.command,
                env: &[],
            },
            s.scope.as_ref(),
            &s.matrix_values,
            Facts::Preview {
                params: &map(json!({ "s": raw })),
                declared: &ir.interface.inputs,
                event: &serde_json::Map::new(),
            },
        )
        .map(|r| r.command);
        // The preview never shows a command the launch would not run, and
        // never refuses one the launch runs. (A caller value that does not
        // convert is left unknown in the preview, by design.)
        match (&r, &pv) {
            (Ok(l), Ok(p)) if !p.iter().any(|a| a.contains("${{")) => {
                assert_eq!(&l.command, p, "{raw}")
            }
            (Ok(_), Err(e)) => panic!("{raw}: preview refuses what the launch runs: {e}"),
            _ => {}
        }
        match want {
            Some(w) => assert_eq!(r.unwrap().command, ["tool", w], "{raw}"),
            None => assert!(r.is_err(), "{raw:?} rendered: {:?}", r.map(|r| r.command)),
        }
    }
}

/// A library `choice` bound to the caller's free text: only a listed value
/// renders.
#[test]
fn a_library_choice_bound_to_caller_free_text_renders_only_listed_values() {
    let lib = "kind: library\ninterface: { inputs: [{ name: r, type: choice, options: [a, b] }] }\nsteps:\n  - { id: s, image: busybox, command: [tool, \"${{ inputs.r }}\"] }\n";
    let yaml = "interface: { inputs: [{ name: s, type: string }] }\nsteps:\n  - { id: c, invoke: file:l.yaml, with: { r: \"${{ inputs.s }}\" } }\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    let s = step(&ir, "c/s");
    assert_eq!(
        launch_compiled(&ir, s, json!({ "s": "a" }), json!({}))
            .unwrap()
            .command,
        ["tool", "a"]
    );
    for raw in ["c", "; id", "a ", "-a"] {
        assert!(
            launch_compiled(&ir, s, json!({ "s": raw }), json!({})).is_err(),
            "{raw}"
        );
    }
    // A caller template with text around the read is text: same rule.
    let yaml2 = "interface: { inputs: [{ name: s, type: string }] }\nsteps:\n  - { id: c, invoke: file:l.yaml, with: { r: \"x${{ inputs.s }}\" } }\n";
    let ir2 = ok(yaml2, &files(&[("l.yaml", lib)]));
    let s2 = step(&ir2, "c/s");
    assert!(launch_compiled(&ir2, s2, json!({ "s": "; id" }), json!({})).is_err());
}

/// A library `choice` bound to a caller's Step result: a result is outside,
/// but the library's declared type fixes it.
#[test]
fn a_library_choice_bound_to_a_result_renders_only_listed_values() {
    let lib = "kind: library\ninterface: { inputs: [{ name: r, type: choice, options: [a, b] }] }\nsteps:\n  - { id: s, image: busybox, command: [tool, \"${{ inputs.r }}\"] }\n";
    let yaml = "steps:\n  - { id: p, image: busybox, results: [v] }\n  - { id: c, invoke: file:l.yaml, needs: [p], with: { r: \"${{ outputs.p.v }}\" } }\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    let s = step(&ir, "c/s");
    assert_eq!(
        launch_compiled(&ir, s, json!({}), json!({ "p": { "v": "b" } }))
            .unwrap()
            .command,
        ["tool", "b"]
    );
    assert!(launch_compiled(&ir, s, json!({}), json!({ "p": { "v": "$(id)" } })).is_err());
}

/// Three levels: string → choice → number → library command. Each level
/// converts to its own declared type.
#[test]
fn three_levels_of_libraries_keep_the_innermost_shape() {
    let inner = "kind: library\ninterface: { inputs: [{ name: k, type: number }] }\nsteps:\n  - { id: s, image: busybox, command: [tool, \"${{ inputs.k }}\"] }\n";
    let mid = "kind: library\ninterface: { inputs: [{ name: m, type: choice, options: [\"7\", \"-7\", \"; id\"] }] }\nsteps:\n  - { id: i, invoke: file:inner.yaml, with: { k: \"${{ inputs.m }}\" } }\n";
    let outer = "kind: library\ninterface: { inputs: [{ name: o, type: string }] }\nsteps:\n  - { id: m, invoke: file:mid.yaml, with: { m: \"${{ inputs.o }}\" } }\n";
    let yaml = "interface: { inputs: [{ name: s, type: string }] }\nsteps:\n  - { id: top, invoke: file:outer.yaml, with: { o: \"${{ inputs.s }}\" } }\n";
    let libs = files(&[
        ("inner.yaml", inner),
        ("mid.yaml", mid),
        ("outer.yaml", outer),
    ]);
    let ir = ok(yaml, &libs);
    let s = ir
        .steps
        .iter()
        .find(|s| s.id.ends_with("/s"))
        .expect("inlined inner step");
    assert_eq!(
        launch_compiled(&ir, s, json!({ "s": "7" }), json!({}))
            .unwrap()
            .command,
        ["tool", "7"]
    );
    // Listed in the middle, but a negative number at the innermost type.
    assert!(launch_compiled(&ir, s, json!({ "s": "-7" }), json!({})).is_err());
    // Listed in the middle, not a number at all.
    assert!(launch_compiled(&ir, s, json!({ "s": "; id" }), json!({})).is_err());
    // Not listed in the middle.
    assert!(launch_compiled(&ir, s, json!({ "s": "8" }), json!({})).is_err());
}

/// A library `string` input bound to the caller's `choice`: the library
/// declared free text, so its command may not hold it (refused at compile).
#[test]
fn a_library_string_bound_to_a_caller_choice_is_free_text() {
    let lib = "kind: library\ninterface: { inputs: [{ name: t, type: string }] }\nsteps:\n  - { id: s, image: busybox, command: [tool, \"${{ inputs.t }}\"] }\n";
    let yaml = "interface: { inputs: [{ name: c, type: choice, options: [a] }] }\nsteps:\n  - { id: c, invoke: file:l.yaml, with: { t: \"${{ inputs.c }}\" } }\n";
    assert!(compile(yaml, &files(&[("l.yaml", lib)])).is_err());
}

/// Matrix values are the Pipeline's own text: a matrix value that names an
/// input is not a read, at launch it renders as that text, never as the
/// input's value.
#[test]
fn a_matrix_value_naming_an_input_renders_as_its_own_text() {
    let yaml = "interface: { inputs: [{ name: s, type: string }] }\nsteps:\n  - id: m\n    image: busybox\n    matrix: { dimensions: { x: [\"${{ inputs.s }}\"] } }\n    command: [tool, \"${{ matrix.x }}\"]\n";
    let ir = match compile(yaml, &Libraries::default()) {
        Err(_) => return, // refused at compile: secure
        Ok(ir) => ir,
    };
    assert!(!ir.steps.is_empty());
    for s in &ir.steps {
        let r = launch_compiled(&ir, s, json!({ "s": "; id" }), json!({}));
        let r = r.expect("a matrix value is the Pipeline's own");
        assert!(
            !r.command.iter().any(|a| a.contains("; id")),
            "{:?}",
            r.command
        );
    }
}

// ===================================================== 3. raw IR / scopes

/// A stored scope whose template binding has no declared type, or no caller
/// scope: free text, refused.
#[test]
fn a_raw_scope_binding_without_a_type_or_caller_is_free_text() {
    let untyped = Scope {
        inputs: ScopeInputs::Bound(BTreeMap::from([(
            "r".to_string(),
            Binding {
                param: None,
                value: BoundValue::Template("${{ inputs.s }}".into()),
            },
        )])),
        outputs: BTreeMap::new(),
        matrix: BTreeMap::new(),
        caller: Some(Box::new(Scope::run())),
    };
    let r = launch(
        &declared(),
        &["tool", "${{ inputs.r }}"],
        Some(&untyped),
        json!({ "s": "5" }),
    );
    assert!(r.is_err(), "{r:?}");
    let no_caller = Scope {
        inputs: ScopeInputs::Bound(BTreeMap::from([(
            "r".to_string(),
            Binding::template(
                "${{ inputs.s }}".into(),
                &param("r", ParamType::Number, None),
            ),
        )])),
        outputs: BTreeMap::new(),
        matrix: BTreeMap::new(),
        caller: None,
    };
    let r = launch(
        &declared(),
        &["tool", "${{ inputs.r }}"],
        Some(&no_caller),
        json!({ "s": "5" }),
    );
    assert!(r.is_err(), "{r:?}");
}

/// The engine reads the Run's interface from the stored IR: a compiled IR's
/// `interface.inputs` round-trips through JSON to the same declarations, so
/// the launch classifies as the compiler and the preview did.
#[test]
fn the_stored_interface_round_trips_to_the_same_declarations() {
    let yaml = "interface:\n  inputs:\n    - { name: c, type: choice, options: [\"us-east-1\", \"eu-west-1\"], required: false, default: \"us-east-1\", description: d }\n    - { name: n, type: number, validate: \"value > 0\" }\n    - { name: b, type: boolean }\n    - s\nsteps:\n  - { id: x, image: busybox, command: [tool, \"${{ inputs.c }}\", \"${{ inputs.n }}\", \"${{ inputs.b }}\"], env: [[S, \"${{ inputs.s }}\"]] }\n";
    let ir = ok(yaml, &Libraries::default());
    let stored = serde_json::to_value(&ir).unwrap();
    let back: Vec<ParamSpec> =
        serde_json::from_value(stored["interface"]["inputs"].clone()).unwrap();
    assert_eq!(back, ir.interface.inputs);
    let r = launch_compiled(
        &ir,
        step(&ir, "x"),
        json!({ "c": "eu-west-1", "n": 2, "b": true, "s": "; id" }),
        json!({}),
    )
    .unwrap();
    assert_eq!(r.command, ["tool", "eu-west-1", "2", "true"]);
}

// ================================================== 6. the script check

/// A fixed-shape value in a script is still a compile error (builder
/// decision; a usability cost noted in the report).
#[test]
fn a_fixed_value_in_a_script_is_a_compile_error() {
    for (ty, extra) in [
        ("number", ""),
        ("boolean", ""),
        ("choice", ", options: [a]"),
    ] {
        let yaml = format!(
            "interface: {{ inputs: [{{ name: v, type: {ty}{extra} }}] }}\nsteps:\n  - {{ id: x, image: busybox, command: [sh, -c, \"echo ${{{{ inputs.v }}}}\"] }}\n"
        );
        assert!(compile(&yaml, &Libraries::default()).is_err(), "{ty}");
    }
}

/// Free text in any command position is a compile error, not only at launch.
#[test]
fn free_text_in_a_command_is_a_compile_error() {
    for c in [
        "[tool, \"${{ inputs.s }}\"]",
        "[tool, \"--x=${{ inputs.s }}\"]",
        "[\"${{ inputs.s }}\"]",
        "[tool, \"${{ dyn(inputs).c }}\"]",
        "[tool, \"${{ inputs.b ? inputs.c : 'x' }}\"]",
    ] {
        let yaml = format!(
            "interface: {{ inputs: [s, {{ name: b, type: boolean }}, {{ name: c, type: choice, options: [a] }}] }}\nsteps:\n  - {{ id: x, image: busybox, command: {c} }}\n"
        );
        // `dyn(inputs).c` and the ternary are refused at launch as free text
        // / computed text; the compiler must not accept a command the
        // launch always refuses for free text.
        let r = compile(&yaml, &Libraries::default());
        if c.contains("inputs.s") || c.contains("dyn(") {
            assert!(r.is_err(), "{c} compiled");
        }
    }
}

// ============================================ 7. fields that are not rendered

/// The only Step templates rendered are image, command and env. A template in
/// a service command/args/env/ready probe, a shared service, a build field,
/// an artifact path or a cache key is refused at compile or kept as its
/// literal text; nothing substitutes an outside value into it.
#[test]
fn other_step_fields_never_hold_an_outside_value() {
    let t = "${{ inputs.s }}";
    let cases = [
        format!("services: [{{ image: postgres, command: [\"{t}\"], args: [\"{t}\"], env: {{ V: \"{t}\" }}, ready: {{ exec: [\"{t}\"] }} }}]"),
        format!("artifacts: [\"{t}\"]"),
        format!("cache: {{ dirs: [\"{t}\"], key: [\"{t}\"] }}"),
    ];
    let mut compiled = 0;
    for extra in &cases {
        let yaml = format!("interface: {{ inputs: [s] }}\nsteps:\n  - {{ id: x, image: busybox, command: [tool], {extra} }}\n");
        if let Ok(ir) = compile(&yaml, &Libraries::default()) {
            compiled += 1;
            let j = serde_json::to_string(step(&ir, "x")).unwrap();
            assert!(j.contains(t), "{extra}: field was substituted: {j}");
        }
    }
    // A pipeline-level shared service.
    let yaml = format!("interface: {{ inputs: [s] }}\nservices: [{{ name: db, image: postgres, command: [\"{t}\"] }}]\nsteps:\n  - {{ id: x, image: busybox, command: [tool], uses: [db] }}\n");
    if let Ok(ir) = compile(&yaml, &Libraries::default()) {
        compiled += 1;
        assert!(serde_json::to_string(&ir.services).unwrap().contains(t));
    }
    // A library's service command with a literal `with:` value: never pasted.
    let lib = "kind: library\ninterface: { inputs: [{ name: t, type: string }] }\nsteps:\n  - { id: s, image: busybox, command: [tool], services: [{ image: postgres, command: [\"${{ inputs.t }}\"] }] }\n";
    let yaml2 = "steps:\n  - { id: c, invoke: file:l.yaml, with: { t: \"; id\" } }\n";
    if let Ok(ir) = compile(yaml2, &files(&[("l.yaml", lib)])) {
        compiled += 1;
        let j = serde_json::to_string(&step(&ir, "c/s").services).unwrap();
        assert!(
            !j.contains("; id"),
            "a with: value was pasted into a service: {j}"
        );
    }
    // Not vacuous: most of these shapes compile.
    assert!(compiled >= 3, "only {compiled} cases compiled");
}
