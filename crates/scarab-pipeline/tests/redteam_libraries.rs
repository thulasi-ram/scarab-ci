//! Red-team attacks on ADR-0076 step 1 (invoke prefixes, `kind: library`,
//! compile-time input binding, invoke-field carry, `origin`).
//!
//! Tests prefixed `survived_` are attacks the first version withstood. The
//! others each began as the evidence for one defect, named in its comment,
//! and now say what is true after the fix.

use std::collections::BTreeMap;

use scarab_pipeline::{
    compile_with_libraries, excluded_steps, render, Facts, InstalledLibrary, Libraries, OriginKind,
    PipelineError, PipelineIr, Rendered, Results, StepSpec, Templates,
};
use serde_json::json;

// ---------------------------------------------------------------- helpers

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

fn with_installed(mut l: Libraries, entries: &[(&str, &str, &str)]) -> Libraries {
    for (name, content, source) in entries {
        l.installed.insert(
            name.to_string(),
            InstalledLibrary {
                content: content.to_string(),
                source: source.to_string(),
            },
        );
    }
    l
}

fn ok(yaml: &str, libs: &Libraries) -> PipelineIr {
    match compile_with_libraries(yaml, libs) {
        Ok(ir) => ir,
        Err(e) => panic!("expected a valid pipeline, got {e}"),
    }
}

fn errs(yaml: &str, libs: &Libraries) -> Vec<String> {
    match compile_with_libraries(yaml, libs) {
        Ok(ir) => panic!("expected a compile error, got {:#?}", ir.steps),
        Err(PipelineError::Validation(d)) => d,
        Err(e) => vec![e.to_string()],
    }
}

fn step<'a>(ir: &'a PipelineIr, id: &str) -> &'a StepSpec {
    ir.steps.iter().find(|s| s.id == id).unwrap_or_else(|| {
        panic!(
            "no step `{id}` in {:?}",
            ir.steps.iter().map(|s| &s.id).collect::<Vec<_>>()
        )
    })
}

/// What the engine does at launch: the one renderer, in the Step's scope,
/// with the Run's launch parameters `run_params` (which a library Step must
/// never read) and no upstream results.
fn launch(spec: &StepSpec, run_params: serde_json::Value) -> Result<Rendered, String> {
    let params: BTreeMap<String, serde_json::Value> = serde_json::from_value(run_params).unwrap();
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
            declared: &[],
            results: &Results::new(),
            event: &serde_json::Map::new(),
        },
    )
}

/// A one-input string library whose single step runs `cmd` as its command
/// and is guarded by `when`.
fn lib_cmd(input_spec: &str, cmd: &str) -> String {
    format!(
        "kind: library\ninterface:\n  inputs:\n    - {input_spec}\nsteps:\n  - id: s\n    image: busybox\n    command: [{cmd}]\n"
    )
}

fn caller(with: &str) -> String {
    format!("steps:\n  - id: c\n    invoke: file:l.yaml\n    with: {with}\n")
}

/// Is library step `c/s` kept (when true) under an empty event context?
fn kept(ir: &PipelineIr) -> bool {
    !excluded_steps(ir, &json!({ "event": {} }))
        .unwrap()
        .contains(&"c/s".to_string())
}

fn lib_when(input_spec: &str, when: &str) -> String {
    format!(
        "kind: library\ninterface:\n  inputs:\n    - {input_spec}\nsteps:\n  - id: s\n    image: busybox\n    when: {when}\n"
    )
}

// ============================================================ FIXED

/// Was: a `//` comment line that ends in `.` made the scanner treat the next
/// `inputs` as a field (`x.inputs`), so the read was not bound and at launch
/// read the RUN's launch parameter `tag`. The scanner now finds the token
/// before a `.` by skipping comments.
#[test]
fn a_comment_ending_in_a_dot_does_not_hide_an_inputs_read() {
    let lib = lib_cmd(
        "{ name: tag, type: string }",
        "\"${{ // pick the tag.\\n inputs.tag }}\"",
    );
    let ir = ok(&caller("{ tag: from-with }"), &files(&[("l.yaml", &lib)]));
    let s = step(&ir, "c/s");
    let launched = launch(s, json!({ "tag": "RUN-PARAM" })).unwrap();
    assert_eq!(
        launched.command[0], "from-with",
        "library inputs.tag must bind to with:, got {:?} (IR command {:?})",
        launched.command, s.command
    );
}

