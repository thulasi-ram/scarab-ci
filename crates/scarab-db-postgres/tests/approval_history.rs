//! The legacy approval history holds one record per gate wait (git-bug
//! 1d06f2d). Two approvals that meet an Environment's rules at the same
//! moment each read "not recorded yet" and each write; the unique key on
//! (run, gate Step, gate Attempt) makes the second write do nothing. A
//! rerun of the gate is a new Attempt and gets its own record, and a
//! record with no wait (written before the key existed) never collides.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_project::{ApprovalHistoryStore, ApprovalWait, Deployment};
use scarab_testkit::legacy_environments::approval_history;

fn record(at: i64, approved_by: &[&str]) -> Deployment {
    Deployment {
        org: "acme".into(),
        project: "web".into(),
        environment: "prod".into(),
        git_ref: "refs/heads/main".into(),
        run: "run-1".into(),
        approved_by: approved_by.iter().map(|s| s.to_string()).collect(),
        at,
    }
}

fn wait(attempt: &str) -> ApprovalWait {
    ApprovalWait {
        gate: "approve".into(),
        attempt: attempt.into(),
    }
}

#[tokio::test]
async fn the_history_holds_one_record_per_gate_wait() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();
    let count = || async {
        approval_history(&tdb.pool, "acme", "web", "prod")
            .await
            .len()
    };

    // The race: both approvals saw the rules unmet, both write for wait a1.
    pg.record_deployment(&record(10, &["amy", "bob"]), Some(&wait("a1")))
        .await
        .unwrap();
    pg.record_deployment(&record(11, &["amy", "bob"]), Some(&wait("a1")))
        .await
        .unwrap();
    assert_eq!(count().await, 1, "one record for the wait");

    // A rerun opened wait a2: its approval is recorded too.
    pg.record_deployment(&record(20, &["amy", "bob"]), Some(&wait("a2")))
        .await
        .unwrap();
    assert_eq!(count().await, 2);

    // Records with no wait are kept as they are.
    pg.record_deployment(&record(30, &["amy"]), None)
        .await
        .unwrap();
    pg.record_deployment(&record(31, &["amy"]), None)
        .await
        .unwrap();
    assert_eq!(count().await, 4);
    tdb.cleanup().await;
}
