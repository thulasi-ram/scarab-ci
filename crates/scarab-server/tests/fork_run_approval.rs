//! Fork Run approval (ADR-0086 §9) at Run creation and at its gate: which
//! Runs of fork code get the gate, what a store outage does, the shared
//! service an excluded Step uses, and the gate's release and Rerun through
//! the real router.
//!
//! Hermetic: a `FakeForge` serves the Pipeline and answers the fork check, a
//! `FakeExecutor` runs the Steps, InMemoryDb is the run store, and a
//! software passkey confirms (the authenticator is the true external).

mod step_up_harness;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::Value;

use scarab_engine::ports::ExecState;
use scarab_engine::{
    Clock, Db, DbError, DelegationTrail, EventPayload, RunId, RunStatus, Scheduler, StepId,
    StepStatus,
};
use scarab_forge::{PullRequestOrigin, RepoRef};
use scarab_identity::{
    ApiTokenStore, ForkRunApproval, ForkRunApprovalStore, IdentityError, PrincipalToken,
    PrincipalTokenKind, Role, Scope,
};
use scarab_project::{EnvironmentGovernanceStore, EnvironmentProjectEligibility};
use scarab_server::{dispatch_run_via, AdmissionPorts, DispatchError, DispatchKind};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, InMemoryDb, InMemoryEnvironmentGovernance,
    InMemoryForkRunApproval,
};
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const HEAD: &str = "1111111111111111111111111111111111111111";
const FORK: &str = "2222222222222222222222222222222222222222";
/// A pull request from another repository: fork code.
const PR_REF: &str = "refs/pull/7/head";
const GATE: &str = "fork-run-approval";
const DAY: i64 = 24 * 60 * 60 * 1000;

/// Two roots, one of them excluded by its `when:`, and a Step behind one.
const CI_YAML: &str = r#"
on:
  manual: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: docs, image: busybox, command: ["true"], when: "false" }
  - { id: test, image: busybox, command: ["true"], needs: [build] }
"#;

/// The attack of the design review: a shared service whose image the fork
/// chose, used only by a Step its own `when:` excludes, beside an ordinary
/// root. Before every root had the gate in front of it, the excluded user
/// had no gate upstream, so the service started before any release.
const SERVICE_YAML: &str = r#"
on:
  manual: {}
services:
  - { name: db, image: postgres }
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: sneaky, image: busybox, command: ["true"], uses: [db], when: "false" }
"#;

/// [`CI_YAML`] in the Environment `prod`, so the Run records whether it is
/// locked out of `prod`'s secrets. No Step uses its authority, so the
/// Environment adds no gate.
const PROD_YAML: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: docs, image: busybox, command: ["true"], when: "false" }
  - { id: test, image: busybox, command: ["true"], needs: [build] }
"#;

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

/// A forge serving `.scarab/<name>.yaml` for each of `files`, with `main` at
/// [`HEAD`] and pull request 7 from another repository at [`FORK`].
fn forge(files: &[(&str, &str)]) -> FakeForge {
    let mut f = FakeForge::new()
        .with_commit(PR_REF, FORK)
        .with_commit("refs/heads/main", HEAD)
        .with_branch("main", HEAD);
    for (name, yaml) in files {
        f = f.with_file(format!(".scarab/{name}.yaml"), *yaml);
    }
    f.set_pr_origin(
        7,
        PullRequestOrigin {
            head_sha: FORK.into(),
            head_branch: "patch-1".into(),
            same_repository: false,
        },
    );
    f
}

/// A trail of a PrincipalToken of `kind`, as Run creation records it.
/// `<kind>@<installation>` is a delegated call through that Plugin
/// installation (PluginApi).
fn via(kind: &str) -> DelegationTrail {
    let (kind, plugin_installation) = match kind.split_once('@') {
        Some((kind, installation)) => (kind, Some(installation.to_string())),
        None => (kind, None),
    };
    DelegationTrail {
        token_id: "t1".into(),
        token_kind: kind.into(),
        oauth_client: None,
        plugin_installation,
    }
}

