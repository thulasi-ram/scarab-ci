//! `ScheduleStore` parity contract (git-bug 5d5fc8a): one suite, run against
//! the in-memory fake (always) and the Postgres adapter (a throwaway
//! database per test, skipped when `SCARAB_TEST_DATABASE_URL` is unset).

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{ScheduleEntry, ScheduleFire, ScheduleStore, Timestamp};
use scarab_testkit::InMemorySchedules;

macro_rules! contract_test {
    ($name:ident, $contract:ident) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn in_memory() {
                $contract(&InMemorySchedules::new()).await;
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

contract_test!(
    replacing_keeps_the_next_slot_of_a_schedule_still_declared,
    replace
);
contract_test!(a_slot_is_claimed_once, claim);
contract_test!(advancing_from_a_stale_slot_changes_nothing, advance);
contract_test!(due_rows_are_read_a_page_at_a_time, due_pages);

fn row(pipeline: &str, schedule: &str, next: i64) -> ScheduleEntry {
    ScheduleEntry {
        org: "acme".into(),
        project: "app".into(),
        pipeline: pipeline.into(),
        schedule: schedule.into(),
        next_fire_at: Timestamp(next),
        source_sha: String::new(),
    }
}

async fn replace(store: &dyn ScheduleStore) {
    store
        .replace_project_schedules(
            "acme",
            "app",
            "sha1",
            &[
                row(".scarab/a.yaml", "@hourly", 100),
                row(".scarab/b.yaml", "@daily", 200),
            ],
        )
        .await
        .unwrap();
    // Another project's rows are not touched by this one's replace.
    store
        .replace_project_schedules(
            "acme",
            "other",
            "x",
            &[ScheduleEntry {
                project: "other".into(),
                ..row(".scarab/a.yaml", "@hourly", 5)
            }],
        )
        .await
        .unwrap();
    // `a` keeps its schedule (and its slot, though 999 is offered), `b` is
    // gone, `c` is new.
    store
        .replace_project_schedules(
            "acme",
            "app",
            "sha2",
            &[
                row(".scarab/a.yaml", "@hourly", 999),
                row(".scarab/c.yaml", "@weekly", 300),
            ],
        )
        .await
        .unwrap();
    let rows = store.project_schedules("acme", "app").await.unwrap();
    let got: Vec<(&str, &str, i64, &str)> = rows
        .iter()
        .map(|r| {
            (
                r.pipeline.as_str(),
                r.schedule.as_str(),
                r.next_fire_at.0,
                r.source_sha.as_str(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (".scarab/a.yaml", "@hourly", 100, "sha2"),
            (".scarab/c.yaml", "@weekly", 300, "sha2"),
        ]
    );
    let due = store.due_schedules(Timestamp(150), None, 10).await.unwrap();
    let due: Vec<(&str, i64)> = due
        .iter()
        .map(|r| (r.project.as_str(), r.next_fire_at.0))
        .collect();
    assert_eq!(due, [("other", 5), ("app", 100)]);
    // An empty replace drops the project's index.
    store
        .replace_project_schedules("acme", "app", "sha3", &[])
        .await
        .unwrap();
    assert!(store
        .project_schedules("acme", "app")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .project_schedules("acme", "other")
            .await
            .unwrap()
            .len(),
        1
    );
}

fn fire(pipeline: &str, schedule: &str, slot: i64) -> ScheduleFire {
    ScheduleFire {
        org: "acme".into(),
        project: "app".into(),
        pipeline: pipeline.into(),
        schedule: schedule.into(),
        slot_at: Timestamp(slot),
        fired_at: Timestamp(slot),
        outcome: "pending".into(),
        run_id: None,
    }
}

async fn claim(store: &dyn ScheduleStore) {
    assert!(store
        .claim_fire(&fire(".scarab/a.yaml", "@hourly", 60_000))
        .await
        .unwrap());
    // The same Pipeline and minute through another schedule: already claimed.
    assert!(!store
        .claim_fire(&fire(".scarab/a.yaml", "0 * * * *", 60_000))
        .await
        .unwrap());
    assert!(store
        .claim_fire(&fire(".scarab/a.yaml", "@hourly", 120_000))
        .await
        .unwrap());
    assert!(store
        .claim_fire(&fire(".scarab/b.yaml", "@hourly", 60_000))
        .await
        .unwrap());
    store
        .finish_fire(
            "acme",
            "app",
            ".scarab/a.yaml",
            Timestamp(120_000),
            "run",
            Some("r-1"),
        )
        .await
        .unwrap();
    let last = store.last_fires("acme", "app").await.unwrap();
    let got: Vec<(&str, i64, &str, Option<&str>)> = last
        .iter()
        .map(|f| {
            (
                f.pipeline.as_str(),
                f.slot_at.0,
                f.outcome.as_str(),
                f.run_id.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (".scarab/a.yaml", 120_000, "run", Some("r-1")),
            (".scarab/b.yaml", 60_000, "pending", None),
        ]
    );
    assert_eq!(store.prune_fires(Timestamp(100_000)).await.unwrap(), 2);
    assert_eq!(store.last_fires("acme", "app").await.unwrap().len(), 1);
}

async fn advance(store: &dyn ScheduleStore) {
    let r = row(".scarab/a.yaml", "@hourly", 100);
    store
        .replace_project_schedules("acme", "app", "s", std::slice::from_ref(&r))
        .await
        .unwrap();
    assert!(store
        .advance_schedule(&r, Timestamp(100), Timestamp(200))
        .await
        .unwrap());
    // A second writer that read 100 finds it moved.
    assert!(!store
        .advance_schedule(&r, Timestamp(100), Timestamp(300))
        .await
        .unwrap());
    let rows = store.project_schedules("acme", "app").await.unwrap();
    assert_eq!(rows[0].next_fire_at, Timestamp(200));
    // A deleted row cannot be advanced.
    store
        .replace_project_schedules("acme", "app", "s", &[])
        .await
        .unwrap();
    assert!(!store
        .advance_schedule(&r, Timestamp(200), Timestamp(300))
        .await
        .unwrap());
}

/// git-bug 237ab13: a caller pages through every due row by passing the last
/// row it read; rows that tie on `next_fire_at` are neither skipped nor read
/// twice across a page boundary.
async fn due_pages(store: &dyn ScheduleStore) {
    store
        .replace_project_schedules(
            "acme",
            "app",
            "sha1",
            &[
                row(".scarab/a.yaml", "@hourly", 100),
                row(".scarab/b.yaml", "@hourly", 100),
                row(".scarab/c.yaml", "@hourly", 100),
                row(".scarab/d.yaml", "@daily", 200),
                row(".scarab/e.yaml", "@weekly", 900),
            ],
        )
        .await
        .unwrap();
    let mut read: Vec<String> = Vec::new();
    let mut last: Option<ScheduleEntry> = None;
    loop {
        let page = store
            .due_schedules(Timestamp(500), last.as_ref(), 2)
            .await
            .unwrap();
        assert!(page.len() <= 2);
        read.extend(page.iter().map(|r| r.pipeline.clone()));
        match page.last() {
            Some(r) if page.len() == 2 => last = Some(r.clone()),
            _ => break,
        }
    }
    assert_eq!(
        read,
        [
            ".scarab/a.yaml",
            ".scarab/b.yaml",
            ".scarab/c.yaml",
            ".scarab/d.yaml",
        ]
    );
}
