//! Red-team round 2: attacks on the round-1 fixes for ADR-0076 step 1
//! (commits 3140722..4fe6954). Each test asserts the SECURE / CORRECT
//! behaviour: a failing test is a break. The `survived_` tests are attacks
//! that round 1 already held.

use std::collections::BTreeMap;
use std::time::Instant;

use scarab_pipeline::{
    compile_with_libraries, excluded_steps, interpolate_spec, Libraries, PipelineError, PipelineIr,
    StepSpec, MAX_COMPILED_STEPS,
};
use serde_json::json;

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

fn errs(yaml: &str, libs: &Libraries) -> Vec<String> {
    match compile(yaml, libs) {
        Ok(ir) => panic!(
            "expected a compile error, got {:#?}",
            ir.steps
                .iter()
                .map(|s| (&s.id, &s.command, &s.needs.0))
                .collect::<Vec<_>>()
        ),
        Err(e) => e,
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

/// The engine at launch: `inputs` = the RUN's launch parameters; `outputs`
/// keyed by direct-need id.
fn launch(spec: &StepSpec, run_params: serde_json::Value, outputs: serde_json::Value) -> String {
    interpolate_spec(
        spec,
        &json!({ "inputs": run_params, "outputs": outputs, "matrix": spec.matrix_values }),
    )
    .map(|s| s.command.join(" | "))
    .unwrap_or_else(|e| format!("LAUNCH ERROR: {e}"))
}

// ===================================================== blanking of with: values

/// A caller value that ENDS in `$` (a regex anchor) joins library text
/// `{{ inputs.p }}` into `${{ inputs.p }}`, which at launch would read the
/// Run's launch parameter `p`. The check scans the bound string by where each
/// byte came from, so a `${{ … }}` that crosses from the value into the
/// library's text is refused.
#[test]
#[ignore = "held: finding 1, span-based binding (local branch wip/libraries-span-binding)"]
fn a_caller_value_ending_in_dollar_cannot_join_a_library_read() {
    let lib = "kind: library\ninterface: { inputs: [{ name: pat, type: string }] }\nsteps:\n  - id: s\n    image: busybox\n    command: [\"${{ inputs.pat }}{{ inputs.p }}\"]\n";
    let yaml = "interface: { inputs: [{ name: p, type: string, required: false, default: secret }] }\nsteps:\n  - { id: b, image: busybox, results: [name] }\n  - id: c\n    invoke: file:l.yaml\n    needs: [b]\n    with: { pat: \"^${{ outputs.b.name }}$\" }\n";
    match compile(yaml, &files(&[("l.yaml", lib)])) {
        Err(e) => assert!(
            e.iter()
                .any(|e| e.contains("holds `${{ inputs.p }}`, which an input's value forms")),
            "{e:?}"
        ),
        Ok(ir) => {
            let s = step(&ir, "c/s");
            let launched = launch(
                s,
                json!({ "p": "RUN-PARAM" }),
                json!({ "b": { "name": "x" } }),
            );
            panic!(
                "library read the Run's launch parameter: IR command {:?} launches as {launched:?}",
                s.command
            );
        }
    }
}

/// Same join, through a nested invoke's `with:`: refused where the `with:`
/// value is bound.
#[test]
#[ignore = "held: finding 1, span-based binding (local branch wip/libraries-span-binding)"]
fn a_join_through_a_nested_with_is_refused() {
    let a = "kind: library\ninterface: { inputs: [{ name: pat, type: string }] }\nsteps:\n  - id: n\n    invoke: file:b.yaml\n    with: { x: \"${{ inputs.pat }}{{ inputs.p }}\" }\n";
    let b = "kind: library\ninterface: { inputs: [{ name: x, type: string }] }\nsteps: [{ id: s, image: busybox, command: [\"${{ inputs.x }}\"] }]\n";
    let yaml = "interface: { inputs: [{ name: p, type: string, required: false, default: d }] }\nsteps:\n  - { id: b, image: busybox, results: [name] }\n  - { id: c, invoke: file:a.yaml, needs: [b], with: { pat: \"^${{ outputs.b.name }}$\" } }\n";
    let ab = files(&[("a.yaml", a), ("b.yaml", b)]);
    if let Err(e) = compile(yaml, &ab) {
        assert!(
            e.iter()
                .any(|e| e.contains("`with.x` holds `${{ inputs.p }}`")),
            "{e:?}"
        );
    }
    if let Ok(ir) = compile(yaml, &ab) {
        let s = step(&ir, "c/n/s");
        panic!(
            "nested: library read the Run's `p`: {:?} -> {:?}",
            s.command,
            launch(
                s,
                json!({ "p": "RUN-PARAM" }),
                json!({ "b": { "name": "x" } })
            )
        );
    }
}

/// A join built only from library text and plain caller values is not
/// blanked, so the parser check sees it: a library default `$` plus a caller
/// value `inputs` cannot assemble `${{ inputs.p }}`, even when another input
/// holds a caller expression (so the blanking path runs).
#[test]
fn survived_library_text_and_plain_values_cannot_assemble_a_read() {
    let lib = "kind: library\ninterface:\n  inputs:\n    - { name: d, type: string, required: false, default: \"$\" }\n    - { name: v, type: string }\n    - { name: e, type: string }\nsteps:\n  - id: s\n    image: busybox\n    command: [\"${{ inputs.d }}{{ ${{ inputs.v }}.p }}\", \"${{ inputs.e }}\"]\n";
    for v in ["inputs", ".inputs", " inputs"] {
        let yaml = format!(
            "interface: {{ inputs: [{{ name: q, type: string }}] }}\nsteps:\n  - {{ id: c, invoke: file:l.yaml, with: {{ v: \"{v}\", e: \"${{{{ inputs.q }}}}\" }} }}\n"
        );
        let e = errs(&yaml, &files(&[("l.yaml", lib)]));
        assert!(e.iter().any(|e| e.contains("reads `inputs`")), "{v}: {e:?}");
    }
}

// ============================================== nested levels: values survive

#[test]
fn survived_values_bind_correctly_through_two_levels() {
    let a = "kind: library\ninterface: { inputs: [{ name: a, type: string }, { name: n, type: number }] }\nsteps:\n  - id: k\n    invoke: file:b.yaml\n    with: { x: \"${{ inputs.a }}\", m: \"${{ inputs.n }}\" }\n";
    let b = "kind: library\ninterface: { inputs: [{ name: x, type: string }, { name: m, type: number }] }\nsteps:\n  - { id: s, image: busybox, when: \"inputs.m > 4\", command: [\"${{ inputs.x }}\", \"${{ inputs.m + 1 }}\"] }\n";
    let l = files(&[("a.yaml", a), ("b.yaml", b)]);
    // A plain value and a number.
    let ir = ok(
        "steps: [{ id: c, invoke: file:a.yaml, with: { a: hello, n: \"5\" } }]",
        &l,
    );
    let s = step(&ir, "c/k/s");
    assert_eq!(
        launch(s, json!({ "a": "RUN", "n": 0 }), json!({})),
        "hello | 6"
    );
    assert!(!excluded_steps(&ir, &json!({ "inputs": { "m": 0 } }))
        .unwrap()
        .contains(&"c/k/s".to_string()));
    // The Pipeline's own launch parameter through two levels.
    let ir = ok(
        "interface: { inputs: [{ name: p, type: string }] }\nsteps: [{ id: c, invoke: file:a.yaml, with: { a: \"${{ inputs.p }}\", n: \"5\" } }]",
        &l,
    );
    let s = step(&ir, "c/k/s");
    assert_eq!(s.command[0], "${{ inputs.p }}");
    assert_eq!(launch(s, json!({ "p": "RUN-P" }), json!({})), "RUN-P | 6");
}

// ======================================== needs wiring for outputs.X in with:

/// X is an invoke step read in bracket form `outputs['x'].img`: rewritten to
/// the backing step and wired, like the dotted form.
#[test]
fn bracket_read_of_an_invoke_output_through_with() {
    let lib = "kind: library\ninterface: { outputs: [img] }\nsteps:\n  - { id: img, image: busybox, results: [img] }\n  - { id: post, image: busybox, needs: [img] }\n";
    let user = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps: [{ id: a, image: busybox, command: [\"${{ inputs.v }}\"] }]\n";
    let yaml = "steps:\n  - { id: x, invoke: file:l.yaml }\n  - { id: u, invoke: file:u.yaml, needs: [x], with: { v: \"${{ outputs['x'].img }}\" } }\n";
    {
        let ir = ok(yaml, &files(&[("l.yaml", lib), ("u.yaml", user)]));
        let a = step(&ir, "u/a");
        // What the engine would give `u/a`: outputs of its direct needs.
        let outs: serde_json::Map<String, serde_json::Value> = a
            .needs
            .0
            .iter()
            .map(|n| (n.clone(), json!({ "img": format!("IMG-of-{n}") })))
            .collect();
        let launched = launch(a, json!({}), serde_json::Value::Object(outs));
        assert_eq!(
            launched, "IMG-of-x/img",
            "u/a command {:?} needs {:?}",
            a.command, a.needs.0
        );
    }
}

/// X is a Step inside the SAME library as the nested invoke. The read
/// `outputs.b.tag` must reach library step `b` (now `c/b`).
#[test]
#[ignore = "held: finding 4 needs who wrote each byte, or caller reads are renamed too (wip/libraries-span-binding)"]
fn library_internal_output_read_through_nested_with() {
    let a = "kind: library\nsteps:\n  - { id: b, image: busybox, results: [tag] }\n  - id: n\n    invoke: file:b.yaml\n    needs: [b]\n    with: { x: \"${{ outputs.b.tag }}\" }\n";
    let b = "kind: library\ninterface: { inputs: [{ name: x, type: string }] }\nsteps: [{ id: s, image: busybox, command: [\"${{ inputs.x }}\"] }]\n";
    let ir = ok(
        "steps: [{ id: c, invoke: file:a.yaml }]",
        &files(&[("a.yaml", a), ("b.yaml", b)]),
    );
    let s = step(&ir, "c/n/s");
    let outs: serde_json::Map<String, serde_json::Value> = s
        .needs
        .0
        .iter()
        .map(|n| (n.clone(), json!({ "tag": format!("TAG-of-{n}") })))
        .collect();
    assert_eq!(
        launch(s, json!({}), serde_json::Value::Object(outs)),
        "TAG-of-c/b",
        "command {:?} needs {:?}",
        s.command,
        s.needs.0
    );
}

/// The caller reads ITS step `b`; the library also has a step `b`. The
/// caller value `${{ outputs.b.tag }}` flows into the library's nested
/// `with:`; it was written by the caller, so it is not matched against the
/// library's own `b`, and the Pipeline compiles.
#[test]
#[ignore = "held: finding 3 needs who wrote each byte of a with: value (wip/libraries-span-binding)"]
fn caller_output_name_colliding_with_a_library_step_compiles() {
    let a = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - { id: b, image: busybox }\n  - { id: n, invoke: file:b.yaml, with: { x: \"${{ inputs.v }}\" } }\n";
    let b = "kind: library\ninterface: { inputs: [{ name: x, type: string }] }\nsteps: [{ id: s, image: busybox, command: [\"${{ inputs.x }}\"] }]\n";
    let yaml = "steps:\n  - { id: b, image: busybox, results: [tag] }\n  - { id: c, invoke: file:a.yaml, needs: [b], with: { v: \"${{ outputs.b.tag }}\" } }\n";
    let ir = ok(yaml, &files(&[("a.yaml", a), ("b.yaml", b)]));
    let s = step(&ir, "c/n/s");
    assert!(s.needs.0.contains(&"b".to_string()));
    assert_eq!(s.command, vec!["${{ outputs.b.tag }}"]);
}

#[test]
fn survived_with_output_reads_matrix_invoke_and_forms() {
    let lib = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - { id: a, image: busybox }\n  - { id: z, image: busybox, needs: [a], command: [\"${{ inputs.v }}\"] }\n";
    let l = files(&[("l.yaml", lib)]);
    // Matrixed X by its authored id; and `outputs.X["y"]`.
    for v in ["${{ outputs.build.v }}", "${{ outputs.build[\\\"v\\\"] }}"] {
        let yaml = format!("steps:\n  - {{ id: build, image: busybox, matrix: {{ dimensions: {{ os: [l, m] }} }} }}\n  - {{ id: c, invoke: file:l.yaml, needs: [build], with: {{ v: \"{v}\" }} }}\n");
        let ir = ok(&yaml, &l);
        let z = step(&ir, "c/z");
        assert!(
            z.needs.0.contains(&"build[os=l]".to_string())
                && z.needs.0.contains(&"build[os=m]".to_string()),
            "{v}: {:?}",
            z.needs.0
        );
    }
    // Missing needs, in each form.
    for v in [
        "${{ outputs.build.v }}",
        "${{ outputs['build'].v }}",
        "${{ outputs[ \\\"build\\\" ].v }}",
    ] {
        let yaml = format!("steps:\n  - {{ id: build, image: busybox }}\n  - {{ id: c, invoke: file:l.yaml, with: {{ v: \"{v}\" }} }}\n");
        let e = errs(&yaml, &l);
        assert!(
            e.iter().any(|e| e.contains("does not `needs: [build]`")),
            "{v}: {e:?}"
        );
    }
}

/// CEL allows whitespace around `.`: `outputs .build.v`. The read is found
/// like `outputs.build.v`, so the missing need is refused.
#[test]
fn spaced_select_on_outputs_in_with_is_wired_or_refused() {
    let lib = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - { id: a, image: busybox }\n  - { id: z, image: busybox, needs: [a], command: [\"${{ inputs.v }}\"] }\n";
    let yaml = "steps:\n  - { id: build, image: busybox, results: [v] }\n  - { id: c, invoke: file:l.yaml, with: { v: \"${{ outputs .build.v }}\" } }\n";
    match compile(yaml, &files(&[("l.yaml", lib)])) {
        Err(e) => assert!(
            e.iter().any(|e| e.contains("does not `needs: [build]`")),
            "{e:?}"
        ),
        Ok(ir) => {
            let z = step(&ir, "c/z");
            assert!(
                z.needs.0.contains(&"build".to_string()),
                "compiled, but c/z cannot see outputs.build at launch: needs {:?}",
                z.needs.0
            );
        }
    }
}

// ================================================================ the cap

fn plain_steps(n: usize) -> String {
    let mut y = String::from("steps:\n");
    for i in 0..n {
        y.push_str(&format!("  - {{ id: s{i}, image: busybox }}\n"));
    }
    y
}

#[test]
fn survived_cap_off_by_one_without_invokes() {
    let none = Libraries::default();
    assert_eq!(
        ok(&plain_steps(MAX_COMPILED_STEPS), &none).steps.len(),
        1024
    );
    let e = errs(&plain_steps(MAX_COMPILED_STEPS + 1), &none);
    assert_eq!(
        e,
        vec!["step `s1024`: the pipeline compiles to more than 1024 Steps and gates after inlining `invoke:` steps and expanding matrices; the limit is 1024".to_string()]
    );
    // A matrix of exactly 1024 cells compiles; 1025 (5 x 205) does not.
    let dim = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let ir = ok(
        &format!("steps: [{{ id: m, image: busybox, matrix: {{ dimensions: {{ a: [{}], b: [{}] }} }} }}]", dim(4), dim(256)),
        &none,
    );
    assert_eq!(ir.steps.len(), 1024);
    let e = errs(
        &format!("steps: [{{ id: m, image: busybox, matrix: {{ dimensions: {{ a: [{}], b: [{}] }} }} }}]", dim(5), dim(205)),
        &none,
    );
    assert!(
        e[0].starts_with("step `m`: the pipeline compiles to more than 1024"),
        "{e:?}"
    );
}

/// The cap counts the cells a matrix keeps AFTER `exclude`: a 40 x 40 matrix
/// whose `exclude` keeps 40 cells compiles to 40 Steps. The product before
/// `exclude` has a larger bound of its own.
#[test]
fn cap_counts_a_matrix_after_exclude() {
    let dim = (0..40).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let yaml = format!("steps: [{{ id: m, image: busybox, matrix: {{ dimensions: {{ a: [{dim}], b: [{dim}] }}, exclude: [\"a != b\"] }} }}]");
    let ir = ok(&yaml, &Libraries::default());
    assert_eq!(ir.steps.len(), 40);
    // 257 x 256 = 65,792 combinations before `exclude`.
    let dim = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let yaml = format!(
        "steps: [{{ id: m, image: busybox, matrix: {{ dimensions: {{ a: [{}], b: [{}] }}, exclude: [\"a != b\"] }} }}]",
        dim(257),
        dim(256)
    );
    assert_eq!(
        errs(&yaml, &Libraries::default()),
        vec!["step `m`: the matrix has more than 65536 combinations before `exclude`; the limit is 65536".to_string()]
    );
}

/// An invoke that fails (plugin target, missing file, empty library) adds a
/// diagnostic and no Step. Fan-out over failing invokes (30^3 = 27,000 from
/// 3 small files) stops after 100 problems.
#[test]
fn failing_invokes_fan_out_stops_after_100_problems() {
    let fan = 30;
    let calls = |target: &str| -> String {
        (0..fan)
            .map(|k| format!("  - {{ id: k{k}, invoke: \"{target}\" }}\n"))
            .collect()
    };
    let l1 = format!("kind: library\nsteps:\n{}", calls("file:l2.yaml"));
    let l2 = format!("kind: library\nsteps:\n{}", calls("plugin:x/y"));
    let top = format!("steps:\n{}", calls("file:l1.yaml"));
    let t = Instant::now();
    let e = errs(&top, &files(&[("l1.yaml", &l1), ("l2.yaml", &l2)]));
    let took = t.elapsed();
    assert!(
        e.len() <= MAX_COMPILED_STEPS,
        "{} diagnostics in {took:?} from 3 files of {fan} lines",
        e.len()
    );
    assert_eq!(e.len(), 101, "{e:?}");
    assert_eq!(
        e.last().unwrap(),
        "the compiler stopped after 100 problems; fix these and compile again to see any others"
    );
}

/// Timing of one more level (30^4 = 810,000 failing invokes).
#[test]
#[ignore]
fn failing_invokes_fan_out_timing() {
    let fan = 30;
    let calls = |target: &str| -> String {
        (0..fan)
            .map(|k| format!("  - {{ id: k{k}, invoke: \"{target}\" }}\n"))
            .collect()
    };
    let l1 = format!("kind: library\nsteps:\n{}", calls("file:l2.yaml"));
    let l2 = format!("kind: library\nsteps:\n{}", calls("file:l3.yaml"));
    let l3 = format!("kind: library\nsteps:\n{}", calls("plugin:x/y"));
    let top = format!("steps:\n{}", calls("file:l1.yaml"));
    let t = Instant::now();
    let e = errs(
        &top,
        &files(&[("l1.yaml", &l1), ("l2.yaml", &l2), ("l3.yaml", &l3)]),
    );
    eprintln!("TIMING: {} diagnostics in {:?}", e.len(), t.elapsed());
}

// =================================================== library-level fields

#[test]
fn survived_library_level_fields() {
    // `deploy: true` on the library still marks every non-gate Step.
    let lib = "kind: library\ndeploy: true\nname: whatever\nsteps:\n  - { id: s, image: busybox }\n  - { id: g, gate: { approvers: [a] } }\n";
    match compile(
        "steps: [{ id: c, invoke: file:l.yaml }]",
        &files(&[("l.yaml", lib)]),
    ) {
        Ok(ir) => {
            assert!(step(&ir, "c/s").deploy);
            assert!(!step(&ir, "c/g").deploy);
        }
        Err(e) => {
            // The gate syntax may differ; retry without it.
            let lib =
                "kind: library\ndeploy: true\nname: whatever\nsteps: [{ id: s, image: busybox }]\n";
            let ir = ok(
                "steps: [{ id: c, invoke: file:l.yaml }]",
                &files(&[("l.yaml", lib)]),
            );
            assert!(step(&ir, "c/s").deploy, "{e:?}");
        }
    }
    // An exposed output naming no step is refused.
    let lib = "kind: library\ninterface: { outputs: [nope] }\nsteps: [{ id: s, image: busybox }]\n";
    let e = errs(
        "steps: [{ id: c, invoke: file:l.yaml }]",
        &files(&[("l.yaml", lib)]),
    );
    assert!(
        e.iter()
            .any(|e| e.contains("exposes output `nope` but has no step `nope`")),
        "{e:?}"
    );
}

// ===================================== ordinary Pipelines: no change in text

#[test]
fn survived_plain_pipeline_text_is_unchanged() {
    let yaml = "interface: { inputs: [{ name: p, type: string }] }\nsteps:\n  - { id: a, image: busybox, results: [v] }\n  - id: b\n    image: \"${{ .inputs.p }}\"\n    needs: [a]\n    when: \"inputs.p == 'x' // c.\"\n    command: [\"${{ inputs.p }}\", \"${{ outputs.a.v + inputs.p }}\"]\n";
    let ir = ok(yaml, &Libraries::default());
    let b = step(&ir, "b");
    assert_eq!(b.image, "${{ .inputs.p }}");
    assert_eq!(b.when.as_ref().unwrap().0, "inputs.p == 'x' // c.");
    assert_eq!(
        b.command,
        vec!["${{ inputs.p }}", "${{ outputs.a.v + inputs.p }}"]
    );
    assert_eq!(b.needs.0, vec!["a".to_string()]);
}

/// Control for `library_internal_output_read_through_nested_with`: the same
/// read written directly in a library Step (no nested invoke). If this fails
/// too, the defect is the library-internal `outputs.<id>` namespacing, not
/// the round-1 `with:` wiring.
#[test]
#[ignore = "held: finding 4 needs who wrote each byte, or caller reads are renamed too (wip/libraries-span-binding)"]
fn library_internal_output_read_direct() {
    let a = "kind: library\nsteps:\n  - { id: b, image: busybox, results: [tag] }\n  - { id: s, image: busybox, needs: [b], command: [\"${{ outputs.b.tag }}\"] }\n";
    let ir = ok(
        "steps: [{ id: c, invoke: file:a.yaml }]",
        &files(&[("a.yaml", a)]),
    );
    let s = step(&ir, "c/s");
    let outs: serde_json::Map<String, serde_json::Value> = s
        .needs
        .0
        .iter()
        .map(|n| (n.clone(), json!({ "tag": format!("TAG-of-{n}") })))
        .collect();
    assert_eq!(
        launch(s, json!({}), serde_json::Value::Object(outs)),
        "TAG-of-c/b",
        "command {:?} needs {:?}",
        s.command,
        s.needs.0
    );
}
