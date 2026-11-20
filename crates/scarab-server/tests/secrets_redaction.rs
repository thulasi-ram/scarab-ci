//! Scoped-secret injection + log redaction acceptance (ADR-0014, 0013): a step's
//! secret is resolved and injected as env, and its value never appears in the
//! stored or streamed logs. Hermetic — FakeSecrets + the in-memory log pipeline.

use std::sync::Arc;

use scarab_engine::{AttemptId, MountedValue, MountedValueKind, RunId, StepId, Timestamp};
use scarab_secrets::SecretScope;
use scarab_server::{resolve_step_secrets, LogService};
use scarab_testkit::{FakeSecrets, InMemoryDb, InMemoryObjectStore};

fn secret_values(env: &[(String, Vec<u8>)]) -> Vec<MountedValue> {
    env.iter()
        .map(|(_, v)| MountedValue {
            kind: MountedValueKind::Secret,
            value: v.clone(),
            repo: None,
            minted: None,
        })
        .collect()
}

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

    // Resolve the step's secret; the launch records it in the unit's value
    // ledger.
    let (run, step, attempt) = (
        RunId("run-1".into()),
        StepId("build".into()),
        AttemptId("a1".into()),
    );
    let env = resolve_step_secrets(&secrets, &scope(), &["TOKEN".to_string()], false)
        .await
        .unwrap();
    assert_eq!(env, vec![("TOKEN".to_string(), b"sup3r-s3cret".to_vec())]);
    let unit = scarab_engine::LogStreamId::new(&run, &step, &attempt);
    logs.record_launch_values(&unit, Some("h1"), secret_values(&env), Timestamp(0))
        .await
        .unwrap();

    // The step echoes its secret to stdout.
    logs.append(
        &run,
        &step,
        &attempt,
        b"connecting with TOKEN=sup3r-s3cret ok\n",
    )
    .await
    .unwrap();

    // Stored (replayed) log is redacted too.
    let stored = logs.read_all(&run, &step, &attempt).await.unwrap();
    let stored = String::from_utf8_lossy(&stored);
    assert!(
        !stored.contains("sup3r-s3cret"),
        "stored log leaked the secret: {stored}"
    );
    assert!(stored.contains("TOKEN=***"));
}

/// A backend diagnostic is redacted too (ADR-0068).
///
/// This is the channel the log pipeline does NOT cover. A failure cause and an
/// infra condition quote the backend verbatim — and a backend quotes registry
/// URLs and auth-failure bodies — then land in Postgres
/// (`attempts.failure_detail`, the event log) and are served on the API, none of
/// which passes through `LogService::append`. Before the k8s messages started
/// travelling in `cause`, nothing but a drain error ever did, so this was
/// harmless; now it is the one uncensored path out of a step.
#[tokio::test]
async fn a_backend_diagnostic_is_redacted_like_a_log_chunk() {
    let logs = LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        Arc::new(InMemoryDb::new()),
    );
    let unit = scarab_engine::LogStreamId::new(
        &RunId("run-1".into()),
        &StepId("build".into()),
        &AttemptId("a1".into()),
    );
    logs.record_launch_values(
        &unit,
        Some("h1"),
        secret_values(&[("T".into(), b"sup3r-s3cret".to_vec())]),
        Timestamp(0),
    )
    .await
    .unwrap();

    // The shape a registry rejection actually takes: the credential is inside
    // the message the kubelet reports.
    let cause = "ErrImagePull: failed to pull \
                 https://user:sup3r-s3cret@registry.example.com/acme/builder:v9";
    let redacted = logs.redact_text_for_handle("h1", cause).await;

    assert!(
        !redacted.contains("sup3r-s3cret"),
        "the diagnostic leaked the secret: {redacted}"
    );
    assert!(
        redacted.contains("registry.example.com/acme/builder:v9"),
        "redaction must keep the part that makes the diagnosis useful: {redacted}"
    );
}
