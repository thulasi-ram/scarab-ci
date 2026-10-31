//! Library scopes (ADR-0076), end to end over in-memory fakes: a Pipeline and
//! its libraries read from a FakeForge, dispatched with launch parameters,
//! compiled and stored by the server, then launched by the real scheduler.
//! Each test follows one way a value enters a library Step, and asserts what
//! the Step launched with.
//!
//! The rules: a library Step's `inputs` are its own bound inputs, never the
//! Run's launch parameters; a caller's `with:` template renders in the
//! caller's scope when the Step launches, then is typed and checked against
//! the input's `validate:`; rendered text is never rendered again.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;

use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{Db, FailureKind, RunId, Scheduler, StepId, StepSpec, StepStatus};
use scarab_forge::RepoRef;
use scarab_server::{dispatch_run, DispatchKind};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryDb};

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

/// A Run dispatched from `.scarab/ship.yaml` (`pipeline`) and the library
/// files in `libs`, with `params`, then driven by the scheduler until every
/// Step settles. `results` are what each Step (by id) emits on success.
struct Driven {
    db: Arc<InMemoryDb>,
    exec: FakeExecutor,
    run: RunId,
}

async fn drive(
    pipeline: &str,
    libs: &[(&str, &str)],
    params: serde_json::Value,
    results: &[(&str, serde_json::Value)],
) -> Driven {
    let mut forge = FakeForge::new()
        .with_file(".scarab/ship.yaml", pipeline)
        .with_commit("refs/heads/main", "5ba1");
    for (path, src) in libs {
        forge = forge.with_file(*path, *src);
    }
    let db = Arc::new(InMemoryDb::new());
    let clock = FakeClock::new(1_000);
    let params: BTreeMap<String, serde_json::Value> = serde_json::from_value(params).unwrap();
    let run = dispatch_run(
        &forge,
        db.as_ref(),
        &clock,
        scarab_server::AdmissionPorts::governance(None),
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ship".into(),
        params,
        DispatchKind::Manual,
        None,
    )
    .await
    .unwrap_or_else(|e| panic!("dispatch: {e}"));
    let exec = FakeExecutor::new();
    for _ in 0..64 {
        exec.script_outcome(ExecState::Succeeded);
    }
    for (step, r) in results {
        exec.set_results(step, serde_json::from_value(r.clone()).unwrap());
    }
    let sched = Scheduler::new(db.as_ref(), &clock, &exec, "scheduler-1");
    for _ in 0..24 {
        sched.tick(&run).await.expect("tick");
    }
    Driven { db, exec, run }
}

impl Driven {
    /// What `step` launched with, if it launched.
    fn launched(&self, step: &str) -> Option<StepSpec> {
        self.exec
            .launched_spec(&ExecHandle(format!("fake://{}/{step}/a1", self.run.0)))
    }

    /// What `step` launched with: its command, then the values of its
    /// probe env variables `A0`, `A1`, ... (a value from outside the
    /// Pipeline reaches a Step in env, ADR-0076, 2026-10-30).
    fn values(&self, step: &str) -> Vec<String> {
        let spec = self
            .launched(step)
            .unwrap_or_else(|| panic!("`{step}` did not launch"));
        spec.command
            .into_iter()
            .chain(
                spec.env
                    .into_iter()
                    .filter(|(k, _)| k.starts_with('A'))
                    .map(|(_, v)| v),
            )
            .collect()
    }

    /// The `SCARAB_PARAM_*` variables `step` launched with.
    fn params(&self, step: &str) -> Vec<(String, String)> {
        self.launched(step)
            .unwrap_or_else(|| panic!("`{step}` did not launch"))
            .env
            .into_iter()
            .filter(|(k, _)| k.starts_with("SCARAB_PARAM_"))
            .collect()
    }

    /// The status of `step` and, for a failure, its reason.
    async fn outcome(&self, step: &str) -> (StepStatus, Option<FailureKind>, Option<String>) {
        let s = self
            .db
            .steps_of_run(&self.run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step == StepId(step.into()))
            .unwrap_or_else(|| panic!("no step `{step}`"));
        let last = s.attempts.last();
        (
            s.status,
            last.and_then(|a| a.failure),
            last.and_then(|a| a.failure_detail.clone()),
        )
    }
}

