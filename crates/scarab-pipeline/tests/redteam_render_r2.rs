//! Red-team round 2 on "lexical scopes, render once" (ADR-0076, HEAD
//! effbd85): attacks on the round-1 fixes (leading-`-` rule, script check,
//! size caps, stored scopes). Each test asserts the SECURE behaviour: a
//! failing test is a break. Tests named `survived_` are attacks that held.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use scarab_pipeline::{
    compile_with_libraries, lint, render, validate, Facts, Libraries, PipelineError, PipelineIr,
    Results, Scope, StepSpec, Templates,
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

/// Run creation's preview of the command, as `create_run` calls it.
fn preview(spec: &StepSpec, params: Value) -> Result<scarab_pipeline::Rendered, String> {
    let params = map(params);
    render(
        Templates {
            image: "",
            command: &spec.command,
            env: &[],
        },
        spec.scope.as_ref(),
        &spec.matrix_values,
        Facts::Preview { params: &params },
    )
}

/// YAML for a flow-sequence element: a double-quoted string.
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

/// A Pipeline-file Step (scope: the Run) with one string input `p`, a need
/// `b` with result `v`, and `command`.
fn run_level(command: &[&str]) -> PipelineIr {
    let cmd: Vec<String> = command.iter().map(|c| q(c)).collect();
    let yaml = format!(
        "interface: {{ inputs: [{{ name: p, type: string }}, {{ name: e, type: string, required: false, default: \"\" }}] }}\nsteps:\n  - {{ id: b, image: busybox, results: [v] }}\n  - {{ id: s, image: busybox, needs: [b], command: [{}] }}\n",
        cmd.join(", ")
    );
    ok(&yaml, &Libraries::default())
}

const EVIL: &str = "--upload-pack=touch /tmp/pwned";

// ======================================================= 1. leading `-`

/// Characters that look like `-` but are not ASCII 0x2D, and a `-` after
/// something invisible. getopt only reads 0x2D at byte 0, so none of these is
/// an option; the rendered argument must not start with an ASCII `-`.
#[test]
fn survived_lookalike_dashes_and_prefixed_dashes_are_not_options() {
    let ir = run_level(&["git", "fetch", "origin", "${{ inputs.p }}"]);
    for v in [
        "\u{2010}-upload-pack=x",
        "\u{2011}-upload-pack=x",
        "\u{2012}-upload-pack=x",
        "\u{2013}-upload-pack=x",
        "\u{2014}-upload-pack=x",
        "\u{2015}-upload-pack=x",
        "\u{2212}-upload-pack=x",
        "\u{ff0d}-upload-pack=x",
        " --upload-pack=x",
        "\t--upload-pack=x",
        "\u{200b}--upload-pack=x",
        "\u{feff}--upload-pack=x",
        "\u{0}--upload-pack=x",
    ] {
        match launch(step(&ir, "s"), json!({ "p": v }), json!({})) {
            Err(_) => {}
            Ok(r) => assert!(!r.command[3].starts_with('-'), "{v:?} -> {:?}", r.command),
        }
    }
}

/// An empty first piece, own or outside, then an outside `-` value: the
/// argument still starts with the outside value.
#[test]
fn survived_empty_first_piece_then_outside_dash() {
    for arg in [
        "${{ '' }}${{ inputs.p }}",
        "${{ inputs.e }}${{ inputs.p }}",
        "${{ \"\" }}${{ '' }}${{ inputs.p }}",
        "${{ inputs.p }}${{ '' }}",
        "${{ outputs.b.v }}",
        "${{ '' }}${{ outputs.b.v }}",
    ] {
        let ir = run_level(&["git", "fetch", "origin", arg]);
        let r = launch(
            step(&ir, "s"),
            json!({ "p": EVIL, "e": "" }),
            json!({ "b": { "v": EVIL } }),
        );
        assert!(r.is_err(), "{arg}: {:?}", r.map(|r| r.command));
    }
}

