//! `PolicyStore` parity contract (ADR-0075 §9, §11): one suite, run against
//! the in-memory fake (`scarab_testkit::InMemoryPolicies`, always) and the
//! Postgres adapter (a throwaway database per test, skipping cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset — see `common`). The style of
//! `db_contract.rs`.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_policy::evaluate::{evaluate, MatchContext};
use scarab_policy::ir::{compile, Level};
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::run_create::{self, TriggerKind};
use scarab_policy::store::{
    DecisionCursor, DecisionRecord, NewPolicy, PolicyChange, PolicyScope, PolicyStore,
    PolicyStoreError,
};
use scarab_testkit::InMemoryPolicies;
use serde_json::json;

macro_rules! contract_test {
    ($name:ident, $contract:ident) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn in_memory() {
                $contract(&InMemoryPolicies::new()).await;
            }

            #[tokio::test]
            async fn postgres() {
                let Some(tdb) = fresh_db().await else {
                    eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
                    return;
                };
                let db = PostgresDb::with_pool(tdb.pool.clone());
                db.migrate().await.unwrap();
                $contract(&db).await;
                tdb.cleanup().await;
            }
        }
    };
}

contract_test!(versions_and_optimistic_concurrency, versions_and_occ);
contract_test!(names_are_unique_among_live_policies, unique_names);
contract_test!(effective_is_org_then_project, effective_policies);
contract_test!(decisions_round_trip_and_page, decisions);

fn body(name: &str, checkpoint: &str, when: &str) -> Policy {
    Policy::from_json(json!({
        "name": name,
        "checkpoint": checkpoint,
        "rules": [{ "when": when, "effect": "deny", "message": format!("{name} denies") }]
    }))
    .unwrap()
}

fn org(o: &str) -> PolicyScope {
    PolicyScope::Org { org: o.into() }
}

fn project(o: &str, p: &str) -> PolicyScope {
    PolicyScope::Project {
        org: o.into(),
        project: p.into(),
    }
}

fn new(id: &str, scope: PolicyScope, body: Policy) -> NewPolicy {
    NewPolicy {
        id: id.into(),
        scope,
        body,
        author: "user:amy".into(),
        reason: "first".into(),
        at: 1_000,
    }
}

fn change(at: i64, step_up: Option<&str>) -> PolicyChange {
    PolicyChange {
        author: "user:bob".into(),
        reason: format!("change at {at}"),
        step_up_ref: step_up.map(str::to_string),
        at,
    }
}

