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
        org: Some("acme".into()),
        installation_id: Some(41),
    }
}

fn forgejo_conn() -> ForgeConnection {
    ForgeConnection {
        id: "codeberg-acme".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://codeberg.org".into(),
        credential_ref: "codeberg-acme-token".into(),
        org: Some("acme".into()),
        installation_id: None,
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
        &repo("acme", "web-mirror"),
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
        .resolve(&repo("acme", "web-mirror"))
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

    // Re-binding re-homes the repo (upsert by coordinate) between two
    // connections of one org.
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

/// git-bug 19b0a45: a connection serves one Org, and no bind moves a repo
/// out of the Org it is bound in. A bind through a connection of another
/// Org is refused by the store itself, so every bind path (manual, re-sync,
/// installation sync, config) gets the same answer.
#[tokio::test]
async fn a_bind_never_moves_a_repo_across_orgs() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    db.put_connection(&github_conn()).await.unwrap();
    let globex = ForgeConnection {
        id: "gh-globex".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "github-app".into(),
        org: Some("globex".into()),
        installation_id: Some(42),
    };
    db.put_connection(&globex).await.unwrap();
    let legacy = ForgeConnection {
        id: "legacy".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://git.example".into(),
        credential_ref: "legacy-token".into(),
        org: None,
        installation_id: None,
    };
    db.put_connection(&legacy).await.unwrap();

    // An assigned connection binds only its own Org's repos.
    let err = db
        .bind_repo("gh-globex", &repo("acme", "web"), "acme", "web")
        .await
        .expect_err("globex's connection cannot bind an acme repo");
    assert!(
        matches!(err, scarab_forge::RegistryError::OrgMismatch { .. }),
        "{err:?}"
    );

    // A repo bound in acme stays there: an unassigned connection's re-bind
    // is refused, and the binding is unchanged.
    db.bind_repo("gh-acme", &repo("acme", "web"), "acme", "web")
        .await
        .unwrap();
    let err = db
        .bind_repo("legacy", &repo("acme", "web"), "acme", "web")
        .await
        .expect_err("a bind does not move a repo out of its org");
    assert!(
        matches!(err, scarab_forge::RegistryError::BoundElsewhere { ref connection, .. } if connection == "gh-acme"),
        "{err:?}"
    );
    assert_eq!(
        db.resolve(&repo("acme", "web"))
            .await
            .unwrap()
            .unwrap()
            .connection
            .id,
        "gh-acme"
    );

    // Off an unassigned connection, a repo moves.
    db.bind_repo("legacy", &repo("acme", "old"), "acme", "old")
        .await
        .unwrap();
    db.bind_repo("gh-acme", &repo("acme", "old"), "acme", "old")
        .await
        .unwrap();
    let moved = db.resolve(&repo("acme", "old")).await.unwrap().unwrap();
    assert_eq!(moved.connection.id, "gh-acme");
    assert_eq!(moved.connection.org.as_deref(), Some("acme"));
    assert_eq!(moved.connection.installation_id, Some(41));

    tdb.cleanup().await;
}

/// git-bug 19b0a45: the Org record. A connection's Org and a binding's Org
/// enter it, and it outlives the connection that brought it.
#[tokio::test]
async fn orgs_are_recorded_and_outlive_their_connections() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    assert!(db.list_orgs().await.unwrap().is_empty());

    db.put_connection(&github_conn()).await.unwrap();
    let legacy = ForgeConnection {
        id: "legacy".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://git.example".into(),
        credential_ref: "legacy-token".into(),
        org: None,
        installation_id: None,
    };
    db.put_connection(&legacy).await.unwrap();
    db.bind_repo("legacy", &repo("initech", "tps"), "initech", "tps")
        .await
        .unwrap();
    assert_eq!(
        db.list_orgs().await.unwrap(),
        vec!["acme".to_string(), "initech".to_string()]
    );

    db.delete_connection("gh-acme").await.unwrap();
    db.delete_connection("legacy").await.unwrap();
    assert_eq!(
        db.list_orgs().await.unwrap(),
        vec!["acme".to_string(), "initech".to_string()]
    );

    tdb.cleanup().await;
}

/// git-bug 19b0a45: a repo moves only off an unassigned connection. A
/// second connection of the same Org does not take it either: the admin
/// unbinds it where it is first. Re-binding through the connection that
/// holds it still updates the binding.
#[tokio::test]
async fn a_bind_never_moves_a_repo_off_a_connection_that_serves_an_org() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    db.put_connection(&github_conn()).await.unwrap();
    db.put_connection(&forgejo_conn()).await.unwrap();

    db.bind_repo("gh-acme", &repo("acme", "web"), "acme", "web")
        .await
        .unwrap();
    let err = db
        .bind_repo("codeberg-acme", &repo("acme", "web"), "acme", "web")
        .await
        .expect_err("a same-org connection does not take the repo");
    assert!(
        matches!(err, scarab_forge::RegistryError::BoundElsewhere { ref connection, .. } if connection == "gh-acme"),
        "{err:?}"
    );
    db.bind_repo("gh-acme", &repo("acme", "web"), "acme", "web-renamed")
        .await
        .unwrap();
    let held = db.resolve(&repo("acme", "web")).await.unwrap().unwrap();
    assert_eq!(held.connection.id, "gh-acme");
    assert_eq!(held.project, "web-renamed");

    tdb.cleanup().await;
}

/// git-bug 19b0a45: two binds of the same unbound repo at once, through two
/// connections that serve an Org. The guard is in the upsert itself, so one
/// wins and the other is refused; a check made before the write would let
/// both pass and the second overwrite the first.
#[tokio::test]
async fn two_concurrent_binds_of_an_unbound_repo_do_not_both_win() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    db.put_connection(&github_conn()).await.unwrap();
    db.put_connection(&forgejo_conn()).await.unwrap();

    for i in 0..16 {
        let r = repo("acme", &format!("race-{i}"));
        let (a, b) = tokio::join!(
            db.bind_repo("gh-acme", &r, "acme", &r.name),
            db.bind_repo("codeberg-acme", &r, "acme", &r.name),
        );
        assert!(
            a.is_ok() != b.is_ok(),
            "exactly one bind of {} wins: {a:?} {b:?}",
            r.name
        );
        let winner = if a.is_ok() {
            "gh-acme"
        } else {
            "codeberg-acme"
        };
        assert_eq!(db.resolve(&r).await.unwrap().unwrap().connection.id, winner);
    }

    tdb.cleanup().await;
}

/// git-bug 19b0a45: during a rolling upgrade a server built before the
/// `orgs` table binds repos without recording their Org. The Org list still
/// names it, so its admins are not refused on every connection route.
#[tokio::test]
async fn the_org_list_includes_orgs_only_a_binding_names() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    db.put_connection(&forgejo_conn()).await.unwrap();
    // What an old server writes: the binding, and no `orgs` row.
    sqlx::query(
        "INSERT INTO forge_repos (connection_id, owner, name, org, project)
         VALUES ('codeberg-acme', 'globex', 'site', 'globex', 'site')",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    assert_eq!(
        db.list_orgs().await.unwrap(),
        vec!["acme".to_string(), "globex".to_string()]
    );

    tdb.cleanup().await;
}
