//! Observability surface (ADR-0053), hermetic: /metrics exposes the run/
//! outbox gauges in Prometheus text form, /readyz reflects dependency health,
//! and every response carries a correlating x-request-id.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{RunId, RunStatus};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

fn app(db: Arc<InMemoryDb>) -> axum::Router {
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));
    router(AppState::new(db, Arc::new(FakeClock::new(1_000)), logs).with_artifact_store(store))
}

async fn text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn metrics_expose_run_and_outbox_gauges() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    db.seed_run(&RunId("r1".into()), RunStatus::Running);
    db.seed_run(&RunId("r2".into()), RunStatus::Succeeded);
    db.seed_run(&RunId("r3".into()), RunStatus::Succeeded);

    let resp = app(db)
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; version=0.0.4")
    );
    let body = text(resp).await;
    assert!(
        body.contains(r#"scarab_runs{status="running"} 1"#),
        "{body}"
    );
    assert!(
        body.contains(r#"scarab_runs{status="succeeded"} 2"#),
        "{body}"
    );
    assert!(body.contains("scarab_outbox_depth 0"), "{body}");
    // ba921db: forge delivery failures are counted, not just logged — the series
    // is exposed (at 0 in a fresh process) so an alert can be written against it
    // before anything has broken.
    assert!(
        body.contains("# TYPE scarab_forge_status_failures_total counter"),
        "{body}"
    );
    assert!(
        body.contains("scarab_forge_status_failures_total 0"),
        "{body}"
    );
    assert!(
        body.contains("scarab_forge_status_dead_lettered_total 0"),
        "{body}"
    );
}

#[tokio::test]
async fn readyz_is_ok_and_every_response_carries_a_request_id() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = app(db);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().contains_key("x-request-id"),
        "request id stamped"
    );

    // An inbound id is honored (correlation across services).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("x-request-id", "corr-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("corr-123")
    );
}

/// A request's span is current only while the request is polled. With a
/// `span.enter()` guard held across the handler's await, the span stayed
/// the thread's current span after the request yielded. A request that then
/// finished on another worker left its closed span on the first worker's
/// span stack, and the next span made there panicked in tracing-subscriber's
/// registry ("tried to clone a span that already closed"), failing that
/// request (the e2e plugin-argocd pass on PR #205). Kills: the guard held
/// across the await.
#[test]
fn a_waiting_request_leaves_no_span_entered_on_its_thread() {
    use std::future::Future;
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};

    let _default = tracing::subscriber::set_default(tracing_subscriber::registry());
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let released = Arc::new(Mutex::new(Some(released)));
    let seen = Arc::new(Mutex::new(None::<&'static str>));
    let handler = {
        let (released, seen) = (released.clone(), seen.clone());
        move || async move {
            *seen.lock().unwrap() = tracing::Span::current().metadata().map(|m| m.name());
            let rx = released.lock().unwrap().take().expect("one request");
            let _ = rx.await;
            "ok"
        }
    };
    let app = axum::Router::new()
        .route("/wait", axum::routing::get(handler))
        .layer(axum::middleware::from_fn(
            scarab_server::request_id_middleware,
        ));

    let mut call =
        Box::pin(app.oneshot(Request::builder().uri("/wait").body(Body::empty()).unwrap()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        call.as_mut().poll(&mut cx).is_pending(),
        "the handler waits"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        Some("request"),
        "the handler runs in the request span"
    );
    assert_eq!(
        tracing::Span::current().metadata().map(|m| m.name()),
        None,
        "the waiting request's span is still entered on this thread"
    );

    release.send(()).unwrap();
    let resp = match call.as_mut().poll(&mut cx) {
        Poll::Ready(r) => r.unwrap(),
        Poll::Pending => panic!("the request finishes once released"),
    };
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("x-request-id"));
    assert!(tracing::Span::current().is_none());
}
