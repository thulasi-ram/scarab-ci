//! The Deployment record's input log (ADR-0070, migration 0053) against
//! *real* Postgres.
//!
//! The fold is pure and tested in `scarab-deployment`; what only Postgres can
//! show is the log itself: inputs round-trip exactly (both kinds, a report's
//! link present or absent), seqs are per-Org and gap-free from 1 even under
//! concurrent appends — the property a Deployment's public id rests on — the
//! stamped time is the caller's `received_at` kept monotone per Org, a
//! `received_at` running far ahead of the database clock is refused, the
//! table refuses a row that is neither a well-formed request nor a well-formed
//! report, and the legacy approval-history `deployments` table neither feeds
//! nor is fed by it. Every test gets its own database (`fresh_db`), so Orgs
//! never leak a counter between tests. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_deployment::{
    DeploymentInput, DeploymentRequester, DeploymentStatusReport, DeploymentStore,
    DeploymentStoreError, DeploymentTarget, DeploymentVersion, SourceAssertedStatus,
    RECEIVED_AT_MAX_LEAD_MS,
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
        // Evidence round-trips with the request (migration 0058).
        evidence: vec!["sha256:aaa".into(), "sha256:bbb".into()],
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

/// `input` as the store should have logged it: the caller's `at` replaced by
/// the receipt time `at`, a request's `effective_at` clamped to it.
fn stamped(input: &DeploymentInput, receipt: i64) -> DeploymentInput {
    let mut input = input.clone();
    match &mut input {
        DeploymentInput::Request {
            at, effective_at, ..
        } => {
            *at = receipt;
            *effective_at = (*effective_at).min(receipt);
        }
        DeploymentInput::Report { report, .. } => report.at = receipt,
    }
    input
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

    let mut appended = Vec::new();
    // `received_at` is the control plane's clock, passed in: these are long in
    // the past, so the database clock never refuses them.
    for (org, input, received_at, seq) in [
        ("acme", &asked, 1_000, 1),
        ("acme", &in_progress, 1_010, 2),
        // Another Org's log starts its own count: ids are Org-scoped.
        ("globex", &asked, 1_015, 1),
        // Received "earlier" than the last input (another replica's clock
        // running behind): stamped at the Org's last `at`, never before it.
        ("acme", &live, 1_005, 3),
        ("acme", &failed, 1_030, 4),
    ] {
        let logged = db.append(org, input, received_at).await.unwrap();
        assert_eq!(logged.seq, seq);
        if org == "acme" {
            appended.push(logged);
        }
    }

    let prod = db.inputs("acme", "prod").await.unwrap();
    assert_eq!(
        prod.iter().map(|l| l.seq).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "one Environment's inputs only, in seq order"
    );
    assert_eq!(prod, appended[..3], "append returns exactly what is logged");

    // Core stamped the time: `max(received_at, the Org's last at)` — not the
    // caller's `at` (100, 110, …) — so it never goes backwards.
    let receipts: Vec<i64> = appended.iter().map(|l| l.input.at()).collect();
    assert_eq!(receipts, vec![1_000, 1_010, 1_010, 1_030]);

    // Everything else round-trips; a report keeps its source's time aside.
    assert_eq!(prod[0].input, stamped(&asked, receipts[0]));
    assert_eq!(prod[0].report_source_at, None);
    assert_eq!(
        prod[1].input,
        stamped(&in_progress, receipts[1]),
        "a link survives"
    );
    assert_eq!(prod[1].report_source_at, Some(110));
    assert_eq!(
        prod[2].input,
        stamped(&live, receipts[2]),
        "an absent link stays absent"
    );

    let staging = db.inputs("acme", "staging").await.unwrap();
    assert_eq!(staging.len(), 1);
    assert_eq!(staging[0].seq, 4);
    assert_eq!(staging[0].input, stamped(&failed, receipts[3]));
    assert_eq!(staging[0].report_source_at, Some(130));

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

/// A request's `effective_at` can never be later than when core received it:
/// a step cannot have started after its success was recorded.
#[tokio::test]
async fn a_request_effective_after_its_receipt_is_clamped() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let far_future = i64::MAX / 2;
    let logged = db
        .append(
            "acme",
            &DeploymentInput::Request {
                target: target("prod", "web"),
                version: DeploymentVersion("v1".into()),
                requester: DeploymentRequester {
                    run: "run-1".into(),
                    step: "bump".into(),
                },
                effective_at: far_future,
                at: far_future,
                evidence: Vec::new(),
            },
            500,
        )
        .await
        .unwrap();
    let DeploymentInput::Request {
        effective_at, at, ..
    } = logged.input
    else {
        panic!("a request comes back a request");
    };
    assert_eq!(at, 500, "the caller's `at` is ignored");
    assert_eq!(effective_at, 500);

    tdb.cleanup().await;
}