/// Was: a library input whose `default` failed its own `validate:` passed
/// every check, `resolve_params` failed, and NOTHING was bound, with no
/// diagnostic: `${{ inputs.env }}` read the Run's parameter. A default is now
/// checked against its `validate:`, and a resolve error that no other check
/// reported is a diagnostic.
#[test]
fn a_default_failing_its_own_validate_is_refused() {
    let lib = lib_cmd(
        "{ name: env, type: string, required: false, default: prod, validate: \"value != 'prod'\" }",
        "\"${{ inputs.env }}\"",
    );
    let e = errs(&caller("{}"), &files(&[("l.yaml", &lib)]));
    assert!(
        e.iter().any(|e| e.contains("input")
            || e.contains("parameter `env` default \"prod\" fails its own validate")),
        "{e:?}"
    );
    // The same rule holds for a Pipeline's own launch parameters.
    let e = errs(
        "interface:\n  inputs: [{ name: env, type: string, required: false, default: prod, validate: \"value != 'prod'\" }]\nsteps: [{ id: a, image: busybox }]\n",
        &Libraries::default(),
    );
    assert!(
        e.iter()
            .any(|e| e.contains("parameter `env` default \"prod\" fails its own validate")),
        "{e:?}"
    );
}

/// Was: a `with:` value `${{ outputs.build.v }}` was bound into EVERY
/// inlined step (and its SCARAB_PARAM_* env), but only the library's roots
/// needed `build`; the scheduler builds `outputs` from direct needs only, so
/// a non-root step failed at launch. `build` is now a need of every inlined
/// step (fanned over a matrix), and the invoke step must need it.
#[test]
fn a_caller_output_through_with_is_a_need_of_every_inlined_step() {
    let lib = "kind: library\ninterface:\n  inputs: [{ name: v, type: string }]\nsteps:\n  - { id: a, image: busybox }\n  - { id: b, image: busybox, needs: [a], env: [[V, \"${{ inputs.v }}\"]] }\n";
    let l = files(&[("l.yaml", lib)]);
    let yaml = "steps:\n  - { id: build, image: busybox, results: [v] }\n  - id: c\n    invoke: file:l.yaml\n    needs: [build]\n    with: { v: \"${{ outputs.build.v }}\" }\n";
    let ir = ok(yaml, &l);
    for id in ["c/a", "c/b"] {
        let s = step(&ir, id);
        assert!(
            s.needs.0.contains(&"build".to_string()),
            "{id}: {:?}",
            s.needs.0
        );
    }
    assert_eq!(
        step(&ir, "c/b").needs.0,
        vec!["c/a".to_string(), "build".into()]
    );

    // A matrixed `build` fans onto each of its instances.
    let yaml = "steps:\n  - { id: build, image: busybox, matrix: { dimensions: { os: [l, m] } } }\n  - id: c\n    invoke: file:l.yaml\n    needs: [build]\n    with: { v: \"${{ outputs['build[os=l]'].v }}\" }\n";
    let ir = ok(yaml, &l);
    let b = step(&ir, "c/b");
    assert!(
        b.needs.0.contains(&"build[os=l]".to_string())
            && b.needs.0.contains(&"build[os=m]".to_string()),
        "{:?}",
        b.needs.0
    );

    // Reading a Step's outputs through `with:` without needing it is refused.
    let yaml = "steps:\n  - { id: build, image: busybox }\n  - id: c\n    invoke: file:l.yaml\n    with: { v: \"${{ outputs.build.v }}\" }\n";
    let e = errs(yaml, &l);
    assert!(
        e.iter()
            .any(|e| e == "step `c`: `with:` reads `outputs.build` but does not `needs: [build]`"),
        "{e:?}"
    );
}

/// Was: `format!("({outer}) && ({inner})")` put the closing `)` on the line
/// of a `//` comment, so a valid `when:` ending in a comment no longer
/// parsed once ANDed. Each side now sits on its own lines.
#[test]
fn a_when_ending_in_a_comment_is_anded_on_both_sides() {
    let lib = "kind: library\nsteps:\n  - { id: s, image: busybox, when: \"false // never\" }\n";
    let yaml = "steps:\n  - { id: c, invoke: file:l.yaml, when: \"true // always\" }\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    assert!(!kept(&ir), "{:?}", step(&ir, "c/s").when);
}

/// Was: library-level `environment:`, `concurrency:`, `services:`,
/// `budget:` and `retention_profile:` were parsed and dropped without a
/// word, so a library saying `environment: production` ran with none of its
/// governance. Each is now refused in a library.
#[test]
fn pipeline_level_fields_in_a_library_are_refused() {
    for (field, yaml) in [
        ("environment", "environment: production"),
        ("concurrency", "concurrency: { group: g }"),
        ("services", "services: [{ name: db, image: pg }]"),
        ("budget", "budget: 60"),
        ("retention_profile", "retention_profile: short"),
    ] {
        let lib = format!("kind: library\n{yaml}\nsteps: [{{ id: s, image: busybox }}]\n");
        let e = errs(
            "steps: [{ id: c, invoke: file:l.yaml }]",
            &files(&[("l.yaml", &lib)]),
        );
        assert!(
            e.iter().any(|e| e.contains(&format!(
                "library `l.yaml` sets `{field}`, which only a Pipeline can set"
            ))),
            "{field}: {e:?}"
        );
    }
    // `name`, `deploy` and `interface` stay allowed.
    ok(
        "steps: [{ id: c, invoke: file:l.yaml }]",
        &files(&[(
            "l.yaml",
            "kind: library\nname: L\ndeploy: true\nir_version: 2\nsteps: [{ id: s, image: busybox }]\n",
        )]),
    );
}