/// Dispatch `pipeline` at `git_ref` in `acme/web`, reading the Fork Run
/// approval setting from `store`, started through a token of `token_kind`
/// (`None`: a session).
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    forge: &FakeForge,
    db: &dyn Db,
    clock: &dyn Clock,
    governance: Option<&InMemoryEnvironmentGovernance>,
    store: &dyn ForkRunApprovalStore,
    git_ref: &str,
    pipeline: &str,
    token_kind: Option<&str>,
) -> Result<RunId, DispatchError> {
    let mut ports = AdmissionPorts::default();
    if let Some(g) = governance {
        ports.governance = Some(g as &dyn EnvironmentGovernanceStore);
    }
    ports.fork_run_approval = Some(store);
    dispatch_run_via(
        forge,
        db,
        clock,
        ports,
        "alice".into(),
        repo(),
        git_ref.into(),
        pipeline.into(),
        BTreeMap::new(),
        DispatchKind::Manual,
        None,
        token_kind.map(via),
    )
    .await
}

/// The Steps of a stored IR that are Fork Run approval gates.
fn fork_gates(ir: &Value) -> Vec<Value> {
    ir["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| {
            s["origin"]
                .as_array()
                .is_some_and(|o| o.iter().any(|l| l["kind"] == "fork_run_approval_gate"))
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Which Runs get the gate
// ---------------------------------------------------------------------------

/// The setting, the code and the token decide whether the Run gets the gate.
/// A gated Run stores one gate in the documented shape, and every root Step
/// needs it, the `when:`-excluded root included (it starts Skipped); the
/// Step behind a root keeps its own `needs`. An ungated Run has no gate and
/// its roots need nothing. `agent_started` (also the default, with no row)
/// gates only a trail that is not an `issued` token, an unknown kind
/// included, or one that names a Plugin installation (a delegated call). The setting is read whatever letter case it was stored under.
/// Kills: the gate on own code or under `off`; a default that gates
/// everything or nothing; an unknown token kind read as a person; a
/// delegated call through a Plugin read as a person; a root
/// left out because its `when:` is false; a setting stored as `ACME` that
/// Run creation for `acme` does not find.
#[tokio::test]
async fn the_setting_decides_which_runs_of_fork_code_get_the_gate() {
    use ForkRunApproval::{AgentStarted, All, Off};
    #[allow(clippy::type_complexity)]
    let cases: Vec<(
        &str,
        &str,
        Option<(&str, ForkRunApproval)>,
        Option<&str>,
        bool,
    )> = vec![
        ("all", PR_REF, Some(("acme", All)), None, true),
        (
            "all, stored as ACME",
            PR_REF,
            Some(("ACME", All)),
            None,
            true,
        ),
        (
            "off, oauth token",
            PR_REF,
            Some(("acme", Off)),
            Some("oauth"),
            false,
        ),
        ("no setting, a session", PR_REF, None, None, false),
        (
            "no setting, issued token",
            PR_REF,
            None,
            Some("issued"),
            false,
        ),
        ("no setting, oauth token", PR_REF, None, Some("oauth"), true),
        (
            "no setting, issued token through a plugin endpoint",
            PR_REF,
            None,
            Some("issued@inst-1"),
            true,
        ),
        (
            "no setting, unknown token kind",
            PR_REF,
            None,
            Some("device"),
            true,
        ),
        (
            "agent_started stored, oauth token",
            PR_REF,
            Some(("acme", AgentStarted)),
            Some("oauth"),
            true,
        ),
        (
            "own branch, all, oauth token",
            "refs/heads/main",
            Some(("acme", All)),
            Some("oauth"),
            false,
        ),
    ];
    for (case, git_ref, stored, token_kind, gated) in cases {
        let db = InMemoryDb::new();
        let store = InMemoryForkRunApproval::new();
        if let Some((org, setting)) = stored {
            assert!(store
                .put_fork_run_approval(org, ForkRunApproval::default(), setting, "seed", 0)
                .await
                .unwrap());
        }
        let run = dispatch(
            &forge(&[("ci", CI_YAML)]),
            &db,
            &FakeClock::new(1_000),
            None,
            &store,
            git_ref,
            "ci",
            token_kind,
        )
        .await
        .unwrap_or_else(|e| panic!("{case}: {e}"));

        let ir = db
            .run_ir(&run)
            .await
            .unwrap()
            .expect("the Run stores its IR");
        let gates = fork_gates(&ir);
        let rows = db.steps_of_run(&run).await.unwrap();
        let row = |id: &str| {
            rows.iter()
                .find(|s| s.step.0 == id)
                .unwrap_or_else(|| panic!("{case}: no Step {id}"))
                .clone()
        };
        if gated {
            assert_eq!(gates.len(), 1, "{case}: {ir}");
            let g = &gates[0];
            assert_eq!(g["id"], GATE, "{case}");
            assert_eq!(g["gate"], "manual", "{case}");
            assert_eq!(g["gate_expires_after"], 259_200, "{case}");
            assert_eq!(g["environment_approval_gate"], false, "{case}");
            assert_eq!(g["origin"][0]["name"], "fork_run_approval", "{case}");
            for root in ["build", "docs"] {
                assert_eq!(row(root).needs, vec![StepId(GATE.into())], "{case}: {root}");
            }
            assert_eq!(row("test").needs, vec![StepId("build".into())], "{case}");
            assert_eq!(row(GATE).gate_kind.as_deref(), Some("manual"), "{case}");
            assert_eq!(row(GATE).status, StepStatus::Pending, "{case}");
            assert_eq!(row("docs").status, StepStatus::Skipped, "{case}");
        } else {
            assert!(gates.is_empty(), "{case}: {ir}");
            assert!(row("build").needs.is_empty(), "{case}");
            assert!(row("docs").needs.is_empty(), "{case}");
            assert!(rows.iter().all(|s| !s.is_gate()), "{case}");
        }
    }
}

/// A store that cannot answer.
struct Unreadable;

#[async_trait::async_trait]
impl ForkRunApprovalStore for Unreadable {
    async fn fork_run_approval(
        &self,
        _org: &str,
    ) -> Result<Option<ForkRunApproval>, IdentityError> {
        Err(IdentityError::Issuance("database unreachable".into()))
    }

    async fn put_fork_run_approval(
        &self,
        _org: &str,
        _expected: ForkRunApproval,
        _new: ForkRunApproval,
        _updated_by: &str,
        _updated_at: i64,
    ) -> Result<bool, IdentityError> {
        Err(IdentityError::Issuance("database unreachable".into()))
    }
}

/// A Run of fork code whose org setting cannot be read is not created: the
/// gate is never skipped by default. The repository's own code never reads
/// the setting, so the outage does not stop it.
/// Kills: an unreadable setting read as the default (or as `off`).
#[tokio::test]
async fn a_store_that_cannot_answer_refuses_a_run_of_fork_code() {
    let forge = forge(&[("ci", CI_YAML)]);
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);
    let err = dispatch(&forge, &db, &clock, None, &Unreadable, PR_REF, "ci", None)
        .await
        .expect_err("refused");
    match &err {
        DispatchError::Db(DbError::Other(m)) => {
            assert!(m.contains("Fork Run approval"), "{m}")
        }
        other => panic!("expected the store's refusal, got {other}"),
    }
    assert!(db.list_runs(100).await.unwrap().is_empty(), "no Run");

    dispatch(
        &forge,
        &db,
        &clock,
        None,
        &Unreadable,
        "refs/heads/main",
        "ci",
        None,
    )
    .await
    .expect("own code does not read the setting");
}

/// The shared service of a fork Run whose only user is a `when:`-excluded
/// Step is not started before the release: the excluded root has the gate
/// upstream too. The contrast row (`off`, no gate) shows the same reconcile
/// does start it, so the gated row cannot pass by launching nothing at all.
/// Kills: the gate fronting only included roots (the service image the
/// fork chose runs before anyone released the Run).
#[tokio::test]
async fn an_excluded_steps_shared_service_waits_for_the_release() {
    for (setting, starts) in [(ForkRunApproval::All, false), (ForkRunApproval::Off, true)] {
        let db = InMemoryDb::new();
        let store = InMemoryForkRunApproval::new();
        store
            .put_fork_run_approval("acme", ForkRunApproval::default(), setting, "seed", 0)
            .await
            .unwrap();
        let clock = FakeClock::new(1_000);
        let run = dispatch(
            &forge(&[("svc", SERVICE_YAML)]),
            &db,
            &clock,
            None,
            &store,
            PR_REF,
            "svc",
            None,
        )
        .await
        .expect("admitted");

        let exec = FakeExecutor::new();
        let sched = Scheduler::new(&db, &clock, &exec, "drv");
        sched.reconcile_services(&run).await.unwrap();
        if starts {
            assert_eq!(
                exec.launched_services(),
                vec![FakeExecutor::service_handle(&run.0, 1, "db").0],
                "{setting:?}"
            );
        } else {
            assert!(exec.launched_services().is_empty(), "{setting:?}");
            assert!(
                db.run_services(&run).await.unwrap().is_empty(),
                "{setting:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Release and Rerun through the router
// ---------------------------------------------------------------------------

/// amy owns `acme` and has a passkey; `prod` admits every Project; the org
/// setting is `all`.
async fn world() -> (
    Harness,
    Arc<InMemoryEnvironmentGovernance>,
    Arc<InMemoryForkRunApproval>,
    Sess,
    SoftPasskey,
) {
    let governance = Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
        "acme",
        "prod",
        EnvironmentProjectEligibility::AllProjects,
    ));
    let store = Arc::new(InMemoryForkRunApproval::new());
    store
        .put_fork_run_approval(
            "acme",
            ForkRunApproval::default(),
            ForkRunApproval::All,
            "seed",
            0,
        )
        .await
        .unwrap();
    let (wired, wired_store) = (governance.clone(), store.clone());
    let h = harness_with(move |st| {
        st.with_environment_governance(wired)
            .with_fork_run_approval(wired_store)
    });
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (h, governance, store, s, soft)
}

/// Tick `run` until it is `until`.
async fn drive(h: &Harness, exec: &FakeExecutor, run: &RunId, until: RunStatus) {
    let sched = Scheduler::new(h.db.as_ref(), h.clock.as_ref() as &dyn Clock, exec, "sched");
    for _ in 0..20 {
        sched.tick(run).await.unwrap();
        if h.db.run_status(run).await.unwrap() == Some(until) {
            return;
        }
    }
    panic!(
        "{} never reached {until:?}: {:?}",
        run.0,
        h.db.run_status(run).await.unwrap()
    );
}

/// Dispatch [`PROD_YAML`] at the fork's ref and drive it to the gate.
async fn at_fork_gate(
    h: &Harness,
    governance: &InMemoryEnvironmentGovernance,
    store: &InMemoryForkRunApproval,
    exec: &FakeExecutor,
) -> RunId {
    let run = dispatch(
        &forge(&[("prod", PROD_YAML)]),
        h.db.as_ref(),
        h.clock.as_ref(),
        Some(governance),
        store,
        PR_REF,
        "prod",
        None,
    )
    .await
    .expect("admitted");
    for _ in 0..6 {
        exec.script_outcome(ExecState::Succeeded);
    }
    drive(h, exec, &run, RunStatus::Suspended).await;
    assert_eq!(step_status(h, &run, "build").await, StepStatus::Pending);
    run
}

async fn step_status(h: &Harness, run: &RunId, id: &str) -> StepStatus {
    h.db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == id)
        .unwrap_or_else(|| panic!("no Step {id}"))
        .status
}

/// The `(by, via)` of every `GateApproved` of the Fork Run approval gate.
async fn approvals(h: &Harness, run: &RunId) -> Vec<(String, Option<DelegationTrail>)> {
    h.db.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::GateApproved { step, by, via } if step.0 == GATE => Some((by, via)),
            _ => None,
        })
        .collect()
}