/// Expressions that read an outside value through a function, an operator,
/// a ternary, a list or map literal, a comprehension (whose variable is
/// named `matrix`), or an index. Each is outside.
#[test]
fn survived_outside_value_through_cel_operators_is_outside() {
    let mut launched = Vec::new();
    for arg in [
        "${{ string(inputs.p) }}",
        "${{ inputs.p + '' }}",
        "${{ '' + inputs.p }}",
        "${{ true ? inputs.p : 'x' }}",
        "${{ inputs.p == '' ? 'x' : inputs.p }}",
        "${{ [inputs.p][0] }}",
        "${{ {'k': inputs.p}.k }}",
        "${{ {'k': inputs.p}['k'] }}",
        "${{ inputs['p'] }}",
        "${{ dyn(inputs).p }}",
        "${{ [inputs.p].map(matrix, matrix)[0] }}",
        "${{ [inputs.p].map(x, x)[0] }}",
        "${{ [inputs.p].filter(matrix, true)[0] }}",
        "${{ outputs['b'].v }}",
        "${{ outputs.b['v'] }}",
        "${{ [outputs.b.v][0] }}",
    ] {
        let ir = run_level(&["git", "fetch", "origin", arg]);
        if let Ok(r) = launch(
            step(&ir, "s"),
            json!({ "p": EVIL }),
            json!({ "b": { "v": EVIL } }),
        ) {
            if r.command[3].starts_with('-') {
                launched.push((arg, r.command[3].clone()));
            }
        }
    }
    assert!(launched.is_empty(), "outside `-` value launched: {launched:#?}");
}

/// The same expressions in a library, two levels from the Run parameter,
/// each level passing it on with an exact `${{ }}` or through CEL.
#[test]
fn survived_outside_value_two_library_levels_down_is_outside() {
    let mut launched = Vec::new();
    for (l1_with, l2_arg) in [
        ("${{ inputs.a }}", "${{ inputs.v }}"),
        ("${{ inputs.a + '' }}", "${{ inputs.v }}"),
        ("${{ string(inputs.a) }}", "${{ [inputs.v][0] }}"),
        ("${{ true ? inputs.a : 'x' }}", "${{ {'k': inputs.v}.k }}"),
        ("${{ inputs.a }}", "${{ inputs['v'] }}"),
        ("${{ inputs.a }}", "${{ dyn(inputs).v }}"),
        ("${{ inputs.a }}", "${{ inputs.lit }}${{ inputs.v }}"),
    ] {
        let l2 = format!(
            "kind: library\ninterface: {{ inputs: [{{ name: v, type: string }}, {{ name: lit, type: string, required: false, default: \"\" }}] }}\nsteps: [{{ id: s, image: alpine/git, command: [git, fetch, origin, {}] }}]\n",
            q(l2_arg)
        );
        let l1 = format!(
            "kind: library\ninterface: {{ inputs: [{{ name: a, type: string }}] }}\nsteps: [{{ id: n, invoke: file:l2.yaml, with: {{ v: {} }} }}]\n",
            q(l1_with)
        );
        let yaml = "interface: { inputs: [{ name: ref, type: string }] }\nsteps: [{ id: c, invoke: file:l1.yaml, with: { a: \"${{ inputs.ref }}\" } }]\n";
        let libs = files(&[("l1.yaml", &l1), ("l2.yaml", &l2)]);
        let Ok(ir) = compile(yaml, &libs) else { continue };
        if let Ok(r) = launch(step(&ir, "c/n/s"), json!({ "ref": EVIL }), json!({})) {
            if r.command[3].starts_with('-') {
                launched.push((l1_with, l2_arg, r.command[3].clone()));
            }
        }
    }
    assert!(launched.is_empty(), "outside `-` value launched: {launched:#?}");
}