/// A `received_at` running more than a minute ahead of the database clock is
/// refused, and refused *whole*: the Org's counter is untouched, so the next
/// append gets the next seq and a stamp the runaway clock did not ratchet.
/// Without the refusal one replica with a clock an hour fast would stamp every
/// later input of the Org an hour late, for good.
#[tokio::test]
async fn a_received_at_far_ahead_of_the_database_clock_is_refused() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let first = db
        .append("acme", &request("prod", "web", "v1", "run-1", 0), 1_000)
        .await
        .unwrap();
    assert_eq!(first.seq, 1);

    let db_now: i64 =
        sqlx::query_scalar("SELECT (extract(epoch FROM clock_timestamp()) * 1000)::bigint")
            .fetch_one(&tdb.pool)
            .await
            .unwrap();
    // Ten lead-windows ahead: comfortably past the guard whatever the skew
    // between this process and the database.
    let runaway = db_now + 10 * RECEIVED_AT_MAX_LEAD_MS;
    let refused = db
        .append("acme", &request("prod", "web", "v2", "run-2", 0), runaway)
        .await;
    assert!(
        matches!(
            refused,
            Err(DeploymentStoreError::ReceivedAtAhead { received_at, store_now: Some(_) })
                if received_at == runaway
        ),
        "{refused:?}"
    );

    let next = db
        .append("acme", &request("prod", "web", "v3", "run-3", 0), 1_100)
        .await
        .unwrap();
    assert_eq!(next.seq, 2, "the refused append consumed no seq");
    assert_eq!(next.input.at(), 1_100, "nor ratcheted the Org's floor");
    assert_eq!(db.inputs("acme", "prod").await.unwrap().len(), 2);

    tdb.cleanup().await;
}

/// The public id of a Deployment is the seq of the input that created it, so
/// concurrent appends must still hand out exactly 1..=N with no gap and no
/// duplicate — the counter row serialises them, and their stamped times too.
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
                    // Racing appends whose readings disagree with their
                    // commit order: the stamp must still be monotone in seq.
                    1_000 + (i * 7919) % 50,
                )
                .await
                .unwrap()
                .seq
            })
        })
        .collect();
    let mut seqs = Vec::new();
    for h in handles {
        seqs.push(h.await.unwrap());
    }
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=50).collect::<Vec<u64>>());

    let logged = db.inputs("acme", "prod").await.unwrap();
    assert_eq!(
        logged.iter().map(|l| l.seq).collect::<Vec<_>>(),
        (1..=50).collect::<Vec<u64>>()
    );
    // Racing appends still agree on time order and seq order — what the fold
    // requires of its input.
    assert!(
        logged
            .windows(2)
            .all(|w| w[0].input.at() <= w[1].input.at()),
        "receipt times went backwards across seqs"
    );

    tdb.cleanup().await;
}

/// One `deployment_inputs` row with every per-kind column spelled out, so each
/// rejected case below differs from a well-formed row in exactly the way its
/// message names.
#[derive(Clone, Copy)]
struct RawInputRow {
    kind: &'static str,
    at: i64,
    requester_run: Option<&'static str>,
    requester_step: Option<&'static str>,
    effective_at: Option<i64>,
    report_status: Option<&'static str>,
    report_label: Option<&'static str>,
    report_link: Option<&'static str>,
    report_source_at: Option<i64>,
}

const WELL_FORMED_REQUEST: RawInputRow = RawInputRow {
    kind: "request",
    at: 10,
    requester_run: Some("run-1"),
    requester_step: Some("bump"),
    effective_at: Some(5),
    report_status: None,
    report_label: None,
    report_link: None,
    report_source_at: None,
};

