//! ADR-0086 §5-§6: a PrincipalToken limited to a list of ApiOperations, the
//! kinds of token the core routes accept, a token's gate approval waiting for
//! its owner, and the DelegationTrail (`via`) a token's write records.
//!
//! Hermetic: the in-memory stores, a `FakeForge` and a `FakeExecutor` (the
//! external ports), the real router, guard and engine. Step-up is enforced,
//! and amy confirms with a software passkey (the authenticator is the one
//! true external of a confirmation).

mod step_up_harness;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_engine::ports::ExecState;
use scarab_engine::{
    Clock, Db, DelegationTrail, EventPayload, RunId, RunStatus, Scheduler, StepId, StepStatus,
};
use scarab_forge::RepoRef;
use scarab_identity::{
    api_token_hash, ApiTokenStore, PrincipalToken, PrincipalTokenKind, Role, Scope,
};
use scarab_project::{
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProtectionRules,
};
use scarab_server::step_up::{ops, StepUpMachineUse};
use scarab_server::{dispatch_run, AdmissionPorts, DispatchKind};
use scarab_testkit::{FakeExecutor, FakeForge, InMemoryEnvironmentGovernance};
use scarab_webauthn::soft::SoftPasskey;

use step_up_harness::*;

const DAY: i64 = 24 * 60 * 60 * 1000;

const PLAIN_YAML: &str = r#"
on:
  manual: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], needs: [approve] }
"#;

/// A deploy into `prod`: its `approve` gate guards `ship`, which uses prod's
/// authority, so it is an Environment approval gate.
const DEPLOY_YAML: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], deploy: true, needs: [approve] }
"#;

fn forge() -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/plain.yaml", PLAIN_YAML)
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit(
            "refs/heads/main",
            "1234567890abcdef1234567890abcdef12345678",
        )
}

/// amy owns `acme`, has a passkey, and `prod` admits every Project.
async fn world() -> (
    Harness,
    Arc<InMemoryEnvironmentGovernance>,
    Sess,
    SoftPasskey,
) {
    let governance = Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
        "acme",
        "prod",
        EnvironmentProjectEligibility::AllProjects,
    ));
    let wired = governance.clone();
    let h = harness_with(move |st| st.with_environment_governance(wired));
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (h, governance, s, soft)
}

/// Dispatch `pipeline` in `acme/web` and drive it to its `approve` gate.
async fn at_gate(h: &Harness, governance: &InMemoryEnvironmentGovernance, pipeline: &str) -> RunId {
    let run = dispatch_run(
        &forge(),
        h.db.as_ref(),
        h.clock.as_ref(),
        AdmissionPorts::governance(Some(governance)),
        "bob".into(),
        RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        "refs/heads/main".into(),
        pipeline.into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect("admitted");
    let exec = FakeExecutor::new();
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(
        h.db.as_ref(),
        h.clock.as_ref() as &dyn Clock,
        &exec,
        "sched",
    );
    for _ in 0..6 {
        sched.tick(&run).await.unwrap();
        if h.db.run_status(&run).await.unwrap() == Some(RunStatus::Suspended) {
            break;
        }
    }
    assert_eq!(
        h.db.run_status(&run).await.unwrap(),
        Some(RunStatus::Suspended)
    );
    run
}

/// A token of amy's at `acme` (Admin ceiling): `operations` its list (`None`
/// for none), `kind` and `audience` as given. Returns its plaintext.
async fn token(
    h: &Harness,
    id: &str,
    operations: Option<&[&str]>,
    kind: PrincipalTokenKind,
    audience: Option<&str>,
) -> String {
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
        operations: operations.map(|list| list.iter().map(|o| o.to_string()).collect()),
        audience: audience.map(str::to_string),
        oauth_client: None,
    };
    // Stored as the mint stores it: a restricted token under its `r1:` hash.
    h.tokens
        .put(&record, &record.stored_hash(&plaintext))
        .await
        .unwrap();
    plaintext
}

/// An issued token limited to `operations` (`None`: no list).
async fn issued(h: &Harness, id: &str, operations: Option<&[&str]>) -> String {
    token(h, id, operations, PrincipalTokenKind::Issued, None).await
}

/// The status of a request, without reading its body: a log stream of a
/// Run that is still going never ends.
async fn status_only(h: &Harness, method: &str, uri: &str, bearer: &str) -> StatusCode {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    h.app.clone().oneshot(req).await.unwrap().status()
}

#[track_caller]
fn assert_not_granted(st: StatusCode, b: &Value, op: &str) {
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "operation_not_granted", "{b}");
    assert!(b["message"].as_str().unwrap().contains(op), "{b}");
}

