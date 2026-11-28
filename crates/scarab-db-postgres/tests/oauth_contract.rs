//! The OAuth authorization server's store (ADR-0086 §7, migration 0113):
//! fake/real parity. Each contract is written once over the port and runs
//! against the in-memory store the server tests use
//! (`scarab_testkit::InMemoryOAuth`, unconditional) and the Postgres adapter
//! (skipping when `SCARAB_TEST_DATABASE_URL` is unset; CI sets it), as
//! `db_contract.rs` does for the `Db` port.
//!
//! What the server's safety rests on, and so what both must agree on: a
//! code is spent once, only by its client, redirect URI and challenge, and
//! only before it expires, and a spent one is recognised when replayed; it
//! can be read without being spent, by the same four and expiry; a
//! rotation and the one retry each move only the generation and access token
//! they name; a revocation happens once and takes every access token minted
//! for the grant with it, and so do removing a metadata host and an
//! uninstall.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{
    ApiTokenStore, OAuthAuthorizationCode, OAuthClientMetadataHost, OAuthGrant, OAuthStore,
    PrincipalToken, PrincipalTokenKind, Role, Scope,
};
use scarab_testkit::{InMemoryApiTokens, InMemoryOAuth};

macro_rules! contract_test {
    ($name:ident, $contract:ident) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn in_memory() {
                let tokens = Arc::new(InMemoryApiTokens::new());
                let oauth = InMemoryOAuth::with_api_tokens(tokens.clone());
                $contract(&oauth, tokens.as_ref()).await;
            }

            #[tokio::test]
            async fn postgres() {
                let Some(tdb) = fresh_db().await else {
                    eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
                    return;
                };
                let db = PostgresDb::with_pool(tdb.pool.clone());
                db.migrate().await.unwrap();
                $contract(&db, &db).await;
                tdb.cleanup().await;
            }
        }
    };
}

const RESOURCE: &str =
    "https://ci.example.com/v1/orgs/acme/plugin-installations/0000000001/endpoints/mcp";
const CLIENT: &str = "https://claude.ai/oauth/mcp-oauth-client-metadata";
const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";

fn code(grant_id: &str, expires_at: i64) -> OAuthAuthorizationCode {
    OAuthAuthorizationCode {
        org: "acme".into(),
        installation_id: "0000000001".into(),
        resource: RESOURCE.into(),
        client_id: CLIENT.into(),
        client_host: "claude.ai".into(),
        redirect_uri: REDIRECT.into(),
        code_challenge: "challenge-1".into(),
        principal_subject: "amy".into(),
        operations: vec!["run.read".into(), "step.logs.read".into()],
        role: Role::Member,
        created_at: 1_000,
        expires_at,
        grant_id: grant_id.into(),
    }
}

fn grant(id: &str, org: &str, host: &str, installation: &str, created_at: i64) -> OAuthGrant {
    OAuthGrant {
        id: id.into(),
        org: org.into(),
        installation_id: installation.into(),
        resource: RESOURCE.into(),
        client_id: CLIENT.into(),
        client_host: host.into(),
        principal_subject: "amy".into(),
        operations: vec!["run.read".into()],
        role: Role::Member,
        created_at,
        last_used_at: created_at,
        expires_at: created_at + 1_000_000,
        generation: 0,
        rotated_at: None,
        access_token_id: None,
        retried: false,
        revoked_at: None,
        revoked_reason: None,
    }
}

fn access_token(id: &str) -> PrincipalToken {
    PrincipalToken {
        id: id.into(),
        name: format!("claude.ai {id}"),
        owner_subject: "amy".into(),
        scope: Scope::Org("acme".into()),
        role: Role::Member,
        expires_at: 10_000_000,
        created_by: "amy".into(),
        created_at: 1_000,
        last_used_at: None,
        use_count: 0,
        revoked_at: None,
        kind: PrincipalTokenKind::Oauth,
        operations: Some(vec!["run.read".into()]),
        audience: Some(RESOURCE.into()),
        oauth_client: Some(CLIENT.into()),
    }
}

/// Store access token `id` for grant `grant_id`.
async fn put_token(oauth: &dyn OAuthStore, id: &str, grant_id: &str) {
    let token = access_token(id);
    let hash = token.stored_hash(&format!("scarab_oat_{id}"));
    oauth
        .put_oauth_access_token(&token, &hash, grant_id)
        .await
        .unwrap();
}