// ============================================================ SURVIVED

/// A value that closes a CEL string literal cannot add `|| true` to a
/// library `when:`; quotes, backslashes, braces, newlines, `\u` text and
/// control characters round-trip as data.
#[test]
fn survived_with_values_are_cel_data_in_when() {
    for when in ["\"inputs.x == 'safe'\"", "'inputs.x == \"safe\"'"] {
        let lib = lib_when("{ name: x, type: string }", when);
        for attack in [
            "' || true || '",
            "\" || true || \"",
            "\\\" || true || \\\"",
            "x\") || (true",
            "}} ${{ true",
            "a\nb",
            "\\",
        ] {
            let yaml = caller(&format!(
                "{{ x: {} }}",
                serde_json::to_string(attack).unwrap()
            ));
            match compile_with_libraries(&yaml, &files(&[("l.yaml", &lib)])) {
                // A value holding `${{` is refused in `when:` (by design).
                Err(e) => assert!(attack.contains("${{"), "`{attack}`: {e}"),
                Ok(ir) => assert!(!kept(&ir), "`{attack}` turned `{when}` true"),
            }
        }
    }
    // Round-trip: the bound value equals itself.
    for v in [
        "a\"b",
        "a\\b",
        "a}}b",
        "a{b",
        "\\u007b",
        "tab\there",
        "ctl\u{1}x",
        "ls\u{2028}x",
        "é🦀",
    ] {
        let lib2 = "kind: library\ninterface:\n  inputs: [{ name: x, type: string }]\nsteps:\n  - id: s\n    image: busybox\n    command: [\"${{ inputs.x + '' }}\"]\n";
        let ir = ok(
            &caller(&format!("{{ x: {} }}", serde_json::to_string(v).unwrap())),
            &files(&[("l.yaml", lib2)]),
        );
        let launched = launch(step(&ir, "c/s"), json!({ "x": "RUN" })).unwrap();
        assert_eq!(launched.command[0], v, "IR {:?}", step(&ir, "c/s").command);
    }
}

/// Number and boolean inputs accept only literals.
#[test]
fn survived_typed_inputs_refuse_expressions() {
    for (ty, v) in [
        ("number", "1 || true"),
        ("number", "1) || (true"),
        ("number", "inf"),
        ("number", "NaN"),
        ("number", "0x10"),
        ("boolean", "true || x"),
        ("boolean", "1"),
    ] {
        let lib = lib_when(
            &format!("{{ name: x, type: {ty} }}"),
            "\"inputs.x == inputs.x\"",
        );
        let e = errs(
            &caller(&format!("{{ x: \"{v}\" }}")),
            &files(&[("l.yaml", &lib)]),
        );
        assert!(e.iter().any(|e| e.contains("input `x`")), "{ty} {v}: {e:?}");
    }
    // A negative number binds parenthesised: `0 - -1` stays arithmetic.
    let lib = lib_when("{ name: n, type: number }", "\"0 - inputs.n == 1\"");
    assert!(kept(&ok(
        &caller("{ n: \"-1\" }"),
        &files(&[("l.yaml", &lib)])
    )));
}

/// Every form of `inputs` inside a library reads the library's own inputs
/// (its scope), never the Run's launch parameters: there is no form to
/// escape to. A read of an input the library does not declare is refused,
/// by name or by constant key. `inputs.x` inside a string literal is not a
/// read. (Before scopes, these forms escaped the text binding and were
/// refused for that reason.)
#[test]
fn survived_inputs_escapes_are_refused_or_literal() {
    for expr in [
        "inputs['x'] == 'a'",
        "inputs .x == 'a'",
        "inputs. x == 'a'",
        "'x' in inputs && !('p' in inputs)",
        "[1].exists(inputs, inputs == 1)",
        "size(inputs) == 1",
        ".inputs.x == 'a'",
    ] {
        let lib = lib_when("{ name: x, type: string }", &format!("\"{expr}\""));
        let ir = ok(&caller("{ x: a }"), &files(&[("l.yaml", &lib)]));
        assert!(kept(&ir), "`{expr}` must read the library's own `x`");
    }
    for expr in ["inputs.y == 'a'", "inputs['y'] == 'a'", "has(inputs.y)"] {
        let lib = lib_when("{ name: x, type: string }", &format!("\"{expr}\""));
        let e = errs(&caller("{ x: a }"), &files(&[("l.yaml", &lib)]));
        assert!(
            e.iter()
                .any(|e| e.contains("reads `inputs.y`, which library `l.yaml` does not declare")),
            "`{expr}` should be refused: {e:?}"
        );
    }
    // In a string literal, `inputs.x` is text.
    let lib = lib_cmd(
        "{ name: x, type: string }",
        "\"${{ 'inputs.x' + inputs.x }}\"",
    );
    let ir = ok(&caller("{ x: a }"), &files(&[("l.yaml", &lib)]));
    let launched = launch(step(&ir, "c/s"), json!({ "x": "RUN" })).unwrap();
    assert_eq!(launched.command[0], "inputs.xa");
    // Outside `${{ }}`, `inputs.x` is plain text.
    let lib = lib_cmd("{ name: x, type: string }", "\"inputs.x\"");
    let ir = ok(&caller("{ x: a }"), &files(&[("l.yaml", &lib)]));
    assert_eq!(step(&ir, "c/s").command[0], "inputs.x");
}

