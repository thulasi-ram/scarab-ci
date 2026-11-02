//! Org-scoped forge connections (git-bug 19b0a45) against real Postgres:
//! the cases whose answer depends on what the database holds after
//! migration 0076 and on the `orgs` table.
//!
//! * An unassigned `github-install-<id>` row that an old re-sync left
//!   holding another installation's repository still mints for that
//!   repository from the installation that covers it: the adapter is pinned
//!   to the row's own installation only once the row serves an Org.
//! * An unassigned `github-install-<id>` row whose bindings all belong to
//!   one owner that is not its account stays unassigned after the upgrade,
//!   and that owner's repository keeps minting through the lookup.
//! * An installation delivery does not attach to an Org that exists but has
//!   no connection left (its secrets, members and policies remain).
//!
//! GitHub is a stub HTTP server (a true external, ADR-0017). Skips cleanly
//! when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_forge::{ForgeConnectionStore, ForgePort, RepoRef};
use scarab_server::forge_router::RegistryForge;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeSecrets, InMemoryDb, InMemoryObjectStore};

const VERSION: i64 = 76;
const SECRET: &[u8] = b"topsecret";

fn repo(owner: &str, name: &str) -> RepoRef {
    RepoRef {
        owner: owner.into(),
        name: name.into(),
    }
}

/// One App key for the binary: 2048-bit keygen is slow.
fn app_pem() -> &'static str {
    static PEM: OnceLock<String> = OnceLock::new();
    PEM.get_or_init(|| {
        let mut rng = rand::rngs::OsRng;
        rsa::RsaPrivateKey::new(&mut rng, 2048)
            .unwrap()
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .to_string()
    })
}

#[derive(Default)]
struct Recorder {
    seen: Vec<(String, String)>,
    replies: VecDeque<(StatusCode, String)>,
}

/// A stub GitHub that answers scripted replies in order and records each
/// request's method and path.
async fn stub_github(replies: Vec<(StatusCode, &str)>) -> (String, Arc<Mutex<Recorder>>) {
    async fn handle(
        State(rec): State<Arc<Mutex<Recorder>>>,
        method: Method,
        uri: Uri,
        _body: Bytes,
    ) -> Response {
        let reply = {
            let mut r = rec.lock().unwrap();
            r.seen.push((method.to_string(), uri.path().to_string()));
            r.replies.pop_front()
        };
        match reply {
            Some((status, body)) => (status, body).into_response(),
            None => (StatusCode::IM_A_TEAPOT, "unscripted request").into_response(),
        }
    }
    let rec = Arc::new(Mutex::new(Recorder {
        seen: Vec::new(),
        replies: replies
            .into_iter()
            .map(|(s, b)| (s, b.to_string()))
            .collect(),
    }));
    let app = axum::Router::new().fallback(handle).with_state(rec.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), rec)
}

async fn apply(pool: &sqlx::PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

/// The upgrade case of git-bug 19b0a45: before this change a re-sync walked
/// every installation of the App and bound what it found to the connection
/// it ran on, so `github-install-5` (account acme) came to hold `globex/site`
/// of installation 9. Migration 0076 leaves that row unassigned with
/// installation id 5. Pinning it to installation 5 would mint every
/// `globex/site` token from an installation that does not cover it, and its
/// clones and statuses would fail after the upgrade. Unassigned, it keeps
/// the per-repo lookup.
#[tokio::test]
async fn an_unassigned_legacy_installation_row_still_mints_through_the_lookup() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let (base, rec) = stub_github(vec![
        (StatusCode::OK, r#"{"id": 9}"#),
        (StatusCode::CREATED, r#"{"token": "ghs_from_9"}"#),
    ])
    .await;

    apply(&pool, |v| v < VERSION).await;
    sqlx::query(
        "INSERT INTO forge_connections (id, kind, base_url, credential_ref)
         VALUES ('github-install-5', 'github', $1, 'github-app')",
    )
    .bind(&base)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(
        "INSERT INTO forge_repos (connection_id, owner, name, org, project) VALUES
            ('github-install-5', 'acme',   'web',  'acme',   'web'),
            ('github-install-5', 'globex', 'site', 'globex', 'site');",
    )
    .execute(&pool)
    .await
    .unwrap();
    apply(&pool, |v| v >= VERSION).await;

    let pg = Arc::new(PostgresDb::with_pool(pool.clone()));
    let row = pg
        .get_connection("github-install-5")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.org, None, "two owners: unassigned");
    assert_eq!(row.installation_id, Some(5));

    let forge = RegistryForge::new(
        pg.clone(),
        Arc::new(FakeSecrets::new()),
        Some("4323648".into()),
        Some(app_pem().to_string()),
    );
    let cred = forge
        .mint_checkout_credential(&repo("globex", "site"), true)
        .await
        .expect("mints from the installation that covers globex/site");
    assert_eq!(cred.token, "ghs_from_9");
    let paths: Vec<String> = rec
        .lock()
        .unwrap()
        .seen
        .iter()
        .map(|(_, p)| p.clone())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/repos/globex/site/installation".to_string(),
            "/app/installations/9/access_tokens".to_string()
        ]
    );

    tdb.cleanup().await;
}

