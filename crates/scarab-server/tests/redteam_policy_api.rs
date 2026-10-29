//! Red team (ADR-0075 step 2): the Policy API and `run.create` admission
//! under concurrent writes, partial failure, second deliveries, cross-org
//! ids and role boundaries, and the policy test API against admission. A
//! passing test is a failed attack kept as a regression test; a failing one
//! names a finding (redteam.md).

mod common;
mod step_up_harness;

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_engine::Db;
use scarab_forge::{Event, RepoRef};
use scarab_identity::{Role, Scope};
use scarab_policy::evaluate::{DecisionOutcome, PolicyDecision};
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::store::{
    DecisionCursor, DecisionRecord, NewPolicy, PolicyChange, PolicyScope, PolicyStore,
    PolicyStoreError, PolicyVersion, StoredPolicy,
};
use scarab_server::{trigger_runs_from_event, AdmissionPorts, TriggerError};
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryPolicies};

use step_up_harness::*;

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: test, image: busybox, command: ["true"] }
"#;

fn audit_policy(name: &str) -> Value {
    json!({
        "name": name, "checkpoint": "run.create", "mode": "audit",
        "rules": [{ "when": "steps.exists(s, s.id == \"test\")", "effect": "deny", "message": "m" }]
    })
}

fn enforced_policy(name: &str, id: &str) -> Value {
    json!({
        "name": name, "checkpoint": "run.create", "mode": "enforce",
        "rules": [{ "when": format!("steps.exists(s, s.id == \"{id}\")"), "effect": "deny", "message": "m" }]
    })
}

fn with_policies() -> (Harness, Arc<InMemoryPolicies>) {
    let policies = Arc::new(InMemoryPolicies::new());
    let p = policies.clone();
    (harness_with(move |st| st.with_policies(p)), policies)
}

async fn admin(h: &Harness) -> (Sess, scarab_webauthn::soft::SoftPasskey) {
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
}

async fn create(h: &Harness, s: &Sess, uri: &str, policy: Value) -> Value {
    let (st, b) = h
        .cookie(
            "POST",
            uri,
            Some(json!({ "policy": policy, "reason": "r" })),
            s,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    b
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "m".into(),
    }
}

fn forge_with(ci: &str) -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/ci.yaml", ci)
        .with_commit("refs/heads/main", SHA)
}

/// Sends `req`s on separate tasks of a multi-threaded runtime, so the two
/// handlers can interleave.
async fn race(h: &Harness, reqs: Vec<axum::http::Request<axum::body::Body>>) -> Vec<StatusCode> {
    let tasks: Vec<_> = reqs
        .into_iter()
        .map(|r| {
            let app = h.app.clone();
            tokio::spawn(async move { app.oneshot(r).await.unwrap().status() })
        })
        .collect();
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out.sort();
    out
}

// --- concurrency --------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redteam_api_two_puts_at_one_expected_version_write_once() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    for round in 0..20 {
        let name = format!("race-{round}");
        let created = create(&h, &s, "/v1/orgs/acme/policies", audit_policy(&name)).await;
        let id = created["id"].as_str().unwrap().to_string();
        let uri = format!("/v1/orgs/acme/policies/{id}");
        let body = |when: &str| {
            let mut p = audit_policy(&name);
            p["rules"][0]["when"] = json!(when);
            Some(json!({ "policy": p, "expected_version": 1, "reason": "x" }))
        };
        let got = race(
            &h,
            vec![
                cookie_req("PUT", &uri, body("true"), &s),
                cookie_req("PUT", &uri, body("false"), &s),
            ],
        )
        .await;
        assert_eq!(
            got,
            vec![StatusCode::OK, StatusCode::CONFLICT],
            "round {round}"
        );
        assert_eq!(policies.versions(&id).await.unwrap().len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redteam_api_delete_racing_update_writes_once() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    for round in 0..20 {
        let name = format!("dr-{round}");
        let created = create(&h, &s, "/v1/orgs/acme/policies", audit_policy(&name)).await;
        let id = created["id"].as_str().unwrap().to_string();
        let uri = format!("/v1/orgs/acme/policies/{id}");
        let got = race(
            &h,
            vec![
                cookie_req(
                    "PUT",
                    &uri,
                    Some(json!({ "policy": audit_policy(&name), "expected_version": 1, "reason": "x" })),
                    &s,
                ),
                cookie_req("DELETE", &format!("{uri}?expected_version=1"), None, &s),
            ],
        )
        .await;
        assert_eq!(
            got.iter().filter(|s| **s == StatusCode::OK).count(),
            1,
            "{got:?}"
        );
        assert_eq!(policies.versions(&id).await.unwrap().len(), 2);
    }
}