async fn versions_and_occ(s: &dyn PolicyStore) {
    let v1 = body("libs", "run.create", "pipeline.deploy");
    let created = s.create(&new("p1", org("acme"), v1.clone())).await.unwrap();
    assert_eq!(created.version, 1);
    assert_eq!(created.content_hash, v1.content_hash());
    assert_eq!((created.created_at, created.updated_at), (1_000, 1_000));
    assert!(!created.deleted);
    assert_eq!(s.get("p1").await.unwrap(), Some(created.clone()));

    let v2 = body("libs", "run.create", "!pipeline.deploy");
    let updated = s.update("p1", 1, &v2, &change(2_000, None)).await.unwrap();
    assert_eq!(updated.version, 2);
    assert_eq!(updated.body, v2);
    assert_eq!(updated.created_at, 1_000);
    assert_eq!(updated.updated_at, 2_000);

    // A writer that read version 1 lost the race.
    assert_eq!(
        s.update("p1", 1, &v1, &change(3_000, None)).await,
        Err(PolicyStoreError::Conflict)
    );
    assert_eq!(
        s.delete("p1", 1, &change(3_000, None)).await,
        Err(PolicyStoreError::Conflict)
    );
    assert_eq!(s.get("p1").await.unwrap().unwrap().version, 2);

    let deleted = s
        .delete("p1", 2, &change(4_000, Some("stepup-1")))
        .await
        .unwrap();
    assert!(deleted.deleted);
    assert_eq!(deleted.version, 3);
    assert_eq!(deleted.body, v2, "a delete keeps the last body");
    assert_eq!(s.get("p1").await.unwrap(), Some(deleted));
    assert!(s.list(&org("acme")).await.unwrap().is_empty());
    assert_eq!(
        s.update("p1", 3, &v1, &change(5_000, None)).await,
        Err(PolicyStoreError::NotFound)
    );
    assert_eq!(
        s.update("nope", 1, &v1, &change(5_000, None)).await,
        Err(PolicyStoreError::NotFound)
    );
    assert_eq!(s.get("nope").await.unwrap(), None);

    let history = s.versions("p1").await.unwrap();
    let summary: Vec<_> = history
        .iter()
        .map(|v| {
            (
                v.version,
                v.author.as_str(),
                v.deleted,
                v.step_up_ref.as_deref(),
                v.at,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (1, "user:amy", false, None, 1_000),
            (2, "user:bob", false, None, 2_000),
            (3, "user:bob", true, Some("stepup-1"), 4_000),
        ]
    );
    assert_eq!(history[0].body, v1);
    assert_eq!(history[0].reason, "first");
    assert_eq!(s.version("p1", 2).await.unwrap().unwrap().body, v2);
    assert_eq!(s.version("p1", 9).await.unwrap(), None);
    assert!(s.versions("nope").await.unwrap().is_empty());
}

async fn unique_names(s: &dyn PolicyStore) {
    let a = body("a", "run.create", "true");
    s.create(&new("p1", org("acme"), a.clone())).await.unwrap();
    assert_eq!(
        s.create(&new("p2", org("acme"), a.clone())).await,
        Err(PolicyStoreError::NameTaken("a".into()))
    );
    // Another scope, another org: the same name is free.
    s.create(&new("p3", project("acme", "api"), a.clone()))
        .await
        .unwrap();
    s.create(&new("p4", org("other"), a.clone())).await.unwrap();
    assert_eq!(
        s.create(&new("p5", project("acme", "api"), a.clone()))
            .await,
        Err(PolicyStoreError::NameTaken("a".into()))
    );

    // Renaming onto a live name is refused; the Policy is unchanged.
    let b = body("b", "run.create", "true");
    s.create(&new("p6", org("acme"), b.clone())).await.unwrap();
    assert_eq!(
        s.update("p6", 1, &a, &change(2_000, None)).await,
        Err(PolicyStoreError::NameTaken("a".into()))
    );
    assert_eq!(s.get("p6").await.unwrap().unwrap().body, b);

    // A deleted Policy's name can be used again.
    s.delete("p1", 1, &change(3_000, None)).await.unwrap();
    s.create(&new("p7", org("acme"), a.clone())).await.unwrap();
    let names: Vec<_> = s
        .list(&org("acme"))
        .await
        .unwrap()
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    assert_eq!(
        names,
        [("p7".into(), "a".into()), ("p6".into(), "b".into())]
    );
}

async fn effective_policies(s: &dyn PolicyStore) {
    s.create(&new("o-z", org("acme"), body("z", "run.create", "true")))
        .await
        .unwrap();
    s.create(&new("o-a", org("acme"), body("a", "run.create", "true")))
        .await
        .unwrap();
    s.create(&new("o-step", org("acme"), body("s", "step.admit", "true")))
        .await
        .unwrap();
    s.create(&new(
        "p-api",
        project("acme", "api"),
        body("b", "run.create", "true"),
    ))
    .await
    .unwrap();
    s.create(&new(
        "p-web",
        project("acme", "web"),
        body("c", "run.create", "true"),
    ))
    .await
    .unwrap();
    s.create(&new("other", org("other"), body("d", "run.create", "true")))
        .await
        .unwrap();
    s.create(&new("gone", org("acme"), body("e", "run.create", "true")))
        .await
        .unwrap();
    s.delete("gone", 1, &change(2_000, None)).await.unwrap();

    let ids = |v: Vec<scarab_policy::store::StoredPolicy>| -> Vec<String> {
        v.into_iter().map(|p| p.id).collect()
    };
    assert_eq!(
        ids(s
            .effective("acme", Some("api"), Checkpoint::RunCreate)
            .await
            .unwrap()),
        ["o-a", "o-z", "p-api"]
    );
    assert_eq!(
        ids(s
            .effective("acme", None, Checkpoint::RunCreate)
            .await
            .unwrap()),
        ["o-a", "o-z"]
    );
    assert_eq!(
        ids(s
            .effective("acme", Some("api"), Checkpoint::StepAdmit)
            .await
            .unwrap()),
        ["o-step"]
    );
    assert_eq!(
        ids(s.list(&project("acme", "web")).await.unwrap()),
        ["p-web"]
    );
}

fn record(id: &str, run: Option<&str>, at: i64, deny: bool) -> DecisionRecord {
    let p = body("libs", "run.create", "pipeline.deploy");
    let ir = compile(&p, Level::Org).unwrap().with_identity("p1", 1);
    let input = json!({
        "org": { "name": "acme" }, "project": { "name": "api" },
        "trigger": { "kind": "push", "fork": false },
        "pipeline": { "name": "ci", "deploy": deny },
        "steps": [], "gates": [], "time": "2026-10-29T10:00:00Z"
    });
    let ctx = MatchContext {
        project: "api".into(),
        environment: None,
        trigger_kind: TriggerKind::Push,
    };
    let decision = evaluate(
        Checkpoint::RunCreate,
        run_create::SCHEMA_VERSION,
        &input,
        &ctx,
        &[ir],
    );
    assert_eq!(decision.is_denied(), deny);
    DecisionRecord {
        id: id.into(),
        org: "acme".into(),
        project: Some("api".into()),
        run_id: run.map(str::to_string),
        step_id: None,
        attempt: None,
        decision,
        at,
    }
}

async fn decisions(s: &dyn PolicyStore) {
    let allowed = record("d1", Some("run-1"), 1_000, false);
    let refused = record("d2", None, 2_000, true);
    let later = record("d3", Some("run-1"), 3_000, false);
    let same_ms = record("d4", Some("run-2"), 3_000, false);
    for r in [&allowed, &refused, &later, &same_ms] {
        assert_eq!(s.record_decision(r).await.unwrap(), r.id);
    }
    assert_eq!(
        s.record_decision(&allowed).await,
        Err(PolicyStoreError::Conflict),
        "an id is recorded once"
    );

    assert_eq!(
        s.decisions_for_run("run-1").await.unwrap(),
        [allowed.clone(), later.clone()]
    );
    assert!(s.decisions_for_run("run-9").await.unwrap().is_empty());

    // The org log: newest first, ties by id, paged by cursor.
    let page = s.org_decisions("acme", 0, None).await.unwrap();
    assert_eq!(
        page,
        [
            same_ms.clone(),
            later.clone(),
            refused.clone(),
            allowed.clone()
        ]
    );
    assert_eq!(page[2].decision.messages(), ["libs denies"]);
    let first = s.org_decisions("acme", 2, None).await.unwrap();
    assert_eq!(first, [same_ms.clone(), later.clone()]);
    let cursor = DecisionCursor {
        at: first[1].at,
        id: first[1].id.clone(),
    };
    assert_eq!(
        s.org_decisions("acme", 2, Some(&cursor)).await.unwrap(),
        [refused, allowed]
    );
    assert!(s.org_decisions("other", 0, None).await.unwrap().is_empty());

    // step.admit: one decision per Attempt.
    let admit = |id: &str, attempt: u32| {
        let mut r = record(id, Some("run-1"), 4_000, false);
        r.decision.checkpoint = Checkpoint::StepAdmit;
        r.step_id = Some("deploy".into());
        r.attempt = Some(attempt);
        r
    };
    s.record_decision(&admit("a1", 1)).await.unwrap();
    s.record_decision(&admit("a2", 2)).await.unwrap();
    assert_eq!(
        s.record_decision(&admit("a3", 1)).await,
        Err(PolicyStoreError::Conflict)
    );
    let run1 = s.decisions_for_run("run-1").await.unwrap();
    assert_eq!(run1.len(), 4);
    assert_eq!(run1[2], admit("a1", 1));
}