const WELL_FORMED_REPORT: RawInputRow = RawInputRow {
    kind: "report",
    at: 10,
    requester_run: None,
    requester_step: None,
    effective_at: None,
    report_status: Some("live"),
    report_label: Some("healthy"),
    report_link: Some("https://argo.example/web"),
    report_source_at: Some(9),
};

async fn insert_raw(pool: &sqlx::PgPool, seq: i64, row: RawInputRow) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO deployment_inputs
             (org, seq, environment, target_source, target_id, version, kind, at,
              requester_run, requester_step, effective_at,
              report_status, report_label, report_link, report_source_at)
         VALUES ('acme', $1, 'prod', 'argocd-prod', 'web', 'v1', $2, $3,
                 $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(seq)
    .bind(row.kind)
    .bind(row.at)
    .bind(row.requester_run)
    .bind(row.requester_step)
    .bind(row.effective_at)
    .bind(row.report_status)
    .bind(row.report_label)
    .bind(row.report_link)
    .bind(row.report_source_at)
    .execute(pool)
    .await
    .map(|_| ())
}

/// The table refuses rows that are neither a whole request nor a whole report
/// — including a source-asserted status a source may not assert (INV-DEP-6
/// at rest) and a request whose step "started" after it was recorded.
#[tokio::test]
async fn malformed_rows_are_rejected_by_the_table() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let pool = &tdb.pool;

    // The well-formed shapes go in, so the rejections below are the CHECKs.
    insert_raw(pool, 1, WELL_FORMED_REQUEST).await.unwrap();
    insert_raw(pool, 2, WELL_FORMED_REPORT).await.unwrap();
    let linkless = RawInputRow {
        report_link: None,
        ..WELL_FORMED_REPORT
    };
    insert_raw(pool, 3, linkless).await.unwrap();
    let started_as_recorded = RawInputRow {
        effective_at: Some(10),
        ..WELL_FORMED_REQUEST
    };
    insert_raw(pool, 4, started_as_recorded).await.unwrap();

    let rejected = [
        (
            RawInputRow {
                requester_run: None,
                ..WELL_FORMED_REQUEST
            },
            "a request with no requester run",
        ),
        (
            RawInputRow {
                requester_step: None,
                ..WELL_FORMED_REQUEST
            },
            "a request with no requester step",
        ),
        (
            RawInputRow {
                effective_at: None,
                ..WELL_FORMED_REQUEST
            },
            "a request with no effective time",
        ),
        (
            RawInputRow {
                effective_at: Some(11),
                ..WELL_FORMED_REQUEST
            },
            "a request effective after it was recorded",
        ),
        (
            RawInputRow {
                report_status: Some("live"),
                ..WELL_FORMED_REQUEST
            },
            "a request carrying a report status",
        ),
        (
            RawInputRow {
                report_link: Some("https://argo.example/web"),
                ..WELL_FORMED_REQUEST
            },
            "a request carrying a report link",
        ),
        (
            RawInputRow {
                report_status: None,
                ..WELL_FORMED_REPORT
            },
            "a report with no status",
        ),
        (
            RawInputRow {
                report_label: None,
                ..WELL_FORMED_REPORT
            },
            "a report with no label",
        ),
        (
            RawInputRow {
                report_source_at: None,
                ..WELL_FORMED_REPORT
            },
            "a report with no source time",
        ),
        (
            RawInputRow {
                requester_run: Some("run-1"),
                requester_step: Some("bump"),
                ..WELL_FORMED_REPORT
            },
            "a report carrying a requester",
        ),
        (
            RawInputRow {
                effective_at: Some(5),
                ..WELL_FORMED_REPORT
            },
            "a report carrying an effective time",
        ),
        (
            RawInputRow {
                report_status: Some("superseded"),
                ..WELL_FORMED_REPORT
            },
            "superseded is core-derived",
        ),
        (
            RawInputRow {
                kind: "approval",
                ..WELL_FORMED_REQUEST
            },
            "an unknown kind",
        ),
    ];
    for (seq, (row, case)) in (10..).zip(rejected) {
        assert!(insert_raw(pool, seq, row).await.is_err(), "{case}");
    }

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

    db.append("acme", &request("prod", "web", "v1", "run-1", 200), 1_000)
        .await
        .unwrap();
    assert_eq!(
        db.deployments("acme", "web", "prod").await.unwrap(),
        vec![approval]
    );

    tdb.cleanup().await;
}
