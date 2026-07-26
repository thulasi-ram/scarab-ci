//! Regression: the executor decorators must FORWARD the ADR-0058 shared-service
//! methods to their inner executor. Without an explicit forward they inherit the
//! trait's DEFAULT `launch_service`, which REJECTS shared services — so a
//! secrets-wired deployment would refuse every service even on the k8s backend
//! (fixed in 82f5840). The Slice-4 tests missed this by wrapping a bare
//! `FakeExecutor` and never exercising the decorator.

use std::sync::Arc;

use scarab_engine::ports::Executor;
use scarab_engine::RunId;
use scarab_server::{LogService, SecretInjectingExecutor};
use scarab_testkit::{FakeExecutor, FakeSecrets, InMemoryDb, InMemoryObjectStore};

#[tokio::test]
async fn secret_injecting_executor_forwards_launch_service_to_inner() {
    let inner = Arc::new(FakeExecutor::new());
    let db = Arc::new(InMemoryDb::new());
    let secrets = Arc::new(FakeSecrets::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));

    let decorator = SecretInjectingExecutor::new(inner.clone(), db, secrets, logs);

    let run = RunId("run-1".into());
    let spec = scarab_pipeline::ServiceSpec {
        image: "postgres:16".into(),
        ..Default::default()
    };

    // Must NOT return the default-impl reject error — it must forward.
    let handle = decorator
        .launch_service(&run, 0, "db", &spec)
        .await
        .expect("launch_service must forward to inner, not hit the reject default");

    // And the inner FakeExecutor must have actually recorded the launch.
    assert_eq!(
        inner.launched_services(),
        vec![handle.0],
        "inner executor should have recorded exactly the launched service handle",
    );
}