/// The 256 cap is documented as soft (`create_at`): this records how far
/// two racing creates go past it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redteam_api_creates_racing_at_the_cap() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    for i in 0..255 {
        policies
            .create(&NewPolicy {
                id: format!("seed-{i}"),
                scope: PolicyScope::Org { org: "acme".into() },
                body: Policy::from_json(audit_policy(&format!("p{i}"))).unwrap(),
                author: "amy".into(),
                reason: String::new(),
                at: 1,
            })
            .await
            .unwrap();
    }
    let reqs = (0..8)
        .map(|i| {
            cookie_req(
                "POST",
                "/v1/orgs/acme/policies",
                Some(json!({ "policy": audit_policy(&format!("late-{i}")), "reason": "r" })),
                &s,
            )
        })
        .collect();
    let got = race(&h, reqs).await;
    let live = policies
        .list(&PolicyScope::Org { org: "acme".into() })
        .await
        .unwrap()
        .len();
    eprintln!("racing creates: {got:?}; live = {live}");
    // Documented as soft: each racer that read 255 creates, so the overshoot
    // is bounded by the number of racers (8 racers reached 259 here).
    assert!(live <= 255 + 8, "{live}");
}

/// Postgres: two updates and an update racing a delete, at one version.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redteam_api_postgres_compare_and_set_under_a_race() {
    let Some(tdb) = common::fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(scarab_db_postgres::PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let change = PolicyChange {
        author: "a".into(),
        reason: "r".into(),
        step_up_ref: None,
        at: 2,
    };
    for round in 0..10 {
        let id = format!("pol-{round}");
        pg.create(&NewPolicy {
            id: id.clone(),
            scope: PolicyScope::Org { org: "acme".into() },
            body: Policy::from_json(audit_policy(&id)).unwrap(),
            author: "a".into(),
            reason: "r".into(),
            at: 1,
        })
        .await
        .unwrap();
        let body = Policy::from_json(enforced_policy(&id, "x")).unwrap();
        let (a, b) = {
            let (p1, p2) = (pg.clone(), pg.clone());
            let (i1, i2) = (id.clone(), id.clone());
            let (b1, c1, c2) = (body.clone(), change.clone(), change.clone());
            let t1 = tokio::spawn(async move { p1.update(&i1, 1, &b1, &c1).await });
            let t2 = tokio::spawn(async move {
                if round % 2 == 0 {
                    p2.delete(&i2, 1, &c2).await
                } else {
                    p2.update(&i2, 1, &body, &c2).await
                }
            });
            (t1.await.unwrap(), t2.await.unwrap())
        };
        let oks = [a.is_ok(), b.is_ok()].iter().filter(|x| **x).count();
        assert_eq!(oks, 1, "round {round}: {a:?} / {b:?}");
        let loser = if a.is_ok() { b } else { a };
        // An update that loses to a delete finds the Policy gone.
        assert!(
            matches!(
                loser,
                Err(PolicyStoreError::Conflict | PolicyStoreError::NotFound)
            ),
            "{loser:?}"
        );
        assert_eq!(pg.versions(&id).await.unwrap().len(), 2);
    }
    tdb.cleanup().await;
}

// --- partial failure ----------------------------------------------------------

