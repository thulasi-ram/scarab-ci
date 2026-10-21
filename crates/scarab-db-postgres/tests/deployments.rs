//! The Deployment record's input log (ADR-0070, migration 0053) against
//! *real* Postgres.
//!
//! The fold is pure and tested in `scarab-deployment`; what only Postgres can
//! show is the log itself: inputs round-trip exactly (both kinds, a report's
//! link present or absent), seqs are per-Org and gap-free from 1 even under
//! concurrent appends — the property a Deployment's public id rests on — the
//! table refuses a row that is neither a well-formed request nor a well-formed
//! report, and the legacy approval-history `deployments` table neither feeds
//! nor is fed by it. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_deployment::{
    DeploymentInput, DeploymentRequester, DeploymentStatusReport, DeploymentStore,
    DeploymentTarget, DeploymentVersion, SourceAssertedStatus,
};
use scarab_project::EnvironmentStore;

fn target(environment: &str, id: &str) -> DeploymentTarget {
    DeploymentTarget {
        environment: environment.into(),
        source: "argocd-prod".into(),
        id: id.into(),
    }
}

fn request(environment: &str, id: &str, version: &str, run: &str, at: i64) -> DeploymentInput {
    DeploymentInput::Request {
        target: target(environment, id),
        version: DeploymentVersion(version.into()),
        requester: DeploymentRequester {
            run: run.into(),
            step: "bump".into(),
        },
        effective_at: at - 5,
        at,
    }
}

fn report(
    environment: &str,
    id: &str,
    version: &str,
    status: SourceAssertedStatus,
    link: Option<&str>,
    at: i64,
) -> DeploymentInput {
    DeploymentInput::Report {
        target: target(environment, id),
        version: DeploymentVersion(version.into()),
        report: DeploymentStatusReport {
            status,
            label: "sync wave 5: migrate".into(),
            link: link.map(String::from),
            at,
        },
    }
}

#[tokio::test]
async fn inputs_round_trip_and_seq_is_per_org_from_one() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let asked = request("prod", "web", "v1", "run-1", 100);
    let in_progress = report(
        "prod",
        "web",
        "v1",
        SourceAssertedStatus::InProgress,
        Some("https://argo.example/web"),
        110,
    );
    let live = report("prod", "web", "v1", SourceAssertedStatus::Live, None, 120);
    let failed = report(
        "staging",
        "api",
        "a1",
        SourceAssertedStatus::Failed,
        None,
        130,
    );

    assert_eq!(db.append("acme", &asked).await.unwrap(), 1);
    assert_eq!(db.append("acme", &in_progress).await.unwrap(), 2);
    // Another Org's log starts its own count: ids are Org-scoped.
    assert_eq!(db.append("globex", &asked).await.unwrap(), 1);
    assert_eq!(db.append("acme", &live).await.unwrap(), 3);
    assert_eq!(db.append("acme", &failed).await.unwrap(), 4);

    let prod = db.inputs("acme", "prod").await.unwrap();
    assert_eq!(
        prod.iter().map(|l| l.seq).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "one Environment's inputs only, in seq order"
    );
    assert_eq!(prod[0].input, asked);
    assert_eq!(prod[1].input, in_progress, "a link survives");
    assert_eq!(prod[2].input, live, "an absent link stays absent");

    let staging = db.inputs("acme", "staging").await.unwrap();
    assert_eq!(staging.len(), 1);
    assert_eq!(staging[0].seq, 4);
    assert_eq!(staging[0].input, failed);

    assert_eq!(
        db.environments("acme").await.unwrap(),
        vec!["prod".to_string(), "staging".to_string()]
    );
    assert_eq!(
        db.environments("globex").await.unwrap(),
        vec!["prod".to_string()]
    );
    assert!(db.environments("initech").await.unwrap().is_empty());
    assert!(db.inputs("initech", "prod").await.unwrap().is_empty());

    tdb.cleanup().await;
}

