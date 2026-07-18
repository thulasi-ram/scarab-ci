//! Scoped-secret injection + log redaction acceptance (ADR-0014, 0013): a step's
//! secret is resolved and injected as env, and its value never appears in the
//! stored or streamed logs. Hermetic — FakeSecrets + the in-memory log pipeline.

use std::sync::Arc;

use scarab_engine::{AttemptId, RunId, StepId};
use scarab_secrets::SecretScope;
use scarab_server::{resolve_step_secrets, LogService};
use scarab_testkit::{FakeSecrets, InMemoryDb, InMemoryObjectStore};

fn scope() -> SecretScope {
    SecretScope::Repo {
        org: "acme".into(),
        repo: "app".into(),
    }
}

#[tokio::test]
async fn injected_secret_is_redacted_from_stored_and_streamed_logs() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let logs = LogService::new(Arc::new(InMemoryObjectStore::new()), db);
    let secrets = FakeSecrets::new().with_secret(&scope(), "TOKEN", b"sup3r-s3cret");

    // Resolve the step's secret: it comes back as env AND is registered with the
    // redactor.
    let env = resolve_step_secrets(&secrets, &logs, &scope(), &["TOKEN".to_string()], false)
        .await
        .unwrap();
    assert_eq!(env, vec![("TOKEN".to_string(), "sup3r-s3cret".to_string())]);

    let (run, step, attempt) = (
        RunId("run-1".into()),
        StepId("build".into()),
        AttemptId("a1".into()),
    );

    // Subscribe to the live stream before the step "logs" the secret.
    let mut live = logs.subscribe(&run, &step, &attempt);

    // The step echoes its secret to stdout.
    logs.append(
        &run,
        &step,
        &attempt,
        b"connecting with TOKEN=sup3r-s3cret ok\n",
    )
    .await
    .unwrap();

    // Streamed (live) chunk is redacted.
    let streamed = live.try_recv().expect("a live chunk");
    let streamed = String::from_utf8_lossy(&streamed);
    assert!(
        !streamed.contains("sup3r-s3cret"),
        "streamed log leaked the secret: {streamed}"
    );
    assert!(streamed.contains("TOKEN=***"));

    // Stored (replayed) log is redacted too.
    let stored = logs.read_all(&run, &step, &attempt).await.unwrap();
    let stored = String::from_utf8_lossy(&stored);
    assert!(
        !stored.contains("sup3r-s3cret"),
        "stored log leaked the secret: {stored}"
    );
    assert!(stored.contains("TOKEN=***"));
}