const ON_MANUAL: &str = "on: { manual: {} }\n";

/// A one-Step library whose Step runs `echo` with the templates `args` (a
/// JSON list) in env variables `A0`, `A1`, ... (see [`Driven::values`]).
fn lib(inputs: &str, args: &str) -> String {
    let args: Vec<String> = serde_json::from_str(args).unwrap();
    let env: Vec<String> = args
        .iter()
        .enumerate()
        .map(|(i, a)| format!("[A{i}, {}]", serde_json::to_string(a).unwrap()))
        .collect();
    format!(
        "kind: library\ninterface: {{ inputs: [{inputs}] }}\nsteps:\n  - {{ id: s, image: busybox, command: [echo], env: [{}] }}\n",
        env.join(", ")
    )
}

#[tokio::test]
async fn a_literal_reaches_the_library_and_its_params_are_its_own_inputs() {
    let pipeline = format!(
        "{ON_MANUAL}interface: {{ inputs: [{{ name: region, type: string }}, {{ name: tag, type: string }}] }}\nsteps:\n  - {{ id: c, invoke: file:.scarab/lib/l.yaml, with: {{ tag: v1 }} }}\n"
    );
    let l = lib(
        "{ name: tag, type: string }",
        r#"["${{ inputs.tag }}", "${{ size(inputs) }}"]"#,
    );
    let d = drive(
        &pipeline,
        &[(".scarab/lib/l.yaml", &l)],
        json!({ "region": "eu", "tag": "RUN-TAG" }),
        &[],
    )
    .await;
    assert_eq!(d.values("c/s"), ["echo", "v1", "1"]);
    // `SCARAB_PARAM_*` for a library Step is its own inputs only, never the
    // Run's launch parameters.
    assert_eq!(
        d.params("c/s"),
        [("SCARAB_PARAM_TAG".to_string(), "v1".to_string())]
    );
}

#[tokio::test]
async fn a_caller_template_reads_the_callers_outputs() {
    let pipeline = format!(
        "{ON_MANUAL}steps:\n  - {{ id: build, image: busybox, results: [image] }}\n  - {{ id: c, invoke: file:.scarab/lib/l.yaml, needs: [build], with: {{ img: \"${{{{ outputs.build.image }}}}\" }} }}\n"
    );
    let l = "kind: library\ninterface: { inputs: [{ name: img, type: string }] }\nsteps:\n  - { id: build, image: busybox, results: [image] }\n  - { id: s, image: \"${{ inputs.img }}\", needs: [build], command: [echo], env: [[A0, \"${{ outputs.build.image }}\"]] }\n";
    let d = drive(
        &pipeline,
        &[(".scarab/lib/l.yaml", l)],
        json!({}),
        &[
            ("build", json!({ "image": "registry/app@sha256:caller" })),
            ("c/build", json!({ "image": "library-own" })),
        ],
    )
    .await;
    let s = d.launched("c/s").expect("launched");
    // The caller's `outputs.build` is the caller's Step; the library's
    // `outputs.build` is its own Step of the same name.
    assert_eq!(s.image, "registry/app@sha256:caller");
    assert_eq!(d.values("c/s"), ["echo", "library-own"]);
}

