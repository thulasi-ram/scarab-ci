//! ADR-0073 step-up stores against *real* Postgres: passkeys (the counter
//! CAS and clone flag), single-use challenges, window and action
//! confirmations, and StepUpConfirmationRequests (exactly-once execution,
//! expiry, the sealed body cleared when settled). Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::time::Duration;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{
    BeginExecution, ChallengePurpose, ChallengeStore, ConfirmationKind, IdentityError, Passkey,
    PasskeyStore, PasskeyUse, PendingCaps, RaiseRequest, RequestState, Role, StepUpConfirmation,
    StepUpRequest, StepUpRequester, StepUpStore, WebauthnChallenge, STEP_UP_WINDOW_MS,
};

fn passkey(id: &str, principal: &str, credential: &[u8], count: u32) -> Passkey {
    Passkey {
        id: id.into(),
        principal: principal.into(),
        credential_id: credential.to_vec(),
        public_key_cose: vec![0xa5, 1, 2],
        alg: -7,
        sign_count: count,
        label: format!("{id} label"),
        transports: vec!["internal".into(), "hybrid".into()],
        aaguid: Some(vec![7; 16]),
        backup_eligible: true,
        backup_state: false,
        created_at: 100,
        created_session_hash: "sess-enroll".into(),
        last_used_at: None,
        clone_suspected_at: None,
    }
}

fn challenge(id: &str, session: &str, expires_at: i64) -> WebauthnChallenge {
    WebauthnChallenge {
        id: id.into(),
        principal: "amy".into(),
        session_hash: session.into(),
        purpose: ChallengePurpose::Action,
        nonce: vec![1; 32],
        action_digest: Some(vec![2; 32]),
        request_id: None,
        label: None,
        summary: Some(serde_json::json!({"operation": "Write a secret"})),
        created_at: 0,
        expires_at,
        consumed_at: None,
    }
}

fn confirmation(
    id: &str,
    session: &str,
    kind: ConfirmationKind,
    digest: Option<&[u8]>,
    confirmed_at: i64,
) -> StepUpConfirmation {
    StepUpConfirmation {
        id: id.into(),
        principal: "amy".into(),
        session_hash: session.into(),
        passkey_id: "pk1".into(),
        kind,
        action_digest: digest.map(<[u8]>::to_vec),
        request_id: None,
        confirmed_at,
        expires_at: confirmed_at + STEP_UP_WINDOW_MS,
        consumed_at: None,
    }
}

fn request(id: &str, requester: StepUpRequester, expires_at: i64) -> StepUpRequest {
    StepUpRequest {
        id: id.into(),
        org: Some("acme".into()),
        op: "secret.put".into(),
        method: "POST".into(),
        path: "/v1/secrets".into(),
        query: String::new(),
        sealed: Some(vec![9; 40]),
        action_digest: vec![3; 32],
        summary: serde_json::json!({"operation": "Write a secret", "target": "org acme"}),
        for_human: "amy".into(),
        requester,
        session_role_cap: Some(Role::Admin),
        created_at: 0,
        expires_at,
        state: RequestState::Pending,
        confirmed_by: None,
        confirmed_at: None,
        confirmation_id: None,
        executed_at: None,
        result_status: None,
        result_summary: None,
    }
}

fn token(id: &str) -> StepUpRequester {
    StepUpRequester::ApiToken { id: id.into() }
}

async fn db() -> Option<(common::TestDb, PostgresDb)> {
    let tdb = fresh_db().await?;
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    Some((tdb, db))
}