/// Whether token `id` is revoked.
async fn revoked(tokens: &dyn ApiTokenStore, id: &str) -> bool {
    tokens
        .by_id(id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("token {id} is stored"))
        .revoked_at
        .is_some()
}

async fn codes_spend_once_only_with_all_four_and_before_expiry(
    oauth: &dyn OAuthStore,
    _tokens: &dyn ApiTokenStore,
) {
    oauth
        .put_authorization_code(&code("g1", 2_000), "h1")
        .await
        .unwrap();
    oauth
        .put_authorization_code(&code("g2", 2_000), "h2")
        .await
        .unwrap();

    for (why, client, redirect, challenge) in [
        (
            "another client",
            "https://evil.example/c",
            REDIRECT,
            "challenge-1",
        ),
        (
            "another redirect",
            CLIENT,
            "https://claude.ai/other",
            "challenge-1",
        ),
        ("another verifier", CLIENT, REDIRECT, "challenge-2"),
    ] {
        assert_eq!(
            oauth
                .consume_authorization_code("h1", client, redirect, challenge, 1_500)
                .await
                .unwrap(),
            None,
            "{why}"
        );
        assert_eq!(
            oauth
                .pending_authorization_code("h1", client, redirect, challenge, 1_500)
                .await
                .unwrap(),
            None,
            "{why}: not pending for it"
        );
    }
    // Reading the code spends nothing, however often it is read.
    for _ in 0..2 {
        assert_eq!(
            oauth
                .pending_authorization_code("h1", CLIENT, REDIRECT, "challenge-1", 1_500)
                .await
                .unwrap(),
            Some(code("g1", 2_000))
        );
    }
    assert_eq!(
        oauth
            .pending_authorization_code("h1", CLIENT, REDIRECT, "challenge-1", 2_000)
            .await
            .unwrap(),
        None,
        "not pending at its expiry"
    );
    assert_eq!(
        oauth
            .replayed_authorization_code("h1", CLIENT, REDIRECT, "challenge-1")
            .await
            .unwrap(),
        None,
        "an unspent code is not a replay"
    );
    // None of the wrong guesses spent it.
    assert_eq!(
        oauth
            .consume_authorization_code("h1", CLIENT, REDIRECT, "challenge-1", 1_500)
            .await
            .unwrap(),
        Some(code("g1", 2_000))
    );
    assert_eq!(
        oauth
            .consume_authorization_code("h1", CLIENT, REDIRECT, "challenge-1", 1_500)
            .await
            .unwrap(),
        None,
        "spent once"
    );
    assert_eq!(
        oauth
            .pending_authorization_code("h1", CLIENT, REDIRECT, "challenge-1", 1_500)
            .await
            .unwrap(),
        None,
        "a spent code is not pending"
    );
    assert_eq!(
        oauth
            .replayed_authorization_code("h1", CLIENT, REDIRECT, "challenge-1")
            .await
            .unwrap(),
        Some("g1".to_string())
    );
    assert_eq!(
        oauth
            .replayed_authorization_code("h1", CLIENT, REDIRECT, "challenge-2")
            .await
            .unwrap(),
        None,
        "a replay without the verifier names no grant"
    );

    assert_eq!(
        oauth
            .consume_authorization_code("h2", CLIENT, REDIRECT, "challenge-1", 2_000)
            .await
            .unwrap(),
        None,
        "expired at its expiry"
    );
    assert_eq!(
        oauth
            .delete_expired_authorization_codes(1_999)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        oauth
            .delete_expired_authorization_codes(2_000)
            .await
            .unwrap(),
        2
    );
}
contract_test!(codes, codes_spend_once_only_with_all_four_and_before_expiry);

