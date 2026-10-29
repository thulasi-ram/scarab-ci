//! LocalExecutor acceptance (ADR-0036): steps run as host child processes.
//! A zero-exit command Succeeds, a non-zero command Fails with its code,
//! relaunching a fence re-attaches, an unknown handle is Lost, and cancel kills
//! a running child. No Docker/k8s.

use scarab_engine::ports::{ExecState, FailureClass};
use scarab_engine::{Executor, RunId, StepId, StepRun, StepSpec};
use scarab_executor_local::LocalExecutor;

fn step(run: &str, id: &str) -> StepRun {
    StepRun::new(RunId(run.into()), StepId(id.into()))
}

fn spec(cmd: &[&str]) -> StepSpec {
    StepSpec {
        image: String::new(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        forge_token: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
    }
}

/// Poll until terminal (bounded so a bug can't hang the test).
async fn drive(exec: &LocalExecutor, handle: &scarab_engine::ports::ExecHandle) -> ExecState {
    for _ in 0..200 {
        match exec.poll(handle).await.unwrap() {
            ExecState::Pending | ExecState::Running => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            terminal => return terminal,
        }
    }
    panic!("process did not finish within the poll budget");
}

#[tokio::test]
async fn zero_exit_command_succeeds() {
    let exec = LocalExecutor::new();
    let s = step("r1", "ok");
    let handle = exec
        .launch(&s, &spec(&["sh", "-c", "exit 0"]))
        .await
        .unwrap();
    assert_eq!(drive(&exec, &handle).await, ExecState::Succeeded);
}

#[tokio::test]
async fn nonzero_exit_command_fails_with_its_code() {
    let exec = LocalExecutor::new();
    let s = step("r1", "boom");
    let handle = exec
        .launch(&s, &spec(&["sh", "-c", "exit 3"]))
        .await
        .unwrap();
    // A non-zero exit is the step's own verdict: class Step (ADR-0047).
    assert_eq!(
        drive(&exec, &handle).await,
        ExecState::Failed {
            exit_code: Some(3),
            class: FailureClass::Step,
            cause: None,
        }
    );
}

/// ADR-0047: a signal kill has no exit code — the platform ended a started
/// process, so the local executor classifies it post-start `Infra`.
#[tokio::test]
async fn signal_kill_is_post_start_infra() {
    let exec = LocalExecutor::new();
    let s = step("r1", "sigkill");
    let handle = exec
        .launch(&s, &spec(&["sh", "-c", "kill -9 $$"]))
        .await
        .unwrap();
    assert_eq!(
        drive(&exec, &handle).await,
        ExecState::Failed {
            exit_code: None,
            class: FailureClass::Infra {
                never_started: false
            },
            cause: None,
        }
    );
}

#[tokio::test]
async fn relaunching_the_same_fence_reattaches() {
    let exec = LocalExecutor::new();
    let s = step("r1", "once");
    let h1 = exec
        .launch(&s, &spec(&["sh", "-c", "exit 0"]))
        .await
        .unwrap();
    // A second launch of the same fence returns the same handle and does not
    // start a second process (idempotent re-attach).
    let h2 = exec
        .launch(&s, &spec(&["sh", "-c", "exit 0"]))
        .await
        .unwrap();
    assert_eq!(h1, h2);
    assert_eq!(drive(&exec, &h1).await, ExecState::Succeeded);
}

#[tokio::test]
async fn a_command_is_required() {
    let exec = LocalExecutor::new();
    let s = step("r1", "nocmd");
    assert!(
        exec.launch(&s, &spec(&[])).await.is_err(),
        "no command → launch error"
    );
}

#[tokio::test]
async fn unknown_handle_is_lost() {
    let exec = LocalExecutor::new();
    let bogus = scarab_engine::ports::ExecHandle("local://nope/nope/0".into());
    assert_eq!(exec.poll(&bogus).await.unwrap(), ExecState::Lost);
}

/// ADR-0047: the local kill-timer enforces the step deadline — a step that
/// outlives its `timeout:` is killed and classified `Timeout`.
#[tokio::test]
async fn kill_timer_times_out_a_hung_step() {
    let exec = LocalExecutor::new();
    let s = step("r1", "hung");
    let mut sp = spec(&["sh", "-c", "sleep 30"]);
    sp.timeout_seconds = Some(1);
    let handle = exec.launch(&s, &sp).await.unwrap();
    assert_eq!(exec.poll(&handle).await.unwrap(), ExecState::Running);
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    assert_eq!(
        exec.poll(&handle).await.unwrap(),
        ExecState::Failed {
            exit_code: None,
            class: FailureClass::Timeout,
            cause: None,
        }
    );
}

#[tokio::test]
async fn cancel_kills_a_running_child() {
    let exec = LocalExecutor::new();
    let s = step("r1", "sleeper");
    let handle = exec
        .launch(&s, &spec(&["sh", "-c", "sleep 30"]))
        .await
        .unwrap();
    // It's running...
    assert_eq!(exec.poll(&handle).await.unwrap(), ExecState::Running);
    exec.cancel(&handle).await.unwrap();
    // ...and after cancel it is terminal (not Running).
    assert!(matches!(
        exec.poll(&handle).await.unwrap(),
        ExecState::Failed { .. }
    ));
}

/// ADR-0041: a step writes `$SCARAB_RESULTS/<name>.json`, and the executor reads
/// it back as a typed named result after the step completes.
#[tokio::test]
async fn named_results_are_read_back_from_the_results_dir() {
    let exec = LocalExecutor::new();
    let s = step("r1", "emit");
    // The step writes a string result `url` and a numeric result `replicas`.
    let handle = exec
        .launch(
            &s,
            &spec(&[
                "sh",
                "-c",
                "printf '\"https://svc\"' > \"$SCARAB_RESULTS/url.json\"; printf '3' > \"$SCARAB_RESULTS/replicas.json\"",
            ]),
        )
        .await
        .unwrap();
    assert_eq!(drive(&exec, &handle).await, ExecState::Succeeded);

    let results = exec.results(&handle).await.unwrap();
    assert_eq!(
        results.get("url").unwrap(),
        &serde_json::json!("https://svc")
    );
    assert_eq!(
        results.get("replicas").unwrap(),
        &serde_json::json!(3),
        "int type preserved"
    );
}

/// A step that emits nothing yields an empty result map (not an error).
#[tokio::test]
async fn no_results_emitted_is_an_empty_map() {
    let exec = LocalExecutor::new();
    let s = step("r1", "silent");
    let handle = exec
        .launch(&s, &spec(&["sh", "-c", "exit 0"]))
        .await
        .unwrap();
    assert_eq!(drive(&exec, &handle).await, ExecState::Succeeded);
    assert!(exec.results(&handle).await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// The Pod-shaped contracts the host-process backend cannot honor (ADR-0036).
//
// Each of these is a step kind whose contract is a *container* — a canonical
// image, rootless BuildKit, a co-located sidecar. The local backend runs a bare
// child process, so it must FAIL with direction rather than run the step
// without the thing that makes it that kind of step. A checkout without its credential
// or a service-less step that exits 0 is the failure mode being pinned here:
// the step would look green while having done something else entirely.
// ---------------------------------------------------------------------------

/// ADR-0076 §9: a forge credential is mounted as files into the step's own
/// container. Locally that is unhonorable — reject, never run a checkout that
/// quietly has no credential, or leaves one where any process can read it.
#[tokio::test]
async fn a_forge_token_step_is_rejected() {
    let exec = LocalExecutor::new();
    let s = step("r1", "checkout/git");
    let mut sp = spec(&["sh", "-c", "exit 0"]);
    sp.forge_token = Some(scarab_engine::ForgeTokenConfig {
        access: scarab_pipeline::ForgeToken::Read,
        owner: "acme".into(),
        name: "web".into(),
        read_only: false,
        trusted_library: None,
        credential: None,
    });

    let err = exec
        .launch(&s, &sp)
        .await
        .expect_err("a forge_token step must not launch on the local backend");
    let msg = err.to_string();
    assert!(
        msg.contains("k8s executor") && msg.contains("forge_token"),
        "the rejection has to point at the backend that CAN do it: {msg}"
    );
}

/// ADR-0018: a build step's contract is rootless BuildKit in a Pod.
#[tokio::test]
async fn a_build_step_is_rejected() {
    let exec = LocalExecutor::new();
    let s = step("r1", "image");
    let mut sp = spec(&["sh", "-c", "exit 0"]);
    sp.build = Some(Default::default());

    let err = exec
        .launch(&s, &sp)
        .await
        .expect_err("a build step must not launch on the local backend");
    let msg = err.to_string();
    assert!(
        msg.contains("k8s executor") && msg.contains("ADR-0018"),
        "the rejection has to point at the backend that CAN do it: {msg}"
    );
}

/// ADR-0058: a sidecar service is a container co-located in the step's Pod. A
/// step whose command dials `localhost:5432` would exit non-zero here for a
/// confusing reason; rejecting at launch says why.
#[tokio::test]
async fn a_step_with_sidecar_services_is_rejected() {
    let exec = LocalExecutor::new();
    let s = step("r1", "needs-pg");
    let mut sp = spec(&["sh", "-c", "exit 0"]);
    sp.services = vec![scarab_pipeline::ServiceSpec {
        image: "postgres:16".into(),
        ports: vec![5432],
        ..Default::default()
    }];

    let err = exec
        .launch(&s, &sp)
        .await
        .expect_err("a step with services must not launch on the local backend");
    let msg = err.to_string();
    assert!(
        msg.contains("k8s executor") && msg.contains("ADR-0058"),
        "the rejection has to point at the backend that CAN do it: {msg}"
    );
}

/// ADR-0047: a step that authors no `timeout:` inherits the executor's
/// configured global default — the knob, not just the compiled-in hour.
#[tokio::test]
async fn a_step_without_a_timeout_inherits_the_configured_default() {
    let exec = LocalExecutor::default().with_default_step_timeout_secs(1);
    let s = step("r1", "no-timeout-authored");
    let sp = spec(&["sh", "-c", "sleep 30"]);
    assert!(sp.timeout_seconds.is_none(), "the step authors no timeout");

    let handle = exec.launch(&s, &sp).await.unwrap();
    assert_eq!(exec.poll(&handle).await.unwrap(), ExecState::Running);
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    assert_eq!(
        exec.poll(&handle).await.unwrap(),
        ExecState::Failed {
            exit_code: None,
            class: FailureClass::Timeout,
            cause: None,
        },
        "the configured default deadline is enforced, not only an authored one"
    );
}

// ---------------------------------------------------------------------------
// The evidence ports (ADR-0068 / 2e1a458). Both are REQUIRED trait methods
// with no default precisely because a defaulted evidence method gets answered
// by a decorator instead of the executor it wraps — that has silently dropped
// evidence twice (98ea804, 56220d7). The local backend's answer is a
// deliberate `None`; these pin that it is None *by decision*, so that a future
// local infra plane (a container backend, say) cannot be added without either
// reporting or knowingly re-affirming the silence here.
// ---------------------------------------------------------------------------

/// A child process has no scheduler to reject it, no image to pull and no node
/// to be too small — there is no infra plane to narrate.
#[tokio::test]
async fn the_local_backend_reports_no_infra_condition() {
    let exec = LocalExecutor::new();
    let s = step("r1", "quiet");
    let handle = exec
        .launch(&s, &spec(&["sh", "-c", "sleep 30"]))
        .await
        .unwrap();

    assert_eq!(
        exec.infra_condition(&handle).await.unwrap(),
        None,
        "the local backend has no infra plane to report on"
    );
    // ...and the same for a fence it never launched: absence of a unit is not
    // an error, it is still no condition.
    let bogus = scarab_engine::ports::ExecHandle("local://nope/nope/0".into());
    assert_eq!(exec.infra_condition(&bogus).await.unwrap(), None);
}

/// No workspace CAS leg locally (parity explicitly deferred, ADR-0036), so
/// there is no fan-in sensor and nothing to report about provisioning.
#[tokio::test]
async fn the_local_backend_reports_no_workspace_provisioning() {
    let exec = LocalExecutor::new();
    let s = step("r1", "no-cas");
    let handle = exec
        .launch(&s, &spec(&["sh", "-c", "sleep 30"]))
        .await
        .unwrap();

    assert_eq!(
        exec.workspace_provisioning(&handle).await.unwrap(),
        None,
        "the local backend has no workspace CAS leg to report a fan-in over"
    );
    let bogus = scarab_engine::ports::ExecHandle("local://nope/nope/0".into());
    assert_eq!(exec.workspace_provisioning(&bogus).await.unwrap(), None);
}

/// Results are handed over in-process (`results` reads the step's own dir):
/// there is no delivery leg that could lose them apart from the step, so an
/// exit-0 step that wrote a result reports no delivery failure.
#[tokio::test]
async fn the_local_backend_reports_no_results_delivery_failure() {
    let exec = LocalExecutor::new();
    let s = step("r1", "delivered");
    let handle = exec
        .launch(
            &s,
            &spec(&["sh", "-c", "echo 1 > \"$SCARAB_RESULTS/n.json\""]),
        )
        .await
        .unwrap();
    assert_eq!(drive(&exec, &handle).await, ExecState::Succeeded);

    assert_eq!(
        exec.results_delivery_failure(&handle).await.unwrap(),
        None,
        "the local backend has no delivery leg to fail"
    );
    let bogus = scarab_engine::ports::ExecHandle("local://nope/nope/0".into());
    assert_eq!(exec.results_delivery_failure(&bogus).await.unwrap(), None);
}

/// git-bug 333d389: when a launch's handle was never recorded, the engine
/// tears down `unit_handle(step)` instead — so it must name the unit `launch`
/// created, or the cancel misses it and the child runs on.
#[tokio::test]
async fn unit_handle_names_the_unit_launch_created() {
    let exec = LocalExecutor::new();
    let s = step("r1", "orphan");
    let launched = exec
        .launch(&s, &spec(&["sh", "-c", "sleep 30"]))
        .await
        .unwrap();

    let unit = exec.unit_handle(&s).expect("the local unit is derivable");
    assert_eq!(unit, launched);
    exec.cancel(&unit).await.unwrap();
    assert!(matches!(
        exec.poll(&launched).await.unwrap(),
        ExecState::Failed { .. }
    ));
}

/// git-bug f22056d: `cancel` SIGKILLs with no termination grace, so a
/// cancelled unit is torn down as soon as `cancel` returns; a fence this
/// executor never launched has nothing running either.
#[tokio::test]
async fn a_cancelled_unit_is_torn_down() {
    let exec = LocalExecutor::new();
    let s = step("r1", "teardown");
    let handle = exec
        .launch(&s, &spec(&["sh", "-c", "sleep 30"]))
        .await
        .unwrap();
    exec.cancel(&handle).await.unwrap();

    assert!(exec.torn_down(&handle).await.unwrap());
    let bogus = scarab_engine::ports::ExecHandle("local://nope/nope/0".into());
    assert!(exec.torn_down(&bogus).await.unwrap());
}