/// A store that loads Policies but cannot record a decision.
struct NoRecord(InMemoryPolicies);

#[async_trait]
impl PolicyStore for NoRecord {
    async fn create(&self, n: &NewPolicy) -> Result<StoredPolicy, PolicyStoreError> {
        self.0.create(n).await
    }
    async fn update(
        &self,
        a: &str,
        b: u64,
        c: &Policy,
        d: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        self.0.update(a, b, c, d).await
    }
    async fn delete(
        &self,
        a: &str,
        b: u64,
        c: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        self.0.delete(a, b, c).await
    }
    async fn get(&self, a: &str) -> Result<Option<StoredPolicy>, PolicyStoreError> {
        self.0.get(a).await
    }
    async fn list(&self, a: &PolicyScope) -> Result<Vec<StoredPolicy>, PolicyStoreError> {
        self.0.list(a).await
    }
    async fn versions(&self, a: &str) -> Result<Vec<PolicyVersion>, PolicyStoreError> {
        self.0.versions(a).await
    }
    async fn version(&self, a: &str, b: u64) -> Result<Option<PolicyVersion>, PolicyStoreError> {
        self.0.version(a, b).await
    }
    async fn effective(
        &self,
        a: &str,
        b: Option<&str>,
        c: Checkpoint,
    ) -> Result<Vec<StoredPolicy>, PolicyStoreError> {
        self.0.effective(a, b, c).await
    }
    async fn record_decision(&self, _: &DecisionRecord) -> Result<String, PolicyStoreError> {
        Err(PolicyStoreError::Store("disk full".into()))
    }
    async fn decision(&self, a: &str) -> Result<Option<DecisionRecord>, PolicyStoreError> {
        self.0.decision(a).await
    }
    async fn decisions_for_run(&self, a: &str) -> Result<Vec<DecisionRecord>, PolicyStoreError> {
        self.0.decisions_for_run(a).await
    }
    async fn org_decisions(
        &self,
        a: &str,
        b: usize,
        c: Option<&DecisionCursor>,
    ) -> Result<Vec<DecisionRecord>, PolicyStoreError> {
        self.0.org_decisions(a, b, c).await
    }
}

#[tokio::test]
async fn redteam_api_a_failed_decision_write_after_an_allow_creates_no_run() {
    let store = NoRecord(InMemoryPolicies::new());
    store
        .create(&NewPolicy {
            id: "p".into(),
            scope: PolicyScope::Org { org: "acme".into() },
            body: Policy::from_json(enforced_policy("never", "nope")).unwrap(),
            author: "a".into(),
            reason: "r".into(),
            at: 1,
        })
        .await
        .unwrap();
    let db = InMemoryDb::new();
    let err = trigger_runs_from_event(
        &forge_with(CI_YAML),
        &db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_policies(&store),
        &push(),
    )
    .await
    .expect_err("fail closed");
    assert!(matches!(err, TriggerError::Db(_)), "{err}");
    assert!(db.list_runs(10).await.unwrap().is_empty());
}

// --- second run ---------------------------------------------------------------

#[tokio::test]
async fn redteam_api_the_same_push_twice_records_one_decision_per_run() {
    let policies = InMemoryPolicies::new();
    policies
        .create(&NewPolicy {
            id: "p".into(),
            scope: PolicyScope::Org { org: "acme".into() },
            body: Policy::from_json(audit_policy("watch")).unwrap(),
            author: "a".into(),
            reason: "r".into(),
            at: 1,
        })
        .await
        .unwrap();
    let db = InMemoryDb::new();
    let mut runs = Vec::new();
    for _ in 0..2 {
        let o = trigger_runs_from_event(
            &forge_with(CI_YAML),
            &db,
            &FakeClock::new(1_000),
            AdmissionPorts::default().with_policies(&policies),
            &push(),
        )
        .await
        .unwrap();
        runs.extend(o.runs);
    }
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[0], runs[1]);
    for r in &runs {
        let d = policies.decisions_for_run(&r.0).await.unwrap();
        assert_eq!(d.len(), 1, "{r:?}");
    }
}