/// A library's matrix value that holds `${{ inputs.v }}` (an outside
/// value through `with:`) is refused, or stays text: it is never a read.
#[test]
fn survived_matrix_value_naming_an_input_is_not_a_read() {
    let lib = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - id: s\n    image: busybox\n    command: [git, fetch, \"${{ matrix.m }}\"]\n    matrix: { dimensions: { m: [\"${{ inputs.v }}\"] } }\n";
    let yaml = "interface: { inputs: [{ name: ref, type: string }] }\nsteps: [{ id: c, invoke: file:l.yaml, with: { v: \"${{ inputs.ref }}\" } }]\n";
    let Ok(ir) = compile(yaml, &files(&[("l.yaml", lib)])) else {
        return;
    };
    let s = ir.steps.iter().find(|s| s.id.starts_with("c/s")).unwrap();
    let r = launch(s, json!({ "ref": EVIL }), json!({})).unwrap();
    assert_eq!(r.command[2], "${{ inputs.v }}");
}

/// The `--` exemption assumes the `--` ends the options of the program
/// that reads the next argument. In `sh -c 'script' -- "$value"` the `--`
/// is `$0`, not an end of options, and the script passes `$1` to git:
/// `git fetch origin --upload-pack=…` runs a command. The script holds no
/// `${{`, so the script check is silent too.
#[test]
#[ignore = "red: render r2"]
fn dash_dash_exemption_does_not_cover_a_shell_positional() {
    let ir = run_level(&[
        "sh",
        "-c",
        "git fetch origin \"$1\"",
        "--",
        "${{ inputs.p }}",
    ]);
    let r = launch(step(&ir, "s"), json!({ "p": EVIL }), json!({}));
    assert!(
        r.is_err(),
        "an outside `-` value reached `$1` of a shell script past `--` ($0): {:?}",
        r.map(|r| r.command)
    );
}

/// `--` in front of a program that forwards what follows to another
/// program as options: `npm run <script> -- <args>` and `cargo run --
/// <args>` hand `<args>` to the script / binary, which reads `-` as an
/// option.
#[test]
#[ignore = "red: render r2"]
fn dash_dash_exemption_does_not_cover_forwarded_arguments() {
    let mut launched = Vec::new();
    for cmd in [
        &["npm", "run", "lint", "--", "${{ inputs.p }}"][..],
        &["cargo", "run", "--", "${{ inputs.p }}"][..],
        &["yarn", "test", "--", "${{ inputs.p }}"][..],
    ] {
        let ir = run_level(cmd);
        if let Ok(r) = launch(step(&ir, "s"), json!({ "p": "--config=/tmp/evil.js" }), json!({})) {
            launched.push(r.command);
        }
    }
    assert!(launched.is_empty(), "outside `-` value forwarded past `--`: {launched:#?}");
}

/// Preview and launch with the same Run parameters agree on whether the
/// leading-`-` rule refuses a library Step's command: the value is a Run
/// parameter, known at Run creation, reached through an exact `with:`.
#[test]
#[ignore = "red: render r2"]
fn preview_and_launch_agree_on_leading_dash_through_a_library() {
    let lib = "kind: library\ninterface: { inputs: [{ name: r, type: string }] }\nsteps: [{ id: s, image: alpine/git, command: [git, fetch, origin, \"${{ inputs.r }}\"] }]\n";
    let yaml = "interface: { inputs: [{ name: ref, type: string }] }\nsteps: [{ id: c, invoke: file:l.yaml, with: { r: \"${{ inputs.ref }}\" } }]\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    let s = step(&ir, "c/s");
    let at_launch = launch(s, json!({ "ref": EVIL }), json!({})).is_err();
    let at_preview = preview(s, json!({ "ref": EVIL })).is_err();
    assert!(at_launch, "launch accepted");
    assert_eq!(
        at_preview, at_launch,
        "preview (Run creation) says the Step will launch; the launch refuses it"
    );
}

/// The Pipeline-file form: preview and launch agree.
#[test]
fn survived_preview_and_launch_agree_on_leading_dash_in_the_pipeline() {
    let ir = run_level(&["git", "fetch", "origin", "${{ inputs.p }}"]);
    let s = step(&ir, "s");
    for v in [EVIL, "main", "-", "x-"] {
        let a = launch(s, json!({ "p": v }), json!({})).is_err();
        let b = preview(s, json!({ "p": v })).is_err();
        assert_eq!(a, b, "{v}");
    }
}

/// A program that splits one argument into a command line again (`env
/// -S`) or hands it to a shell (`ssh host '…'`, `su -c`, `flock -c`):
/// literal text in front of the outside value passes the leading-`-` rule,
/// and the script check does not know the program, so the Pipeline
/// compiles and the launch hands the value to a parser that reads `-` or
/// `;` as code.
#[test]
#[ignore = "held: owner decision on script scope"]
fn an_outside_value_in_a_re_split_argument_is_refused() {
    let mut launched = Vec::new();
    for (cmd, value) in [
        (&["env", "-S", "git fetch origin ${{ inputs.p }}"][..], EVIL),
        (&["ssh", "deploy@prod", "cd app && git checkout ${{ inputs.p }}"][..], "main; id"),
        (&["su", "-c", "git checkout ${{ inputs.p }}"][..], "main; id"),
        (&["flock", "/tmp/l", "-c", "git checkout ${{ inputs.p }}"][..], "main; id"),
    ] {
        let cmd_yaml: Vec<String> = cmd.iter().map(|c| q(c)).collect();
        let yaml = format!(
            "interface: {{ inputs: [{{ name: p, type: string }}] }}\nsteps: [{{ id: s, image: busybox, command: [{}] }}]\n",
            cmd_yaml.join(", ")
        );
        let Ok(ir) = compile(&yaml, &Libraries::default()) else { continue };
        if let Ok(r) = launch(step(&ir, "s"), json!({ "p": value }), json!({})) {
            launched.push(r.command);
        }
    }
    assert!(launched.is_empty(), "compiled and launched: {launched:#?}");
}

// ============================================== 2. program-name control

/// An outside value as the program a Step runs, directly or after a
/// wrapper. Whoever dispatches the Run chooses the program.
#[test]
#[ignore = "red: render r2"]
fn an_outside_value_may_not_name_the_program() {
    let mut launched = Vec::new();
    for cmd in [
        &["${{ inputs.p }}", "build"][..],
        &["env", "${{ inputs.p }}", "build"][..],
        &["env", "--", "${{ inputs.p }}", "build"][..],
        &["timeout", "60", "${{ inputs.p }}", "build"][..],
        &["sudo", "${{ inputs.p }}", "build"][..],
        &["xargs", "${{ inputs.p }}"][..],
    ] {
        let ir = run_level(cmd);
        if let Ok(r) = launch(step(&ir, "s"), json!({ "p": "/bin/sh" }), json!({})) {
            launched.push(r.command);
        }
    }
    assert!(launched.is_empty(), "outside value chose the program: {launched:#?}");
}

// ================================================== 3. script check

/// The script check's findings for `command`, with `p` a Run parameter.
fn script_findings(command: &str) -> (Vec<String>, Result<(), Vec<String>>) {
    let yaml = format!(
        "interface: {{ inputs: [{{ name: p, type: string }}, {{ name: sh, type: string, required: false, default: sh }}] }}\nsteps: [{{ id: s, image: busybox, command: {command}, matrix: {{ dimensions: {{ shell: [sh] }} }} }}]\n"
    );
    let ir = match compile(&yaml, &Libraries::default()) {
        Ok(ir) => ir,
        Err(e) => return (Vec::new(), Err(e)),
    };
    let warns: Vec<String> = lint(&ir)
        .into_iter()
        .filter(|w| w.contains("is a script"))
        .collect();
    (warns, validate(&ir))
}

fn missed(cmds: &[&str]) -> Vec<String> {
    cmds.iter()
        .filter(|cmd| {
            let (warns, errs) = script_findings(cmd);
            warns.is_empty() && errs.is_ok()
        })
        .map(|c| c.to_string())
        .collect()
}

/// Forms round 1 did not try that the check does read.
#[test]
fn survived_script_check_reads_these_forms() {
    let m = missed(&[
        r#"[bash, -lc, "echo ${{ inputs.p }}"]"#,
        r#"[bash, -euxo, pipefail, -c, "echo ${{ inputs.p }}"]"#,
        r#"[bash, --login, -c, "echo ${{ inputs.p }}"]"#,
        r#"[sh, -c, -e, "echo ${{ inputs.p }}"]"#,
        r#"[/bin/busybox, ash, -c, "echo ${{ inputs.p }}"]"#,
        r#"[./sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[python3, -I, -c, "print('${{ inputs.p }}')"]"#,
        r#"[python3, -Wignore, -c, "print('${{ inputs.p }}')"]"#,
        r#"[python3, "-c${{ inputs.p }}"]"#,
        r#"[node, --eval, "${{ inputs.p }}"]"#,
        r#"[node, "--eval=${{ inputs.p }}"]"#,
        r#"[perl, -E, "say '${{ inputs.p }}'"]"#,
        r#"[perl, -Mstrict, -le, "${{ inputs.p }}"]"#,
        r#"[ruby, -rjson, -e, "${{ inputs.p }}"]"#,
        r#"[nice, -n, "5", sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[env, -i, PATH=/bin, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[sudo, -u, root, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[stdbuf, -oL, bash, -c, "echo ${{ inputs.p }}"]"#,
        r#"[timeout, -s, KILL, 60, sh, -c, "echo ${{ inputs.p }}"]"#,
    ]);
    assert!(m.is_empty(), "script check missed: {m:#?}");
}

/// `node -pe 'code'` (print + eval, the common spelling): the check reads
/// `-pe` as `-p` with its script glued on (`e`), so the script it checks
/// is `-pe` and the next argument, the real script, is not checked.
#[test]
#[ignore = "red: render r2"]
fn script_check_reads_node_pe() {
    let m = missed(&[
        r#"[node, -pe, "${{ inputs.p }}"]"#,
        r#"[node, -ep, "${{ inputs.p }}"]"#,
    ]);
    assert!(m.is_empty(), "script check missed: {m:#?}");
}

/// Wrappers the check does not skip, and `env`'s `-S` (split string),
/// whose argument is itself a command line.
#[test]
#[ignore = "held: owner decision on script scope"]
fn script_check_sees_through_other_wrappers() {
    let m = missed(&[
        r#"[env, -S, "sh -c 'echo ${{ inputs.p }}'"]"#,
        r#"[env, "--split-string=sh -c 'echo ${{ inputs.p }}'"]"#,
        r#"[env, -iS, "sh -c 'echo ${{ inputs.p }}'"]"#,
        r#"[xargs, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[find, ., -maxdepth, "0", -exec, sh, -c, "echo ${{ inputs.p }}", ";"]"#,
        r#"[tini, --, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[dumb-init, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[gosu, nobody, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[su-exec, nobody, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[su, -c, "echo ${{ inputs.p }}"]"#,
        r#"[doas, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[chroot, /, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[flock, /tmp/l, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[flock, /tmp/l, -c, "echo ${{ inputs.p }}"]"#,
        r#"[ionice, -c, "3", sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[unshare, -r, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[script, -qc, "echo ${{ inputs.p }}", /dev/null]"#,
        r#"[watch, -n1, "echo ${{ inputs.p }}"]"#,
        r#"[strace, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[time, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[ssh, deploy@host, "cd app && git checkout ${{ inputs.p }}"]"#,
        r#"[kubectl, exec, pod, --, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[docker, run, --rm, alpine, sh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[uv, run, python, -c, "print('${{ inputs.p }}')"]"#,
    ]);
    assert!(m.is_empty(), "script check missed: {m:#?}");
}

/// Interpreters and programs that run a script from their arguments,
/// which the check does not know.
#[test]
#[ignore = "held: owner decision on script scope"]
fn script_check_sees_other_interpreters() {
    let m = missed(&[
        r#"[pwsh, -Command, "echo ${{ inputs.p }}"]"#,
        r#"[powershell, -c, "echo ${{ inputs.p }}"]"#,
        r#"[deno, eval, "console.log('${{ inputs.p }}')"]"#,
        r#"[bun, -e, "console.log('${{ inputs.p }}')"]"#,
        r#"[php, -r, "echo '${{ inputs.p }}';"]"#,
        r#"[fish, -c, "echo ${{ inputs.p }}"]"#,
        r#"[tcsh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[csh, -c, "echo ${{ inputs.p }}"]"#,
        r#"[pypy3, -c, "print('${{ inputs.p }}')"]"#,
        r#"[Rscript, -e, "print('${{ inputs.p }}')"]"#,
        r#"[lua, -e, "print('${{ inputs.p }}')"]"#,
        r#"[awk, "BEGIN { system(\"echo ${{ inputs.p }}\") }"]"#,
        r#"[make, "--eval=x: ; echo ${{ inputs.p }}", x]"#,
        r#"[git, -c, "core.sshCommand=${{ inputs.p }}", fetch]"#,
        r#"[python3, -m, timeit, "${{ inputs.p }}"]"#,
        r#"["${{ matrix.shell }}", -c, "echo ${{ inputs.p }}"]"#,
        r#"["${{ inputs.sh }}", -c, "echo ${{ inputs.p }}"]"#,
    ]);
    assert!(m.is_empty(), "script check missed: {m:#?}");
}

/// Classification: each of these is an outside value in a script, so an
/// error, never a warning.
#[test]
fn survived_script_check_classifies_these_as_outside() {
    // A Pipeline input with a default that the dispatcher may override.
    let yaml = "interface: { inputs: [{ name: p, type: string, required: false, default: safe }] }\nsteps: [{ id: s, image: busybox, command: [sh, -c, \"echo ${{ inputs.p }}\"] }]\n";
    assert!(compile(yaml, &Libraries::default()).is_err(), "default-able Run param");
    // A library input bound to `${{ outputs.b.v }}` by its caller.
    let lib = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps: [{ id: s, image: busybox, command: [sh, -c, \"echo ${{ inputs.v }}\"] }]\n";
    let yaml = "steps:\n  - { id: b, image: busybox, results: [v] }\n  - { id: c, invoke: file:l.yaml, needs: [b], with: { v: \"${{ outputs.b.v }}\" } }\n";
    assert!(compile(yaml, &files(&[("l.yaml", lib)])).is_err(), "result via with:");
    // Two levels, the outer one by CEL over a Run parameter.
    let l1 = "kind: library\ninterface: { inputs: [{ name: a, type: string }] }\nsteps: [{ id: n, invoke: file:l.yaml, with: { v: \"pre-${{ inputs.a + '' }}\" } }]\n";
    let yaml = "interface: { inputs: [{ name: p, type: string }] }\nsteps: [{ id: c, invoke: file:l1.yaml, with: { a: \"${{ inputs.p }}\" } }]\n";
    assert!(
        compile(yaml, &files(&[("l.yaml", lib), ("l1.yaml", l1)])).is_err(),
        "Run param two levels down"
    );
    // A literal `with:` and a library default are own: a warning.
    let yaml = "steps: [{ id: c, invoke: file:l.yaml, with: { v: hello } }]\n";
    let ir = ok(yaml, &files(&[("l.yaml", lib)]));
    assert!(lint(&ir).iter().any(|w| w.contains("is a script")));
}

// ======================================================= 4. size caps

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let v = f();
    (v, t.elapsed())
}

/// YAML aliases expanding a small file into a large value (billion laughs).
#[test]
fn survived_yaml_alias_expansion_is_bounded() {
    let mut yaml = String::from("x0: &a0 [\"lol\", \"lol\", \"lol\", \"lol\", \"lol\", \"lol\", \"lol\", \"lol\", \"lol\"]\n");
    for i in 1..10 {
        let refs = vec![format!("*a{}", i - 1); 9].join(", ");
        yaml.push_str(&format!("x{i}: &a{i} [{refs}]\n"));
    }
    yaml.push_str("steps: [{ id: s, image: busybox, env: [[A, *a9]] }]\n");
    let (r, took) = timed(|| compile(&yaml, &Libraries::default()));
    eprintln!("billion laughs: {} B -> {:?} in {took:?}", yaml.len(), r.as_ref().map(|_| ()));
    assert!(took < Duration::from_secs(3), "{took:?}");
    // The same in a library.
    let lib = format!("kind: library\n{yaml}");
    let (r, took) = timed(|| compile("steps: [{ id: c, invoke: file:l.yaml }]\n", &files(&[("l.yaml", &lib)])));
    eprintln!("billion laughs (library): {:?} in {took:?}", r.as_ref().map(|_| ()));
    assert!(took < Duration::from_secs(3), "{took:?}");
}

/// A library just under 1 MiB of comments, with one small Step, invoked
/// many times: each invoke parses the library's source again (and the
/// level's output scan parses it once more), so the compile time grows with
/// invokes x library size, not with what is compiled. 1000 invokes took
/// 7.8 s (release) and 70 s (debug); 8192 invoke expansions are allowed.
#[test]
#[ignore = "red: render r2"]
fn compile_time_is_bounded_for_many_invokes_of_a_large_library() {
    let small = String::from("kind: library\nsteps: [{ id: s, image: busybox }]\n");
    let mut large = small.clone();
    let line = format!("#{}\n", "x".repeat(1022));
    while large.len() + line.len() < (1 << 20) - 64 {
        large.push_str(&line);
    }
    let mut yaml = String::from("steps:\n");
    for i in 0..200 {
        yaml.push_str(&format!("  - {{ id: c{i}, invoke: file:l.yaml }}\n"));
    }
    let (a, small_took) = timed(|| compile(&yaml, &files(&[("l.yaml", &small)])));
    let (b, large_took) = timed(|| compile(&yaml, &files(&[("l.yaml", &large)])));
    eprintln!(
        "200 invokes: {} B library {small_took:?}, {} B library {large_took:?} ({:?}, {:?})",
        small.len(),
        large.len(),
        a.as_ref().map(|ir| ir.steps.len()),
        b.as_ref().map(|ir| ir.steps.len()),
    );
    assert!(
        large_took < Duration::from_secs(1) || large_took < small_took * 5,
        "the same 200 Steps took {large_took:?} from a 1 MiB library, {small_took:?} from a small one"
    );
}

/// A matrix under the combination cap whose `exclude` list is long: each
/// predicate is checked on every combination before the Step cap is, and
/// the list has no length cap. A 1.2 KB file with 64 predicates took 6-8 s
/// (release) and 17.5 s (debug); 1 MiB holds ~50 000 predicates.
#[test]
#[ignore = "red: render r2"]
fn compile_time_is_bounded_for_a_long_matrix_exclude_list() {
    let mut dims = String::new();
    for d in 0..16 {
        dims.push_str(&format!("d{d}: [a, b], "));
    }
    let mut excl = String::new();
    for i in 0..48 {
        excl.push_str(&format!("\"d0 == 'z{i}'\", "));
    }
    let yaml = format!(
        "steps: [{{ id: s, image: busybox, matrix: {{ dimensions: {{ {dims} }}, exclude: [{excl}] }} }}]\n"
    );
    let (r, took) = timed(|| compile(&yaml, &Libraries::default()));
    eprintln!(
        "{} B source, 65536 combinations x 48 excludes: {:?} in {took:?}",
        yaml.len(),
        r.as_ref().map(|ir| ir.steps.len()).map_err(|e| e[0].chars().take(80).collect::<String>())
    );
    assert!(took < Duration::from_secs(3), "{took:?}");
}

/// This process's resident set size, in bytes, by `ps` (macOS and Linux).
fn rss() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().unwrap_or(0) * 1024
}

/// The most this process's RSS rose above its start while `f` ran,
/// sampled every 20 ms.
fn peak_rss_growth<T>(f: impl FnOnce() -> T) -> (T, u64) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    let base = rss();
    let done = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(base));
    let sampler = {
        let (done, peak) = (done.clone(), peak.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                peak.fetch_max(rss(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let v = f();
    peak.fetch_max(rss(), Ordering::Relaxed);
    done.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    (v, peak.load(Ordering::Relaxed).saturating_sub(base))
}

/// A matrix of 16 dimensions with 2 long values each: 65 536 combinations,
/// each a copy of every dimension's value, built (with a CEL context per
/// combination) before the Step cap is checked. A 33 KB file; memory grows
/// with value length x 65 536 x 2, so a file near 1 MiB asks for ~70 GB.
#[test]
#[ignore = "red: render r2"]
fn compile_memory_is_bounded_for_a_matrix_of_long_values() {
    let v = "v".repeat(1024);
    let mut dims = String::new();
    for d in 0..16 {
        dims.push_str(&format!("d{d}: [a{v}, b{v}], "));
    }
    let yaml = format!("steps: [{{ id: s, image: busybox, matrix: {{ dimensions: {{ {dims} }} }} }}]\n");
    let ((r, took), grew) = peak_rss_growth(|| timed(|| compile(&yaml, &Libraries::default())));
    eprintln!(
        "{} B source -> RSS +{} MB, {:?} in {took:?}",
        yaml.len(),
        grew >> 20,
        r.as_ref().map(|ir| ir.steps.len()).map_err(|e| e[0].chars().take(80).collect::<String>())
    );
    assert!(grew < 256 << 20, "a {} B Pipeline grew RSS by {} MB", yaml.len(), grew >> 20);
}

/// A library input with a long `options` list and a `validate:`, bound by
/// a template: the declaration is stored on each inlined Step. The caps
/// stop the IR; the compile must stay fast.
#[test]
fn survived_long_options_list_per_binding_is_capped_and_fast() {
    let opts: Vec<String> = (0..40_000).map(|i| format!("o{i}")).collect();
    let mut steps = String::new();
    for i in 0..200 {
        steps.push_str(&format!("  - {{ id: s{i}, image: busybox }}\n"));
    }
    let lib = format!(
        "kind: library\ninterface: {{ inputs: [{{ name: v, type: choice, options: [{}] }}] }}\nsteps:\n{steps}",
        opts.join(", ")
    );
    let yaml = "interface: { inputs: [{ name: p, type: string }] }\nsteps: [{ id: c, invoke: file:l.yaml, with: { v: \"${{ inputs.p }}\" } }]\n";
    let (r, took) = timed(|| compile(yaml, &files(&[("l.yaml", &lib)])));
    let out = r.as_ref().map(|ir| serde_json::to_vec(ir).unwrap().len());
    eprintln!("{} B library -> {:?} in {took:?}", lib.len(), out.as_ref().map_err(|e| e[0].chars().take(80).collect::<String>()));
    assert!(took < Duration::from_secs(5), "{took:?}");
    if let Ok(n) = out {
        assert!(n <= 16 << 20, "{n}");
    }
}

// ================================================= 5. stored scopes

/// A stored template binding with no declared type does not render (so no
/// `validate:` is skipped).
#[test]
fn survived_stored_template_binding_without_param_does_not_render() {
    let scope: Scope = serde_json::from_value(json!({
        "inputs": { "bound": { "v": { "value": { "template": "${{ inputs.ref }}" } } } },
        "caller": { "inputs": "run" }
    }))
    .unwrap();
    let spec: StepSpec = serde_json::from_value(json!({
        "id": "s", "image": "busybox", "command": ["echo", "${{ inputs.v }}"], "scope": scope
    }))
    .unwrap();
    let r = launch(&spec, json!({ "ref": "main" }), json!({}));
    assert!(r.is_err(), "{r:?}");
}