/// Prefix parsing refuses every malformed or remote target.
#[test]
fn survived_invoke_prefixes() {
    let l = files(&[(
        "a.yaml",
        "kind: library\nsteps: [{ id: a, image: busybox }]",
    )]);
    for t in [
        "file:../a.yaml",
        "file:x/../a.yaml",
        "file:/a.yaml",
        "file:",
        "file:   ",
        "file:x@y",
        "file: a.yaml",
        "file:https://h/x",
        "file:a\\\\b",
        "file:./../a.yaml",
        "file:.//a.yaml",
        "lib:a/b/c",
        "lib:-a/b",
        "lib:a/",
        "lib:a/..",
        "lib: a/b",
        "LIB:a/b",
        "File:a.yaml",
        "url:",
        "url:a.yaml",
        "plugin:",
        "plugin:a/b",
        ":a.yaml",
        "a.yaml",
    ] {
        let e = errs(&format!("steps: [{{ id: s, invoke: \"{t}\" }}]"), &l);
        assert!(e.iter().any(|e| e.starts_with("step `s`")), "`{t}`: {e:?}");
    }
    // Whitespace around the target is trimmed, and `./` is stripped; a space
    // after the prefix is refused for every prefix.
    for t in [" file:a.yaml ", "file:./a.yaml"] {
        ok(&format!("steps: [{{ id: s, invoke: \"{t}\" }}]"), &l);
    }
    for (t, want) in [
        ("file: a.yaml", "has a space after `file:`"),
        ("lib: a/b", "has a space after `lib:`"),
        (
            "LIB:a/b",
            "unknown prefix `LIB:`; use `file:`, `lib:` or `plugin:`",
        ),
        (
            "File:a.yaml",
            "unknown prefix `File:`; use `file:`, `lib:` or `plugin:`",
        ),
    ] {
        let e = errs(&format!("steps: [{{ id: s, invoke: \"{t}\" }}]"), &l);
        assert!(e.iter().any(|e| e.contains(want)), "`{t}`: {e:?}");
    }
}

/// Installed libraries are closed through any depth, cycles through `lib:`
/// names are caught, and the depth cap holds for `lib:` chains.
#[test]
fn survived_closed_libraries_cycles_and_depth() {
    let file_lib = (
        "evil.yaml",
        "kind: library\nsteps: [{ id: e, image: evil }]",
    );
    let l = with_installed(
        files(&[file_lib]),
        &[
            (
                "a/one",
                "kind: library\nsteps: [{ id: x, invoke: \"lib:a/two\" }]",
                "src1",
            ),
            (
                "a/two",
                "kind: library\nsteps: [{ id: y, invoke: \"file:evil.yaml\" }]",
                "src2",
            ),
        ],
    );
    let e = errs("steps: [{ id: c, invoke: \"lib:a/one\" }]", &l);
    assert!(
        e.iter()
            .any(|e| e.contains("can invoke only `lib:` and `plugin:`")),
        "{e:?}"
    );

    let l = with_installed(
        Libraries::default(),
        &[
            (
                "a/p",
                "kind: library\nsteps: [{ id: x, invoke: \"lib:a/q\" }]",
                "s",
            ),
            (
                "a/q",
                "kind: library\nsteps: [{ id: y, invoke: \"lib:a/p\" }]",
                "s",
            ),
        ],
    );
    let e = errs("steps: [{ id: c, invoke: \"lib:a/p\" }]", &l);
    assert!(
        e.iter()
            .any(|e| e.contains("invoke cycle detected: lib:a/p -> lib:a/q -> lib:a/p")),
        "{e:?}"
    );

    let mut entries = Vec::new();
    for i in 0..10 {
        entries.push((
            format!("a/l{i}"),
            format!(
                "kind: library\nsteps: [{{ id: s, invoke: \"lib:a/l{}\" }}]",
                i + 1
            ),
        ));
    }
    entries.push((
        "a/l10".into(),
        "kind: library\nsteps: [{ id: s, image: busybox }]".into(),
    ));
    let mut l = Libraries::default();
    for (k, v) in &entries {
        l.installed.insert(
            k.clone(),
            InstalledLibrary {
                content: v.clone(),
                source: "s".into(),
            },
        );
    }
    let e = errs("steps: [{ id: c, invoke: \"lib:a/l0\" }]", &l);
    assert!(e.iter().any(|e| e.contains("depth cap of 8")), "{e:?}");
}