/// The public id of a Deployment is the seq of the input that created it, so
/// concurrent appends must still hand out exactly 1..=N with no gap and no
/// duplicate — the counter row serialises them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_get_exactly_one_through_n() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    db.migrate().await.unwrap();

    let handles: Vec<_> = (0..50)
        .map(|i| {
            let db = db.clone();
            tokio::spawn(async move {
                db.append(
                    "acme",
                    &request("prod", &format!("svc-{i}"), "v1", "run-1", 100 + i),
                )
                .await
                .unwrap()
            })
        })
        .collect();
    let mut seqs = Vec::new();
    for h in handles {
        seqs.push(h.await.unwrap());
    }
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=50).collect::<Vec<u64>>());

    let logged: Vec<u64> = db
        .inputs("acme", "prod")
        .await
        .unwrap()
        .iter()
        .map(|l| l.seq)
        .collect();
    assert_eq!(logged, (1..=50).collect::<Vec<u64>>());

    tdb.cleanup().await;
}

/// The table refuses rows that are neither a whole request nor a whole report
/// — including a source-asserted status a source may not assert (INV-DEP-6
/// at rest).
#[tokio::test]
async fn malformed_rows_are_rejected_by_the_table() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let insert = |seq: i64, kind: &str, run: Option<&str>, status: Option<&str>| {
        let (kind, run, status) = (
            kind.to_string(),
            run.map(String::from),
            status.map(String::from),
        );
        let pool = tdb.pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO deployment_inputs
                     (org, seq, environment, target_source, target_id, version, kind, at,
                      requester_run, requester_step, effective_at, report_status, report_label)
                 VALUES ('acme', $1, 'prod', 'argocd-prod', 'web', 'v1', $2, 0,
                         $3, $3, CASE WHEN $3 IS NULL THEN NULL ELSE 0 END,
                         $4, CASE WHEN $4 IS NULL THEN NULL ELSE 'x' END)",
            )
            .bind(seq)
            .bind(kind)
            .bind(run)
            .bind(status)
            .execute(&pool)
            .await
        }
    };

    // The well-formed shapes go in, so the rejections below are the CHECKs.
    insert(1, "request", Some("run-1"), None).await.unwrap();
    insert(2, "report", None, Some("live")).await.unwrap();

    assert!(
        insert(3, "request", None, None).await.is_err(),
        "no requester"
    );
    assert!(
        insert(4, "request", Some("run-1"), Some("live"))
            .await
            .is_err(),
        "a request carrying a report"
    );
    assert!(insert(5, "report", None, None).await.is_err(), "no status");
    assert!(
        insert(6, "report", Some("run-1"), Some("live"))
            .await
            .is_err(),
        "a report carrying a requester"
    );
    assert!(
        insert(7, "report", None, Some("superseded")).await.is_err(),
        "superseded is core-derived"
    );
    assert!(
        insert(8, "approval", None, None).await.is_err(),
        "unknown kind"
    );

    tdb.cleanup().await;
}

/// The legacy `deployments` table stays what it was — approval history — and
/// the input log is a separate thing: recording an approval appends nothing to
/// it, and appending to it records no approval.
#[tokio::test]
async fn legacy_approval_history_is_untouched() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let approval = scarab_project::Deployment {
        org: "acme".into(),
        project: "web".into(),
        environment: "prod".into(),
        git_ref: "refs/heads/main".into(),
        run: "run-1".into(),
        approved_by: vec!["alice".into()],
        at: 100,
    };
    db.record_deployment(&approval).await.unwrap();
    assert!(db.inputs("acme", "prod").await.unwrap().is_empty());
    assert!(db.environments("acme").await.unwrap().is_empty());

    db.append("acme", &request("prod", "web", "v1", "run-1", 200))
        .await
        .unwrap();
    assert_eq!(
        db.deployments("acme", "web", "prod").await.unwrap(),
        vec![approval]
    );

    tdb.cleanup().await;
}