/// J1 of git-bug 19b0a45: an old re-sync of installation 5 (account acme,
/// whose own installation covered nothing) left `globex/site` of
/// installation 9 as its only binding. Every binding is globex's, but the
/// row is an installation, so 0076 leaves it unassigned: assigned to
/// globex it would be pinned to installation 5, every `globex/site` token
/// would be asked of an installation that does not cover it, and globex's
/// own installation could not take the repo back. Unassigned, the repo
/// mints from the installation the lookup finds. Before the fix the row
/// was assigned `globex` and minted from installation 5.
#[tokio::test]
async fn an_installation_row_holding_only_another_owners_repo_stays_unassigned_and_mints() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let (base, rec) = stub_github(vec![
        (StatusCode::OK, r#"{"id": 9}"#),
        (StatusCode::CREATED, r#"{"token": "ghs_from_9"}"#),
    ])
    .await;

    apply(&pool, |v| v < VERSION).await;
    sqlx::query(
        "INSERT INTO forge_connections (id, kind, base_url, credential_ref)
         VALUES ('github-install-5', 'github', $1, 'github-app')",
    )
    .bind(&base)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO forge_repos (connection_id, owner, name, org, project)
         VALUES ('github-install-5', 'globex', 'site', 'globex', 'site')",
    )
    .execute(&pool)
    .await
    .unwrap();
    apply(&pool, |v| v >= VERSION).await;

    let pg = Arc::new(PostgresDb::with_pool(pool.clone()));
    let row = pg
        .get_connection("github-install-5")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.org, None,
        "an installation row is not assigned by its bindings"
    );

    let forge = RegistryForge::new(
        pg.clone(),
        Arc::new(FakeSecrets::new()),
        Some("4323648".into()),
        Some(app_pem().to_string()),
    );
    let cred = forge
        .mint_checkout_credential(&repo("globex", "site"), true)
        .await
        .expect("mints from the installation that covers globex/site");
    assert_eq!(cred.token, "ghs_from_9");
    let paths: Vec<String> = rec
        .lock()
        .unwrap()
        .seen
        .iter()
        .map(|(_, p)| p.clone())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/repos/globex/site/installation".to_string(),
            "/app/installations/9/access_tokens".to_string()
        ]
    );

    tdb.cleanup().await;
}

/// A signed `installation` delivery for installation `id` on account `login`.
fn installation_request(delivery: &str, id: u64, login: &str, repos: &[&str]) -> Request<Body> {
    let repositories: Vec<serde_json::Value> = repos
        .iter()
        .map(|r| serde_json::json!({ "full_name": r }))
        .collect();
    let body = serde_json::to_vec(&serde_json::json!({
        "action": "created",
        "installation": { "id": id, "account": { "login": login } },
        "repositories": repositories,
    }))
    .unwrap();
    let sig = scarab_forge_github::sign_hex(SECRET, &body);
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "installation")
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", &sig)
        .body(Body::from(body))
        .unwrap()
}

/// D7 of git-bug 19b0a45: an Org whose connections are all gone still has
/// its record (and with it its secrets, members and policies). A GitHub
/// account of the same name does not join it by name; its installation is
/// registered unassigned and binds nothing. An account with no Org of its
/// name gets one.
#[tokio::test]
async fn an_installation_does_not_join_an_org_that_has_no_connection() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    sqlx::query("INSERT INTO orgs (name) VALUES ('acme')")
        .execute(&tdb.pool)
        .await
        .unwrap();

    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forge_connections(pg.clone()),
    );

    let resp = app
        .clone()
        .oneshot(installation_request("pg-d7-1", 9, "acme", &["acme/web"]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let conn = pg
        .get_connection("github-install-9")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.org, None, "registered unassigned");
    assert!(pg.repos_of("github-install-9").await.unwrap().is_empty());

    let resp = app
        .clone()
        .oneshot(installation_request(
            "pg-d7-2",
            10,
            "umbrella",
            &["umbrella/app"],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let conn = pg
        .get_connection("github-install-10")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.org.as_deref(), Some("umbrella"));
    assert_eq!(
        pg.repos_of("github-install-10").await.unwrap(),
        vec![repo("umbrella", "app")]
    );

    tdb.cleanup().await;
}