/// A webhook actor is the Run's `principal` when its immutable account id is
/// pinned to one through identity links (ADR-0069, ADR-0075 §2): a Policy on
/// `trigger.principal` denies the linked account's push, and admits the same
/// login under an id nobody linked. The forge serving the repo picks the
/// login provider, as forge import does.
#[tokio::test]
async fn a_linked_webhook_actor_is_the_run_create_principal() {
    use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind};
    use scarab_identity::{IdentityStore, ProviderIdentity};
    use scarab_server::config::OAuthConfig;
    use scarab_server::oauth::OAuthAuthenticator;
    use scarab_server::ActorLinks;

    let db = InMemoryDb::new();
    db.put_connection(&ForgeConnection {
        id: "gh".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com/".into(),
        credential_ref: "github-app".into(),
    })
    .await
    .unwrap();
    let repo = RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    };
    db.bind_repo("gh", &repo, "acme", "web").await.unwrap();
    let identities = scarab_testkit::InMemoryIdentities::new();
    let pinned = identities
        .pin(
            &ProviderIdentity {
                provider: "github".into(),
                external_id: "583231".into(),
                handle: "octocat".into(),
                email: None,
                email_verified: false,
                display_name: None,
            },
            "github:octocat",
            1,
        )
        .await
        .unwrap();
    let providers = [Arc::new(OAuthAuthenticator::new(OAuthConfig {
        client_id: "cid".into(),
        client_secret: "s".into(),
        authorize_url: "https://github.com/login/oauth/authorize".into(),
        token_url: "https://github.com/login/oauth/access_token".into(),
        userinfo_url: "https://api.github.com/user".into(),
        oidc_issuer: None,
        scopes: String::new(),
        owners: vec![],
        provider_id: "github".into(),
        unqualified_ids: false,
        name: "GitHub".into(),
        forge_url: Some("https://api.github.com".into()),
    }))];

    let policies = InMemoryPolicies::new();
    policies
        .create(&NewPolicy {
            id: "p".into(),
            scope: PolicyScope::Org { org: "acme".into() },
            body: Policy::from_json(json!({
                "name": "no-octocat", "checkpoint": "run.create", "mode": "enforce",
                "rules": [{
                    "when": format!("has(trigger.principal) && trigger.principal == \"{pinned}\""),
                    "effect": "deny", "message": "not this Principal"
                }]
            }))
            .unwrap(),
            author: "a".into(),
            reason: "r".into(),
            at: 1,
        })
        .await
        .unwrap();
    let ports = AdmissionPorts {
        links: ActorLinks {
            connections: Some(&db),
            identities: Some(&identities),
            providers: &providers,
        },
        ..AdmissionPorts::default().with_policies(&policies)
    };
    let push_by = |actor_id: &str| match push() {
        Event::Push {
            actor,
            repo,
            r#ref,
            after,
            message,
            ..
        } => Event::Push {
            actor,
            actor_id: actor_id.into(),
            repo,
            r#ref,
            after,
            message,
        },
        _ => unreachable!(),
    };

    let linked = trigger_runs_from_event(
        &forge_with(CI_YAML),
        &db,
        &FakeClock::new(1_000),
        ports,
        &push_by("583231"),
    )
    .await
    .unwrap();
    assert!(linked.runs.is_empty(), "the linked account is denied");
    assert_eq!(linked.refused.len(), 1);
    let denied = policies.org_decisions("acme", 0, None).await.unwrap();
    assert_eq!(denied.len(), 1);
    assert_eq!(
        denied[0].decision.input["trigger"]["principal"],
        json!(pinned)
    );
    assert_eq!(denied[0].decision.input["principal"], json!(pinned));

    // The same login under an id nobody linked has no `principal`.
    let unlinked = trigger_runs_from_event(
        &forge_with(CI_YAML),
        &db,
        &FakeClock::new(1_000),
        ports,
        &push_by("1"),
    )
    .await
    .unwrap();
    assert_eq!(unlinked.runs.len(), 1);
    let d = policies
        .decisions_for_run(&unlinked.runs[0].0)
        .await
        .unwrap();
    assert_eq!(d[0].decision.input["trigger"]["actor"], json!("octocat"));
    assert_eq!(d[0].decision.input["trigger"]["principal"], json!(null));
}

