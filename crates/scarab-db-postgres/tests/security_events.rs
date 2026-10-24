//! ADR-0073 §7: the OrgSecurityEventLog against *real* Postgres — append-only
//! in the schema itself, and the three views (Org, own, install-wide). Skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{
    security_event_kind as kind, Binding, BindingOrigin, RbacStore, Role, Scope, SecurityEvent,
    SecurityEventFilter, SecurityEventLog,
};

fn event(id: &str, org: Option<&str>, kind: &str, subject: &str) -> SecurityEvent {
    SecurityEvent {
        seq: 0,
        id: id.into(),
        org: org.map(str::to_string),
        at: 1_000,
        kind: kind.into(),
        subject: subject.into(),
        caller: "session:cookie".into(),
        op: Some("secret.put".into()),
        target: Some("org acme".into()),
        confirmation_id: Some("conf-1".into()),
        request_id: None,
        passkey_id: Some("pk-1".into()),
        session_hash: Some("sess-h".into()),
        detail: serde_json::json!({"status": 201}),
    }
}

fn ids(events: &[SecurityEvent]) -> Vec<&str> {
    events.iter().map(|e| e.id.as_str()).collect()
}

fn filter() -> SecurityEventFilter {
    SecurityEventFilter {
        limit: 100,
        ..Default::default()
    }
}

#[tokio::test]
async fn the_log_refuses_every_update_and_delete() {
    let Some(tdb) = fresh_db().await else { return };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let e = event("e1", Some("acme"), kind::STEP_UP_CONFIRMED, "amy");
    let seq = db.append(&e).await.unwrap();
    let got = SecurityEventLog::list(&db, &filter()).await.unwrap();
    assert_eq!(got, vec![SecurityEvent { seq, ..e }], "round-trips whole");

    for statement in [
        "UPDATE org_security_events SET kind = 'nothing.happened'",
        "UPDATE org_security_events SET detail = '{}' WHERE id = 'e1'",
        "DELETE FROM org_security_events WHERE id = 'e1'",
        "DELETE FROM org_security_events",
    ] {
        let err = sqlx::query(statement)
            .execute(&tdb.pool)
            .await
            .expect_err(statement);
        assert!(
            err.to_string().contains("append-only"),
            "{statement}: {err}"
        );
    }
    assert_eq!(
        SecurityEventLog::list(&db, &filter()).await.unwrap()[0].kind,
        kind::STEP_UP_CONFIRMED,
        "nothing changed"
    );

    // An id is appended once.
    assert!(db
        .append(&event("e1", None, kind::STEP_UP_REFUSED, "amy"))
        .await
        .is_err());

    tdb.cleanup().await;
}

/// The Org log is the Org's own events plus Org-less events (passkeys,
/// confirmations) about its current members — never another Org's, never a
/// stranger's, never a revoked member's.
#[tokio::test]
async fn an_org_sees_its_events_and_its_members_passkey_events_only() {
    let Some(tdb) = fresh_db().await else { return };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let bind = |subject: &str, scope: Scope| Binding {
        subject: subject.into(),
        scope,
        role: Role::Member,
    };
    // amy: member of acme. erin: a Project binding in acme. bob: globex only.
    // dave: was in acme, revoked (a tombstone).
    db.grant(
        &bind("amy", Scope::Org("acme".into())),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    db.grant(
        &bind(
            "erin",
            Scope::Project {
                org: "acme".into(),
                name: "web".into(),
            },
        ),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    db.grant(
        &bind("bob", Scope::Org("globex".into())),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    db.grant(
        &bind("dave", Scope::Org("acme".into())),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    db.revoke("dave", &Scope::Org("acme".into())).await.unwrap();

    for e in [
        event("acme-op", Some("acme"), kind::OP_AUTHORISED, "amy"),
        event("globex-op", Some("globex"), kind::OP_AUTHORISED, "bob"),
        event("amy-enrolled", None, kind::PASSKEY_ENROLLED, "amy"),
        event("erin-confirmed", None, kind::STEP_UP_CONFIRMED, "erin"),
        event("bob-enrolled", None, kind::PASSKEY_ENROLLED, "bob"),
        event("cat-enrolled", None, kind::PASSKEY_ENROLLED, "cat"),
        event("dave-enrolled", None, kind::PASSKEY_ENROLLED, "dave"),
        // An acme event about a non-member is still acme's.
        event("acme-refused", Some("acme"), kind::STEP_UP_REFUSED, "cat"),
    ] {
        db.append(&e).await.unwrap();
    }

    let acme = SecurityEventLog::list(
        &db,
        &SecurityEventFilter {
            org: Some("acme".into()),
            ..filter()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        ids(&acme),
        vec!["acme-refused", "erin-confirmed", "amy-enrolled", "acme-op"],
        "newest first"
    );

    let globex = SecurityEventLog::list(
        &db,
        &SecurityEventFilter {
            org: Some("globex".into()),
            ..filter()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&globex), vec!["bob-enrolled", "globex-op"]);

    // The own log: everything about one subject, Org or not.
    let amy = SecurityEventLog::list(
        &db,
        &SecurityEventFilter {
            subject: Some("amy".into()),
            ..filter()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&amy), vec!["amy-enrolled", "acme-op"]);

    // Install-wide: all of it.
    assert_eq!(
        SecurityEventLog::list(&db, &filter()).await.unwrap().len(),
        8
    );

    // Kinds, keyset pagination and the limit narrow any view.
    let enrolled = SecurityEventLog::list(
        &db,
        &SecurityEventFilter {
            kinds: vec![kind::PASSKEY_ENROLLED.into()],
            ..filter()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        ids(&enrolled),
        vec![
            "dave-enrolled",
            "cat-enrolled",
            "bob-enrolled",
            "amy-enrolled"
        ]
    );
    let page = SecurityEventLog::list(
        &db,
        &SecurityEventFilter {
            org: Some("acme".into()),
            before_seq: Some(acme[1].seq),
            limit: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&page), vec!["amy-enrolled"]);

    tdb.cleanup().await;
}
