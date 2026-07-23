//! ADR-0058 slice 2 — the durable Run-scoped shared-service resource, exercised
//! in-process against the `InMemoryDb` fake (no Postgres server). Proves the
//! `{run, take, name}` keying: idempotent birth, status/handle transitions, and
//! a fresh instance per Take that never collides with the prior Take's row.

use scarab_engine::{Db, RunId, RunService, ServiceStatus, Timestamp};
use scarab_testkit::InMemoryDb;

fn svc<'a>(list: &'a [RunService], take: i64, name: &str) -> &'a RunService {
    list.iter()
        .find(|s| s.take == take && s.name == name)
        .unwrap_or_else(|| panic!("no service {name} at take {take} in {list:?}"))
}

#[tokio::test]
async fn shared_service_is_born_keyed_by_run_take_name_and_is_idempotent() {
    let db = InMemoryDb::new();
    let run = RunId("r1".into());

    // Born at Take 1 in `starting`.
    db.create_run_service(&run, 1, "db", Timestamp(100))
        .await
        .unwrap();
    // A re-tick / crash resume must NOT provision a second instance.
    db.create_run_service(&run, 1, "db", Timestamp(999))
        .await
        .unwrap();

    let services = db.run_services(&run).await.unwrap();
    assert_eq!(services.len(), 1, "idempotent on {{run, take, name}}");
    let s = svc(&services, 1, "db");
    assert_eq!(s.status, ServiceStatus::Starting);
    assert_eq!(s.handle, None);
    assert_eq!(s.created_at, Timestamp(100), "first birth time is kept");
}

#[tokio::test]
async fn status_and_handle_transitions_are_recorded() {
    let db = InMemoryDb::new();
    let run = RunId("r1".into());
    db.create_run_service(&run, 1, "db", Timestamp(0))
        .await
        .unwrap();

    // starting → ready, recording the launch handle.
    db.set_run_service(&run, 1, "db", ServiceStatus::Ready, Some("pod://db"))
        .await
        .unwrap();
    let s = svc(&db.run_services(&run).await.unwrap(), 1, "db").clone();
    assert_eq!(s.status, ServiceStatus::Ready);
    assert_eq!(s.handle.as_deref(), Some("pod://db"));

    // A later status-only update preserves the already-recorded handle.
    db.set_run_service(&run, 1, "db", ServiceStatus::TornDown, None)
        .await
        .unwrap();
    let s = svc(&db.run_services(&run).await.unwrap(), 1, "db").clone();
    assert_eq!(s.status, ServiceStatus::TornDown);
    assert_eq!(s.handle.as_deref(), Some("pod://db"), "handle preserved");
    assert!(s.status.is_terminal());
}

#[tokio::test]
async fn a_rerun_take_gets_a_fresh_instance_beside_the_prior_take() {
    let db = InMemoryDb::new();
    let run = RunId("r1".into());

    // Take 1 ran and was torn down.
    db.create_run_service(&run, 1, "db", Timestamp(0))
        .await
        .unwrap();
    db.set_run_service(&run, 1, "db", ServiceStatus::TornDown, Some("pod://t1"))
        .await
        .unwrap();

    // A Rerun opens Take 2 → a fresh instance, distinct row, no shared state.
    db.create_run_service(&run, 2, "db", Timestamp(500))
        .await
        .unwrap();

    let services = db.run_services(&run).await.unwrap();
    assert_eq!(services.len(), 2, "one row per {{take, name}}");
    assert_eq!(svc(&services, 1, "db").status, ServiceStatus::TornDown);
    let take2 = svc(&services, 2, "db");
    assert_eq!(
        take2.status,
        ServiceStatus::Starting,
        "fresh, not the old row"
    );
    assert_eq!(take2.handle, None);
    // The engine's "current take" is the max take across the run's services.
    let current = services.iter().map(|s| s.take).max().unwrap();
    assert_eq!(current, 2);
}