/// Saving an enforced Policy unchanged needs no step-up; saving an earlier
/// version's body back ("restore") does.
#[tokio::test]
async fn redteam_api_resave_identical_is_exempt_and_restore_is_not() {
    let (h, policies) = with_policies();
    let (s, mut soft) = admin(&h).await;
    let created = create(&h, &s, "/v1/orgs/acme/policies", enforced_policy("e", "a")).await;
    let id = created["id"].as_str().unwrap().to_string();
    let uri = format!("/v1/orgs/acme/policies/{id}");
    let put = |p: Value, v: u64| Some(json!({ "policy": p, "expected_version": v, "reason": "r" }));

    let (st, b) = h
        .cookie("PUT", &uri, put(enforced_policy("e", "a"), 1), &s)
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let v = b["version"].as_u64().unwrap();

    h.open_window(&s, &mut soft).await;
    let (st, b) = h
        .cookie("PUT", &uri, put(enforced_policy("e", "b"), v), &s)
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let v = b["version"].as_u64().unwrap();

    // A second admin with no window open restores the first body.
    h.grant("ann", Scope::Org("acme".into()), Role::Owner).await;
    let ann = h.browser_session(owner("ann")).await;
    let mut soft2 = passkey();
    h.enroll(&ann, &mut soft2, "Phone").await;
    let (st, b) = h
        .cookie("PUT", &uri, put(enforced_policy("e", "a"), v), &ann)
        .await;
    assert_step_up(st, &b, "window");
    assert_eq!(policies.get(&id).await.unwrap().unwrap().version, v);
}

// --- authorization -------------------------------------------------------------

fn draft_input() -> Value {
    json!({
        "org": { "name": "acme" }, "project": { "name": "web" },
        "trigger": { "kind": "push", "ref": "refs/heads/main", "fork": false },
        "pipeline": { "name": "ci", "deploy": false },
        "steps": [], "gates": [], "time": "2026-10-29T00:00:00Z"
    })
}