async fn a_rotation_and_one_retry_move_only_what_they_name(
    oauth: &dyn OAuthStore,
    _tokens: &dyn ApiTokenStore,
) {
    let mut g = grant("g1", "acme", "claude.ai", "0000000001", 100);
    g.access_token_id = Some("t0".into());
    oauth.put_oauth_grant(&g).await.unwrap();

    assert!(
        !oauth
            .rotate_oauth_grant("g1", 0, Some("t-other"), "t1", 200)
            .await
            .unwrap(),
        "the current access token moved meanwhile"
    );
    assert!(oauth
        .rotate_oauth_grant("g1", 0, Some("t0"), "t1", 200)
        .await
        .unwrap());
    assert!(
        !oauth
            .rotate_oauth_grant("g1", 0, Some("t1"), "t9", 200)
            .await
            .unwrap(),
        "the generation moved"
    );
    let now = oauth.oauth_grant("g1").await.unwrap().unwrap();
    assert_eq!(
        (
            now.generation,
            now.rotated_at,
            now.last_used_at,
            now.retried
        ),
        (1, Some(200), 200, false)
    );
    assert_eq!(now.access_token_id.as_deref(), Some("t1"));

    assert!(oauth
        .replace_oauth_grant_access_token("g1", 1, Some("t1"), "t2", 210)
        .await
        .unwrap());
    let now = oauth.oauth_grant("g1").await.unwrap().unwrap();
    assert_eq!(
        (now.generation, now.retried, now.last_used_at),
        (1, true, 210)
    );
    assert_eq!(now.access_token_id.as_deref(), Some("t2"));
    assert!(
        !oauth
            .replace_oauth_grant_access_token("g1", 1, Some("t2"), "t3", 220)
            .await
            .unwrap(),
        "one retry per rotation"
    );
    assert!(oauth
        .rotate_oauth_grant("g1", 1, Some("t2"), "t3", 230)
        .await
        .unwrap());
    let now = oauth.oauth_grant("g1").await.unwrap().unwrap();
    assert_eq!((now.generation, now.retried), (2, false));
}
contract_test!(rotation, a_rotation_and_one_retry_move_only_what_they_name);

async fn a_revocation_happens_once_and_takes_every_token_of_the_grant(
    oauth: &dyn OAuthStore,
    tokens: &dyn ApiTokenStore,
) {
    let mut g = grant("g1", "acme", "claude.ai", "0000000001", 100);
    g.access_token_id = Some("t2".into());
    oauth.put_oauth_grant(&g).await.unwrap();
    oauth
        .put_oauth_grant(&grant("g2", "acme", "claude.ai", "0000000001", 100))
        .await
        .unwrap();
    for id in ["t0", "t1", "t2"] {
        put_token(oauth, id, "g1").await;
    }
    put_token(oauth, "t-other", "g2").await;

    let gone = oauth
        .revoke_oauth_grant("g1", "disconnected", 500)
        .await
        .unwrap()
        .expect("the first revocation returns the grant");
    assert_eq!(gone.id, "g1");
    assert_eq!(
        oauth.revoke_oauth_grant("g1", "again", 600).await.unwrap(),
        None
    );
    let stored = oauth.oauth_grant("g1").await.unwrap().unwrap();
    assert_eq!(stored.revoked_at, Some(500));
    assert_eq!(stored.revoked_reason.as_deref(), Some("disconnected"));
    for id in ["t0", "t1", "t2"] {
        assert!(revoked(tokens, id).await, "{id}");
    }
    assert!(!revoked(tokens, "t-other").await);
    assert!(
        !oauth
            .rotate_oauth_grant("g1", 0, Some("t2"), "t3", 700)
            .await
            .unwrap(),
        "a revoked grant does not rotate"
    );
    assert!(!oauth
        .replace_oauth_grant_access_token("g1", 0, Some("t2"), "t3", 700)
        .await
        .unwrap());
}
contract_test!(
    revocation,
    a_revocation_happens_once_and_takes_every_token_of_the_grant
);