/// amy confirms StepUpConfirmationRequest `id` in her browser: the request
/// runs (replays) as its token.
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
        Some(json!({ "challenge_id": ch["id"], "credential": cred })),
        s,
    )
    .await
}

fn trail(id: &str) -> Option<DelegationTrail> {
    Some(DelegationTrail {
        token_id: id.into(),
        token_kind: "issued".into(),
        oauth_client: None,
    })
}

async fn gate_status(h: &Harness, run: &RunId) -> StepStatus {
    h.db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == StepId("approve".into()))
        .unwrap()
        .status
}

/// The `(by, via)` of every `GateApproved` of the `approve` gate.
async fn approvals(h: &Harness, run: &RunId) -> Vec<(String, Option<DelegationTrail>)> {
    h.db.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::GateApproved { step, by, via } if step.0 == "approve" => Some((by, via)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Operation lists
// ---------------------------------------------------------------------------

/// A token with a list may use only the operations it names. Every refusal
/// is a 403 that names the operation, and happens before anything runs: the
/// cancel cancels nothing, and a crown jewel raises no
/// StepUpConfirmationRequest (the contrast row shows a token with no list
/// does raise one). A token with no list and a session are unchanged.
/// Kills: a list stored but never checked; a check after the guard raised a
/// request; a check that does not reach the log stream's handler.
#[tokio::test]
async fn a_listed_token_uses_only_its_operations() {
    let (h, governance, amy, _soft) = world().await;
    let run = at_gate(&h, &governance, "plain").await;
    let run_uri = format!("/v1/runs/{}", run.0);
    let logs_uri = format!("/v1/runs/{}/logs", run.0);
    let cancel_uri = format!("/v1/runs/{}/cancel", run.0);
    let secret = json!({"org": "acme", "name": "DB_PASSWORD", "value": "v1"});

    let reader = issued(&h, "reader", Some(&["run.read"])).await;
    let logs_reader = issued(&h, "logs", Some(&["step.logs.read"])).await;
    let unlimited = issued(&h, "unlimited", None).await;

    // Allowed: the route's operation is in the list.
    let (st, b) = h.bearer("GET", &run_uri, None, &reader).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        status_only(&h, "GET", &logs_uri, &logs_reader).await,
        StatusCode::OK,
        "the log stream's handler sees the route's operation"
    );

    // Refused: a read, a write and a crown jewel outside the list.
    let (st, b) = h.bearer("GET", &logs_uri, None, &reader).await;
    assert_not_granted(st, &b, "step.logs.read");
    let (st, b) = h.bearer("GET", &run_uri, None, &logs_reader).await;
    assert_not_granted(st, &b, "run.read");
    let (st, b) = h.bearer("POST", &cancel_uri, None, &reader).await;
    assert_not_granted(st, &b, "run.cancel");
    assert_eq!(
        h.db.run_status(&run).await.unwrap(),
        Some(RunStatus::Suspended),
        "a refused cancel cancels nothing"
    );
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret.clone()), &reader)
        .await;
    assert_not_granted(st, &b, "secret.put");
    assert!(
        h.mem_step_up().requests().is_empty(),
        "a token refused its operation raises no request"
    );

    // Unchanged: a token with no list, and a session.
    assert_eq!(
        status_only(&h, "GET", &logs_uri, &unlimited).await,
        StatusCode::OK
    );
    let (st, b) = h
        .bearer("POST", "/v1/secrets", Some(secret), &unlimited)
        .await;
    assert_step_up(st, &b, "window");
    assert_eq!(h.mem_step_up().requests().len(), 1);
    let (st, b) = h.cookie("GET", &run_uri, None, &amy).await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

/// The rule `authenticate` applies: no list admits anything; a list admits
/// only what it names, and never a request whose route names no operation.
/// The real router can not send such a request (the guard is a
/// `route_layer`, so every request that reaches a handler matched a route of
/// the table), so this is the only place that branch is held.
/// Kills: a list that fails open when the operation is unknown.
#[test]
fn a_list_never_admits_an_unknown_operation() {
    let with = |operations: Option<Vec<String>>| PrincipalToken {
        id: "t".into(),
        name: "t".into(),
        owner_subject: "amy".into(),
        scope: Scope::Org("acme".into()),
        role: Role::Admin,
        expires_at: 1,
        created_by: "amy".into(),
        created_at: 0,
        last_used_at: None,
        use_count: 0,
        revoked_at: None,
        kind: PrincipalTokenKind::Issued,
        operations,
        audience: None,
        oauth_client: None,
    };
    let listed = with(Some(vec!["run.read".into()]));
    let unlisted = with(None);
    for (token, op, admitted) in [
        (&listed, Some("run.read"), true),
        (&listed, Some("run.cancel"), false),
        (&listed, None, false),
        (&unlisted, Some("run.cancel"), true),
        (&unlisted, None, true),
    ] {
        assert_eq!(
            token.admits_operation(op),
            admitted,
            "{:?} on {op:?}",
            token.operations
        );
    }
}