#[tokio::test]
async fn redteam_api_cross_org_ids_and_role_boundaries() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    let acme = create(&h, &s, "/v1/orgs/acme/policies", audit_policy("a")).await;
    let acme_id = acme["id"].as_str().unwrap();

    // A Policy and a decision of another org.
    let other = policies
        .create(&NewPolicy {
            id: "other-pol".into(),
            scope: PolicyScope::Org {
                org: "other".into(),
            },
            body: Policy::from_json(audit_policy("o")).unwrap(),
            author: "x".into(),
            reason: String::new(),
            at: 1,
        })
        .await
        .unwrap();
    let other_decision = policies
        .record_decision(&DecisionRecord {
            id: "other-dec".into(),
            org: "other".into(),
            project: Some("web".into()),
            run_id: None,
            step_id: None,
            attempt: None,
            decision: PolicyDecision {
                checkpoint: Checkpoint::RunCreate,
                schema_version: 1,
                input: draft_input(),
                policy_set_digest: String::new(),
                results: vec![],
                outcome: DecisionOutcome::Allow,
            },
            at: 1,
        })
        .await
        .unwrap();

    let test = |body: Value, who: Sess| {
        let h = &h;
        async move {
            h.cookie("POST", "/v1/orgs/acme/policy-test", Some(body), &who)
                .await
        }
    };
    let (st, _) = test(
        json!({ "policy_id": other.id, "input": draft_input() }),
        s.clone(),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "another org's Policy");
    let (st, _) = test(json!({ "decision_id": other_decision }), s.clone()).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "another org's decision");
    let (st, _) = h
        .cookie(
            "GET",
            &format!("/v1/orgs/acme/policies/{}", other.id),
            None,
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = h
        .cookie(
            "GET",
            &format!("/v1/orgs/acme/projects/web/policies/{acme_id}"),
            None,
            &s,
        )
        .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "an org Policy through a Project route"
    );
    let (st, _) = h
        .cookie(
            "PUT",
            &format!("/v1/orgs/acme/projects/web/policies/{acme_id}"),
            Some(json!({ "policy": audit_policy("a"), "expected_version": 1 })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // A Viewer of acme may evaluate stored Policies (org Read: a Policy by
    // id, or the effective ones, as `scarab validate --server` does) but not
    // a draft, which needs org Administer, as saving it would (finding
    // F1-F3's route); a Viewer of another org may not; a Project admin has
    // no org Read.
    h.grant("vic", Scope::Org("acme".into()), Role::Viewer)
        .await;
    h.grant("olga", Scope::Org("other".into()), Role::Owner)
        .await;
    h.grant(
        "pat",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Admin,
    )
    .await;
    let vic = h.browser_session(scoped("vic")).await;
    let olga = h.browser_session(scoped("olga")).await;
    let pat = h.browser_session(scoped("pat")).await;
    let draft = json!({ "policy": audit_policy("d"), "input": draft_input() });
    let (st, b) = test(draft.clone(), vic.clone()).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a Viewer's draft: {b}");
    let (st, b) = test(draft.clone(), s.clone()).await;
    assert_eq!(st, StatusCode::OK, "an Owner's draft: {b}");
    let (st, b) = test(json!({ "input": draft_input() }), vic.clone()).await;
    assert_eq!(st, StatusCode::OK, "a Viewer, effective Policies: {b}");
    assert_eq!(
        b["decision"]["results"][0]["policy_id"],
        json!(acme_id),
        "{b}"
    );
    let (st, b) = test(
        json!({ "policy_id": acme_id, "input": draft_input() }),
        vic.clone(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "a Viewer, a stored Policy: {b}");
    let (st, b) = test(
        json!({ "pipeline": {
            "yaml": CI_YAML, "project": "web",
            "trigger": { "kind": "push", "ref": "refs/heads/main" }
        }}),
        vic.clone(),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "a Viewer, the validate --server form: {b}"
    );
    let (st, _) = test(draft.clone(), olga.clone()).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, b) = test(draft.clone(), pat.clone()).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "project admin on org policy-test: {b}"
    );
    // Read on a Project evaluates that Project's effective Policies, as a
    // refused Run of the Project would show them, and nothing else: not
    // another Project's, not an org Policy by id. Another org's Owner has
    // neither.
    h.grant(
        "rita",
        Scope::Project {
            org: "acme".into(),
            name: "web".into(),
        },
        Role::Viewer,
    )
    .await;
    let rita = h.browser_session(scoped("rita")).await;
    let (st, b) = test(json!({ "input": draft_input() }), rita.clone()).await;
    assert_eq!(st, StatusCode::OK, "a Project Viewer, its Project: {b}");
    let validate = |project: &str| {
        json!({ "pipeline": {
            "yaml": CI_YAML, "project": project,
            "trigger": { "kind": "push", "ref": "refs/heads/main" }
        }})
    };
    let (st, b) = test(validate("web"), rita.clone()).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "a Project Viewer, validate --server: {b}"
    );
    let (st, _) = test(validate("api"), rita.clone()).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "a Project Viewer, another Project"
    );
    let (st, _) = test(
        json!({ "policy_id": acme_id, "input": draft_input() }),
        rita.clone(),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a Project Viewer, an org Policy");
    let (st, _) = test(json!({ "input": draft_input() }), olga.clone()).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "another org's Owner");
    // A Viewer cannot write.
    let (st, _) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies",
            Some(json!({ "policy": audit_policy("v") })),
            &vic,
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Nothing was recorded by any test.
    assert!(policies
        .org_decisions("acme", 0, None)
        .await
        .unwrap()
        .is_empty());
}

