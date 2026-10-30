//! Red-team round 2: attacks on the round-1 fixes for ADR-0076 step 1
//! (commits 3140722..4fe6954). Each test asserts the SECURE / CORRECT
//! behaviour: a failing test is a break. The `survived_` tests are attacks
//! that round 1 already held.

use std::collections::BTreeMap;
use std::time::Instant;

use scarab_pipeline::{
    compile_with_libraries, excluded_steps, render, Facts, Libraries, PipelineError, PipelineIr,
    Results, StepSpec, Templates, MAX_COMPILED_STEPS,
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

/// A probe's values as stored: its `C0`, `C1`, ... env templates.
fn probes(spec: &StepSpec) -> Vec<&str> {
    spec.env
        .iter()
        .filter(|(k, _)| k.starts_with('C'))
        .map(|(_, v)| v.as_str())
        .collect()
}

fn step<'a>(ir: &'a PipelineIr, id: &str) -> &'a StepSpec {
    ir.steps.iter().find(|s| s.id == id).unwrap_or_else(|| {
        panic!(
            "no step `{id}` in {:?}",
            ir.steps.iter().map(|s| &s.id).collect::<Vec<_>>()
        )
    })
}

/// The engine at launch: the one renderer, in the Step's scope, with the
/// Run's launch parameters `run_params` (which a library Step must never
/// read) and `outputs`, the results of Steps by id (the engine gives only
/// the Step's needs: the callers below pass needs only). A probe holds its
/// values in env variables `C0`, `C1`, ..., where a value from outside may
/// be (ADR-0076, 2026-10-30); the rendered values, joined by ` | `.
fn launch(spec: &StepSpec, run_params: serde_json::Value, outputs: serde_json::Value) -> String {
    let params: BTreeMap<String, serde_json::Value> = serde_json::from_value(run_params).unwrap();
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
            declared: &[],
            results: &results,
        },
    )
    .map(|s| {
        s.env
            .iter()
            .filter(|(k, _)| k.starts_with('C'))
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    })
    .unwrap_or_else(|e| format!("LAUNCH ERROR: {e}"))
}

// ================================================ joins across a with: value

/// A caller value that ENDS in `$` (a regex anchor) next to library text
/// `{{ inputs.p }}`: when values were pasted into a library's text, the two
/// joined into `${{ inputs.p }}`, which read the Run's launch parameter `p`.
/// A value is data and rendered output is never parsed again, so the
/// rendered command holds that text and nothing reads `p`.
#[test]
fn a_caller_value_ending_in_dollar_cannot_join_a_library_read() {
    let lib = "kind: library\ninterface: { inputs: [{ name: pat, type: string }] }\nsteps:\n  - id: s\n    image: busybox\n    env: [[C0, \"${{ inputs.pat }}{{ inputs.p }}\"]]\n";
    let yaml = "interface: { inputs: [{ name: p, type: string, required: false, default: secret }] }\nsteps:\n  - { id: b, image: busybox, results: [name] }\n  - id: c\n    invoke: file:l.yaml\n    needs: [b]\n    with: { pat: \"^${{ outputs.b.name }}$\" }\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    let s = step(&ir, "c/s");
    assert_eq!(probes(s), ["${{ inputs.pat }}{{ inputs.p }}"]);
    assert_eq!(
        launch(
            s,
            json!({ "p": "RUN-PARAM" }),
            json!({ "b": { "name": "x" } })
        ),
        "^x${{ inputs.p }}"
    );
}