/// Every operation the web UI's mint dialog offers is one the mint accepts:
/// an operation of the route table that an API token can perform.
/// Kills: a UI choice renamed or never added on the server, which would make
/// every mint that ticks it fail with 400.
#[test]
fn the_mint_dialog_offers_only_mintable_operations() {
    const UI: &str = include_str!("../../../ui/scarab-web-ui/src/api-tokens.ts");
    let offered: Vec<&str> = UI
        .lines()
        .filter_map(|l| l.trim().strip_prefix("{ op: \""))
        .filter_map(|l| l.split('"').next())
        .collect();
    assert!(offered.len() >= 10, "parsed the choices: {offered:?}");
    for id in offered {
        let op = ops::by_id(id).unwrap_or_else(|| panic!("`{id}` is not an operation"));
        assert_eq!(op.machine, StepUpMachineUse::Requestable, "`{id}`");
    }
}

// ---------------------------------------------------------------------------
// Token kinds
// ---------------------------------------------------------------------------

/// Only an issued token with no audience authenticates on the core routes.
/// Kills: an `oauth` token, bound to the one URL it was granted for, used
/// anywhere; an audience on an issued row trusted rather than refused.
#[tokio::test]
async fn only_an_issued_token_with_no_audience_authenticates() {
    let (h, _governance, _amy, _soft) = world().await;
    for (id, kind, audience, want) in [
        ("issued", PrincipalTokenKind::Issued, None, StatusCode::OK),
        (
            "oauth",
            PrincipalTokenKind::Oauth,
            Some("https://ci.example.com/mcp"),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "aud",
            PrincipalTokenKind::Issued,
            Some("https://ci.example.com/mcp"),
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        let bearer = token(&h, id, None, kind, audience).await;
        let (st, b) = h.bearer("GET", "/v1/me", None, &bearer).await;
        assert_eq!(st, want, "{id}: {b}");
    }
}

// ---------------------------------------------------------------------------
// A token never approves a gate by itself
// ---------------------------------------------------------------------------

/// An API token's approval of a plain gate, and of a gate of an Environment
/// with approvers, raises a StepUpConfirmationRequest and records nothing;
/// when amy confirms, the replay approves as amy through the token: the gate
/// releases and `GateApproved` carries the token as `via`. The token's list
/// names only `gate.approve`, so the replay is held to the list too.
/// Kills: a plain gate a token releases with no confirmation; a replay
/// checked against the confirm route's operation instead of its own; an
/// approval that loses its DelegationTrail.
#[tokio::test]
async fn a_token_approval_waits_for_its_owner() {
    let (h, governance, amy, mut soft) = world().await;
    let plain = at_gate(&h, &governance, "plain").await;
    let deploy = at_gate(&h, &governance, "deploy").await;
    governance
        .put_environment_protection(
            "acme",
            "prod",
            &EnvironmentProtectionRules {
                approvers: vec!["amy".into()],
                ..Default::default()
            },
            "seed",
            0,
        )
        .await
        .unwrap();
    let bearer = issued(&h, "approver", Some(&["gate.approve"])).await;

    for run in [plain, deploy] {
        let uri = format!("/v1/runs/{}/gates/approve/approve", run.0);
        let (st, b) = h.bearer("POST", &uri, None, &bearer).await;
        assert_step_up(st, &b, "window");
        assert_eq!(b["op"], "gate.approve", "{b}");
        let id = b["request_id"].as_str().expect("a request id").to_string();
        assert!(approvals(&h, &run).await.is_empty(), "{}: recorded", run.0);
        assert_ne!(gate_status(&h, &run).await, StepStatus::Succeeded);

        let (st, done) = confirm(&h, &amy, &mut soft, &id).await;
        assert_eq!(st, StatusCode::OK, "{}: {done}", run.0);
        assert_eq!(done["state"], "executed", "{}: {done}", run.0);
        assert_eq!(done["result_status"], 202, "{}: {done}", run.0);
        assert_eq!(gate_status(&h, &run).await, StepStatus::Succeeded);
        assert_eq!(
            approvals(&h, &run).await,
            vec![("amy".to_string(), trail("approver"))],
            "{}",
            run.0
        );
    }
}

// ---------------------------------------------------------------------------
// The DelegationTrail
// ---------------------------------------------------------------------------

/// A token's cancel and reject record the token as `via` beside `by`; a
/// session's record none.
/// Kills: a handler that drops the trail on its way to the engine.
#[tokio::test]
async fn a_token_write_records_its_trail() {
    let (h, governance, amy, _soft) = world().await;
    let bearer = issued(&h, "writer", None).await;
    let cancel = |run: &RunId| format!("/v1/runs/{}/cancel", run.0);
    let reject = |run: &RunId| format!("/v1/runs/{}/gates/approve/reject", run.0);
    let reason = json!({ "reason": "not today" });

    let by_token = at_gate(&h, &governance, "plain").await;
    let by_session = at_gate(&h, &governance, "plain").await;
    let (st, b) = h.bearer("POST", &cancel(&by_token), None, &bearer).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    let (st, b) = h.cookie("POST", &cancel(&by_session), None, &amy).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    for (run, want) in [(&by_token, trail("writer")), (&by_session, None)] {
        let vias: Vec<_> =
            h.db.events(run)
                .await
                .unwrap()
                .into_iter()
                .filter_map(|e| match e.kind {
                    EventPayload::RunCancelRequested { via, .. } => Some(via),
                    _ => None,
                })
                .collect();
        assert_eq!(vias, vec![want], "{}", run.0);
    }

    let by_token = at_gate(&h, &governance, "plain").await;
    let by_session = at_gate(&h, &governance, "plain").await;
    let (st, b) = h
        .bearer("POST", &reject(&by_token), Some(reason.clone()), &bearer)
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    let (st, b) = h
        .cookie("POST", &reject(&by_session), Some(reason), &amy)
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    for (run, want) in [(&by_token, trail("writer")), (&by_session, None)] {
        let vias: Vec<_> =
            h.db.events(run)
                .await
                .unwrap()
                .into_iter()
                .filter_map(|e| match e.kind {
                    EventPayload::GateResolved {
                        step,
                        passed: false,
                        via,
                        ..
                    } if step.0 == "approve" => Some(via),
                    _ => None,
                })
                .collect();
        assert_eq!(vias, vec![want], "{}", run.0);
    }
}

// ---------------------------------------------------------------------------
// Restricted tokens under their own hash; the trail on a Run's start
// ---------------------------------------------------------------------------

/// J1 (rolling upgrade): a token with an operation list is stored under its
/// `r1:` hash, so the plain-hash lookup a server built before 0111 makes does
/// not find it; it still authenticates here, and so does a plain token.
/// Kills: a listed token stored under the plain hash (an older replica would
/// let it do everything its role allows), and an in-memory store that matches
/// a row under the hash its restrictions do not call for.
#[tokio::test]
async fn a_listed_token_is_invisible_to_a_plain_hash_lookup_and_still_authenticates() {
    let (h, _governance, _amy, _soft) = world().await;
    let listed = issued(&h, "listed-j1", Some(&["account.read"])).await;
    let plain = issued(&h, "plain-j1", None).await;

    assert_eq!(
        h.tokens
            .by_hash(&api_token_hash(&listed), "")
            .await
            .unwrap(),
        None,
        "a plain-hash lookup must not find a listed token"
    );
    assert!(h
        .tokens
        .by_hash(&api_token_hash(&plain), "")
        .await
        .unwrap()
        .is_some());
    for (id, bearer) in [("listed", &listed), ("plain", &plain)] {
        let (st, b) = h.bearer("GET", "/v1/me", None, bearer).await;
        assert_eq!(st, StatusCode::OK, "{id}: {b}");
    }
}

/// J11: a dispatch made with a token records the token on the Run's start;
/// a session's dispatch records none.
/// Kills: a dispatch handler that drops the caller's trail, and an admission
/// path that never stamps it.
#[tokio::test]
async fn a_token_dispatch_records_its_trail_on_the_run() {
    let h = harness_with(|st| st.with_forge(Arc::new(forge())));
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let bearer = issued(&h, "dispatcher", None).await;
    let body = json!({ "ref": "refs/heads/main", "pipeline": "plain" });

    let (st, b) = h
        .bearer(
            "POST",
            "/v1/repos/acme/web/dispatch",
            Some(body.clone()),
            &bearer,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let by_token = RunId(b["id"].as_str().expect("a run id").to_string());
    assert_eq!(
        h.db.run_origin_via(&by_token).await.unwrap(),
        trail("dispatcher")
    );

    let s = h.browser_session(owner("amy")).await;
    let (st, b) = h
        .cookie("POST", "/v1/repos/acme/web/dispatch", Some(body), &s)
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let by_session = RunId(b["id"].as_str().expect("a run id").to_string());
    assert_eq!(h.db.run_origin_via(&by_session).await.unwrap(), None);
}