#[tokio::test]
async fn passkeys_round_trip_and_a_credential_registers_once() {
    let Some((tdb, db)) = db().await else { return };

    let pk = passkey("pk1", "amy", b"cred-1", 0);
    PasskeyStore::insert(&db, &pk).await.unwrap();
    assert_eq!(
        PasskeyStore::list(&db, "amy").await.unwrap(),
        vec![pk.clone()]
    );
    assert_eq!(
        db.get_by_credential(b"cred-1").await.unwrap(),
        Some(pk.clone())
    );
    assert_eq!(db.get_by_credential(b"nope").await.unwrap(), None);

    // The same credential id, even for someone else, is refused.
    let dup = passkey("pk2", "bob", b"cred-1", 0);
    assert!(matches!(
        PasskeyStore::insert(&db, &dup).await,
        Err(IdentityError::Conflict(_))
    ));

    // Remove is per principal: bob cannot remove amy's.
    assert!(!db.remove("bob", "pk1").await.unwrap());
    PasskeyStore::insert(&db, &passkey("pk3", "amy", b"cred-3", 0))
        .await
        .unwrap();
    assert!(db.remove("amy", "pk1").await.unwrap());
    assert!(!db.remove("amy", "pk1").await.unwrap());
    assert_eq!(db.remove_all("amy").await.unwrap(), 1);
    assert!(PasskeyStore::list(&db, "amy").await.unwrap().is_empty());

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_user_handle_is_minted_once_and_kept() {
    let Some((tdb, db)) = db().await else { return };
    let first = db.user_handle("amy", [1; 32]).await.unwrap();
    assert_eq!(first, [1; 32]);
    assert_eq!(
        db.user_handle("amy", [2; 32]).await.unwrap(),
        [1; 32],
        "a later candidate never replaces the handle"
    );
    assert_eq!(db.user_handle("bob", [3; 32]).await.unwrap(), [3; 32]);
    tdb.cleanup().await;
}

#[tokio::test]
async fn the_counter_only_moves_forward_and_a_regression_flags_the_clone_forever() {
    let Some((tdb, db)) = db().await else { return };
    PasskeyStore::insert(&db, &passkey("pk1", "amy", b"cred-1", 0))
        .await
        .unwrap();
    PasskeyStore::insert(&db, &passkey("pk2", "amy", b"cred-2", 0))
        .await
        .unwrap();

    // 0/0 is accepted (platform passkeys keep no counter) and recorded.
    assert_eq!(
        db.record_use("pk1", 0, true, 500).await.unwrap(),
        PasskeyUse::Recorded
    );
    assert_eq!(
        db.record_use("pk1", 5, true, 600).await.unwrap(),
        PasskeyUse::Recorded
    );
    let stored = db.get_by_credential(b"cred-1").await.unwrap().unwrap();
    assert_eq!(stored.sign_count, 5);
    assert_eq!(stored.last_used_at, Some(600));
    assert!(stored.backup_state);

    // Equal is not forward: a replayed counter flags the credential.
    assert_eq!(
        db.record_use("pk1", 5, true, 700).await.unwrap(),
        PasskeyUse::CloneSuspected
    );
    let flagged = db.get_by_credential(b"cred-1").await.unwrap().unwrap();
    assert_eq!(flagged.clone_suspected_at, Some(700));
    assert_eq!(flagged.sign_count, 5, "a refused use records nothing");
    assert_eq!(flagged.last_used_at, Some(600));
    // ...and a flagged credential is never accepted again, even moving
    // forward; the first flag time stands.
    assert_eq!(
        db.record_use("pk1", 9, true, 800).await.unwrap(),
        PasskeyUse::CloneSuspected
    );
    assert_eq!(
        db.get_by_credential(b"cred-1")
            .await
            .unwrap()
            .unwrap()
            .clone_suspected_at,
        Some(700)
    );

    // A counter that drops to 0 after being non-zero is a clone too.
    db.record_use("pk2", 3, false, 100).await.unwrap();
    assert_eq!(
        db.record_use("pk2", 0, false, 200).await.unwrap(),
        PasskeyUse::CloneSuspected
    );

    assert_eq!(
        db.record_use("nope", 1, false, 1).await.unwrap(),
        PasskeyUse::NotFound
    );

    // Only usable passkeys count as "has a passkey".
    PasskeyStore::insert(&db, &passkey("pk3", "bob", b"cred-3", 0))
        .await
        .unwrap();
    let holders = db
        .principals_with_passkeys(&["amy".into(), "bob".into(), "cat".into()])
        .await
        .unwrap();
    assert_eq!(
        holders.into_iter().collect::<Vec<_>>(),
        vec!["bob".to_string()]
    );

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_challenge_is_consumed_once_only_in_its_session_and_before_expiry() {
    let Some((tdb, db)) = db().await else { return };
    let c = challenge("c1", "sess-a", 1_000);
    ChallengeStore::put(&db, &c).await.unwrap();

    assert_eq!(
        db.consume("c1", "sess-b", 10).await.unwrap(),
        None,
        "another session cannot spend it"
    );
    let got = db
        .consume("c1", "sess-a", 10)
        .await
        .unwrap()
        .expect("consumed");
    assert_eq!(
        got,
        WebauthnChallenge {
            consumed_at: Some(10),
            ..c.clone()
        }
    );
    assert_eq!(
        db.consume("c1", "sess-a", 11).await.unwrap(),
        None,
        "single use"
    );
    assert_eq!(db.consume("nope", "sess-a", 11).await.unwrap(), None);

    // Expiry is exclusive at the instant.
    ChallengeStore::put(&db, &challenge("c2", "sess-a", 1_000))
        .await
        .unwrap();
    assert_eq!(db.consume("c2", "sess-a", 1_000).await.unwrap(), None);
    ChallengeStore::put(&db, &challenge("c3", "sess-a", 1_000))
        .await
        .unwrap();
    assert!(db.consume("c3", "sess-a", 999).await.unwrap().is_some());

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_window_covers_its_session_until_a_fixed_expiry() {
    let Some((tdb, db)) = db().await else { return };
    let t = 10_000;
    db.put_confirmation(&confirmation(
        "w1",
        "sess-a",
        ConfirmationKind::Window,
        None,
        t,
    ))
    .await
    .unwrap();

    assert!(db
        .has_window("sess-a", "amy", t + STEP_UP_WINDOW_MS - 1)
        .await
        .unwrap());
    // Using it changes nothing: it still ends at t + 5 min.
    assert_eq!(
        db.window_expiry("sess-a", "amy", t + 60_000).await.unwrap(),
        Some(t + STEP_UP_WINDOW_MS)
    );
    assert!(!db
        .has_window("sess-a", "amy", t + STEP_UP_WINDOW_MS)
        .await
        .unwrap());
    assert!(
        !db.has_window("sess-b", "amy", t + 1).await.unwrap(),
        "another session"
    );
    assert!(
        !db.has_window("sess-a", "bob", t + 1).await.unwrap(),
        "another principal"
    );

    // An action confirmation is not a window.
    db.put_confirmation(&confirmation(
        "a1",
        "sess-c",
        ConfirmationKind::Action,
        Some(&[1; 32]),
        t,
    ))
    .await
    .unwrap();
    assert!(!db.has_window("sess-c", "amy", t + 1).await.unwrap());

    tdb.cleanup().await;
}

#[tokio::test]
async fn an_action_confirmation_is_spent_once_and_only_on_its_digest() {
    let Some((tdb, db)) = db().await else { return };
    let t = 10_000;
    let d = [7u8; 32];
    db.put_confirmation(&confirmation(
        "w1",
        "sess-a",
        ConfirmationKind::Window,
        None,
        t,
    ))
    .await
    .unwrap();
    // A window never satisfies always-ask.
    assert_eq!(
        db.consume_action("sess-a", "amy", &d, t + 1).await.unwrap(),
        None
    );

    db.put_confirmation(&confirmation(
        "a1",
        "sess-a",
        ConfirmationKind::Action,
        Some(&d),
        t,
    ))
    .await
    .unwrap();
    assert_eq!(
        db.consume_action("sess-a", "amy", &[8; 32], t + 1)
            .await
            .unwrap(),
        None,
        "another action's digest"
    );
    assert_eq!(
        db.consume_action("sess-b", "amy", &d, t + 1).await.unwrap(),
        None
    );
    assert_eq!(
        db.consume_action("sess-a", "bob", &d, t + 1).await.unwrap(),
        None
    );
    assert_eq!(
        db.consume_action("sess-a", "amy", &d, t + STEP_UP_WINDOW_MS)
            .await
            .unwrap(),
        None,
        "expired"
    );
    assert_eq!(
        db.consume_action("sess-a", "amy", &d, t + 1).await.unwrap(),
        Some("a1".to_string())
    );
    assert_eq!(
        db.consume_action("sess-a", "amy", &d, t + 2).await.unwrap(),
        None,
        "single use"
    );

    // Two confirmations of one digest are two uses, not one.
    db.put_confirmation(&confirmation(
        "a2",
        "sess-a",
        ConfirmationKind::Action,
        Some(&d),
        t + 3,
    ))
    .await
    .unwrap();
    db.put_confirmation(&confirmation(
        "a3",
        "sess-a",
        ConfirmationKind::Action,
        Some(&d),
        t + 4,
    ))
    .await
    .unwrap();
    assert_eq!(
        db.consume_action("sess-a", "amy", &d, t + 5).await.unwrap(),
        Some("a2".to_string())
    );
    assert_eq!(
        db.consume_action("sess-a", "amy", &d, t + 6).await.unwrap(),
        Some("a3".to_string())
    );

    tdb.cleanup().await;
}

#[tokio::test]
async fn deleting_standing_authority_spares_spent_and_request_confirmations() {
    let Some((tdb, db)) = db().await else { return };
    let d = [7u8; 32];
    for c in [
        confirmation("w1", "sess-a", ConfirmationKind::Window, None, 0),
        confirmation("w2", "sess-b", ConfirmationKind::Window, None, 0),
        confirmation("a1", "sess-a", ConfirmationKind::Action, Some(&d), 0),
        confirmation("a2", "sess-a", ConfirmationKind::Action, Some(&d), 1),
        confirmation("r1", "sess-a", ConfirmationKind::Request, Some(&d), 0),
    ] {
        db.put_confirmation(&c).await.unwrap();
    }
    db.consume_action("sess-a", "amy", &d, 2).await.unwrap(); // spends a1
    let other = StepUpConfirmation {
        principal: "bob".into(),
        ..confirmation("w3", "sess-c", ConfirmationKind::Window, None, 0)
    };
    db.put_confirmation(&other).await.unwrap();

    // w1, w2 and the unspent a2.
    assert_eq!(db.delete_windows_for("amy").await.unwrap(), 3);
    assert!(!db.has_window("sess-a", "amy", 1).await.unwrap());
    assert_eq!(
        db.consume_action("sess-a", "amy", &d, 3).await.unwrap(),
        None
    );
    assert!(
        db.has_window("sess-c", "bob", 1).await.unwrap(),
        "bob untouched"
    );

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_request_round_trips_and_settles_once_dropping_its_sealed_body() {
    let Some((tdb, db)) = db().await else { return };
    let r = request("r1", token("t1"), 5_000);
    db.put_request(&r).await.unwrap();
    assert_eq!(db.get_request("r1").await.unwrap(), Some(r.clone()));
    assert_eq!(db.get_request("nope").await.unwrap(), None);

    let BeginExecution::Begun(begun) = db
        .begin_execution("r1", "amy", "conf-1", 100)
        .await
        .unwrap()
    else {
        panic!("a pending, live request begins");
    };
    assert_eq!(begun.state, RequestState::Executing);
    assert_eq!(begun.confirmed_by.as_deref(), Some("amy"));
    assert_eq!(begun.confirmation_id.as_deref(), Some("conf-1"));
    assert_eq!(begun.confirmed_at, Some(100));
    assert_eq!(begun.sealed, r.sealed, "the replay needs the body");
    assert_eq!(
        db.begin_execution("r1", "amy", "conf-2", 101)
            .await
            .unwrap(),
        BeginExecution::NotPending(RequestState::Executing)
    );

    assert!(matches!(
        db.finish_request("r1", RequestState::Pending, None, None, 200)
            .await,
        Err(IdentityError::Conflict(_))
    ));
    assert!(db
        .finish_request(
            "r1",
            RequestState::Executed,
            Some(201),
            Some("created"),
            200
        )
        .await
        .unwrap());
    let done = db.get_request("r1").await.unwrap().unwrap();
    assert_eq!(done.state, RequestState::Executed);
    assert_eq!(done.sealed, None, "the parked body is gone once settled");
    assert_eq!(done.result_status, Some(201));
    assert_eq!(done.result_summary.as_deref(), Some("created"));
    assert_eq!(done.executed_at, Some(200));
    assert!(
        !db.finish_request("r1", RequestState::Failed, Some(500), None, 300)
            .await
            .unwrap(),
        "settled once"
    );
    assert_eq!(
        db.begin_execution("r1", "amy", "conf-3", 400)
            .await
            .unwrap(),
        BeginExecution::NotPending(RequestState::Executed)
    );
    assert_eq!(
        db.begin_execution("nope", "amy", "c", 1).await.unwrap(),
        BeginExecution::NotFound
    );

    // A Bearer-session requester round-trips too.
    let s = request(
        "r2",
        StepUpRequester::BearerSession {
            session_hash: "sess-h".into(),
        },
        5_000,
    );
    db.put_request(&s).await.unwrap();
    assert_eq!(db.get_request("r2").await.unwrap(), Some(s));

    tdb.cleanup().await;
}

#[tokio::test]
async fn an_expired_request_cannot_begin() {
    let Some((tdb, db)) = db().await else { return };
    db.put_request(&request("r1", token("t1"), 5_000))
        .await
        .unwrap();
    assert_eq!(
        db.begin_execution("r1", "amy", "c", 5_000).await.unwrap(),
        BeginExecution::Expired,
        "expiry is exclusive at the instant"
    );
    let r = db.get_request("r1").await.unwrap().unwrap();
    assert_eq!(r.state, RequestState::Pending, "nothing was stamped");
    assert_eq!(r.confirmed_by, None);
    assert!(r.is_expired(5_000));
    tdb.cleanup().await;
}

/// Two confirmers race for one request. The interleaving is constructed,
/// not hoped for: a third transaction holds the row lock while BOTH
/// `begin_execution` calls are observed waiting on it, then lets go. Exactly
/// one may win; the other must see it executing.
#[tokio::test]
async fn two_concurrent_confirmers_begin_a_request_exactly_once() {
    let Some((tdb, db)) = db().await else { return };
    db.put_request(&request("r1", token("t1"), 50_000))
        .await
        .unwrap();

    let mut holder = tdb.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM step_up_requests WHERE id = 'r1' FOR UPDATE")
        .execute(&mut *holder)
        .await
        .unwrap();

    let db = std::sync::Arc::new(db);
    let racers: Vec<_> = ["amy", "amy-again"]
        .into_iter()
        .map(|who| {
            let db = db.clone();
            tokio::spawn(async move { db.begin_execution("r1", who, who, 100).await.unwrap() })
        })
        .collect();

    // Both racers are inside the UPDATE, blocked on the held row lock.
    let mut waiting = 0;
    for _ in 0..500 {
        waiting = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'
               AND query LIKE 'UPDATE step_up_requests%'",
        )
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
        if waiting == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(waiting, 2, "both confirmers must be waiting on the row");
    holder.commit().await.unwrap();

    let mut outcomes = Vec::new();
    for r in racers {
        outcomes.push(r.await.unwrap());
    }
    let begun = outcomes
        .iter()
        .filter(|o| matches!(o, BeginExecution::Begun(_)))
        .count();
    assert_eq!(begun, 1, "exactly one confirmer executes: {outcomes:?}");
    assert!(outcomes.contains(&BeginExecution::NotPending(RequestState::Executing)));

    tdb.cleanup().await;
}

#[tokio::test]
async fn pending_count_counts_only_this_requesters_live_pending_requests() {
    let Some((tdb, db)) = db().await else { return };
    let session = StepUpRequester::BearerSession {
        session_hash: "t1".into(),
    };
    db.put_request(&request("r1", token("t1"), 5_000))
        .await
        .unwrap();
    db.put_request(&request("r2", token("t1"), 5_000))
        .await
        .unwrap();
    db.put_request(&request("r3", token("t1"), 1_000))
        .await
        .unwrap(); // expires first
    db.put_request(&request("r4", token("t2"), 5_000))
        .await
        .unwrap();
    // Same key text, different credential kind: never counted together.
    db.put_request(&request("r5", session.clone(), 5_000))
        .await
        .unwrap();
    db.begin_execution("r2", "amy", "c", 10).await.unwrap();

    assert_eq!(db.pending_count(&token("t1"), 10).await.unwrap(), 2); // r1, r3
    assert_eq!(db.pending_count(&token("t1"), 1_000).await.unwrap(), 1); // r3 expired
    assert_eq!(db.pending_count(&token("t2"), 10).await.unwrap(), 1);
    assert_eq!(db.pending_count(&session, 10).await.unwrap(), 1);
    assert_eq!(db.pending_count(&token("t9"), 10).await.unwrap(), 0);

    tdb.cleanup().await;
}

/// A request names exactly the credential that raised it: the requester
/// column must match `requested_via`, in the schema itself.
#[tokio::test]
async fn a_request_row_must_name_its_requester() {
    let Some((tdb, db)) = db().await else { return };
    let err = sqlx::query(
        "INSERT INTO step_up_requests
             (id, op, method, path, action_digest, summary, for_human, requested_via,
              created_at, expires_at, state)
         VALUES ('x', 'secret.put', 'POST', '/v1/secrets', '\\x00', '{}', 'amy', 'api_token',
                 0, 1, 'pending')",
    )
    .execute(&tdb.pool)
    .await;
    let err = err.expect_err("an api_token request with no token id is refused");
    assert!(err.to_string().contains("check constraint"), "{err}");
    drop(db);
    tdb.cleanup().await;
}

/// ADR-0073 C5: an Org's waiting requests (the security summary).
#[tokio::test]
async fn pending_in_org_counts_only_that_org_s_live_pending_requests() {
    let Some((tdb, db)) = db().await else { return };
    db.put_request(&request("r1", token("t1"), 5_000))
        .await
        .unwrap();
    db.put_request(&request("r2", token("t1"), 1_000))
        .await
        .unwrap();
    db.put_request(&request("r3", token("t1"), 5_000))
        .await
        .unwrap();
    let mut other = request("r4", token("t1"), 5_000);
    other.org = Some("beta".into());
    db.put_request(&other).await.unwrap();
    db.begin_execution("r3", "amy", "c", 10).await.unwrap();

    assert_eq!(db.pending_in_org("acme", 10).await.unwrap(), 2); // r1, r2
    assert_eq!(db.pending_in_org("acme", 1_000).await.unwrap(), 1); // r2 expired
    assert_eq!(db.pending_in_org("beta", 10).await.unwrap(), 1);
    assert_eq!(db.pending_in_org("zeta", 10).await.unwrap(), 0);
    tdb.cleanup().await;
}

/// ADR-0073 C5 retention: expired challenges and confirmations go; an
/// expired (or abandoned-executing) request's sealed body goes at once; a
/// request settled, expired or abandoned before the cutoff goes; live rows
/// stay.
#[tokio::test]
async fn reaping_drops_dead_rows_and_dead_bodies_only() {
    let Some((tdb, db)) = db().await else { return };
    let now = 1_000_000;
    db.put(&challenge("c-dead", "s1", now)).await.unwrap();
    db.put(&challenge("c-live", "s1", now + 1)).await.unwrap();
    assert_eq!(db.reap_expired(now).await.unwrap(), 1);
    assert!(db.consume("c-dead", "s1", 0).await.unwrap().is_none());
    assert!(db.consume("c-live", "s1", now).await.unwrap().is_some());

    let dead = now - STEP_UP_WINDOW_MS; // expires exactly now
    db.put_confirmation(&confirmation(
        "w-dead",
        "s1",
        ConfirmationKind::Window,
        None,
        dead,
    ))
    .await
    .unwrap();
    db.put_confirmation(&confirmation(
        "w-live",
        "s1",
        ConfirmationKind::Window,
        None,
        dead + 1,
    ))
    .await
    .unwrap();

    let cutoff = now - 30 * 24 * 60 * 60 * 1000;
    // Pending, expired now: body cleared, row kept.
    db.put_request(&request("expired", token("t1"), now))
        .await
        .unwrap();
    // Pending, live: untouched.
    db.put_request(&request("live", token("t1"), now + 1))
        .await
        .unwrap();
    // Executing since long ago (a crash mid-replay): body cleared, row kept
    // until the cutoff.
    db.put_request(&request("stuck", token("t1"), now + 1))
        .await
        .unwrap();
    db.begin_execution("stuck", "amy", "c1", now - 6 * 60 * 1000)
        .await
        .unwrap();
    // Settled before the cutoff: gone.
    db.put_request(&request("old", token("t1"), cutoff))
        .await
        .unwrap();
    db.begin_execution("old", "amy", "c2", cutoff - 2)
        .await
        .unwrap();
    db.finish_request("old", RequestState::Executed, Some(204), None, cutoff - 1)
        .await
        .unwrap();
    // Expired before the cutoff: gone.
    db.put_request(&request("ancient", token("t1"), cutoff - 1))
        .await
        .unwrap();

    assert_eq!(db.reap(now, cutoff).await.unwrap(), 1 + 2);
    assert!(
        db.window_expiry("s1", "amy", now).await.unwrap().is_some(),
        "w-live stays"
    );
    assert!(db.get_request("old").await.unwrap().is_none());
    assert!(db.get_request("ancient").await.unwrap().is_none());
    let expired = db.get_request("expired").await.unwrap().unwrap();
    assert!(expired.sealed.is_none());
    let stuck = db.get_request("stuck").await.unwrap().unwrap();
    assert!(stuck.sealed.is_none());
    let live = db.get_request("live").await.unwrap().unwrap();
    assert!(live.sealed.is_some());
    tdb.cleanup().await;
}

/// ADR-0073 §10: the Orgs a subject is a member of (the Owner reset's
/// cross-org rule) — live bindings only, Org- or Project-scoped.
#[tokio::test]
async fn orgs_of_lists_the_orgs_a_subject_is_bound_in() {
    use scarab_identity::{Binding, BindingOrigin, RbacStore, Scope};
    let Some((tdb, db)) = db().await else { return };
    for (scope, role) in [
        (Scope::Org("acme".into()), Role::Admin),
        (
            Scope::Project {
                org: "beta".into(),
                name: "web".into(),
            },
            Role::Member,
        ),
        (Scope::Org("gamma".into()), Role::Viewer),
    ] {
        db.grant(
            &Binding {
                subject: "bob".into(),
                scope,
                role,
            },
            BindingOrigin::Native,
        )
        .await
        .unwrap();
    }
    db.revoke("bob", &Scope::Org("gamma".into())).await.unwrap();
    assert_eq!(db.orgs_of("bob").await.unwrap(), vec!["acme", "beta"]);
    assert!(db.orgs_of("nobody").await.unwrap().is_empty());
    tdb.cleanup().await;
}

/// The break-glass reads (never writes) the applied schema version.
#[tokio::test]
async fn schema_version_reads_the_applied_migrations() {
    let Some(tdb) = fresh_db().await else { return };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    assert_eq!(db.schema_version().await.unwrap(), None);
    db.migrate().await.unwrap();
    assert!(db.schema_version().await.unwrap().unwrap() >= 62);
    tdb.cleanup().await;
}

/// git-bug e582326: `raise_request` caps live pending requests per requester
/// AND per human (across credentials); expired or settled ones do not count.
#[tokio::test]
async fn raise_request_caps_per_requester_and_per_human() {
    let Some((tdb, db)) = db().await else { return };
    let caps = PendingCaps {
        per_requester: 2,
        per_human: 3,
    };
    let raise = |id: &str, requester: StepUpRequester| {
        let r = request(id, requester, 5_000);
        let db = &db;
        async move { db.raise_request(&r, caps, 10).await.unwrap() }
    };
    assert_eq!(raise("r1", token("t1")).await, RaiseRequest::Raised);
    assert_eq!(raise("r2", token("t1")).await, RaiseRequest::Raised);
    assert_eq!(
        raise("r3", token("t1")).await,
        RaiseRequest::RequesterFull { pending: 2 }
    );
    assert_eq!(raise("r4", token("t2")).await, RaiseRequest::Raised);
    assert_eq!(
        raise("r5", token("t3")).await,
        RaiseRequest::HumanFull { pending: 3 }
    );
    // An expired request does not count; nor does a settled one.
    db.put_request(&request("old", token("t9"), 5))
        .await
        .unwrap();
    db.begin_execution("r1", "amy", "c", 10).await.unwrap();
    assert_eq!(raise("r6", token("t3")).await, RaiseRequest::Raised);
    // Another human is not affected.
    let mut bob = request("b1", token("tb"), 5_000);
    bob.for_human = "bob".into();
    assert_eq!(
        db.raise_request(&bob, caps, 10).await.unwrap(),
        RaiseRequest::Raised
    );
    for id in ["r3", "r5"] {
        assert!(db.get_request(id).await.unwrap().is_none(), "{id}");
    }
    tdb.cleanup().await;
}

/// git-bug e582326: the count and the insert are one atomic step. The test
/// holds the human's raise lock, starts a raise (it must wait), lets 3 other
/// requests for the human commit meanwhile, then releases: the raise counts
/// them and is refused. A count-then-insert outside the lock would already
/// have inserted, over the cap.
#[tokio::test]
async fn raise_request_counts_and_inserts_under_the_human_s_lock() {
    let Some((tdb, db)) = db().await else { return };
    let db = std::sync::Arc::new(db);
    let caps = PendingCaps {
        per_requester: 10,
        per_human: 3,
    };
    let mut lock = tdb.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(scarab_db_postgres::step_up_request_lock_key("amy"))
        .execute(&mut *lock)
        .await
        .unwrap();
    let racer = {
        let db = db.clone();
        tokio::spawn(async move {
            db.raise_request(&request("racer", token("t1"), 5_000), caps, 10)
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!racer.is_finished(), "the raise waits for the human's lock");
    for (i, t) in ["t2", "t3", "t4"].iter().enumerate() {
        db.put_request(&request(&format!("other{i}"), token(t), 5_000))
            .await
            .unwrap();
    }
    lock.commit().await.unwrap();
    assert_eq!(racer.await.unwrap(), RaiseRequest::HumanFull { pending: 3 });
    assert!(db.get_request("racer").await.unwrap().is_none());
    tdb.cleanup().await;
}

/// git-bug 6856659: `delete_for_passkey` revokes exactly the standing
/// step-ups that passkey signed — windows and unconsumed action
/// confirmations, in every session — and nothing another passkey signed.
#[tokio::test]
async fn delete_for_passkey_revokes_only_what_it_signed() {
    let Some((tdb, db)) = db().await else { return };
    let mine = |id: &str, session: &str, kind, consumed: bool| {
        let mut c = confirmation(id, session, kind, Some(&[1; 32]), 10);
        c.passkey_id = "pk-gone".into();
        if consumed {
            c.consumed_at = Some(11);
        }
        c
    };
    for c in [
        mine("w1", "s1", ConfirmationKind::Window, false),
        mine("w2", "s2", ConfirmationKind::Window, false),
        mine("a1", "s1", ConfirmationKind::Action, false),
        mine("a2", "s1", ConfirmationKind::Action, true),
        confirmation("other", "s1", ConfirmationKind::Window, None, 10),
    ] {
        db.put_confirmation(&c).await.unwrap();
    }
    assert_eq!(db.delete_for_passkey("pk-gone").await.unwrap(), 3);
    assert_eq!(
        db.live_window("s1", "amy", 20).await.unwrap().map(|w| w.0),
        Some("other".to_string())
    );
    assert!(db.window_expiry("s2", "amy", 20).await.unwrap().is_none());
    assert!(db
        .consume_action("s1", "amy", &[1; 32], 20)
        .await
        .unwrap()
        .is_none());
    assert_eq!(db.delete_for_passkey("pk-gone").await.unwrap(), 0);
    tdb.cleanup().await;
}
