//! Red-team round 1 on "lexical scopes, render once" (ADR-0076, HEAD
//! 99d3e7e). Each test asserts the SECURE behaviour: a failing test is a
//! break. Tests named `survived_` are attacks that held.

use std::collections::BTreeMap;
use std::time::Instant;

use scarab_pipeline::{
    compile_with_libraries, excluded_steps_with_params, lint, oci, preview_image, render, validate,
    Facts, Libraries, PipelineError, PipelineIr, Results, StepSpec, Templates,
};
use serde_json::{json, Value};

fn files(entries: &[(&str, &str)]) -> Libraries {
    Libraries {
        files: entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        installed: BTreeMap::new(),
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
    ir.steps.iter().find(|s| s.id == id).unwrap_or_else(|| {
        panic!(
            "no step `{id}` in {:?}",
            ir.steps.iter().map(|s| &s.id).collect::<Vec<_>>()
        )
    })
}

fn map(v: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

/// The engine at launch: the one renderer, in the Step's scope.
fn launch(
    spec: &StepSpec,
    params: Value,
    outputs: Value,
) -> Result<scarab_pipeline::Rendered, String> {
    let params = map(params);
    let results: Results = serde_json::from_value(outputs).unwrap();
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
            results: &results,
        },
    )
}

fn all_text(r: &scarab_pipeline::Rendered) -> String {
    let mut s = r.image.clone();
    for c in &r.command {
        s.push('\n');
        s.push_str(c);
    }
    for (k, v) in &r.env {
        s.push('\n');
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s
}

// ============================================================ 1. re-render

/// A value assembled from two halves, `$` + `{{ inputs.secret }}` or
/// `${` + `{ inputs.secret }}`, by a caller's `with:` template, then read by
/// a library Step and passed on through a second level's exact `with:`.
/// Whatever text the halves build is data at every hop.
#[test]
fn survived_split_template_halves_assembled_across_two_levels_stay_text() {
    let l2 = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - id: s\n    image: busybox\n    command: [\"${{ inputs.v }}\", \"x${{ inputs.v }}x\"]\n    env: [[E, \"${{ inputs.v }}\"]]\n";
    let l1 = "kind: library\ninterface: { inputs: [{ name: a, type: string }, { name: b, type: string }] }\nsteps:\n  - { id: n, invoke: file:l2.yaml, with: { v: \"${{ inputs.a }}${{ inputs.b }}\" } }\n  - id: m\n    invoke: file:l2.yaml\n    with: { v: \"${{ inputs.a + inputs.b }}\" }\n";
    let libs = files(&[("l1.yaml", l1), ("l2.yaml", l2)]);
    for (a, b) in [
        ("$", "{{ inputs.secret }}"),
        ("${", "{ inputs.secret }}"),
        ("$$", "{{ inputs.secret }}"),
        ("${{", " inputs.secret }}"),
    ] {
        let yaml = format!(
            "interface: {{ inputs: [{{ name: pa, type: string }}, {{ name: pb, type: string }}, {{ name: secret, type: string }}] }}\nsteps:\n  - {{ id: c, invoke: file:l1.yaml, with: {{ a: \"${{{{ inputs.pa }}}}\", b: \"${{{{ inputs.pb }}}}\" }} }}\n"
        );
        let ir = ok(&yaml, &libs);
        let want = format!("{a}{b}");
        for id in ["c/n/s", "c/m/s"] {
            let r = launch(
                step(&ir, id),
                json!({ "pa": a, "pb": b, "secret": "LEAKED" }),
                json!({}),
            )
            .unwrap_or_else(|e| panic!("{id}: {e}"));
            assert!(!all_text(&r).contains("LEAKED"), "{id}: {r:?}");
            assert_eq!(r.command[0], want, "{id}");
            assert_eq!(r.command[1], format!("x{want}x"), "{id}");
        }
    }
}

/// A Step's results, a matrix value, a literal `with:`, a default, and a
/// Run parameter, each holding a template that would read `inputs.secret`,
/// reach image (refused as an image), command, env and `SCARAB_PARAM_*`
/// as text.
#[test]
fn survived_values_holding_templates_reach_every_rendered_field_as_text() {
    let evil = "${{ inputs.secret }}";
    let lib = "kind: library\ninterface:\n  inputs:\n    - { name: lit, type: string }\n    - { name: def, type: string, required: false, default: \"x\" }\n    - { name: out, type: string }\nsteps:\n  - id: s\n    image: busybox\n    command: [\"${{ inputs.lit }}\", \"${{ inputs.def }}\", \"${{ inputs.out }}\"]\n    env: [[A, \"${{ inputs.out }}\"]]\n";
    // A literal `with:` holding `$${{` is a template (it holds `${{`).
    let yaml = "interface: { inputs: [{ name: secret, type: string }, { name: p, type: string }] }\nsteps:\n  - { id: b, image: busybox, results: [v] }\n  - id: c\n    invoke: file:l.yaml\n    needs: [b]\n    with: { lit: \"${{ inputs.p }}\", out: \"${{ outputs.b.v }}\" }\n  - id: d\n    image: busybox\n    needs: [b]\n    command: [\"${{ inputs.p }}\", \"${{ outputs.b.v }}\", \"${{ matrix.m }}\"]\n    matrix: { dimensions: { m: [\"${{ inputs.secret }}\"] } }\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    let params = json!({ "secret": "LEAKED", "p": evil });
    let outs = json!({ "b": { "v": evil } });
    let r = launch(step(&ir, "c/s"), params.clone(), outs.clone()).unwrap();
    assert!(!all_text(&r).contains("LEAKED"), "{r:?}");
    assert_eq!(r.command, [evil, "x", evil]);
    let d = ir
        .steps
        .iter()
        .find(|s| s.id.starts_with("d["))
        .expect("matrix leg");
    // A Pipeline Step gets every Run parameter as `SCARAB_PARAM_*` (by
    // design), so only its rendered fields are checked.
    let r = launch(d, params.clone(), outs.clone()).unwrap();
    assert!(!r.command.concat().contains("LEAKED"), "{r:?}");
    assert_eq!(r.command, [evil, evil, evil]);
    // The same value as an image is refused, never rendered again.
    let mut spec = d.clone();
    spec.image = "${{ inputs.p }}".into();
    let e = launch(&spec, params, outs).unwrap_err();
    assert!(e.contains("not an OCI image reference"), "{e}");
}

// ============================================================ 2. scope escape

/// Three levels, each with an input named `flag` holding a different value,
/// and an invoke `when` at each level reading `inputs.flag`. Each guard must
/// read the scope it was written in (caller_hops), never a neighbour's.
#[test]
fn survived_guards_at_three_levels_read_their_own_scope() {
    // l3: one Step. l2 calls l3 under its own `when`.
    let l3 = "kind: library\ninterface: { inputs: [{ name: flag, type: string }] }\nsteps:\n  - { id: s3, image: busybox, when: \"inputs.flag == 'L3'\" }\n";
    let l2 = "kind: library\ninterface: { inputs: [{ name: flag, type: string }] }\nsteps:\n  - { id: s2, image: busybox, when: \"inputs.flag == 'L2'\" }\n  - { id: go, invoke: file:l3.yaml, when: \"inputs.flag == 'L2'\", with: { flag: L3 } }\n  - { id: no, invoke: file:l3.yaml, when: \"inputs.flag == 'L3'\", with: { flag: L3 } }\n";
    let l1 = "kind: library\ninterface: { inputs: [{ name: flag, type: string }] }\nsteps:\n  - { id: go, invoke: file:l2.yaml, when: \"inputs.flag == 'L1'\", with: { flag: \"${{ inputs.flag == 'L1' ? 'L2' : 'bad' }}\" } }\n";
    let yaml = "interface: { inputs: [{ name: flag, type: string }] }\nsteps:\n  - { id: top, invoke: file:l1.yaml, with: { flag: \"${{ inputs.flag }}\" } }\n";
    let ir = ok(
        yaml,
        &files(&[("l1.yaml", l1), ("l2.yaml", l2), ("l3.yaml", l3)]),
    );
    let ex = excluded_steps_with_params(&ir, &json!({}), &map(json!({ "flag": "L1" }))).unwrap();
    let mut ex: Vec<&str> = ex.iter().map(String::as_str).collect();
    ex.sort();
    assert_eq!(ex, ["top/go/no/s3"], "{:#?}", ir.steps.iter().map(|s| (&s.id, &s.guards)).collect::<Vec<_>>());
    // A Run parameter that is not `L1` turns off everything under l1's invoke.
    let ex = excluded_steps_with_params(&ir, &json!({}), &map(json!({ "flag": "L1x" }))).unwrap();
    assert_eq!(ex.len(), 3, "{ex:?}");
}

/// A library reads `event`, `secrets`, and a name only its caller binds.
/// None is in a library scope: compile refuses or launch fails, never a
/// value.
#[test]
fn survived_library_cannot_read_event_secrets_or_caller_names() {
    for expr in ["event.branch", "secrets.TOKEN", "caller.inputs.secret", "inputs.secret"] {
        let lib = format!(
            "kind: library\ninterface: {{ inputs: [{{ name: v, type: string }}] }}\nsteps: [{{ id: s, image: busybox, command: [\"${{{{ {expr} }}}}\"] }}]\n"
        );
        let yaml = "interface: { inputs: [{ name: secret, type: string }] }\nsteps: [{ id: c, invoke: file:l.yaml, with: { v: x } }]\n";
        match compile(yaml, &files(&[("l.yaml", &lib)])) {
            Err(_) => {}
            Ok(ir) => {
                let r = launch(step(&ir, "c/s"), json!({ "secret": "LEAKED" }), json!({}));
                assert!(r.is_err(), "{expr}: {r:?}");
            }
        }
    }
}

// =================================================== 3. typed coercion/validate

/// A caller's exact template gives a typed value that the input does not
/// declare, or one that only `validate:` stops. Each must fail the launch.
#[test]
fn survived_template_bindings_are_typed_and_validated_at_launch() {
    let cases: &[(&str, Value)] = &[
        // number input, validate < 100: a u64 past i64, a huge float, a
        // numeric string past i64, a bool, a list.
        ("{ name: n, type: number, validate: \"value < 100\" }", json!(18446744073709551615u64)),
        ("{ name: n, type: number, validate: \"value < 100\" }", json!(1e300)),
        ("{ name: n, type: number, validate: \"value < 100\" }", json!("99999999999999999999")),
        ("{ name: n, type: number, validate: \"value < 100\" }", json!(true)),
        ("{ name: n, type: number, validate: \"value < 100\" }", json!([1])),
        // string input: a map, a list, a bool, null.
        ("{ name: n, type: string }", json!({ "a": 1 })),
        ("{ name: n, type: string }", json!(["--x"])),
        ("{ name: n, type: string }", json!(false)),
        ("{ name: n, type: string }", json!(null)),
        // choice outside its options, or not a string.
        ("{ name: n, type: choice, options: [a, b] }", json!("c")),
        ("{ name: n, type: choice, options: [a, b] }", json!(1)),
        // boolean from a number.
        ("{ name: n, type: boolean }", json!(1)),
        // string with validate refusing a leading dash.
        ("{ name: n, type: string, validate: \"!value.startsWith('-')\" }", json!("--upload-pack=x")),
    ];
    for (spec, value) in cases {
        let lib = format!(
            "kind: library\ninterface: {{ inputs: [{spec}] }}\nsteps: [{{ id: s, image: busybox, command: [\"${{{{ inputs.n }}}}\"] }}]\n"
        );
        let yaml = "steps:\n  - { id: b, image: busybox, results: [v] }\n  - { id: c, invoke: file:l.yaml, needs: [b], with: { n: \"${{ outputs.b.v }}\" } }\n";
        let ir = ok(yaml, &files(&[("l.yaml", &lib)]));
        let r = launch(step(&ir, "c/s"), json!({}), json!({ "b": { "v": value } }));
        assert!(r.is_err(), "{spec} <- {value}: launched {r:?}");
    }
}

// ======================================================== 4. image / digest

/// An image whose name is a template and whose digest is literal previews
/// with that digest, and grant admission (`rsplit_once('@')`) reads it as
/// pinned. Whatever the template renders to at launch, a launched image must
/// carry exactly that digest.
#[test]
fn survived_a_templated_name_cannot_change_a_literal_digest() {
    const D: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let yaml = format!(
        "steps:\n  - {{ id: b, image: busybox, results: [n] }}\n  - {{ id: s, image: \"${{{{ outputs.b.n }}}}@{D}\", needs: [b] }}\n"
    );
    let ir = ok(&yaml, &Libraries::default());
    let s = step(&ir, "s");
    let pre = preview_image(&s.image, s.scope.as_ref(), &s.matrix_values, &BTreeMap::new());
    assert!(pre.contains("${{"), "{pre}");
    for n in [
        "evil.io/x",
        "evil.io/x@sha256:ffff",
        "evil.io/x@sha256:ffff#",
        "x@",
        "x\n",
        "x:tag@sha256:ffff",
        "x@sha256:ffff@",
        "x ",
        "--privileged x",
        "x\u{0}",
    ] {
        match launch(s, json!({}), json!({ "b": { "n": n } })) {
            Err(_) => {}
            Ok(r) => {
                let (_, digest) = r.image.split_once('@').expect("digest");
                assert_eq!(digest, D, "{n:?} -> {}", r.image);
                assert!(oci::is_image_reference(&r.image));
            }
        }
    }
}

/// OCI reference tricks a rendered value can hold. Each must be refused.
#[test]
fn survived_oci_reference_tricks_are_refused() {
    for bad in [
        "busybox\t",
        "busybox\r",
        "bus\u{200b}box",
        "ｂusybox",
        "busybox:1.36 --privileged",
        "busybox@sha256:abc\n",
        "evil.io:/x",
        "evil.io:65536x/x",
        "[::1/x",
        "-evil.io/x",
        "evil-.io/x",
        "a/-b",
        "a/b-",
        "a/b_",
        "a/b___c",
        "a:b/c",
        "@sha256:abc",
        ":tag",
        "a@sha256:",
        "a@:abc",
        "a@sha256:abc:def",
        "a@sha256:a/b",
        "a@sha256:a.b",
    ] {
        assert!(!oci::is_image_reference(bad), "accepted {bad:?}");
    }
}

/// Preview and launch with the same values give the same image (no
/// TOCTOU between what admission checks and what runs).
#[test]
fn survived_preview_and_launch_agree_on_known_values() {
    let lib = "kind: library\ninterface: { inputs: [{ name: d, type: string }, { name: t, type: string }] }\nsteps:\n  - { id: s, image: \"app:${{ inputs.t }}@${{ inputs.d }}\", matrix: { dimensions: { m: [a] } } }\n";
    let yaml = "interface: { inputs: [{ name: d, type: string }] }\nsteps:\n  - { id: c, invoke: file:l.yaml, with: { d: \"sha256:abc\", t: \"${{ inputs.d }}\" } }\n  - { id: p, image: \"app@${{ inputs.d }}\" }\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    let params = map(json!({ "d": "sha256:fff" }));
    for s in &ir.steps {
        let pre = preview_image(&s.image, s.scope.as_ref(), &s.matrix_values, &params);
        let at = launch(s, json!({ "d": "sha256:fff" }), json!({}))
            .map(|r| r.image)
            .unwrap_or_else(|e| e);
        if !pre.contains("${{") {
            assert_eq!(pre, at, "{}", s.id);
        }
    }
}

// =========================================== 5. leading `-` for outside values

/// The owner decision for this design (AFK run.md B9): "argv/env no
/// escaping (leading `-` refused for outside values)". A Run parameter is
/// supplied by whoever dispatches the Run. Rendered into an argument, a
/// leading `-` makes it an option: `git fetch origin --upload-pack=…` runs a
/// command.
#[test]
#[ignore = "red: render r1"]
fn a_run_parameter_with_a_leading_dash_is_refused_in_argv() {
    let yaml = "interface: { inputs: [{ name: ref, type: string }] }\non: { manual: {} }\nsteps:\n  - { id: s, image: alpine/git, command: [git, fetch, origin, \"${{ inputs.ref }}\"] }\n";
    let ir = ok(yaml, &Libraries::default());
    let r = launch(
        step(&ir, "s"),
        json!({ "ref": "--upload-pack=touch /tmp/pwned" }),
        json!({}),
    );
    assert!(
        r.is_err(),
        "an outside value with a leading `-` reached argv: {:?}",
        r.map(|r| r.command)
    );
}

/// The same value passed through a library `with:`: the library's argument
/// is still the outsider's text.
#[test]
#[ignore = "red: render r1"]
fn a_run_parameter_with_a_leading_dash_is_refused_through_a_library() {
    let lib = "kind: library\ninterface: { inputs: [{ name: r, type: string }] }\nsteps: [{ id: s, image: alpine/git, command: [git, fetch, origin, \"${{ inputs.r }}\"] }]\n";
    let yaml = "interface: { inputs: [{ name: ref, type: string }] }\nsteps: [{ id: c, invoke: file:l.yaml, with: { r: \"${{ inputs.ref }}\" } }]\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    let r = launch(
        step(&ir, "c/s"),
        json!({ "ref": "--upload-pack=touch /tmp/pwned" }),
        json!({}),
    );
    assert!(
        r.is_err(),
        "an outside value with a leading `-` reached a library's argv: {:?}",
        r.map(|r| r.command)
    );
}

// ======================================================== 6. script lint

fn script_findings(command: &str) -> (Vec<String>, Result<(), Vec<String>>) {
    let yaml = format!(
        "interface: {{ inputs: [{{ name: p, type: string }}] }}\nsteps: [{{ id: s, image: busybox, command: {command} }}]\n"
    );
    let ir = ok(&yaml, &Libraries::default());
    let warns: Vec<String> = lint(&ir)
        .into_iter()
        .filter(|w| w.contains("shell script"))
        .collect();
    (warns, validate(&ir))
}

/// Shell invocations whose script holds `${{ inputs.p }}`. Each must be
/// reported (warning or error).
#[test]
#[ignore = "red: render r1"]
fn script_lint_sees_every_shell_invocation_form() {
    let mut missed = Vec::new();
    for cmd in [
        r#"[sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[sh, -ec, "echo ${{ inputs.p }}"]"#,
        r#"[/bin/sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[bash, --norc, -c, "echo ${{ inputs.p }}"]"#,
        r#"[bash, -o, pipefail, -c, "echo ${{ inputs.p }}"]"#,
        r#"[env, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[/usr/bin/env, bash, -c, "echo ${{ inputs.p }}"]"#,
        r#"[busybox, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[bash, -O, extglob, -c, "echo ${{ inputs.p }}"]"#,
        r#"[bash, --rcfile, /dev/null, -c, "echo ${{ inputs.p }}"]"#,
        r#"[python3, -c, "import os; os.system('${{ inputs.p }}')"]"#,
        r#"[timeout, "60", sh, -c, "echo ${{ inputs.p }}"]"#,
    ] {
        let (warns, errs) = script_findings(cmd);
        if warns.is_empty() && errs.is_ok() {
            missed.push(cmd);
        }
    }
    assert!(missed.is_empty(), "script lint missed: {missed:#?}");
}

/// The owner decision (B9): `${{ }}` inside `sh -c` is a warning for the
/// Pipeline's own values and an error for outside values. A Run parameter
/// is outside.
#[test]
#[ignore = "red: render r1"]
fn an_outside_value_in_a_shell_script_is_an_error() {
    let (_, errs) = script_findings(r#"[sh, -c, "echo ${{ inputs.p }}"]"#);
    assert!(errs.is_err(), "a Run parameter in `sh -c` compiled");
}

/// A library Step's script reading an input that its caller binds to a Run
/// parameter carries an outside value. The check names its source as the
/// library (Pipeline), so once outside values are errors this one is not.
#[test]
#[ignore = "red: render r1"]
fn a_library_script_reading_a_run_parameter_through_with_is_outside() {
    let lib = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps: [{ id: s, image: busybox, command: [sh, -c, \"echo ${{ inputs.v }}\"] }]\n";
    let yaml = "interface: { inputs: [{ name: p, type: string }] }\nsteps: [{ id: c, invoke: file:l.yaml, with: { v: \"${{ inputs.p }}\" } }]\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    // Same severity as the Pipeline-level form.
    let direct = script_findings(r#"[sh, -c, "echo ${{ inputs.p }}"]"#).1.is_err();
    assert_eq!(
        validate(&ir).is_err(),
        direct,
        "a Run parameter reaches a library script at a different severity than in the Pipeline"
    );
    assert!(validate(&ir).is_err(), "outside value through `with:` in `sh -c` compiled");
}

// ========================================================= 7. stored IR

/// A hand-built IR (an API caller, a stored row) with guards that point
/// past the scope chain, or with no scope at all. Never a panic or hang.
#[test]
fn survived_hand_crafted_guards_fail_closed() {
    let origin = json!([{ "kind": "file", "name": "l.yaml", "source": "x", "content_hash": "h" }]);
    for (guards, scope) in [
        (json!([{ "caller_hops": usize::MAX, "when": "true" }]), json!({ "inputs": "run" })),
        (json!([{ "caller_hops": 3, "when": "true" }]), json!({ "inputs": "run" })),
        (json!([{ "caller_hops": 0, "when": "true" }]), Value::Null),
    ] {
        let ir = json!({
            "ir_version": 3,
            "steps": [{ "id": "s", "image": "busybox", "origin": origin, "guards": guards, "scope": scope }]
        });
        let ir: PipelineIr = match serde_json::from_value(ir) {
            Ok(ir) => ir,
            Err(_) => continue,
        };
        let t = Instant::now();
        let r = excluded_steps_with_params(&ir, &json!({}), &BTreeMap::new());
        assert!(t.elapsed().as_secs() < 2);
        assert!(r.is_err(), "{guards} {scope}: {r:?}");
    }
}

/// A stored scope nested past serde's recursion limit is refused on load,
/// and the deepest one that loads renders without exhausting the stack.
#[test]
fn survived_deep_stored_caller_chains() {
    fn chain(depth: usize) -> Value {
        let mut s = json!({ "inputs": "run" });
        for _ in 0..depth {
            s = json!({
                "inputs": { "bound": { "v": {
                    "param": { "name": "v", "type": "string" },
                    "value": { "template": "${{ inputs.v }}" }
                } } },
                "caller": s
            });
        }
        s
    }
    let mut deepest_ok = 0;
    for depth in [8, 16, 32, 60, 100, 1000] {
        let spec = json!({ "id": "s", "image": "busybox", "command": ["${{ inputs.v }}"], "scope": chain(depth) });
        // Postgres hands the jsonb back as text that serde_json parses with
        // its recursion limit (sqlx `Value` decode), then `from_value`.
        let text = serde_json::to_string(&spec).unwrap();
        // Dropping a 1000-deep `Value` recurses too; this is the test's
        // own value, not one production builds.
        std::mem::forget(spec);
        let parsed = serde_json::from_str::<Value>(&text)
            .and_then(serde_json::from_value::<StepSpec>);
        let Ok(spec) = parsed else {
            continue;
        };
        deepest_ok = depth;
        // The innermost Run scope gives v; every level renders it.
        let r = launch(&spec, json!({ "v": "ok" }), json!({})).unwrap();
        assert_eq!(r.command, ["ok"]);
    }
    assert!((8..1000).contains(&deepest_ok), "{deepest_ok}");
}

// ============================================================== 8. DoS

/// Every inlined Step stores a full copy of its scope chain, including each
/// binding's ParamSpec (description, validate, options). A library with
/// many large inputs and many Steps multiplies the source size by the
/// Step count in the compiled IR, which is stored per Step on the Run.
#[test]
#[ignore = "red: render r1"]
fn compiled_ir_is_not_amplified_by_scope_copies() {
    let desc = "d".repeat(1000);
    let mut inputs = String::new();
    for i in 0..100 {
        inputs.push_str(&format!(
            "    - {{ name: i{i}, type: string, required: false, default: x, description: \"{desc}\" }}\n"
        ));
    }
    let mut steps = String::new();
    for i in 0..200 {
        steps.push_str(&format!("  - {{ id: s{i}, image: busybox }}\n"));
    }
    let lib = format!("kind: library\ninterface:\n  inputs:\n{inputs}steps:\n{steps}");
    let yaml = "steps:\n  - { id: a, invoke: file:l.yaml }\n  - { id: b, invoke: file:l.yaml }\n";
    let src = lib.len() + yaml.len();
    let t = Instant::now();
    let ir = ok(yaml, &files(&[("l.yaml", &lib)]));
    let took = t.elapsed();
    let out = serde_json::to_vec(&ir).unwrap().len();
    let ratio = out as f64 / src as f64;
    eprintln!("source {src} B -> IR {out} B (x{ratio:.0}) in {took:?}");
    assert!(
        ratio < 20.0,
        "a {src} B source compiled to a {out} B IR (x{ratio:.0})"
    );
}

/// A guard storm: a 1024-Step pipeline whose Steps each carry the maximum
/// guard depth evaluates within a bounded time at Run creation.
#[test]
fn survived_guard_storm_is_bounded() {
    let mut libs = Vec::new();
    for d in 0..7 {
        let next = if d == 6 {
            "  - { id: s, image: busybox, when: \"inputs.f == 'y'\" }\n".to_string()
        } else {
            format!(
                "  - {{ id: n, invoke: file:l{}.yaml, when: \"inputs.f == 'y'\", with: {{ f: \"${{{{ inputs.f }}}}\" }} }}\n",
                d + 1
            )
        };
        libs.push((
            format!("l{d}.yaml"),
            format!("kind: library\ninterface: {{ inputs: [{{ name: f, type: string }}] }}\nsteps:\n{next}"),
        ));
    }
    let mut yaml = "interface: { inputs: [{ name: f, type: string }] }\nsteps:\n".to_string();
    for i in 0..100 {
        yaml.push_str(&format!(
            "  - {{ id: c{i}, invoke: file:l0.yaml, with: {{ f: \"${{{{ inputs.f }}}}\" }} }}\n"
        ));
    }
    let entries: Vec<(&str, &str)> = libs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let ir = ok(&yaml, &files(&entries));
    let t = Instant::now();
    let ex = excluded_steps_with_params(&ir, &json!({}), &map(json!({ "f": "y" }))).unwrap();
    let took = t.elapsed();
    eprintln!("{} steps, {} excluded, guards evaluated in {took:?}", ir.steps.len(), ex.len());
    assert!(ex.is_empty());
    assert!(took.as_secs() < 30, "guards took {took:?}");
}

/// The same amplification at the Step cap: one library of 1000 Steps and
/// 100 inputs with 4 KB descriptions (a ~0.4 MB file) compiles to an IR
/// whose size, stored with the Run and again per Step, is bounded.
#[test]
#[ignore = "red: render r1"]
fn compiled_ir_at_the_step_cap_is_bounded() {
    let desc = "d".repeat(4000);
    let mut inputs = String::new();
    for i in 0..100 {
        inputs.push_str(&format!(
            "    - {{ name: i{i}, type: string, required: false, default: x, description: \"{desc}\" }}\n"
        ));
    }
    let mut steps = String::new();
    for i in 0..1000 {
        steps.push_str(&format!("  - {{ id: s{i}, image: busybox }}\n"));
    }
    let lib = format!("kind: library\ninterface:\n  inputs:\n{inputs}steps:\n{steps}");
    let yaml = "steps: [{ id: a, invoke: file:l.yaml }]\n";
    let src = lib.len() + yaml.len();
    let t = Instant::now();
    let ir = ok(yaml, &files(&[("l.yaml", &lib)]));
    let took = t.elapsed();
    let out = serde_json::to_vec(&ir).unwrap().len();
    eprintln!("source {src} B -> IR {out} B in {took:?}");
    assert!(out < 64 << 20, "a {src} B source compiled to a {out} B IR in {took:?}");
}