/// Every Step field an invoke step cannot carry is refused, and the carried
/// ones fill only unset fields and never reach a gate.
#[test]
fn survived_carry_rules() {
    let lib = "kind: library\nsteps:\n  - { id: own, image: busybox, timeout: 5, resources: { cpu_millis: 7 }, placement_profiles: [mine] }\n  - { id: bare, image: busybox, needs: [own] }\n  - { id: g, gate: manual, needs: [bare] }\n";
    let l = files(&[("l.yaml", lib)]);
    for f in [
        "image: x",
        "command: [x]",
        "env: [[A, b]]",
        "secrets: [T]",
        "security: {}",
        "gate: manual",
        "retry: { on: failure, max: 1 }",
        "results: [r]",
        "uses: [db]",
        "services: [{ image: pg }]",
        "cache: { key: [k], dirs: [d] }",
        "k8s_overlay: {}",
        "outputs: [o]",
        "inputs: []",
        "artifacts: [a]",
        "gate_after: 1",
        "clone: {}",
        "matrix_values: { a: b }",
        "origin: [{ kind: file, name: x, content_hash: h }]",
    ] {
        let e = errs(
            &format!("steps: [{{ id: c, invoke: file:l.yaml, {f} }}]"),
            &l,
        );
        assert!(!e.is_empty(), "`{f}` accepted");
    }
    let ir = ok(
        "steps: [{ id: c, invoke: file:l.yaml, timeout: 60, resources: { cpu_millis: 100, memory_mib: 64 }, placement_profiles: [big] }]",
        &l,
    );
    let own = step(&ir, "c/own");
    assert_eq!(
        (
            own.timeout,
            own.resources.cpu_millis,
            own.resources.memory_mib
        ),
        (Some(5), Some(7), Some(64))
    );
    assert_eq!(own.placement_profiles, vec!["mine".to_string()]);
    let bare = step(&ir, "c/bare");
    assert_eq!(
        (bare.timeout, bare.resources.cpu_millis),
        (Some(60), Some(100))
    );
    assert_eq!(bare.placement_profiles, vec!["big".to_string()]);
    let g = step(&ir, "c/g");
    assert_eq!(
        (
            g.timeout,
            g.resources.cpu_millis,
            g.placement_profiles.len()
        ),
        (None, None, 0)
    );
}

/// The invoke `when` is ANDed with precedence intact, including onto gates
/// and across a matrix on the invoke step.
#[test]
fn survived_when_and_precedence() {
    // outer `a || b` (true), inner `c || d` (false) -> must be false.
    let lib = "kind: library\nsteps:\n  - { id: s, image: busybox, when: \"false || false\" }\n  - { id: g, gate: manual, when: \"true || true\" }\n";
    let ir = ok(
        "steps: [{ id: c, invoke: file:l.yaml, when: \"true || false\", matrix: { dimensions: { r: [eu, us] } } }]",
        &files(&[("l.yaml", lib)]),
    );
    let ex = excluded_steps(&ir, &json!({ "event": {} })).unwrap();
    assert!(
        ex.contains(&"c[r=eu]/s".to_string()) && ex.contains(&"c[r=us]/s".to_string()),
        "{ex:?}"
    );
    assert!(!ex.iter().any(|e| e.ends_with("/g")), "{ex:?}");
    // outer false must skip the gate too.
    let ir = ok(
        "steps: [{ id: c, invoke: file:l.yaml, when: \"false && true || false\" }]",
        &files(&[("l.yaml", lib)]),
    );
    let ex = excluded_steps(&ir, &json!({ "event": {} })).unwrap();
    assert!(ex.contains(&"c/g".to_string()), "{ex:?}");
}