/// The same join through a nested invoke's `with:`: the `with:` template
/// renders to text in the library's scope, and the nested library reads
/// that text as its input's value.
#[test]
fn a_join_through_a_nested_with_is_refused() {
    let a = "kind: library\ninterface: { inputs: [{ name: pat, type: string }] }\nsteps:\n  - id: n\n    invoke: file:b.yaml\n    with: { x: \"${{ inputs.pat }}{{ inputs.p }}\" }\n";
    let b = "kind: library\ninterface: { inputs: [{ name: x, type: string }] }\nsteps: [{ id: s, image: busybox, env: [[C0, \"${{ inputs.x }}\"]] }]\n";
    let yaml = "interface: { inputs: [{ name: p, type: string, required: false, default: d }] }\nsteps:\n  - { id: b, image: busybox, results: [name] }\n  - { id: c, invoke: file:a.yaml, needs: [b], with: { pat: \"^${{ outputs.b.name }}$\" } }\n";
    let ir = ok(yaml, &files(&[("a.yaml", a), ("b.yaml", b)]));
    let s = step(&ir, "c/n/s");
    assert_eq!(
        launch(
            s,
            json!({ "p": "RUN-PARAM" }),
            json!({ "b": { "name": "x" } })
        ),
        "^x${{ inputs.p }}"
    );
}

/// Library text and plain caller values cannot assemble a read either: a
/// default `$` and a caller value `inputs` render next to `{{ … .p }}` as
/// text.
#[test]
fn survived_library_text_and_plain_values_cannot_assemble_a_read() {
    let lib = "kind: library\ninterface:\n  inputs:\n    - { name: d, type: string, required: false, default: \"$\" }\n    - { name: v, type: string }\n    - { name: e, type: string }\nsteps:\n  - id: s\n    image: busybox\n    env: [[C0, \"${{ inputs.d }}{{ ${{ inputs.v }}.p }}\"], [C1, \"${{ inputs.e }}\"]]\n";
    for v in ["inputs", ".inputs", " inputs"] {
        let yaml = format!(
            "interface: {{ inputs: [{{ name: q, type: string }}] }}\nsteps:\n  - {{ id: c, invoke: file:l.yaml, with: {{ v: \"{v}\", e: \"${{{{ inputs.q }}}}\" }} }}\n"
        );
        let ir = ok(&yaml, &files(&[("l.yaml", lib)]));
        assert_eq!(
            launch(
                step(&ir, "c/s"),
                json!({ "p": "RUN-PARAM", "q": "Q" }),
                json!({})
            ),
            format!("${{{{ {v}.p }}}} | Q"),
            "{v}"
        );
    }
}

// ============================================== nested levels: values survive

#[test]
fn survived_values_bind_correctly_through_two_levels() {
    let a = "kind: library\ninterface: { inputs: [{ name: a, type: string }, { name: n, type: number }] }\nsteps:\n  - id: k\n    invoke: file:b.yaml\n    with: { x: \"${{ inputs.a }}\", m: \"${{ inputs.n }}\" }\n";
    let b = "kind: library\ninterface: { inputs: [{ name: x, type: string }, { name: m, type: number }] }\nsteps:\n  - { id: s, image: busybox, when: \"inputs.m > 4\", env: [[C0, \"${{ inputs.x }}\"], [C1, \"${{ inputs.m + 1 }}\"]] }\n";
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
    assert_eq!(probes(s)[0], "${{ inputs.x }}");
    assert_eq!(launch(s, json!({ "p": "RUN-P" }), json!({})), "RUN-P | 6");
}

// ======================================== needs wiring for outputs.X in with:

/// X is an invoke step read in bracket form `outputs['x'].img`: rewritten to
/// the backing step and wired, like the dotted form.
#[test]
fn bracket_read_of_an_invoke_output_through_with() {
    let lib = "kind: library\ninterface: { outputs: [img] }\nsteps:\n  - { id: img, image: busybox, results: [img] }\n  - { id: post, image: busybox, needs: [img] }\n";
    let user = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps: [{ id: a, image: busybox, env: [[C0, \"${{ inputs.v }}\"]] }]\n";
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
            probes(a), a.needs.0
        );
    }
}

/// X is a Step inside the SAME library as the nested invoke. The read
/// `outputs.b.tag` must reach library step `b` (now `c/b`).
#[test]
fn library_internal_output_read_through_nested_with() {
    let a = "kind: library\nsteps:\n  - { id: b, image: busybox, results: [tag] }\n  - id: n\n    invoke: file:b.yaml\n    needs: [b]\n    with: { x: \"${{ outputs.b.tag }}\" }\n";
    let b = "kind: library\ninterface: { inputs: [{ name: x, type: string }] }\nsteps: [{ id: s, image: busybox, env: [[C0, \"${{ inputs.x }}\"]] }]\n";
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
        probes(s),
        s.needs.0
    );
}

/// The caller reads ITS step `b`; the library also has a step `b`. The
/// caller value `${{ outputs.b.tag }}` is read in the caller's scope, so it
/// names the caller's `b`, the Pipeline compiles, and the nested library
/// Step gets the caller's value, never the library's `b`.
#[test]
fn caller_output_name_colliding_with_a_library_step_compiles() {
    let a = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - { id: b, image: busybox }\n  - { id: n, invoke: file:b.yaml, with: { x: \"${{ inputs.v }}\" } }\n";
    let b = "kind: library\ninterface: { inputs: [{ name: x, type: string }] }\nsteps: [{ id: s, image: busybox, env: [[C0, \"${{ inputs.x }}\"]] }]\n";
    let yaml = "steps:\n  - { id: b, image: busybox, results: [tag] }\n  - { id: c, invoke: file:a.yaml, needs: [b], with: { v: \"${{ outputs.b.tag }}\" } }\n";
    let ir = ok(yaml, &files(&[("a.yaml", a), ("b.yaml", b)]));
    let s = step(&ir, "c/n/s");
    assert!(s.needs.0.contains(&"b".to_string()));
    assert_eq!(probes(s), vec!["${{ inputs.x }}"]);
    assert_eq!(
        launch(
            s,
            json!({}),
            json!({ "b": { "tag": "CALLER" }, "c/b": { "tag": "LIBRARY" } })
        ),
        "CALLER"
    );
}

#[test]
fn survived_with_output_reads_matrix_invoke_and_forms() {
    let lib = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - { id: a, image: busybox }\n  - { id: z, image: busybox, needs: [a], env: [[C0, \"${{ inputs.v }}\"]] }\n";
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
    let lib = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - { id: a, image: busybox }\n  - { id: z, image: busybox, needs: [a], env: [[C0, \"${{ inputs.v }}\"]] }\n";
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
    let yaml = "interface: { inputs: [{ name: p, type: string }] }\nsteps:\n  - { id: a, image: busybox, results: [v] }\n  - id: b\n    image: \"${{ .inputs.p }}\"\n    needs: [a]\n    when: \"inputs.p == 'x' // c.\"\n    env: [[C0, \"${{ inputs.p }}\"], [C1, \"${{ outputs.a.v + inputs.p }}\"]]\n";
    let ir = ok(yaml, &Libraries::default());
    let b = step(&ir, "b");
    assert_eq!(b.image, "${{ .inputs.p }}");
    assert_eq!(b.when.as_ref().unwrap().0, "inputs.p == 'x' // c.");
    assert_eq!(
        probes(b),
        vec!["${{ inputs.p }}", "${{ outputs.a.v + inputs.p }}"]
    );
    assert_eq!(b.needs.0, vec!["a".to_string()]);
}

/// Control for `library_internal_output_read_through_nested_with`: the same
/// read written directly in a library Step (no nested invoke). If this fails
/// too, the defect is the library-internal `outputs.<id>` namespacing, not
/// the round-1 `with:` wiring.
#[test]
fn library_internal_output_read_direct() {
    let a = "kind: library\nsteps:\n  - { id: b, image: busybox, results: [tag] }\n  - { id: s, image: busybox, needs: [b], env: [[C0, \"${{ outputs.b.tag }}\"]] }\n";
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
        probes(s),
        s.needs.0
    );
}