async fn removing_a_host_or_an_installation_revokes_only_its_grants(
    oauth: &dyn OAuthStore,
    tokens: &dyn ApiTokenStore,
) {
    for (org, host) in [
        ("acme", "claude.ai"),
        ("acme", "claude.ai"),
        ("acme", "other.example"),
        ("globex", "claude.ai"),
    ] {
        oauth
            .put_client_metadata_host(&OAuthClientMetadataHost {
                org: org.into(),
                host: host.into(),
                added_by: "ada".into(),
                added_at: 10,
            })
            .await
            .unwrap();
    }
    let hosts = move |org: &'static str| async move {
        oauth
            .client_metadata_hosts(org)
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.host)
            .collect::<Vec<_>>()
    };
    assert_eq!(hosts("acme").await, vec!["claude.ai", "other.example"]);

    for (id, org, host, installation) in [
        ("g-acme-claude", "acme", "claude.ai", "0000000001"),
        ("g-acme-other", "acme", "other.example", "0000000001"),
        ("g-globex-claude", "globex", "claude.ai", "0000000002"),
        ("g-acme-claude-2", "acme", "claude.ai", "0000000003"),
    ] {
        oauth
            .put_oauth_grant(&grant(id, org, host, installation, 100))
            .await
            .unwrap();
        put_token(oauth, &format!("t-{id}"), id).await;
    }

    assert!(oauth
        .delete_client_metadata_host("acme", "claude.ai", 500)
        .await
        .unwrap());
    assert!(!oauth
        .delete_client_metadata_host("acme", "claude.ai", 500)
        .await
        .unwrap());
    assert_eq!(hosts("acme").await, vec!["other.example"]);
    assert_eq!(hosts("globex").await, vec!["claude.ai"]);
    for (id, gone) in [
        ("g-acme-claude", true),
        ("g-acme-claude-2", true),
        ("g-acme-other", false),
        ("g-globex-claude", false),
    ] {
        let g = oauth.oauth_grant(id).await.unwrap().unwrap();
        assert_eq!(g.revoked_at.is_some(), gone, "{id}");
        assert_eq!(revoked(tokens, &format!("t-{id}")).await, gone, "{id}");
    }

    let mut ended: Vec<String> = oauth
        .revoke_installation_oauth_grants("0000000001", "plugin uninstalled", 600)
        .await
        .unwrap()
        .into_iter()
        .map(|g| g.id)
        .collect();
    ended.sort();
    assert_eq!(ended, vec!["g-acme-other".to_string()], "only live grants");
    assert!(revoked(tokens, "t-g-acme-other").await);
    assert!(!revoked(tokens, "t-g-globex-claude").await);
}
contract_test!(
    hosts_and_installations,
    removing_a_host_or_an_installation_revokes_only_its_grants
);

async fn a_principal_s_grants_are_listed_newest_first(
    oauth: &dyn OAuthStore,
    _tokens: &dyn ApiTokenStore,
) {
    oauth
        .put_oauth_grant(&grant("g-old", "acme", "claude.ai", "0000000001", 100))
        .await
        .unwrap();
    oauth
        .put_oauth_grant(&grant("g-new", "acme", "claude.ai", "0000000001", 200))
        .await
        .unwrap();
    let mut someone_else = grant("g-eve", "acme", "claude.ai", "0000000001", 300);
    someone_else.principal_subject = "eve".into();
    oauth.put_oauth_grant(&someone_else).await.unwrap();
    let ids: Vec<String> = oauth
        .oauth_grants_of("amy")
        .await
        .unwrap()
        .into_iter()
        .map(|g| g.id)
        .collect();
    assert_eq!(ids, vec!["g-new", "g-old"]);
    assert_eq!(
        oauth.oauth_grant("g-old").await.unwrap(),
        Some(grant("g-old", "acme", "claude.ai", "0000000001", 100)),
        "a grant reads back as it was stored"
    );
}
contract_test!(listing, a_principal_s_grants_are_listed_newest_first);

/// Reverting 0113 revokes every `oauth` token (a server before it cannot
/// tell their audience apart) and drops the three tables.
#[tokio::test]
async fn the_0113_revert_revokes_oauth_tokens_and_drops_its_tables() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    db.put_oauth_grant(&grant("g1", "acme", "claude.ai", "0000000001", 100))
        .await
        .unwrap();
    put_token(&db, "t1", "g1").await;
    sqlx::raw_sql(include_str!("../rollback/0113_revert.sql"))
        .execute(&tdb.pool)
        .await
        .unwrap();
    let revoked_at: Option<i64> =
        sqlx::query_scalar("SELECT revoked_at FROM api_tokens WHERE id = 't1'")
            .fetch_one(&tdb.pool)
            .await
            .unwrap();
    assert!(revoked_at.is_some());
    for table in [
        "oauth_grants",
        "oauth_authorization_codes",
        "oauth_client_metadata_hosts",
    ] {
        let found: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(table)
            .fetch_one(&tdb.pool)
            .await
            .unwrap();
        assert_eq!(found, None, "{table}");
    }
    tdb.cleanup().await;
}
