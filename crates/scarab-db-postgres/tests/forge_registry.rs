//! ForgeConnection registry acceptance against real Postgres (ADR-0046):
//! connection CRUD, repo binding, and RepoRef → Project resolution — the seam
//! that answers *which forge, which base URL, which credentials* for a repo.
//!
//! Skips cleanly when SCARAB_TEST_DATABASE_URL is unset (see `common`).

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, OrgAssignView, RepoRef};

fn github_conn() -> ForgeConnection {
    ForgeConnection {
        id: "gh-acme".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "gh-acme-app-pem".into(),
        org: Some("acme".into()),
        installation_id: Some(41),
        account_id: None,
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
        account_id: None,
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

    // A repo does not move between two connections that both serve its org:
    // the second bind is refused and the repo stays where it was.
    let refused = db
        .bind_repo("codeberg-acme", &repo("acme", "api"), "acme", "api-moved")
        .await
        .unwrap_err();
    assert!(
        matches!(refused, scarab_forge::RegistryError::BoundElsewhere { .. }),
        "{refused:?}"
    );
    let kept = db.resolve(&repo("acme", "api")).await.unwrap().unwrap();
    assert_eq!(kept.connection.id, "gh-acme");
    assert_eq!(kept.project, "api");

    // Unbind is idempotent and removes resolution.
    db.unbind_repo("gh-acme", &repo("acme", "api")).await.unwrap();
    db.unbind_repo("gh-acme", &repo("acme", "api")).await.unwrap();
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
        account_id: None,
    };
    db.put_connection(&globex).await.unwrap();
    let legacy = ForgeConnection {
        id: "legacy".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://git.example".into(),
        credential_ref: "legacy-token".into(),
        org: None,
        installation_id: None,
        account_id: None,
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
        account_id: None,
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

/// Wait until a backend of this test's database is blocked on a lock, so a
/// step the test spawned is known to be waiting behind the transaction the
/// test holds open. The interleaving is constructed, not raced: the test
/// goes on only once the wait is seen.
async fn until_a_backend_waits(pool: &sqlx::PgPool) {
    for _ in 0..1000 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
              WHERE NOT l.granted AND a.datname = current_database()",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the spawned step never waited on a lock");
}

/// git-bug 19b0a45: a repo held by a connection of another Org (written
/// there by an old re-sync, or by a config whose `org:` changed) moves to
/// a connection of its owner's Org: the holder does not serve its owner, so
/// the repo is not that Org's to keep. Before, only an unassigned holder
/// let a repo go, and such a repo could not leave.
#[tokio::test]
async fn a_repo_held_by_another_orgs_connection_moves_to_its_owners() {
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
        account_id: None,
    };
    db.put_connection(&globex).await.unwrap();
    sqlx::query(
        "INSERT INTO forge_repos (connection_id, owner, name, org, project)
         VALUES ('gh-acme', 'globex', 'site', 'globex', 'site')",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();

    db.bind_repo("gh-globex", &repo("globex", "site"), "globex", "site")
        .await
        .unwrap();
    let moved = db.resolve(&repo("globex", "site")).await.unwrap().unwrap();
    assert_eq!(moved.connection.id, "gh-globex");

    tdb.cleanup().await;
}

/// git-bug 19b0a45: the re-home guard is the upsert's own `WHERE`. A bind
/// that reaches the write while another bind of the same repo is not yet
/// committed waits for it, and is then refused against the row as
/// committed. A check made before the write (it saw no row) would let the
/// second bind overwrite the first.
#[tokio::test]
async fn a_bind_waiting_on_an_uncommitted_bind_is_refused_once_it_commits() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    db.put_connection(&github_conn()).await.unwrap();
    db.put_connection(&forgejo_conn()).await.unwrap();

    // The first bind, written and not yet committed.
    let mut first = tdb.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO forge_repos (connection_id, owner, name, org, project)
         VALUES ('gh-acme', 'acme', 'race', 'acme', 'race')",
    )
    .execute(&mut *first)
    .await
    .unwrap();

    let second_db = PostgresDb::with_pool(tdb.pool.clone());
    let second = tokio::spawn(async move {
        second_db
            .bind_repo("codeberg-acme", &repo("acme", "race"), "acme", "race")
            .await
    });
    until_a_backend_waits(&tdb.pool).await;
    first.commit().await.unwrap();

    let err = second.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, scarab_forge::RegistryError::BoundElsewhere { connection, .. } if connection == "gh-acme"),
        "{err:?}"
    );
    assert_eq!(
        db.resolve(&repo("acme", "race"))
            .await
            .unwrap()
            .unwrap()
            .connection
            .id,
        "gh-acme"
    );

    tdb.cleanup().await;
}

