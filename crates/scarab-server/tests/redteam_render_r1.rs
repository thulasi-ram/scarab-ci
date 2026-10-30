//! Red-team round 1 on "lexical scopes, render once" (ADR-0076), end to end
//! through `dispatch_run` and the real scheduler over in-memory fakes. Each
//! test asserts the SECURE behaviour: a failing test is a break.

use std::collections::BTreeMap;

use serde_json::json;

use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{Db, RunId, Scheduler, StepId};
use scarab_forge::RepoRef;
use scarab_server::{dispatch_run, DispatchKind};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryDb};

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

/// Whoever dispatches a Run supplies its parameters; a `--upload-pack=`
/// value rendered into `git fetch` runs a command in the Step, which holds
/// the Step's secrets. A free-text parameter never reaches a command
/// (ADR-0076, 2026-10-30): the dispatch is refused when the Pipeline
/// compiles, so no Run exists to launch.
#[tokio::test]
async fn a_dispatch_parameter_with_a_leading_dash_does_not_reach_argv() {
    let lib = "kind: library\ninterface: { inputs: [{ name: r, type: string }] }\nsteps: [{ id: s, image: alpine/git, command: [git, fetch, origin, \"${{ inputs.r }}\"] }]\n";
    let pipeline = "on: { manual: {} }\ninterface: { inputs: [{ name: ref, type: string }] }\nsteps:\n  - { id: p, image: alpine/git, command: [git, fetch, origin, \"${{ inputs.ref }}\"] }\n  - { id: c, invoke: file:.scarab/lib/fetch.yaml, with: { r: \"${{ inputs.ref }}\" } }\n";
    let forge = FakeForge::new()
        .with_file(".scarab/ship.yaml", pipeline)
        .with_file(".scarab/lib/fetch.yaml", lib)
        .with_commit("refs/heads/main", "sha-1");
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);
    let evil = "--upload-pack=touch /tmp/pwned";
    let params: BTreeMap<String, serde_json::Value> =
        serde_json::from_value(json!({ "ref": evil })).unwrap();
    let dispatched = dispatch_run(
        &forge,
        &db,
        &clock,
        scarab_server::AdmissionPorts::governance(None),
        "mallory".into(),
        repo(),
        "refs/heads/main".into(),
        "ship".into(),
        params,
        DispatchKind::Manual,
        None,
    )
    .await;
    let run: RunId = match dispatched {
        Err(e) => {
            let e = e.to_string();
            assert!(
                e.contains("step `p`: `command[3]` reads `${{ inputs.ref }}`")
                    && e.contains("library `.scarab/lib/fetch.yaml`, step `s`: `command[3]`"),
                "{e}"
            );
            return;
        }
        Ok(run) => run,
    };
    let exec = FakeExecutor::new();
    for _ in 0..8 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    for _ in 0..12 {
        sched.tick(&run).await.expect("tick");
    }
    let mut reached = Vec::new();
    for step in ["p", "c/s"] {
        let launched =
            exec.launched_spec(&ExecHandle(format!("fake://{}/{step}/a1", run.0)));
        if let Some(spec) = launched {
            if spec.command.iter().any(|a| a == evil) {
                reached.push((step, spec.command));
            }
        }
        let s = db
            .steps_of_run(&run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step == StepId(step.into()))
            .unwrap();
        eprintln!("{step}: {:?}", s.status);
    }
    assert!(reached.is_empty(), "outside `-` value launched in argv: {reached:?}");
}

/// A commit SHA is a fixed shape (ADR-0076, 2026-10-30): `${{ event.sha }}`
/// reaches a command, end to end through dispatch and the scheduler. A
/// stored value that is not a lower-case hex SHA (a store written around
/// Run creation's check) fails the Step at launch, before anything runs.
#[tokio::test]
async fn event_sha_reaches_a_command_and_a_stored_non_sha_fails_at_launch() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let pipeline = "on: { manual: {} }\nsteps:\n  - { id: co, image: alpine/git, command: [git, checkout, \"${{ event.sha }}\"] }\n";
    let forge = FakeForge::new()
        .with_file(".scarab/ship.yaml", pipeline)
        .with_commit("refs/heads/main", sha);
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);
    let dispatch = || {
        dispatch_run(
            &forge,
            &db,
            &clock,
            scarab_server::AdmissionPorts::governance(None),
            "alice".into(),
            repo(),
            "refs/heads/main".into(),
            "ship".into(),
            BTreeMap::new(),
            DispatchKind::Manual,
            None,
        )
    };
    let good = dispatch().await.expect("dispatch");
    let bad = dispatch().await.expect("dispatch");
    for value in [
        json!("0123456789ABCDEF0123456789ABCDEF01234567"),
        json!("--upload-pack=touch /tmp/pwned"),
        json!(7),
    ] {
        let facts = json!({ "sha": value });
        db.set_run_event_facts(&bad, facts.as_object().unwrap())
            .await
            .unwrap();
        let step = StepId("co".into());
        let spec = db.step_spec(&bad, &step).await.unwrap().unwrap();
        let reason = scarab_engine::render::render_launch_spec(&db, &bad, &step, spec)
            .await
            .unwrap()
            .expect_err("a stored non-SHA is refused");
        assert!(
            reason.starts_with("`command[2]`") && reason.contains("not a commit SHA"),
            "{value}: {reason}"
        );
    }
    let exec = FakeExecutor::new();
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    for run in [&good, &bad] {
        for _ in 0..6 {
            sched.tick(run).await.expect("tick");
        }
    }
    let launched = exec
        .launched_spec(&ExecHandle(format!("fake://{}/co/a1", good.0)))
        .expect("the good Run launched");
    assert_eq!(launched.command, ["git", "checkout", sha]);
    assert!(
        exec.launched_spec(&ExecHandle(format!("fake://{}/co/a1", bad.0)))
            .is_none(),
        "a stored non-SHA launched"
    );
    let co = db
        .steps_of_run(&bad)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == StepId("co".into()))
        .unwrap();
    assert_eq!(co.status, scarab_engine::StepStatus::Failed);
}
