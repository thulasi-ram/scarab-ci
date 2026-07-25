//! ForgeConnection registry acceptance against real Postgres (ADR-0046):
//! connection CRUD, repo binding, and RepoRef → Project resolution — the seam
//! that answers *which forge, which base URL, which credentials* for a repo.
//!
//! Skips cleanly when SCARAB_TEST_DATABASE_URL is unset (see `common`).

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};

fn github_conn() -> ForgeConnection {
    ForgeConnection {
        id: "gh-acme".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "gh-acme-app-pem".into(),
    }
}

fn forgejo_conn() -> ForgeConnection {
    ForgeConnection {
        id: "codeberg-acme".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://codeberg.org".into(),
        credential_ref: "codeberg-acme-token".into(),
    }
}

fn repo(owner: &str, name: &str) -> RepoRef {
    RepoRef {
        owner: owner.into(),
        name: name.into(),
    }
}

#[tokio::test]
async fn registration_and_resolution_round_trip() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    // Register two connections — one per forge kind (the two-adapter reality
    // the registry exists for).
    db.put_connection(&github_conn()).await.unwrap();
    db.put_connection(&forgejo_conn()).await.unwrap();
    assert_eq!(
        db.list_connections().await.unwrap(),
        vec![forgejo_conn(), github_conn()], // ordered by id
    );
    assert_eq!(
        db.get_connection("gh-acme").await.unwrap(),
        Some(github_conn())
    );

    // Upsert: rotating the credential handle replaces in place.
    let mut rotated = github_conn();
    rotated.credential_ref = "gh-acme-app-pem-v2".into();
    db.put_connection(&rotated).await.unwrap();
    assert_eq!(
        db.get_connection("gh-acme").await.unwrap(),
        Some(rotated.clone())
    );

    // Bind repos to their governed Projects.
    db.bind_repo("gh-acme", &repo("acme", "web"), "acme", "web")
        .await
        .unwrap();
    db.bind_repo("gh-acme", &repo("acme", "api"), "acme", "api")
        .await
        .unwrap();
    db.bind_repo(
        "codeberg-acme",
        &repo("acme-mirror", "web"),
        "acme",
        "web-mirror",
    )
    .await
    .unwrap();
    assert_eq!(
        db.repos_of("gh-acme").await.unwrap(),
        vec![repo("acme", "api"), repo("acme", "web")],
    );

    // resolve: RepoRef → owning Project + serving connection (with the rotated
    // credential handle, never the secret bytes).
    let hit = db
        .resolve(&repo("acme", "web"))
        .await
        .unwrap()
        .expect("registered");
    assert_eq!(hit.org, "acme");
    assert_eq!(hit.project, "web");
    assert_eq!(hit.connection, rotated);
    let hit = db
        .resolve(&repo("acme-mirror", "web"))
        .await
        .unwrap()
        .expect("registered");
    assert_eq!(hit.connection.kind, ForgeKind::Forgejo);
    assert_eq!(hit.project, "web-mirror");

    // An unregistered repo resolves to None (its webhooks are dropped).
    assert!(db
        .resolve(&repo("stranger", "danger"))
        .await
        .unwrap()
        .is_none());

    // Re-binding re-homes the repo (upsert by coordinate).
    db.bind_repo("codeberg-acme", &repo("acme", "api"), "acme", "api-moved")
        .await
        .unwrap();
    let moved = db.resolve(&repo("acme", "api")).await.unwrap().unwrap();
    assert_eq!(moved.connection.id, "codeberg-acme");
    assert_eq!(moved.project, "api-moved");

    // Unbind is idempotent and removes resolution.
    db.unbind_repo("codeberg-acme", &repo("acme", "api"))
        .await
        .unwrap();
    db.unbind_repo("codeberg-acme", &repo("acme", "api"))
        .await
        .unwrap();
    assert!(db.resolve(&repo("acme", "api")).await.unwrap().is_none());

    // Deleting a connection cascades its bindings.
    db.delete_connection("gh-acme").await.unwrap();
    assert!(db.get_connection("gh-acme").await.unwrap().is_none());
    assert!(db.resolve(&repo("acme", "web")).await.unwrap().is_none());
    assert!(db.repos_of("gh-acme").await.unwrap().is_empty());

    tdb.cleanup().await;
}

/// The single-owner marker (ADR-0060 part D) against real Postgres. Ownership has
/// to be **durable** for the rule to work at all: it is the only way a second
/// boot can tell "the row I provisioned from config" (safe to overwrite) from "a
/// row a human created" (a collision that must refuse the boot).
#[tokio::test]
async fn connection_ownership_is_durable_and_defaults_to_the_database() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    // A connection created the pre-0060 way (API/UI, or the installation
    // webhook) is DB-owned — nothing becomes read-only by accident.
    db.put_connection(&github_conn()).await.unwrap();
    db.put_connection(&forgejo_conn()).await.unwrap();
    assert!(db.config_owned_connection_ids().await.unwrap().is_empty());

    // Boot provisioning claims one.
    db.set_connection_owned_by_config("codeberg-acme", true)
        .await
        .unwrap();
    assert_eq!(
        db.config_owned_connection_ids().await.unwrap(),
        vec!["codeberg-acme".to_string()]
    );

    // Re-provisioning (`put_connection` again, as every boot does) must not
    // clear the claim — otherwise the connection would flip to editable for the
    // rest of the process's life.
    let mut moved = forgejo_conn();
    moved.base_url = "https://codeberg.example".into();
    db.put_connection(&moved).await.unwrap();
    assert_eq!(
        db.config_owned_connection_ids().await.unwrap(),
        vec!["codeberg-acme".to_string()]
    );
    assert_eq!(
        db.get_connection("codeberg-acme").await.unwrap(),
        Some(moved)
    );

    // Releasing (config stopped declaring it) hands ownership back without
    // touching the connection itself.
    db.set_connection_owned_by_config("codeberg-acme", false)
        .await
        .unwrap();
    assert!(db.config_owned_connection_ids().await.unwrap().is_empty());
    assert!(db.get_connection("codeberg-acme").await.unwrap().is_some());

    // Marking an absent connection is a no-op, not an insert.
    db.set_connection_owned_by_config("ghost", true)
        .await
        .unwrap();
    assert!(db.config_owned_connection_ids().await.unwrap().is_empty());

    tdb.cleanup().await;
}