// --- the policy test API against admission ------------------------------------

fn over_cap_yaml() -> String {
    let mut y = String::from("on:\n  push: {}\nsteps:\n");
    for i in 0..=512 {
        y.push_str(&format!(
            "  - {{ id: s{i}, image: busybox, command: [\"true\"] }}\n"
        ));
    }
    y
}

/// A Run over the 512-Step cap with only an audit Policy is admitted by
/// admission (the audit error enforces nothing). The policy test API should
/// answer the same for the same Pipeline and push.
#[tokio::test]
async fn redteam_api_policy_test_answers_as_admission_for_a_run_over_the_cap() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    create(&h, &s, "/v1/orgs/acme/policies", audit_policy("watch")).await;
    let yaml = over_cap_yaml();

    let outcome = trigger_runs_from_event(
        &forge_with(&yaml),
        h.db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_policies(policies.as_ref()),
        &push(),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.runs.len(),
        1,
        "admission admits it: {:?}",
        outcome.refused
    );

    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(json!({ "pipeline": {
                "yaml": yaml, "project": "web",
                "trigger": { "kind": "push", "ref": "refs/heads/main", "sha": SHA, "actor": "octocat" }
            }})),
            &s,
        )
        .await;
    assert_eq!(
        (st, b["decision"]["outcome"]["kind"].clone()),
        (StatusCode::OK, json!("allow")),
        "policy-test answers differently from admission: {b}"
    );
    // The same decision admission recorded: an audit error naming the cap,
    // and no input.
    let recorded = policies.org_decisions("acme", 1, None).await.unwrap()[0]
        .decision
        .clone();
    assert_eq!(b["decision"]["results"], json!(recorded.results), "{b}");
    assert_eq!(b["decision"]["input"], Value::Null);
    assert_eq!(
        b["decision"]["results"][0]["outcome"]["kind"], "error",
        "{b}"
    );
    assert!(
        b["decision"]["results"][0]["outcome"]["message"]
            .as_str()
            .unwrap()
            .starts_with("the Run has 513 Steps"),
        "{b}"
    );

    // With an enforced Policy too, both deny.
    create(
        &h,
        &s,
        "/v1/orgs/acme/policies",
        enforced_policy("gate", "none"),
    )
    .await;
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(json!({ "pipeline": {
                "yaml": yaml, "project": "web",
                "trigger": { "kind": "push", "ref": "refs/heads/main", "sha": SHA, "actor": "octocat" }
            }})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["decision"]["outcome"]["kind"], "deny", "{b}");
    let outcome = trigger_runs_from_event(
        &forge_with(&yaml),
        h.db.as_ref(),
        &FakeClock::new(2_000),
        AdmissionPorts::default().with_policies(policies.as_ref()),
        &push(),
    )
    .await
    .unwrap();
    assert!(outcome.runs.is_empty(), "admission refuses it too");
}