#[tokio::test]
async fn a_launch_parameter_passes_through_two_levels() {
    let pipeline = format!(
        "{ON_MANUAL}interface: {{ inputs: [{{ name: p, type: string }}] }}\nsteps:\n  - {{ id: c, invoke: file:.scarab/lib/a.yaml, with: {{ v: \"${{{{ inputs.p }}}}\" }} }}\n"
    );
    let a = "kind: library\ninterface: { inputs: [{ name: v, type: string }] }\nsteps:\n  - { id: n, invoke: file:.scarab/lib/b.yaml, with: { w: \"w-${{ inputs.v }}\" } }\n";
    let b = lib("{ name: w, type: string }", r#"["${{ inputs.w }}"]"#);
    let d = drive(
        &pipeline,
        &[(".scarab/lib/a.yaml", a), (".scarab/lib/b.yaml", &b)],
        json!({ "p": "P" }),
        &[],
    )
    .await;
    assert_eq!(d.values("c/n/s"), ["echo", "w-P"]);
    assert_eq!(
        d.params("c/n/s"),
        [("SCARAB_PARAM_W".to_string(), "w-P".to_string())]
    );
}

#[tokio::test]
async fn a_caller_passes_its_coordinate_and_a_library_never_reads_it() {
    let pipeline = format!(
        "{ON_MANUAL}steps:\n  - id: c\n    invoke: file:.scarab/lib/l.yaml\n    matrix: {{ dimensions: {{ os: [linux, mac] }} }}\n    with: {{ os: \"${{{{ matrix.os }}}}\" }}\n  - {{ id: peek, invoke: file:.scarab/lib/peek.yaml, matrix: {{ dimensions: {{ os: [linux] }} }} }}\n"
    );
    let l = lib("{ name: os, type: string }", r#"["${{ inputs.os }}"]"#);
    let peek =
        "kind: library\nsteps: [{ id: s, image: busybox, command: [\"${{ matrix.os }}\"] }]\n";
    let d = drive(
        &pipeline,
        &[(".scarab/lib/l.yaml", &l), (".scarab/lib/peek.yaml", peek)],
        json!({}),
        &[],
    )
    .await;
    assert_eq!(d.values("c[os=linux]/s"), ["echo", "linux"]);
    assert_eq!(d.values("c[os=mac]/s"), ["echo", "mac"]);
    // The library's `matrix` is its own Step's coordinate, which is empty:
    // reading the caller's fails the Step before it launches.
    assert!(d.launched("peek[os=linux]/s").is_none());
    let (status, kind, why) = d.outcome("peek[os=linux]/s").await;
    assert_eq!(
        (status, kind),
        (StepStatus::Failed, Some(FailureKind::Config))
    );
    assert!(why.unwrap_or_default().contains("os"));
}

#[tokio::test]
async fn a_template_is_typed_and_checked_against_validate_at_launch() {
    let pipeline = format!(
        "{ON_MANUAL}steps:\n  - {{ id: probe, image: busybox, results: [n, ok, ver] }}\n  - id: c\n    invoke: file:.scarab/lib/l.yaml\n    needs: [probe]\n    with: {{ n: \"${{{{ outputs.probe.n }}}}\", ok: \"${{{{ outputs.probe.ok }}}}\" }}\n  - id: v\n    invoke: file:.scarab/lib/v.yaml\n    needs: [probe]\n    with: {{ ver: \"${{{{ outputs.probe.ver }}}}\" }}\n"
    );
    let l = lib(
        "{ name: n, type: number }, { name: ok, type: boolean }",
        r#"["${{ inputs.n + 1 }}", "${{ !inputs.ok }}"]"#,
    );
    let v = lib(
        "{ name: ver, type: string, validate: \"value.startsWith('v')\" }",
        r#"["${{ inputs.ver }}"]"#,
    );
    let d = drive(
        &pipeline,
        &[(".scarab/lib/l.yaml", &l), (".scarab/lib/v.yaml", &v)],
        json!({}),
        &[("probe", json!({ "n": 5, "ok": true, "ver": "x1" }))],
    )
    .await;
    // An exact `${{ e }}` keeps its type: a number does arithmetic, a
    // boolean negates.
    assert_eq!(d.values("c/s"), ["echo", "6", "false"]);
    // A value that fails `validate:` fails the Step, as a config failure,
    // before anything launches.
    assert!(d.launched("v/s").is_none());
    let (status, kind, why) = d.outcome("v/s").await;
    assert_eq!(
        (status, kind),
        (StepStatus::Failed, Some(FailureKind::Config))
    );
    let why = why.unwrap_or_default();
    assert!(
        why.contains("input `ver`") && why.contains("failed validation"),
        "{why}"
    );
}

#[tokio::test]
async fn a_parameter_or_result_holding_a_template_stays_text() {
    let pipeline = format!(
        "{ON_MANUAL}interface: {{ inputs: [{{ name: p, type: string }}] }}\nsteps:\n  - {{ id: b, image: busybox, results: [v] }}\n  - {{ id: c, invoke: file:.scarab/lib/l.yaml, needs: [b], with: {{ p: \"${{{{ inputs.p }}}}\", v: \"${{{{ outputs.b.v }}}}\" }} }}\n  - {{ id: down, image: busybox, needs: [b], command: [echo], env: [[A0, \"${{{{ outputs.b.v }}}}\"], [A1, \"${{{{ inputs.p }}}}\"]] }}\n"
    );
    let l = lib(
        "{ name: p, type: string }, { name: v, type: string }",
        r#"["${{ inputs.p }}", "${{ inputs.v }}"]"#,
    );
    let evil = "${{ inputs.secret }}";
    let d = drive(
        &pipeline,
        &[(".scarab/lib/l.yaml", &l)],
        json!({ "p": evil }),
        &[("b", json!({ "v": evil }))],
    )
    .await;
    assert_eq!(d.values("c/s"), ["echo", evil, evil]);
    assert_eq!(d.values("down"), ["echo", evil, evil]);
    assert!(d
        .launched("c/s")
        .unwrap()
        .env
        .contains(&("SCARAB_PARAM_P".to_string(), evil.to_string())));
}

#[tokio::test]
async fn a_library_when_reads_its_own_inputs_when_the_run_is_created() {
    let pipeline = format!(
        "{ON_MANUAL}interface: {{ inputs: [{{ name: env, type: string }}] }}\nsteps:\n  - {{ id: c, invoke: file:.scarab/lib/l.yaml, with: {{ target: \"${{{{ inputs.env }}}}\" }} }}\n"
    );
    let l = "kind: library\ninterface: { inputs: [{ name: target, type: string }] }\nsteps:\n  - { id: prod, image: busybox, when: \"inputs.target == 'prod'\" }\n  - { id: any, image: busybox, when: \"inputs.target != 'prod'\" }\n";
    let d = drive(
        &pipeline,
        &[(".scarab/lib/l.yaml", l)],
        json!({ "env": "prod" }),
        &[],
    )
    .await;
    assert!(d.launched("c/prod").is_some());
    assert_eq!(d.outcome("c/any").await.0, StepStatus::Skipped);
}

/// A Step in the Pipeline file reads an invoke's exposed output as
/// `outputs.<invoke>.<name>` (ADR-0041 §2) through its scope's view of the
/// backing Step; the text is kept as written. Its other needs read as
/// before, and its `inputs` stay the Run's launch parameters.
#[tokio::test]
async fn a_pipeline_step_reads_an_invoke_output_through_its_view() {
    let pipeline = format!(
        "{ON_MANUAL}interface: {{ inputs: [{{ name: p, type: string }}] }}\nsteps:\n  - {{ id: other, image: busybox, results: [o] }}\n  - {{ id: d, invoke: file:.scarab/lib/l.yaml }}\n  - {{ id: notify, image: busybox, needs: [d, other], command: [echo], env: [[A0, \"${{{{ outputs.d.url }}}}\"], [A1, \"${{{{ outputs.other.o }}}}\"], [A2, \"${{{{ inputs.p }}}}\"]] }}\n"
    );
    let l = "kind: library\ninterface: { outputs: [url] }\nsteps:\n  - { id: url, image: busybox, results: [url] }\n  - { id: post, image: busybox, needs: [url] }\n";
    let d = drive(
        &pipeline,
        &[(".scarab/lib/l.yaml", l)],
        json!({ "p": "P" }),
        &[
            ("d/url", json!({ "url": "https://svc" })),
            ("other", json!({ "o": "O" })),
        ],
    )
    .await;
    assert_eq!(d.values("notify"), ["echo", "https://svc", "O", "P"]);
    assert_eq!(
        d.params("notify"),
        [("SCARAB_PARAM_P".to_string(), "P".to_string())]
    );
}