/// A token of amy's at `acme` of `kind`; returns its plaintext.
async fn token(h: &Harness, id: &str, kind: PrincipalTokenKind, audience: Option<&str>) -> String {
    let plaintext = format!("scarab_pat_{id}_0123456789abcdef0123456789abcdef");
    let now = h.now().await;
    let record = PrincipalToken {
        id: id.into(),
        name: format!("ci-{id}"),
        owner_subject: "amy".into(),
        scope: Scope::Org("acme".into()),
        role: Role::Admin,
        expires_at: now + 30 * DAY,
        created_by: "amy".into(),
        created_at: now,
        last_used_at: None,
        use_count: 0,
        revoked_at: None,
        kind,
        operations: None,
        audience: audience.map(str::to_string),
        oauth_client: None,
    };
    h.tokens
        .put(&record, &record.stored_hash(&plaintext))
        .await
        .unwrap();
    plaintext
}

/// amy confirms StepUpConfirmationRequest `id` in her browser.
async fn confirm(h: &Harness, s: &Sess, soft: &mut SoftPasskey, id: &str) -> (StatusCode, Value) {
    let (st, ch) = h
        .cookie(
            "POST",
            &format!("/v1/step-up/requests/{id}/options"),
            None,
            s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{ch}");
    let cred = soft.assert(&ch["public_key"], ORIGIN).unwrap();
    h.cookie(
        "POST",
        &format!("/v1/step-up/requests/{id}/confirm"),
        Some(serde_json::json!({ "challenge_id": ch["id"], "credential": cred })),
        s,
    )
    .await
}

/// The gate Run creation added is released like a plain `manual` gate: at
/// once from a browser session; through an issued token only once its owner
/// confirms (the replay records the token as `via`); never through an
/// `oauth` token. After the release the Run runs to the end and is still
/// fork code: locked out of `prod`'s secrets. In the released Run a Rerun
/// of the `when:`-excluded Step is refused (409), and a Rerun of `test`
/// puts the gate and `build` (between the gate and `test`) back to waiting.
/// Kills: a gate the session cannot release (marked an Environment gate);
/// a token that releases it alone; a release that unlocks the Environment;
/// a Rerun that runs fork code on the previous Take's release; a Rerun that
/// starts a Step its `when:` excluded.
#[tokio::test]
async fn the_gate_releases_like_a_plain_gate_and_a_rerun_asks_again() {
    let (h, governance, store, amy, mut soft) = world().await;
    let approve = |run: &RunId| format!("/v1/runs/{}/gates/{GATE}/approve", run.0);

    // An oauth token is refused.
    let exec = FakeExecutor::new();
    let run = at_fork_gate(&h, &governance, &store, &exec).await;
    let bearer = token(
        &h,
        "app",
        PrincipalTokenKind::Oauth,
        Some("https://ci.example.com/mcp"),
    )
    .await;
    let (st, b) = h.bearer("POST", &approve(&run), None, &bearer).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{b}");
    assert_eq!(step_status(&h, &run, GATE).await, StepStatus::Pending);
    assert!(approvals(&h, &run).await.is_empty());

    // An issued token waits for amy's confirmation.
    let exec = FakeExecutor::new();
    let run = at_fork_gate(&h, &governance, &store, &exec).await;
    let bearer = token(&h, "approver", PrincipalTokenKind::Issued, None).await;
    let (st, b) = h.bearer("POST", &approve(&run), None, &bearer).await;
    assert_step_up(st, &b, "window");
    assert!(approvals(&h, &run).await.is_empty());
    assert_eq!(step_status(&h, &run, GATE).await, StepStatus::Pending);
    let id = b["request_id"].as_str().expect("a request id").to_string();
    let (st, done) = confirm(&h, &amy, &mut soft, &id).await;
    assert_eq!(st, StatusCode::OK, "{done}");
    assert_eq!(done["result_status"], 202, "{done}");
    assert_eq!(step_status(&h, &run, GATE).await, StepStatus::Succeeded);
    assert_eq!(
        approvals(&h, &run).await,
        vec![("amy".to_string(), Some(via_issued("approver")))]
    );

    // A browser session releases it at once.
    let exec = FakeExecutor::new();
    let run = at_fork_gate(&h, &governance, &store, &exec).await;
    let (st, b) = h.cookie("POST", &approve(&run), None, &amy).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["released"], true, "{b}");
    assert_eq!(approvals(&h, &run).await, vec![("amy".to_string(), None)]);

    drive(&h, &exec, &run, RunStatus::Succeeded).await;
    assert_eq!(step_status(&h, &run, "test").await, StepStatus::Succeeded);
    assert_eq!(step_status(&h, &run, "docs").await, StepStatus::Skipped);
    assert!(
        h.db.run_deploy_context(&run)
            .await
            .unwrap()
            .expect("prod governs the Run")
            .locked_out,
        "a released Run of fork code is still locked out"
    );

    let rerun = |step: &str| format!("/v1/runs/{}/steps/{step}/rerun", run.0);
    let (st, b) = h.cookie("POST", &rerun("docs"), None, &amy).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(step_status(&h, &run, "docs").await, StepStatus::Skipped);

    let (st, plan) = h.cookie("POST", &rerun("test"), None, &amy).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{plan}");
    let reason = |id: &str| {
        plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["step"] == id)
            .unwrap_or_else(|| panic!("{id} is not in the plan: {plan}"))["reason"]
            .clone()
    };
    assert_eq!(reason(GATE), "fork_run_approval");
    assert_eq!(reason("build"), "fork_run_approval");
    assert_eq!(reason("test"), "target");
    assert_eq!(step_status(&h, &run, GATE).await, StepStatus::Pending);
    assert_eq!(step_status(&h, &run, "build").await, StepStatus::Pending);
    assert_eq!(step_status(&h, &run, "docs").await, StepStatus::Skipped);
}

fn via_issued(id: &str) -> DelegationTrail {
    DelegationTrail {
        token_id: id.into(),
        token_kind: "issued".into(),
        oauth_client: None,
        plugin_installation: None,
    }
}
