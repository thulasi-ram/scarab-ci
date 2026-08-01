//! Regression: the executor decorators must FORWARD the ADR-0058 shared-service
//! methods to their inner executor. Without an explicit forward they inherit the
//! trait's DEFAULT `launch_service`, which REJECTS shared services — so a
//! secrets-wired deployment would refuse every service even on the k8s backend
//! (fixed in 82f5840). The Slice-4 tests missed this by wrapping a bare
//! `FakeExecutor` and never exercising the decorator.
//!
//! Same hazard, evidence flavour (ADR-0061 s8 / ADR-0064 s2): the decorators
//! must forward `output_identity` and `output_durability`, or every real
//! (secrets-wired) deployment silently records NULL evidence. "The
//! decorators" is PLURAL and this file honours that: production nests
//! `clone(secret(k8s))` (main.rs), so `CloneEnrichingExecutor` — the
//! OUTERMOST layer, with its own forwards in clone_executor.rs — is driven
//! here in that exact nesting, not just the secret layer alone.

use std::sync::Arc;

use scarab_engine::ports::Executor;
use scarab_engine::RunId;
use scarab_server::clone_executor::CloneEnrichingExecutor;
use scarab_server::{LogService, SecretInjectingExecutor};
use scarab_testkit::{FakeExecutor, FakeForge, FakeSecrets, InMemoryDb, InMemoryObjectStore};

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

/// The decorator must FORWARD `output_identity` and `output_durability` to the
/// executor it wraps. `output_identity` really was swallowed until ADR-0064 s2:
/// the defaulted trait method shadowed the inner impl, so a secrets-wired
/// deployment recorded no identity, restart compared roots, and
/// skip-if-unchanged could never fire (the 945b1f4 failure shape, reintroduced
/// by the wrapper). `output_durability` is REQUIRED on the trait for exactly
/// this reason — but required only forces *an* impl, not a forwarding one.
/// Mutations killed: reverting either forward to an `Ok(None)` body — the
/// wrapper then answers `None` while the inner demonstrably answers `Some`.
#[tokio::test]
async fn secret_injecting_executor_forwards_output_identity_and_durability() {
    let inner = Arc::new(FakeExecutor::new());
    let db = Arc::new(InMemoryDb::new());
    let secrets = Arc::new(FakeSecrets::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));

    inner.set_output("s1", "root-hash");
    inner.set_output_identity("s1", "identity-hash");
    inner.set_output_durability("s1", "object");
    let decorator = SecretInjectingExecutor::new(inner.clone(), db, secrets, logs);

    // The FakeExecutor's fence-derived handle shape: fake://{run}/{step}/{attempt}.
    let handle = scarab_engine::ports::ExecHandle("fake://r1/s1/a1".into());
    assert_eq!(
        decorator.output_identity(&handle).await.unwrap().as_deref(),
        Some("identity-hash"),
        "the content identity (ADR-0061 s8) must pass through the decorator"
    );
    assert_eq!(
        decorator
            .output_durability(&handle)
            .await
            .unwrap()
            .as_deref(),
        Some("object"),
        "the durability stamp (ADR-0064 s2) must pass through the decorator"
    );
    // And the address itself, completing the evidence trio.
    assert_eq!(
        decorator.output(&handle).await.unwrap().as_deref(),
        Some("root-hash")
    );
}

/// The FULL production nesting (main.rs:535/550 — `clone(secret(k8s))`):
/// `CloneEnrichingExecutor` is the OUTERMOST decorator, with its own
/// `output_identity` / `output_durability` overrides (clone_executor.rs) that
/// the defaulted trait method would silently shadow if either were deleted.
/// The secret-layer test above proves nothing about the layer production
/// calls FIRST, so this drives the evidence trio through both layers.
/// Mutations killed: delete either override in clone_executor.rs (it falls to
/// the defaulted method) or hardcode it to `Ok(None)` — the stack then
/// answers `None` while the inner stub demonstrably answers `Some`.
#[tokio::test]
async fn the_production_decorator_stack_forwards_identity_and_durability() {
    let inner = Arc::new(FakeExecutor::new());
    let db = Arc::new(InMemoryDb::new());
    let secrets = Arc::new(FakeSecrets::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));

    inner.set_output("s1", "root-hash");
    inner.set_output_identity("s1", "identity-hash");
    inner.set_output_durability("s1", "object");

    let secret_layer: Arc<dyn Executor> = Arc::new(SecretInjectingExecutor::new(
        inner.clone(),
        db,
        secrets,
        logs,
    ));
    let stack = CloneEnrichingExecutor::new(
        secret_layer,
        Arc::new(InMemoryDb::new()), // connection registry: empty is fine here
        Arc::new(FakeForge::new()),
    );

    let handle = scarab_engine::ports::ExecHandle("fake://r1/s1/a1".into());
    assert_eq!(
        stack.output_identity(&handle).await.unwrap().as_deref(),
        Some("identity-hash"),
        "the content identity (ADR-0061 s8) must survive BOTH decorator layers"
    );
    assert_eq!(
        stack
            .output_durability(&handle)
            .await
            .unwrap()
            .as_deref(),
        Some("object"),
        "the durability stamp (ADR-0064 s2) must survive BOTH decorator layers"
    );
    // And the address itself, completing the evidence trio through the stack.
    assert_eq!(
        stack.output(&handle).await.unwrap().as_deref(),
        Some("root-hash")
    );
}