/// F2: a supplied input with a string over its cap is a 400 naming the
/// field, before anything is evaluated; one at the cap is evaluated.
#[tokio::test]
async fn redteam_api_policy_test_refuses_an_input_string_over_its_cap() {
    let (h, _) = with_policies();
    let (s, _) = admin(&h).await;
    create(&h, &s, "/v1/orgs/acme/policies", audit_policy("watch")).await;
    let mut input = draft_input();
    input["pipeline"]["name"] = json!("a".repeat(1_200_000));
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(json!({ "input": input })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b.to_string().contains("`pipeline.name` is 1200000 bytes"),
        "{b}"
    );
    input["pipeline"]["name"] = json!("a".repeat(1024));
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(json!({ "input": input })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

/// A supplied input with a list over its cap is a 400 naming the list and
/// the cap, as a string over its cap is: the plugin default reads the
/// input's gates and origins directly, so an uncapped list would be walked
/// whole. One at the caps is evaluated.
#[tokio::test]
async fn redteam_api_policy_test_refuses_an_input_list_over_its_cap() {
    let (h, _) = with_policies();
    let (s, _) = admin(&h).await;
    create(&h, &s, "/v1/orgs/acme/policies", audit_policy("watch")).await;
    let plugin = |i: usize| json!({ "kind": "plugin", "name": format!("p{i}"), "content_hash": "ab".repeat(32) });
    let gate = |i: usize, origins: usize| json!({ "id": format!("g{i}"), "origin": (0..origins).map(plugin).collect::<Vec<_>>(), "dependents": [] });
    let post = |input: Value| {
        let (h, s) = (&h, &s);
        async move {
            h.cookie(
                "POST",
                "/v1/orgs/acme/policy-test",
                Some(json!({ "input": input })),
                s,
            )
            .await
        }
    };

    let mut input = draft_input();
    input["gates"] = json!([gate(0, 10_000)]);
    let (st, b) = post(input.clone()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b.to_string().contains("`gates[0].origin` has 10000 items")
            && b.to_string().contains("at most 8"),
        "{b}"
    );

    input["gates"] = json!((0..33).map(|i| gate(i, 0)).collect::<Vec<_>>());
    let (st, b) = post(input.clone()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b.to_string().contains("`gates` has 33 items") && b.to_string().contains("at most 32"),
        "{b}"
    );

    input["gates"] = json!((0..32).map(|i| gate(i, 8)).collect::<Vec<_>>());
    let (st, b) = post(input).await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

/// A `libs` map of 2,000 unused libraries (~1.5 MB) is compiled away
/// quickly; a 9-deep invoke chain is a 400.
#[tokio::test]
async fn redteam_api_policy_test_with_a_giant_libs_map_and_a_deep_chain() {
    let (h, _) = with_policies();
    let (s, _) = admin(&h).await;
    let lib = format!(
        "steps:\n  - {{ id: x, image: busybox, command: [\"{}\"] }}\n",
        "y".repeat(700)
    );
    let libs: serde_json::Map<String, Value> = (0..2000)
        .map(|i| (format!(".scarab/lib/l{i}.yaml"), json!(lib)))
        .collect();
    let t = std::time::Instant::now();
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(json!({ "policy": audit_policy("d"), "pipeline": {
                "yaml": CI_YAML, "project": "web", "libs": libs,
                "trigger": { "kind": "push", "ref": "refs/heads/main" }
            }})),
            &s,
        )
        .await;
    eprintln!("giant libs: {st} in {:?}", t.elapsed());
    assert_eq!(st, StatusCode::OK, "{b}");

    let mut libs = serde_json::Map::new();
    for i in 0..9 {
        libs.insert(
            format!(".scarab/lib/c{i}.yaml"),
            json!(format!(
                "steps:\n  - {{ id: n{i}, invoke: .scarab/lib/c{}.yaml }}\n",
                i + 1
            )),
        );
    }
    libs.insert(
        ".scarab/lib/c9.yaml".into(),
        json!("steps:\n  - { id: leaf, image: busybox }\n"),
    );
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policy-test",
            Some(json!({ "policy": audit_policy("d"), "pipeline": {
                "yaml": "on:\n  push: {}\nsteps:\n  - { id: top, invoke: .scarab/lib/c0.yaml }\n",
                "project": "web", "libs": libs,
                "trigger": { "kind": "push", "ref": "refs/heads/main" }
            }})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
}

/// A `reason` of exactly 2,000 bytes (1,000 two-byte characters) is kept.
#[tokio::test]
async fn redteam_api_a_2000_byte_multibyte_reason_is_accepted() {
    let (h, policies) = with_policies();
    let (s, _) = admin(&h).await;
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/policies",
            Some(json!({ "policy": audit_policy("r"), "reason": "é".repeat(1000) })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let v = policies.versions(b["id"].as_str().unwrap()).await.unwrap();
    assert_eq!(v[0].reason.len(), 2000);
}