/// `kind: library` rules.
#[test]
fn survived_kind_library_rules() {
    let e = errs(
        "kind: library\nsteps: [{ id: a, image: busybox }]",
        &Libraries::default(),
    );
    assert!(e[0].contains("cannot run as a Pipeline"), "{e:?}");
    assert!(compile_with_libraries("kind: Library\nsteps: []", &Libraries::default()).is_err());
    assert!(compile_with_libraries("kind: pipeline\nsteps: []", &Libraries::default()).is_err());
    let l = files(&[
        (
            "trig.yaml",
            "kind: library\non: { push: {} }\nsteps: [{ id: a, image: busybox }]",
        ),
        ("plain.yaml", "steps: [{ id: a, image: busybox }]"),
    ]);
    let e = errs("steps: [{ id: c, invoke: file:trig.yaml }]", &l);
    assert!(e.iter().any(|e| e.contains("has `on:` triggers")), "{e:?}");
    let e = errs("steps: [{ id: c, invoke: file:plain.yaml }]", &l);
    assert!(e.iter().any(|e| e.contains("is not a library")), "{e:?}");
    assert!(scarab_pipeline::is_library("kind: library\nsteps: []"));
    assert!(!scarab_pipeline::is_library("kind: Library\nsteps: []"));
}

/// `origin` records a lib -> lib -> file? (closed, so lib -> lib) chain
/// outermost first, with each level's own source and content hash; a
/// file -> lib chain records the file then the library. An authored
/// `origin` inside an installed library is refused.
#[test]
fn survived_origin_chain() {
    use sha2_free_hash::sha256_hex;
    let inner = "kind: library\nsteps: [{ id: leaf, image: busybox }]\n";
    let outer = "kind: library\nsteps: [{ id: mid, invoke: \"lib:p/inner\" }]\n";
    let file = "kind: library\nsteps: [{ id: f, invoke: \"lib:p/outer\" }]\n";
    let l = with_installed(
        files(&[("f.yaml", file)]),
        &[
            ("p/inner", inner, "git/inner@1"),
            ("p/outer", outer, "git/outer@2"),
        ],
    );
    let ir = ok("steps: [{ id: c, invoke: file:f.yaml }]", &l);
    let s = step(&ir, "c/f/mid/leaf");
    let got: Vec<_> = s
        .origin
        .iter()
        .map(|o| {
            (
                o.kind,
                o.name.as_str(),
                o.source.as_deref(),
                o.content_hash.clone(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (OriginKind::File, "f.yaml", None, sha256_hex(file)),
            (
                OriginKind::Library,
                "p/outer",
                Some("git/outer@2"),
                sha256_hex(outer)
            ),
            (
                OriginKind::Library,
                "p/inner",
                Some("git/inner@1"),
                sha256_hex(inner)
            ),
        ]
    );
    let bad = "kind: library\nsteps: [{ id: x, image: busybox, origin: [{ kind: library, name: acme/ok, source: trusted, content_hash: h }] }]\n";
    let l = with_installed(Libraries::default(), &[("p/bad", bad, "s")]);
    let e = errs("steps: [{ id: c, invoke: \"lib:p/bad\" }]", &l);
    assert!(
        e.iter()
            .any(|e| e.contains("`origin` is written by the compiler")),
        "{e:?}"
    );
}

/// SHA-256, lowercase hex, with the `sha2` the crate already depends on.
mod sha2_free_hash {
    pub fn sha256_hex(s: &str) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

// ============================================================ probes (second round)

/// Was: a root-qualified `.inputs.x` (CEL's leading-dot form) was skipped by
/// the scanner as a field and left unbound, and the `cel` crate accepts it:
/// the library read the RUN's launch parameter `tag`. The scanner now binds
/// it, in `${{ }}` and in `when:`.
#[test]
fn a_leading_dot_inputs_read_is_bound() {
    let lib = lib_cmd(
        "{ name: tag, type: string }",
        "\"${{ .inputs.tag }}\", \"${{ 'x-' + . inputs.tag }}\"",
    );
    let ir = ok(&caller("{ tag: from-with }"), &files(&[("l.yaml", &lib)]));
    let s = step(&ir, "c/s");
    let l = launch(s, json!({ "tag": "RUN-PARAM" })).unwrap();
    assert_eq!(
        l.command[..2],
        ["from-with", "x-from-with"],
        "IR {:?}",
        s.command
    );
    let lib = lib_when("{ name: x, type: string }", "\".inputs.x == 'a'\"");
    assert!(kept(&ok(&caller("{ x: a }"), &files(&[("l.yaml", &lib)]))));
    // A whole `.inputs` is refused like `inputs`.
    // A whole `.inputs` is the library's inputs, like `inputs`.
    let lib = lib_cmd("{ name: tag, type: string }", "\"${{ size(.inputs) }}\"");
    let ir = ok(&caller("{ tag: t }"), &files(&[("l.yaml", &lib)]));
    let l = launch(step(&ir, "c/s"), json!({ "tag": "RUN", "a": 1, "b": 2 })).unwrap();
    assert_eq!(l.command[0], "1");
}

/// `has(inputs.x)` asks the library's own inputs: true for a declared,
/// bound input, whatever the Run's parameters hold.
#[test]
fn survived_has_inputs_field() {
    let lib = lib_when("{ name: x, type: string }", "\"has(inputs.x)\"");
    let ir = ok(&caller("{ x: a }"), &files(&[("l.yaml", &lib)]));
    assert!(kept(&ir));
    assert_eq!(
        step(&ir, "c/s").when.as_ref().unwrap().0,
        "has(inputs.x)",
        "the library's text is kept as written"
    );
}

/// Was: `${{ inputs.x }}` in a sidecar service's image/env (and in
/// `results`, `artifacts`, `secrets`, cache keys) was neither bound nor
/// refused. Those fields are not interpolated at launch, so any `inputs`
/// read in them is refused in a library, declared or not.
#[test]
fn an_inputs_read_in_a_field_that_is_not_interpolated_is_refused() {
    for (field, yaml) in [
        (
            "services[0].image",
            "services: [{ image: \"postgres:${{ inputs.v }}\" }]",
        ),
        (
            "services[0].env.P",
            "services: [{ image: pg, env: { P: \"${{ .inputs.v }}\" } }]",
        ),
        ("results[0]", "results: [\"${{ inputs.v }}\"]"),
        ("artifacts[0]", "artifacts: [\"${{ inputs.v }}\"]"),
        ("secrets[0]", "secrets: [\"${{ inputs.v }}\"]"),
        (
            "cache.key[0]",
            "cache: { key: [\"${{ inputs.v }}\"], dirs: [d] }",
        ),
    ] {
        let lib = format!(
            "kind: library\ninterface: {{ inputs: [{{ name: v, type: string }}] }}\nsteps:\n  - {{ id: s, image: busybox, {yaml} }}\n"
        );
        let e = errs(&caller("{ v: x }"), &files(&[("l.yaml", &lib)]));
        assert!(
            e.iter()
                .any(|e| e.contains(&format!("`{field}` reads `inputs`"))),
            "{field}: {e:?}"
        );
    }
}

/// Was: an invoke step's `timeout`/`resources`/`placement_profiles` on a
/// library whose Steps are all gates applied to nothing, without a word.
/// They are now refused.
#[test]
fn carried_fields_onto_an_all_gate_library_are_refused() {
    let lib = "kind: library\nsteps: [{ id: g, gate: manual }]\n";
    let e = errs(
        "steps: [{ id: c, invoke: file:l.yaml, timeout: 60, resources: { memory_mib: 1 }, placement_profiles: [big] }]",
        &files(&[("l.yaml", lib)]),
    );
    for field in ["timeout", "resources", "placement_profiles"] {
        assert!(
            e.iter().any(|e| e.contains(&format!(
                "`{field}` applies to the Steps an invoke inlines, and library `l.yaml` inlines only gates"
            ))),
            "{field}: {e:?}"
        );
    }
    // A `when` still applies to gates.
    ok(
        "steps: [{ id: c, invoke: file:l.yaml, when: \"true\" }]",
        &files(&[("l.yaml", lib)]),
    );
}

/// Was (pre-existing, ADR-0041 seam): an exposed output whose backing Step
/// is not a leaf was missing at launch, since a reader that `needs: [c]` was
/// wired to the leaves only. A reader now also needs the backing Step,
/// including a library Step that reads it through a `with:` value.
#[test]
fn a_reader_of_an_exposed_output_needs_its_backing_step() {
    let lib = "kind: library\ninterface: { outputs: [img] }\nsteps:\n  - { id: img, image: busybox, results: [img] }\n  - { id: post, image: busybox, needs: [img] }\n";
    let user = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - { id: a, image: busybox }\n  - { id: b, image: busybox, needs: [a], env: [[V, \"${{ inputs.v }}\"]] }\n";
    let yaml = "steps:\n  - { id: c, invoke: file:l.yaml }\n  - { id: r, image: busybox, needs: [c], env: [[I, \"${{ outputs.c.img }}\"]] }\n  - { id: u, invoke: file:u.yaml, needs: [c], with: { v: \"${{ outputs.c.img }}\" } }\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib), ("u.yaml", user)]));
    let rd = step(&ir, "r");
    assert_eq!(rd.needs.0, vec!["c/post".to_string(), "c/img".into()]);
    assert_eq!(rd.env[0].1, "${{ outputs.c.img }}");
    for id in ["u/a", "u/b"] {
        let s = step(&ir, id);
        assert!(
            s.needs.0.contains(&"c/img".to_string()),
            "{id}: {:?}",
            s.needs.0
        );
    }
}

/// Was (DoS, pre-existing since ADR-0038): fan-out × depth. Each of 7 levels
/// invoking the next 4 times gave 4^7 = 16384 Steps from 8 tiny files, with
/// no cap, on a compile the server runs per webhook. The compile now stops
/// once a list passes `MAX_COMPILED_STEPS`, and a matrix over the cap is
/// refused before it is built.
#[test]
fn invoke_fan_out_and_matrices_stop_at_the_step_cap() {
    let mut l = Libraries::default();
    let depth = 7;
    for i in 0..depth {
        let calls: String = (0..4)
            .map(|k| format!("  - {{ id: k{k}, invoke: \"lib:a/l{}\" }}\n", i + 1))
            .collect();
        l.installed.insert(
            format!("a/l{i}"),
            InstalledLibrary {
                content: format!("kind: library\nsteps:\n{calls}"),
                source: "s".into(),
            },
        );
    }
    l.installed.insert(
        format!("a/l{depth}"),
        InstalledLibrary {
            content: "kind: library\nsteps: [{ id: s, image: busybox }]".into(),
            source: "s".into(),
        },
    );
    let cap = scarab_pipeline::MAX_COMPILED_STEPS;
    let e = errs("steps: [{ id: c, invoke: \"lib:a/l0\" }]", &l);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains(&format!("the limit is {cap}")), "{e:?}");
    // 4^5 = 1024 Steps is at the cap and compiles.
    let ir = ok("steps: [{ id: c, invoke: \"lib:a/l2\" }]", &l);
    assert_eq!(ir.steps.len(), cap);

    // A matrix is counted before it is built, on a Step and on an invoke.
    let big = "{ dimensions: { a: [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32], b: [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33] } }";
    let e = errs(
        &format!("steps: [{{ id: m, image: busybox, matrix: {big} }}]"),
        &l,
    );
    assert!(
        e.iter()
            .any(|e| e.contains("step `m`: the pipeline compiles to more than")),
        "{e:?}"
    );
    let e = errs(
        &format!("steps: [{{ id: m, invoke: \"lib:a/l7\", matrix: {big} }}]"),
        &l,
    );
    assert!(
        e.iter()
            .any(|e| e.contains("step `m`: the pipeline compiles to more than")),
        "{e:?}"
    );
    // Steps that each fit but together pass the cap.
    let many: String = (0..3)
        .map(|i| {
            format!(
                "  - {{ id: m{i}, image: busybox, matrix: {{ dimensions: {{ a: [{}] }} }} }}\n",
                (0..400)
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .collect();
    let e = errs(&format!("steps:\n{many}"), &l);
    assert!(
        e.iter()
            .any(|e| e.contains("step `m2`: the pipeline compiles to more than")),
        "{e:?}"
    );
}

/// Was: a library input's `default:` holding `${{ inputs.p }}` was inserted
/// as text, so at launch it read the RUN's launch parameter `p`. A default
/// holding `${{` is now refused; only the caller's `with:` may hold one.
#[test]
fn a_library_default_holding_an_expression_is_refused() {
    for default in ["\"${{ inputs.p }}\"", "\"${{ outputs.b.v }}\""] {
        let lib = lib_cmd(
            &format!("{{ name: x, type: string, required: false, default: {default} }}"),
            "\"${{ inputs.x }}\"",
        );
        let e = errs(&caller("{ x: given }"), &files(&[("l.yaml", &lib)]));
        assert!(
            e.iter()
                .any(|e| e.contains("input `x` has a default holding `${{ … }}`")),
            "{default}: {e:?}"
        );
    }
}

/// Was: a library invoke's `with:` reading `outputs.<step>.<name>` that
/// `<step>` does not declare compiled, and failed the library Step when it
/// launched (git-bug 6adf89a). A plugin gate's `with:` is checked at compile
/// since e6b9d618; a library's now is too, once for the invoke however many
/// Steps the library has.
#[test]
fn a_library_with_reading_an_undeclared_result_is_refused() {
    let lib = "kind: library\ninterface:\n  inputs:\n    - { name: x, type: string }\nsteps:\n  - { id: s, image: busybox, env: [[X, \"${{ inputs.x }}\"]] }\n  - { id: t, image: busybox, env: [[X, \"${{ inputs.x }}\"]] }\n";
    let pipeline = |read: &str| {
        format!(
            "steps:\n  - {{ id: bump, image: alpine, results: [image] }}\n  - id: c\n    \
             needs: [bump]\n    invoke: file:l.yaml\n    with: {{ x: \"${{{{ {read} }}}}\" }}\n"
        )
    };
    let libs = files(&[("l.yaml", lib)]);
    let e = errs(&pipeline("outputs.bump.nope"), &libs);
    let hits: Vec<&String> = e
        .iter()
        .filter(|e| e.contains("outputs.bump.nope") && e.contains("does not declare"))
        .collect();
    assert_eq!(hits.len(), 1, "{e:?}");
    assert!(hits[0].contains("step `c`"), "{e:?}");
    // A declared result compiles.
    ok(&pipeline("outputs.bump.image"), &libs);
}
