//! The step Pod sweep (git-bug 986542a) against real Postgres: the Run facts
//! come from `Db::run_settlements` over a migrated schema, and the step
//! namespace is an in-memory stand-in for the Kubernetes API (the one true
//! external here). Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Clock, Db, RunId, RunStatus, Timestamp};
use scarab_executor_k8s::{ManagedKind, ManagedObject, ManagedPage};
use scarab_server::step_pods::{sweep_step_pods, StepPodCluster, DELETE_CAP};
use scarab_testkit::FakeClock;

const GRACE_MS: i64 = 60 * 60 * 1000;

/// The step namespace: objects in creation order, paged by index.
#[derive(Default)]
struct FakeNamespace {
    objects: Mutex<Vec<ManagedObject>>,
}

impl FakeNamespace {
    fn add(&self, kind: ManagedKind, name: &str, run: Option<&str>, managed: bool) {
        let mut labels = BTreeMap::new();
        if managed {
            labels.insert(
                "app.kubernetes.io/managed-by".to_string(),
                "scarab".to_string(),
            );
        }
        if let Some(run) = run {
            labels.insert("scarab.io/run".to_string(), run.to_string());
        }
        self.objects.lock().unwrap().push(ManagedObject {
            kind,
            name: name.to_string(),
            labels,
            active: false,
        });
    }

    fn names(&self) -> Vec<String> {
        self.objects
            .lock()
            .unwrap()
            .iter()
            .map(|o| o.name.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl StepPodCluster for FakeNamespace {
    async fn list(
        &self,
        kind: ManagedKind,
        continue_token: Option<String>,
        limit: u32,
    ) -> Result<ManagedPage, String> {
        // Like the real list: only objects with the managed-by label.
        let all: Vec<ManagedObject> = self
            .objects
            .lock()
            .unwrap()
            .iter()
            .filter(|o| o.kind == kind)
            .filter(|o| o.labels.contains_key("app.kubernetes.io/managed-by"))
            .cloned()
            .collect();
        let start: usize = continue_token.map(|t| t.parse().unwrap()).unwrap_or(0);
        let end = (start + limit as usize).min(all.len());
        Ok(ManagedPage {
            objects: all[start..end].to_vec(),
            continue_token: (end < all.len()).then(|| end.to_string()),
        })
    }

    async fn delete(&self, kind: ManagedKind, name: &str) -> Result<(), String> {
        self.objects
            .lock()
            .unwrap()
            .retain(|o| !(o.kind == kind && o.name == name));
        Ok(())
    }
}

async fn seed_run(db: &PostgresDb, id: &str, path: &[(RunStatus, RunStatus)]) {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    for (from, to) in path {
        db.record_transition(&run, *from, *to).await.unwrap();
    }
}

fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn deletes_only_the_leftovers_of_runs_this_database_finished_past_the_grace() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();

    use RunStatus::*;
    seed_run(
        &pg,
        "r-done-old",
        &[(Pending, Running), (Running, Succeeded)],
    )
    .await;
    seed_run(
        &pg,
        "r-dead-old",
        &[(Pending, Running), (Running, DeadLettered)],
    )
    .await;
    seed_run(&pg, "r-cancel-old", &[(Pending, Cancelled)]).await;
    seed_run(
        &pg,
        "r-done-fresh",
        &[(Pending, Running), (Running, Failed)],
    )
    .await;
    seed_run(
        &pg,
        "r-suspended-old",
        &[(Pending, Running), (Running, Suspended)],
    )
    .await;
    // The transitions stamp updated_at with the database's "now"; age the
    // runs that finished (or stalled) long ago.
    sqlx::query(
        "UPDATE runs SET updated_at = 0
         WHERE id IN ('r-done-old', 'r-dead-old', 'r-cancel-old', 'r-suspended-old')",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();

    let ns = Arc::new(FakeNamespace::default());
    // More Pods of one finished Run than a pass may delete, across two pages.
    let many = DELETE_CAP + 60;
    for i in 0..many {
        ns.add(
            ManagedKind::Pod,
            &format!("done-{i}"),
            Some("r-done-old"),
            true,
        );
    }
    ns.add(ManagedKind::Pod, "dead", Some("r-dead-old"), true);
    ns.add(ManagedKind::Pod, "cancel", Some("r-cancel-old"), true);
    ns.add(ManagedKind::Service, "db", Some("r-cancel-old"), true);
    ns.add(
        ManagedKind::NetworkPolicy,
        "svc-netpol",
        Some("r-cancel-old"),
        true,
    );
    ns.add(ManagedKind::Pod, "fresh", Some("r-done-fresh"), true);
    ns.add(ManagedKind::Pod, "suspended", Some("r-suspended-old"), true);
    ns.add(
        ManagedKind::Pod,
        "foreign",
        Some("r-of-another-install"),
        true,
    );
    ns.add(ManagedKind::Pod, "unmanaged", Some("r-done-old"), false);

    let db: Arc<dyn Db> = Arc::new(pg);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(wall_clock_ms()));

    // First pass: stops at the cap.
    let report = sweep_step_pods(&db, ns.as_ref(), &clock, "me", GRACE_MS)
        .await
        .unwrap();
    assert_eq!(report.deleted.len(), DELETE_CAP);

    // Second pass: the rest.
    let report = sweep_step_pods(&db, ns.as_ref(), &clock, "me", GRACE_MS)
        .await
        .unwrap();
    assert_eq!(report.deleted.len(), 60 + 4);

    let mut left = ns.names();
    left.sort();
    assert_eq!(left, ["foreign", "fresh", "suspended", "unmanaged"]);

    // A replica that does not hold the retention lease deletes nothing.
    ns.add(ManagedKind::Pod, "done-late", Some("r-done-old"), true);
    let report = sweep_step_pods(&db, ns.as_ref(), &clock, "other-replica", GRACE_MS)
        .await
        .unwrap();
    assert!(report.deleted.is_empty());
    assert!(ns.names().contains(&"done-late".to_string()));
}