/// An unassigned GitHub connection, as the upgrade leaves one.
fn unassigned(id: &str, kind: ForgeKind, base_url: &str) -> ForgeConnection {
    ForgeConnection {
        id: id.into(),
        kind,
        base_url: base_url.into(),
        credential_ref: format!("{id}-credential"),
        org: None,
        installation_id: None,
        account_id: None,
    }
}

/// Hold what an assign of `id` to `org` holds, and write it, without
/// committing: the Org's advisory lock and the connection row.
async fn assign_in_flight<'c>(
    pool: &sqlx::PgPool,
    id: &str,
    org: &str,
) -> sqlx::Transaction<'c, sqlx::Postgres> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("scarab:org:{org}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM forge_connections WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE forge_connections SET org = $2 WHERE id = $1")
        .bind(id)
        .bind(org)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

/// git-bug 19b0a45: a bind through a connection that is being assigned
/// waits for the assign, then is held to the Org it was given: a repo of
/// another owner is refused. Before the fix the bind read the connection
/// as unassigned outside its transaction and landed `globex/site` on a
/// connection that then served acme.
#[tokio::test]
async fn a_bind_through_a_connection_being_assigned_is_held_to_its_new_org() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    db.put_connection(&unassigned(
        "gh-new",
        ForgeKind::GitHub,
        "https://api.github.com",
    ))
    .await
    .unwrap();

    let assign = assign_in_flight(&tdb.pool, "gh-new", "acme").await;
    let bind_db = PostgresDb::with_pool(tdb.pool.clone());
    let bind = tokio::spawn(async move {
        bind_db
            .bind_repo("gh-new", &repo("globex", "site"), "globex", "site")
            .await
    });
    until_a_backend_waits(&tdb.pool).await;
    assign.commit().await.unwrap();

    let err = bind.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, scarab_forge::RegistryError::OrgMismatch { connection_org, .. } if connection_org == "acme"),
        "{err:?}"
    );
    assert!(db.repos_of("gh-new").await.unwrap().is_empty());

    tdb.cleanup().await;
}

/// git-bug 19b0a45: two assigns to one Org run one after the other. While a
/// Forgejo connection is being assigned to `newco`, an assign of a GitHub
/// connection to `newco` waits, then sees the Forgejo connection serving it
/// and is refused by a rule that keeps an Org on one forge. The trait's
/// default (read, then write, holding nothing) would see `newco` unserved
/// and assign both.
#[tokio::test]
async fn two_assigns_to_one_org_are_decided_one_after_the_other() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    db.put_connection(&unassigned(
        "fj-new",
        ForgeKind::Forgejo,
        "https://codeberg.org",
    ))
    .await
    .unwrap();
    db.put_connection(&unassigned(
        "gh-new",
        ForgeKind::GitHub,
        "https://api.github.com",
    ))
    .await
    .unwrap();

    let first = assign_in_flight(&tdb.pool, "fj-new", "newco").await;
    let second_db = PostgresDb::with_pool(tdb.pool.clone());
    let second = tokio::spawn(async move {
        second_db
            .assign_connection_org("gh-new", "newco", &|v: &OrgAssignView| {
                v.servers
                    .iter()
                    .find(|c| c.kind != v.conn.kind)
                    .map(|c| format!("served by {}", c.id))
            })
            .await
    });
    until_a_backend_waits(&tdb.pool).await;
    first.commit().await.unwrap();

    let outcome = second.await.unwrap().unwrap();
    assert_eq!(outcome, Err("served by fj-new".to_string()));
    assert_eq!(
        db.get_connection("gh-new").await.unwrap().unwrap().org,
        None
    );

    // With nothing in the way the same call assigns, and the Org is recorded.
    let outcome = db
        .assign_connection_org("gh-new", "other", &|_: &OrgAssignView| None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.org.as_deref(), Some("other"));
    assert!(db.list_orgs().await.unwrap().contains(&"other".to_string()));

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
