//! LocalExecutor acceptance (ADR-0036): steps run as host child processes.
//! A zero-exit command Succeeds, a non-zero command Fails with its code,
//! relaunching a fence re-attaches, an unknown handle is Lost, and cancel kills
//! a running child. No Docker/k8s.

use scarab_engine::ports::ExecState;
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
    let handle = exec.launch(&s, &spec(&["sh", "-c", "exit 0"])).await.unwrap();
    assert_eq!(drive(&exec, &handle).await, ExecState::Succeeded);
}

#[tokio::test]
async fn nonzero_exit_command_fails_with_its_code() {
    let exec = LocalExecutor::new();
    let s = step("r1", "boom");
    let handle = exec.launch(&s, &spec(&["sh", "-c", "exit 3"])).await.unwrap();
    assert_eq!(drive(&exec, &handle).await, ExecState::Failed { exit_code: Some(3) });
}

#[tokio::test]
async fn relaunching_the_same_fence_reattaches() {
    let exec = LocalExecutor::new();
    let s = step("r1", "once");
    let h1 = exec.launch(&s, &spec(&["sh", "-c", "exit 0"])).await.unwrap();
    // A second launch of the same fence returns the same handle and does not
    // start a second process (idempotent re-attach).
    let h2 = exec.launch(&s, &spec(&["sh", "-c", "exit 0"])).await.unwrap();
    assert_eq!(h1, h2);
    assert_eq!(drive(&exec, &h1).await, ExecState::Succeeded);
}

#[tokio::test]
async fn a_command_is_required() {
    let exec = LocalExecutor::new();
    let s = step("r1", "nocmd");
    assert!(exec.launch(&s, &spec(&[])).await.is_err(), "no command → launch error");
}

#[tokio::test]
async fn unknown_handle_is_lost() {
    let exec = LocalExecutor::new();
    let bogus = scarab_engine::ports::ExecHandle("local://nope/nope/0".into());
    assert_eq!(exec.poll(&bogus).await.unwrap(), ExecState::Lost);
}

#[tokio::test]
async fn cancel_kills_a_running_child() {
    let exec = LocalExecutor::new();
    let s = step("r1", "sleeper");
    let handle = exec.launch(&s, &spec(&["sh", "-c", "sleep 30"])).await.unwrap();
    // It's running...
    assert_eq!(exec.poll(&handle).await.unwrap(), ExecState::Running);
    exec.cancel(&handle).await.unwrap();
    // ...and after cancel it is terminal (not Running).
    assert!(matches!(exec.poll(&handle).await.unwrap(), ExecState::Failed { .. }));
}
